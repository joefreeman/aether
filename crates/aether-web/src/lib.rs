//! The browser's seam onto the shared client core (docs/web-core.md). `aether-client` is a pure,
//! sans-IO state machine that already compiles for `wasm32-unknown-unknown`; this crate is the thin
//! `wasm-bindgen` shell around it. It owns every `#[wasm_bindgen]` export and every boundary DTO so
//! `aether-client` itself never grows a wasm dependency.
//!
//! The contract mirrors `aether-tui/src/shell.rs`: the TS shell feeds input in (`on_key`,
//! `on_event`, `on_rpc_result`), gets back an `Effect[]` to execute, and reads a `View` to render.
//! The core's own Rust types never cross the boundary — only the JSON DTOs built here.

mod view;

use aether_client::effect::{Effect, Effects, RevealStyle, ShellAction, ToastKind};
use aether_client::keymap::{hover_action, HoverAction, KeyCode, Mods, ScrollDir, ScrollUnit};
use aether_client::session::{buffer_info, HoverText, PasteKind, Session};
use aether_client::transport::RpcError;
use aether_client::update::Event;
use aether_protocol::buffer::BufferOpenResult;
use aether_protocol::cursor::Granularity;
use aether_protocol::envelope::{JsonRpc, Notification};
use aether_protocol::viewport::{ViewportSubscribeResult, ViewportWindowResult};
use aether_protocol::LogicalPosition;
use serde_json::{json, Value};
use wasm_bindgen::prelude::*;

/// A live client session, wrapped for the browser. Construct one, then drive it with the `on_*`
/// methods; each returns the `Effect[]` the shell must execute (see [`effect_value`] for the shape).
#[wasm_bindgen]
pub struct WasmSession {
    inner: Session,
}

#[wasm_bindgen]
impl WasmSession {
    /// A placeholder session (no workspace, empty buffer). Phase 1 uses this to prove the boundary;
    /// the real constructor takes a bootstrapped buffer once `buffer/open` is wired (Phase 3).
    #[wasm_bindgen(constructor)]
    pub fn new() -> WasmSession {
        WasmSession {
            inner: Session::placeholder(),
        }
    }

    /// Feed a key. `key` is the browser `KeyboardEvent.key`; it's normalised to the core's
    /// [`KeyCode`] exactly as `aether-iced/src/input.rs` normalises iced keys. Returns `Effect[]`.
    pub fn on_key(
        &mut self,
        key: &str,
        ctrl: bool,
        alt: bool,
        shift: bool,
        visible_rows: u32,
    ) -> Result<JsValue, JsValue> {
        let effects = self.dispatch_key(key, ctrl, alt, shift, visible_rows);
        to_js(&effects)
    }

    /// The render view (docs/web-core.md): a JSON projection of the session for the shell to
    /// paint. Read after every batch of effects, before rendering.
    pub fn view(&self) -> Result<JsValue, JsValue> {
        to_js(&view::build_view(&self.inner))
    }

    /// Build a real session from the bootstrap landing buffer. `workspace_paths` is a JSON string
    /// array; `open` is the `buffer/open` result JSON. (The `new()` placeholder is for tests.)
    pub fn bootstrap(
        workspace: String,
        workspace_paths: JsValue,
        open: JsValue,
    ) -> Result<WasmSession, JsValue> {
        let paths: Vec<String> = from_js(workspace_paths)?;
        let open: BufferOpenResult = from_js(open)?;
        let buffer = buffer_info(open, &paths);
        Ok(WasmSession {
            inner: Session::new(workspace, paths, buffer),
        })
    }

    /// A server push (a JSON-RPC notification): `method` + `params` JSON. Returns `Effect[]`.
    pub fn on_event(&mut self, method: String, params: JsValue) -> Result<JsValue, JsValue> {
        let params: Value = from_js(params)?;
        to_js(&self.server_push(method, params))
    }

