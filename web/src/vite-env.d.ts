/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Dev-only: the daemon's per-run token, read from $XDG_RUNTIME_DIR/aether/server.json. */
  readonly VITE_AETHER_TOKEN?: string;
  /** Dev-only: WebSocket base, e.g. ws://127.0.0.1:2384. Defaults to the page origin. */
  readonly VITE_AETHER_WS?: string;
  /** Optional: project to activate. Defaults to the first project the server lists. */
  readonly VITE_AETHER_PROJECT?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}

interface Window {
  /** Injected by aether-server when serving the embedded bundle. */
  AETHER_TOKEN?: string;
}
