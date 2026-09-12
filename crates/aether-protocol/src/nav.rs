//! Navigation history — browser-style back/forward across files. (Not the *jumplist* —
//! that's the captured picker-results list, [`crate::jumplist`].)
//!
//! Semantics deliberately mirror browser history: a qualifying jump records a back-entry and
//! truncates the forward stack; there is no interior reordering or dedup (only the client's own
//! "this jump didn't move me" check gates recording). This keeps the terminal client and the web
//! client — which rides the *native* browser history — behaving identically.
//!
//! - Native shells: step the server-side trail via [`NavStep`] with a `direction` (the `Alt-Left` /
//!   `Alt-Right` keys). Recording the origin happens as part of the navigating `view/open`
//!   (its `record_nav_from` field), not a separate call.
//! - Web: uses native browser history + `popstate`; it only needs [`NavGoto`] to restore a stored
//!   entry (open the buffer, reopening a closed file by path, and restore the full
//!   cursor/selection) without polluting the per-buffer motion-undo (`z`) history.
//!
//! The server keeps one trail per **(workspace context, client)**: an entry names its file relative
//! to the roots of the context that recorded it, so a step is always taken in the workspace the
//! client is standing in, and switching workspaces neither clears the trail you had nor carries it
//! across. A context also keeps the trail of the last client to leave it, so a fresh window
//! activating it starts from a copy rather than from nothing. None of that is on the wire — the
//! client names no trail; it asks for a direction and is told where it landed.

use crate::cursor::{CursorState, Direction};
use crate::envelope::RpcMethod;
use crate::view::ViewOpenResult;
use crate::BufferId;
use serde::{Deserialize, Serialize};

/// `nav/step` — step one entry through the nav history in `direction` (`Backward` = back,
/// `Forward` = forward, browser-style) and navigate there. The `Alt-Left` / `Alt-Right` keys.
///
/// The trail stepped is this client's, in the workspace it currently has active; a client with no
/// active workspace has none, and answers `target: None`.
pub struct NavStep;
impl RpcMethod for NavStep {
    const NAME: &'static str = "nav/step";
    type Params = NavStepParams;
    type Result = NavStepResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NavStepParams {
    /// The client's current buffer, pushed onto the opposite stack as we step. Passed explicitly
    /// (rather than inferred from a viewport) since a client may hold several viewports over its
    /// lifetime.
    pub buffer_id: BufferId,
    /// `Backward` walks the back stack (older locations), `Forward` the forward stack.
    pub direction: Direction,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NavStepResult {
    /// The buffer to switch to, with its cursor/selection already restored, or `None` when the
    /// end of the stack is reached (nothing to do).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ViewOpenResult>,
}

/// `nav/goto` — open a stored entry (reopening a closed file by `path_index`/`relative_path` when
/// its `view_id` is gone) and restore the full cursor/selection *without* recording a motion in
/// the per-buffer `z` history. Used by the web client on `popstate`; the back/forward stacks live
/// in the browser there, so this performs no stack bookkeeping.
pub struct NavGoto;
impl RpcMethod for NavGoto {
    const NAME: &'static str = "nav/goto";
    type Params = NavGotoParams;
    type Result = NavStepResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NavGotoParams {
    /// Preferred reference while the view is still open — it says which view of the file, and is
    /// the only handle a scratch has. Falls back to the path fields when it's gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_id: Option<crate::ViewId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative_path: Option<String>,
    /// Reopen handle for a **materialised revision** (a commit's patch, or a file at one), which
    /// has no path: `<repo>@<rev>[:<path>]`. Wins over the path fields when set, and regenerates
    /// the buffer if it has since closed — otherwise following a line out of a diff would leave
    /// nothing to step back to, since a transient patch closes as soon as nothing shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_key: Option<String>,
    /// The cursor/selection to restore (anchor + position). Clamped to the buffer's current
    /// bounds server-side. `match_bracket`/`jumplist_position` are recomputed and may be omitted.
    pub cursor: CursorState,
    /// Whether the entry was captured while **reading** a markdown file, so stepping back lands
    /// in that mode rather than whatever the file has since been shown as. Omitted (or `None`)
    /// leaves the mode to the server's memory, as an ordinary open does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<bool>,
}