    /// The connection dropped: clear parked RPCs and transition connection state. Returns `Effect[]`.
    pub fn connection_lost(&mut self) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.on_event(Event::ConnectionLost)))
    }

    /// The outcome of a prior `Effect::Request`. On success `value` is the result JSON; on failure
    /// it's `{ code, message }`. `method` labels the error in any toast. Returns `Effect[]`.
    pub fn on_rpc_result(
        &mut self,
        token: u64,
        ok: bool,
        method: String,
        value: JsValue,
    ) -> Result<JsValue, JsValue> {
        let value: Value = from_js(value)?;
        to_js(&self.rpc_result(token, ok, &method, value))
    }

    /// Adopt a `viewport/subscribe` result (a geometry RPC the shell issued — see docs/web-core.md
    /// §"Two kinds of RPC"). The shell does its pixel positioning afterward, reading `view()`.
    pub fn adopt_subscribe(&mut self, res: JsValue) -> Result<(), JsValue> {
        let res: ViewportSubscribeResult = from_js(res)?;
        self.inner.adopt_subscribe(res);
        Ok(())
    }

    /// Adopt a window from a geometry RPC (`viewport/scroll`/`scroll_to_row`/`resize`).
    pub fn adopt_window(&mut self, res: JsValue) -> Result<(), JsValue> {
        let res: ViewportWindowResult = from_js(res)?;
        self.inner.adopt_window(res);
        Ok(())
    }

    /// Report the on-screen line range (the shell owns the pixel scroll) so sneak scopes its labels
    /// to what's actually visible. `top_visual_row` is absolute; `viewport_rows` is the visible
    /// height in rows.
    pub fn set_visible_lines(&mut self, top_visual_row: u32, viewport_rows: u32) {
        self.inner.set_visible_lines(top_visual_row, viewport_rows);
    }

    /// Pointer press at an already-resolved buffer position (the shell converts pixels → cell).
    /// `granularity` is `"char"`/`"word"`/`"line"`; `extend` is shift-click. Returns `Effect[]`.
    pub fn pointer_press(
        &mut self,
        line: u32,
        col: u32,
        granularity: &str,
        extend: bool,
    ) -> Result<JsValue, JsValue> {
        let g = match granularity {
            "word" => Granularity::Word,
            "line" => Granularity::Line,
            _ => Granularity::Char,
        };
        let fx = self
            .inner
            .pointer_press(LogicalPosition { line, col }, g, extend);
        to_js(&effects_to_json(fx))
    }

    /// Pointer drag to a new position while held — extends the selection from the press anchor.
    pub fn pointer_drag(&mut self, line: u32, col: u32) -> Result<JsValue, JsValue> {
        let fx = self.inner.pointer_drag(LogicalPosition { line, col });
        to_js(&effects_to_json(fx))
    }

    /// Pointer release — ends the drag.
    pub fn pointer_release(&mut self) {
        self.inner.pointer_release();
    }

    /// Capture a content scroll anchor before a wrap/diff re-layout (in response to the
    /// `SaveContentAnchor` effect). `top_row` is the absolute visual row at the top of the viewport
    /// (`(scrollTop - pad) / lineHeight`); `viewport_rows` its height in rows.
    pub fn capture_scroll_anchor(&mut self, top_row: u32, viewport_rows: u32) {
        self.inner.capture_scroll_anchor(top_row, viewport_rows);
    }

    /// Resolve the anchor captured by [`WasmSession::capture_scroll_anchor`] against the new window
    /// into the absolute visual row that should be at the top of the viewport, or `null` when no
    /// anchor is pending (the shell then reveals the cursor as usual). Call after adopting the
    /// re-laid-out window (wrap `set_wrap` result / diff `WindowAdopted`).
    pub fn resolve_scroll_anchor(&mut self) -> Option<u32> {
        self.inner.resolve_scroll_anchor()
    }

    /// Mouse wheel over the picker results list: move the highlighted row (+down / -up), refetching
    /// the window as needed. Returns `Effect[]`.
    pub fn picker_wheel(&mut self, delta: i32) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.picker_wheel(delta as i64)))
    }

    /// The results list was scrolled so its first visible row is `first_visible_row` (display rows;
    /// the shell converts `scrollTop / row_height`). Refetches the window around that scroll position
    /// when it has left the loaded range — WITHOUT moving the selection (free scroll, unlike
    /// `picker_wheel`). Returns `Effect[]` (empty when the window already covers the view).
    pub fn picker_scrolled(&mut self, first_visible_row: u32) -> Result<JsValue, JsValue> {
        let offset = self
            .inner
            .picker
            .as_ref()
            .and_then(|p| p.scrolled_refetch(first_visible_row));
        // Free pixel scroll — the view moved, not the selection — so the reply must not chase the
        // highlight back (`chase_selection = false`); that would fight the scroll and blank it.
        let fx = offset.map_or_else(Effects::none, |o| self.inner.picker_refetch(o, false));
        to_js(&effects_to_json(fx))
    }

    /// Replace the picker query (the shell's native `<input>` owns text editing and syncs the full
    /// value). Returns `Effect[]`.
    pub fn picker_set_query(&mut self, query: String) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.picker_set_query(query)))
    }

    /// A picker row was clicked (absolute index): highlight it and accept. Returns `Effect[]`.
    pub fn picker_click(&mut self, index: u32) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(
            self.inner.on_event(Event::PickerClicked(index)),
        ))
    }

    /// Dismiss the whole picker (chip editor included) — the shell's click-outside-the-panel gesture.
    /// Returns `Effect[]`.
    pub fn close_picker(&mut self) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.close_picker()))
    }

    /// Open the Workspaces picker — the no-args chooser raised at boot when no workspace was named (the
    /// native shells do the same). Returns `Effect[]`.
    pub fn open_workspaces(&mut self) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.open_picker(
            aether_protocol::picker::PickerKind::Workspaces,
            None,
            None,
            false,
        )))
    }

    /// Select the rightmost filter chip (Left/Backspace at the query start). Returns `Effect[]`.
    pub fn picker_select_last_chip(&mut self) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.picker_select_last_chip()))
    }

    /// Select the rightmost search option chip (Left/Backspace at the search query start). Returns
    /// `Effect[]`.
    pub fn search_select_last_chip(&mut self) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.search_select_last_chip()))
    }

    /// Replace the search query (native search `<input>` owns editing). Returns `Effect[]`.
    pub fn search_set_query(&mut self, query: String) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.search_set_query(query)))
    }

    /// Replace the save-as editor's path-field text (native `<input>` owns editing). Returns
    /// `Effect[]`.
    pub fn save_as_set_input(&mut self, text: String) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.save_as_set_input(text)))
    }

    /// Replace the multi-root save-as editor's root-filter text (native `<input>`). Returns
    /// `Effect[]`.
    pub fn save_as_set_root_filter(&mut self, text: String) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.save_as_set_root_filter(text)))
    }

    /// Replace the open-from-path prompt's path-field text (native `<input>` owns editing).
    /// Returns `Effect[]`.
    pub fn open_path_set_input(&mut self, text: String) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.open_path_set_input(text)))
    }

    /// Click an unfocused save-as segment to focus it (`root: true` for the root). Returns
    /// `Effect[]`.
    pub fn save_as_set_field(&mut self, root: bool) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.save_as_set_field(root)))
    }

    /// Replace the workspace-settings name field (native `<input>` owns editing). Returns `Effect[]`.
    pub fn workspace_settings_set_name(&mut self, text: String) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(
            self.inner.workspace_settings_set_name(text),
        ))
    }

    /// Replace the workspace-settings add-root input (native `<input>` owns editing). Returns `Effect[]`.
    pub fn workspace_settings_set_add(&mut self, text: String) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(
            self.inner.workspace_settings_set_add(text),
        ))
    }

    /// A root row's delete button was clicked (0-based index): open the shared confirm prompt for
    /// that root (same path as the Delete key). Returns `Effect[]`.
    pub fn workspace_settings_remove_root(&mut self, index: u32) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(
            self.inner
                .on_event(Event::WorkspaceSettingsRemoveRoot(index as usize)),
        ))
    }

    /// Replace the chip editor's path-field text (native `<input>` owns editing). Returns `Effect[]`.
    pub fn chip_editor_set_input(&mut self, text: String) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.chip_editor_set_input(text)))
    }

    /// Replace the multi-root dir editor's root-filter text (native `<input>`). Returns `Effect[]`.
    pub fn chip_editor_set_root_filter(&mut self, text: String) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(
            self.inner.chip_editor_set_root_filter(text),
        ))
    }

    /// Click an unfocused dir-editor segment to focus it (`root: true` for the root). Returns `Effect[]`.
    pub fn chip_editor_set_field(&mut self, root: bool) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.chip_editor_set_field(root)))
    }

    /// Flip soft-wrap on/off (the shell follows with a viewport/set_wrap). Returns `Effect[]`.
    pub fn toggle_wrap(&mut self) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.toggle_wrap()))
    }

    /// Toggle the app-settings checkbox at flat row `index` (a click in the overlay). Returns
    /// `Effect[]`.
    pub fn app_settings_toggle(&mut self, index: u32) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(
            self.inner.app_settings_toggle(index as usize),
        ))
    }

    /// Fetch the persisted application settings (`settings/get`) — the soft-wrap default, etc. The
    /// shell calls this once the session is live (at boot, and after a reconnect rebuilds it) and
    /// runs the returned effects. Returns `Effect[]`.
    pub fn startup(&mut self) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.startup()))
    }

    /// Insert literal text at the cursor (an IME composition commit). Returns `Effect[]`.
    pub fn insert_text(&mut self, text: String) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.insert_text(text)))
    }

    /// Deliver the system clipboard text the shell read in response to an `Effect::ReadClipboard`.
    /// `paste` is that effect's paste descriptor (round-tripped back so the core knows the gesture);
    /// `text` is the clipboard contents, or `null` if the read failed/was denied. Returns `Effect[]`.
    pub fn clipboard_read(
        &mut self,
        paste: JsValue,
        text: Option<String>,
    ) -> Result<JsValue, JsValue> {
        let paste: Value = from_js(paste)?;
        let kind = parse_paste(&paste)?;
        to_js(&effects_to_json(
            self.inner.on_event(Event::ClipboardRead(kind, text)),
        ))
    }

    /// Deliver the cursor-line blame the shell fetched and formatted. Like the TUI/iced shells'
    /// `maybe_blame`, the label ("author · 3w ago") is built shell-side because the sans-IO core
    /// deliberately lacks a clock (docs/protocol-composites.md, G); `text` is `null`/absent when the
    /// line has no blame. `buffer_id` is a JS number (`BufferId` ids stay well within f64). The core
    /// keeps it only if it still matches the current buffer + cursor line. Returns `Effect[]`.
    pub fn set_blame(
        &mut self,
        buffer_id: f64,
        line: u32,
        text: Option<String>,
    ) -> Result<JsValue, JsValue> {
        to_js(&effects_to_json(self.inner.on_event(Event::BlameLine {
            buffer_id: buffer_id as u64,
            line,
            text,
        })))
    }
}

