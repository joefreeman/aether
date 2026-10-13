/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Optional: workspace to activate. Defaults to the first workspace the server lists. */
  readonly VITE_AETHER_WORKSPACE?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
