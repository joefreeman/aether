/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Dev-only: WebSocket base, e.g. ws://127.0.0.1:2384. Defaults to the page origin. */
  readonly VITE_AETHER_WS?: string;
  /** Optional: workspace to activate. Defaults to the first workspace the server lists. */
  readonly VITE_AETHER_WORKSPACE?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