/// Rebuild a [`PasteKind`] from the JSON descriptor `paste_value` emitted with `Effect::ReadClipboard`.
fn parse_paste(v: &Value) -> Result<PasteKind, JsValue> {
    let count = v.get("count").and_then(Value::as_u64).unwrap_or(1) as u32;
    match v.get("kind").and_then(Value::as_str) {
        Some("before") => Ok(PasteKind::Before { count }),
        Some("replace") => Ok(PasteKind::Replace { count }),
        Some("at_cursor") => Ok(PasteKind::AtCursor),
        Some("line") => Ok(PasteKind::Line),
        other => Err(JsValue::from_str(&format!("unknown paste kind: {other:?}"))),
    }
}

impl Default for WasmSession {
    fn default() -> Self {
        Self::new()
    }
}

impl WasmSession {
    /// The wrapped core session — test-only access for the view-builder unit tests (the shipping
    /// `view()` reads `self.inner` directly).
    #[cfg(test)]
    pub(crate) fn session(&self) -> &Session {
        &self.inner
    }

    /// The host-testable core of [`WasmSession::on_key`]: normalise, dispatch, lower the effects to
    /// JSON. Kept separate from the `JsValue` conversion so it runs under `cargo test` (no wasm).
    fn dispatch_key(
        &mut self,
        key: &str,
        ctrl: bool,
        alt: bool,
        shift: bool,
        visible_rows: u32,
    ) -> Vec<Value> {
        let Some(code) = parse_keycode(key) else {
            return Vec::new();
        };
        let mods = Mods { ctrl, alt, shift };
        let text = key_text(key, &mods);
        let fx = self.inner.on_key(code, mods, text, visible_rows);
        effects_to_json(fx)
    }

