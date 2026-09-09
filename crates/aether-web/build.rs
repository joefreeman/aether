// The web client is its own leaf: the wasm bundle stamps the commit it was built from exactly as
// the `ae` binary does, so the about panel can say which build the browser is running. One script,
// included rather than copied.
include!("../aether-ae/build.rs");
