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

pub mod chips;
pub mod effect;
pub mod grid;
pub mod keymap;
pub mod labels;
pub mod markdown;
pub mod picker;
pub mod save_as;
pub mod scrollbar;
pub mod session;
pub mod transport;
pub mod update;