    /// Host-testable core of [`WasmSession::on_event`]: wrap the push as a `Notification` and feed
    /// it to the session.
    fn server_push(&mut self, method: String, params: Value) -> Vec<Value> {
        let notif = Notification {
            jsonrpc: JsonRpc,
            method,
            params,
        };
        effects_to_json(self.inner.on_event(Event::ServerPush(notif)))
    }

    /// Host-testable core of [`WasmSession::on_rpc_result`]: rebuild the `Result` the parked mapping
    /// expects (an [`RpcError`] on failure, with `method` interned for its `&'static` label).
    fn rpc_result(&mut self, token: u64, ok: bool, method: &str, value: Value) -> Vec<Value> {
        let result = if ok {
            Ok(value)
        } else {
            let code = value.get("code").and_then(Value::as_i64).unwrap_or(0) as i32;
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string();
            Err(RpcError {
                method: intern(method),
                code,
                message,
            })
        };
        effects_to_json(self.inner.on_rpc_result(token, result))
    }
}

/// Resolve a key for an open hover popover, reusing the editor's own Copy / Scroll bindings (see
/// [`aether_client::keymap::hover_action`]) so the web popover's keys never drift from the keymap.
/// Returns `null` for a non-popover key (the shell then dismisses the popover), `{kind:"copy"}`, or
/// `{kind:"scroll", down:bool, unit:"line"|"half"|"page"}`.
#[wasm_bindgen]
pub fn hover_key(key: &str, ctrl: bool, alt: bool, shift: bool) -> Result<JsValue, JsValue> {
    let value = parse_keycode(key)
        .and_then(|code| hover_action(code, Mods { ctrl, alt, shift }))
        .map(|action| match action {
            HoverAction::Copy => json!({ "kind": "copy" }),
            HoverAction::Scroll { dir, unit } => json!({
                "kind": "scroll",
                "down": matches!(dir, ScrollDir::Down),
                "unit": match unit {
                    ScrollUnit::Line => "line",
                    ScrollUnit::Half => "half",
                    ScrollUnit::Page => "page",
                },
            }),
        })
        .unwrap_or(Value::Null);
    to_js(&value)
}

