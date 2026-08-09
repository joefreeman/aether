//! The platform-free Aether client (docs/client-core.md): modal input model, keymap
//! tables, picker/chip state, session state, and the update function — everything a client
//! *is*, minus rendering and transport. Shells (`aether-iced` today; a TUI port and perhaps
//! a wasm + DOM shell later) feed events in, execute the returned [`effect::Effect`]s, and
//! paint the state.
//!
//! The membership test is portability: everything here must compile for every conceivable
//! shell, wasm included. Native transport (the WebSocket actor) and discovery (reading
//! `$XDG_RUNTIME_DIR`) are *shell* concerns — shared between the native shells, perhaps,
//! but a browser shell bridges `web-sys` sockets and needs no discovery. (Known debt for
//! an actual wasm shell: a `Send`-bound feature toggle on the effect futures.)

pub mod app_info;
pub mod chips;
pub mod effect;
pub mod grid;
pub mod hints;
pub mod keymap;
pub mod labels;
// The markdown block model lives in the shared `aether-markdown` crate (docs/markdown-view.md
// §12 phase 3a) — the server resolves structural edits against the same parse the reading view
// renders from. Re-exported under the old path so shells and the wasm boundary are unchanged.
pub use aether_markdown as markdown;
pub mod path_editor;
pub mod picker;
pub mod read_layout;
pub mod scrollbar;
pub mod session;
pub mod transport;
pub mod update;
