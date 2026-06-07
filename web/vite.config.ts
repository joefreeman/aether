import { defineConfig } from "vite";

// The client talks to the daemon directly over WebSocket (auth is by token in the query string,
// so no cookie/CORS dance and no WS proxy is needed). In dev, point it at the running daemon with
// VITE_AETHER_WS / VITE_AETHER_TOKEN — see README. `vite build` emits dist/, which aether-server
// will embed and serve in production.
export default defineConfig({
  server: { port: 5173 },
  build: { outDir: "dist", emptyOutDir: true },
});