// ---- key normalisation (mirrors aether-iced/src/input.rs) -----------------------------------

/// Browser `KeyboardEvent.key` → the core's [`KeyCode`]. `None` for keys we don't bind (modifier
/// keys, function keys — any multi-char name that isn't one of the named keys below).
fn parse_keycode(key: &str) -> Option<KeyCode> {
    Some(match key {
        "Escape" => KeyCode::Esc,
        "Enter" => KeyCode::Enter,
        "Tab" => KeyCode::Tab,
        "Backspace" => KeyCode::Backspace,
        "Delete" => KeyCode::Delete,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "ArrowLeft" => KeyCode::Left,
        "ArrowRight" => KeyCode::Right,
        "ArrowUp" => KeyCode::Up,
        "ArrowDown" => KeyCode::Down,
        " " => KeyCode::Char(' '),
        s => {
            let mut chars = s.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None; // a named key we don't bind
            }
            KeyCode::Char(c.to_ascii_lowercase())
        }
    })
}

/// The text a key types, if any: a single printable char with neither Ctrl nor Alt held. Named
/// keys (multi-char `key`) type nothing; space types `" "`.
fn key_text(key: &str, mods: &Mods) -> Option<String> {
    if mods.ctrl || mods.alt {
        return None;
    }
    let mut chars = key.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None; // named key, not text
    }
    Some(c.to_string())
}

