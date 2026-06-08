import { defineConfig } from "vite";

// The client talks to the daemon directly over WebSocket (auth is by token in the query string,
// so no cookie/CORS dance and no WS proxy is needed). In dev, point it at the running daemon with
// VITE_AETHER_WS / VITE_AETHER_TOKEN — see README. `vite build` emits dist/, whose JS/CSS bundle
// aether-server serves under a fixed, server-owned index.html in production.
export default defineConfig({
  server: { port: 5173 },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // Stable, unhashed filenames so aether-server can host a fixed index.html that references
    // `/assets/index.js` + `/assets/index.css` directly — the bundle is the only build artifact it
    // serves. (No content hash means no cache-busting; the server sends `Cache-Control: no-store`.)
    rollupOptions: {
      output: {
        entryFileNames: "assets/index.js",
        chunkFileNames: "assets/[name].js",
        assetFileNames: "assets/[name][extname]",
      },
    },
  },
});