// ---- effect lowering (Effect -> JSON; see docs/web-core.md for the contract) ------------------

fn effects_to_json(fx: Effects) -> Vec<Value> {
    fx.0.into_iter().map(effect_value).collect()
}

/// Lower one [`Effect`] to its boundary JSON. The shell matches on `tag`; `params` and RPC results
/// ride as raw JSON so the shell never reconstructs protocol types.
fn effect_value(e: Effect) -> Value {
    match e {
        Effect::Request {
            token,
            method,
            params,
        } => json!({ "tag": "Request", "token": token, "method": method, "params": params }),
        Effect::Toast {
            message,
            kind,
            group,
        } => {
            json!({ "tag": "Toast", "message": message, "level": toast_level(kind), "group": group })
        }
        Effect::WriteClipboard(text) => json!({ "tag": "WriteClipboard", "text": text }),
        Effect::ReadClipboard(paste) => {
            json!({ "tag": "ReadClipboard", "paste": paste_value(&paste) })
        }
        Effect::RevealCursor(style) => json!({
            "tag": "RevealCursor",
            "style": match style {
                RevealStyle::Follow => "follow",
                RevealStyle::Jump => "jump",
            },
        }),
        Effect::Resubscribe => json!({ "tag": "Resubscribe" }),
        Effect::SaveScrollAnchor => json!({ "tag": "SaveScrollAnchor" }),
        Effect::RestoreScrollAnchor => json!({ "tag": "RestoreScrollAnchor" }),
        Effect::SaveContentAnchor => json!({ "tag": "SaveContentAnchor" }),
        Effect::ShowHover(hover) => json!({ "tag": "ShowHover", "hover": hover_value(hover) }),
        Effect::DismissHover => json!({ "tag": "DismissHover" }),
        Effect::WindowAdopted => json!({ "tag": "WindowAdopted" }),
        Effect::RevealPickerSelection(reveal) => {
            json!({ "tag": "RevealPickerSelection", "reveal": reveal_value(reveal) })
        }
        Effect::PickerScrollReset => json!({ "tag": "PickerScrollReset" }),
        Effect::Reconnect { attempt } => json!({ "tag": "Reconnect", "attempt": attempt }),
        Effect::Exit => json!({ "tag": "Exit" }),
        Effect::ToChooser => json!({ "tag": "ToChooser" }),
        Effect::ShellAction(action) => {
            json!({ "tag": "ShellAction", "action": action_value(&action) })
        }
    }
}

fn toast_level(kind: ToastKind) -> &'static str {
    match kind {
        ToastKind::Info => "info",
        ToastKind::Error => "error",
        ToastKind::Warning => "warning",
        ToastKind::Success => "success",
    }
}

fn paste_value(p: &PasteKind) -> Value {
    match p {
        PasteKind::Before { count } => json!({ "kind": "before", "count": count }),
        PasteKind::Replace { count } => json!({ "kind": "replace", "count": count }),
        PasteKind::AtCursor => json!({ "kind": "at_cursor" }),
        PasteKind::Line => json!({ "kind": "line" }),
    }
}

fn hover_value(h: HoverText) -> Value {
    match h {
        HoverText::Blocks(blocks) => json!({
            "kind": "blocks",
            "blocks": blocks.iter().map(|b| json!({
                "text": b.text,
                "severity": b.severity.map(aether_client::session::severity_label),
            })).collect::<Vec<_>>(),
        }),
        // The parsed AST (Block/Inline derive Serialize) — the web shell renders it to DOM.
        HoverText::Markdown(blocks) => json!({
            "kind": "markdown",
            "blocks": serde_json::to_value(&blocks).unwrap_or(Value::Null),
        }),
    }
}

fn reveal_value(r: aether_client::picker::Reveal) -> Value {
    use aether_client::picker::Reveal;
    Value::String(
        match r {
            Reveal::Minimal => "minimal",
            Reveal::Top => "top",
        }
        .into(),
    )
}

/// Lower a [`ShellAction`] to the JSON the web shell's `runShellAction` consumes
/// (`{ name, dir?, unit?, fraction? }`): scrolling, cursor placement, wrap.
fn action_value(a: &ShellAction) -> Value {
    let dbg = |x: &dyn std::fmt::Debug| format!("{x:?}").to_lowercase();
    match a {
        ShellAction::Scroll { dir, unit } => {
            json!({ "name": "scroll", "dir": dbg(dir), "unit": dbg(unit) })
        }
        ShellAction::PlaceCursor(place) => {
            json!({ "name": "place_cursor", "fraction": place.fraction() })
        }
        ShellAction::ToggleWrap => json!({ "name": "toggle_wrap" }),
        // The web shell opens a new browser tab on the same URL (`window.open`). The target payload
        // is ignored: `Space Alt-x` duplicates the current tab, and the picker's Ctrl-Enter is
        // handled shell-side on the web (rows are `<a>` links, `onPickerInputKey` intercepts it), so
        // the picker path never reaches here.
        ShellAction::NewWindow(_) => json!({ "name": "new_window" }),
    }
}

fn to_js<T: serde::Serialize>(v: &T) -> Result<JsValue, JsValue> {
    // Two non-default options, both essential:
    // - `serialize_maps_as_objects`: the default serialises JSON objects as ES `Map`s, which
    //   `JSON.stringify` (the transport) renders as `{}` and `.field` access can't read.
    // - `serialize_missing_as_null`: the default serialises `Value::Null`/`None` as `undefined`, so
    //   the shell's `=== null`/`!== null` checks (e.g. `view.pending`, `view.picker`) misfire. Map
    //   them to JS `null` so a "no value" reads as `null` on the TS side.
    let ser = serde_wasm_bindgen::Serializer::new()
        .serialize_maps_as_objects(true)
        .serialize_missing_as_null(true);
    v.serialize(&ser)
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

fn from_js<T: serde::de::DeserializeOwned>(v: JsValue) -> Result<T, JsValue> {
    serde_wasm_bindgen::from_value(v).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Intern an RPC method name to the `&'static str` the core's [`RpcError`] wants. The set of method
/// names is finite (the `RpcMethod` constants), so this is bounded — a leak only on first sight of
/// each distinct name, the wasm equivalent of the native shells' `M::NAME` literals.
fn intern(method: &str) -> &'static str {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static POOL: RefCell<HashMap<String, &'static str>> = RefCell::new(HashMap::new());
    }
    POOL.with(|p| {
        if let Some(&s) = p.borrow().get(method) {
            return s;
        }
        let leaked: &'static str = Box::leak(method.to_string().into_boxed_str());
        p.borrow_mut().insert(method.to_string(), leaked);
        leaked
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycode_normalises_letters_named_and_space() {
        assert_eq!(parse_keycode("H"), Some(KeyCode::Char('h')));
        assert_eq!(parse_keycode("?"), Some(KeyCode::Char('?')));
        assert_eq!(parse_keycode(" "), Some(KeyCode::Char(' ')));
        assert_eq!(parse_keycode("Escape"), Some(KeyCode::Esc));
        assert_eq!(parse_keycode("ArrowLeft"), Some(KeyCode::Left));
        assert_eq!(parse_keycode("Shift"), None);
        assert_eq!(parse_keycode("F1"), None);
    }

    #[test]
    fn key_text_only_for_unmodified_printables() {
        let none = Mods::default();
        let ctrl = Mods {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(key_text("a", &none), Some("a".into()));
        assert_eq!(key_text(" ", &none), Some(" ".into()));
        assert_eq!(key_text("a", &ctrl), None);
        assert_eq!(key_text("Enter", &none), None);
    }

    #[test]
    fn entering_insert_mode_dispatches_through_the_core() {
        // `i` enters Insert mode — proves a key crosses into the core and produces a real effect
        // list (the whole point of Phase 1's boundary), without needing a live server.
        let mut s = WasmSession::new();
        let _effects = s.dispatch_key("i", false, false, false, 40);
        assert_eq!(s.inner.mode, aether_client::session::Mode::Insert);
    }

    #[test]
    fn intern_is_stable_and_bounded() {
        let a = intern("cursor/move");
        let b = intern("cursor/move");
        assert!(std::ptr::eq(a, b)); // same name → same leaked pointer, not a fresh leak
    }

    #[test]
    fn rpc_result_for_unknown_token_is_a_noop() {
        let mut s = WasmSession::new();
        // No request was parked under token 99 — the outcome is dropped, no effects.
        let fx = s.rpc_result(99, true, "cursor/move", json!({}));
        assert!(fx.is_empty());
    }

    #[test]
    fn server_push_with_unknown_method_is_ignored() {
        let mut s = WasmSession::new();
        let fx = s.server_push("nonsense/event".into(), json!({}));
        assert!(fx.is_empty());
    }

    #[test]
    fn set_blame_surfaces_on_the_cursor_line() {
        // A fresh session sits on buffer 0, line 0 — the blame the shell hands over matches the
        // cursor line, so the core keeps it and the view exposes it for the renderer. Proves the
        // `set_blame` boundary (shell-formatted label → core → view) end to end.
        let mut s = WasmSession::new();
        s.inner.on_event(Event::BlameLine {
            buffer_id: s.inner.buffer.buffer_id,
            line: 0,
            text: Some("ada · 3w ago".into()),
        });
        let v = view::build_view(&s.inner);
        assert_eq!(v["blame"]["line"], 0);
        assert_eq!(v["blame"]["text"], "ada · 3w ago");
    }

    #[test]
    fn set_blame_for_another_line_is_dropped() {
        // Blame fetched for a line the cursor has since left is discarded, never shown stale.
        let mut s = WasmSession::new();
        s.inner.on_event(Event::BlameLine {
            buffer_id: s.inner.buffer.buffer_id,
            line: 7,
            text: Some("grace · 1d ago".into()),
        });
        assert!(view::build_view(&s.inner)["blame"].is_null());
    }

    #[test]
    fn effect_lowering_tags_match_the_contract() {
        let reveal = effect_value(Effect::RevealCursor(RevealStyle::Jump));
        assert_eq!(reveal["tag"], "RevealCursor");
        assert_eq!(reveal["style"], "jump");
        let toast = effect_value(Effect::Toast {
            message: "hi".into(),
            kind: ToastKind::Error,
            group: Some("connection".into()),
        });
        assert_eq!(toast["tag"], "Toast");
        assert_eq!(toast["level"], "error");
        assert_eq!(toast["group"], "connection");
        let req = effect_value(Effect::Request {
            token: 7,
            method: "cursor/move",
            params: json!({ "motion": "char" }),
        });
        assert_eq!(req["tag"], "Request");
        assert_eq!(req["token"], 7);
        assert_eq!(req["method"], "cursor/move");
    }
}
