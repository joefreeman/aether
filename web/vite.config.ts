import { defineConfig } from "vite";

// The daemon only accepts a browser socket whose `Origin` is a page on its own port, so in dev the
// page never talks to it directly: Vite proxies the socket (`/aether-ws`) and the daemon's HTTP
// routes, presenting the daemon's own origin. AETHER_DAEMON picks the daemon (default: the
// `default` profile's port). `vite build` emits dist/, whose JS/CSS bundle aether-server serves
// under a fixed, server-owned index.html in production.
const daemon = process.env.AETHER_DAEMON ?? "http://127.0.0.1:2384";

export default defineConfig({
  server: {
    port: 5173,
    proxy: {
      "/aether-ws": {
        target: daemon,
        ws: true,
        changeOrigin: true,
        rewrite: (path) => path.replace(/^\/aether-ws/, ""),
        configure: (proxy) => proxy.on("proxyReqWs", (req) => req.setHeader("origin", daemon)),
      },
      "/status": { target: daemon, changeOrigin: true },
      "/asset": { target: daemon, changeOrigin: true },
    },
  },
  // `npm test` runs the painter tests under happy-dom: `renderBuffer` builds real DOM, so the only
  // thing it needs that Node lacks is a document. Deliberately no browser — what these cover is the
  // row layout (which chrome, phantom and text rows land where), which is geometry, not rendering.
  test: {
    environment: "happy-dom",
    include: ["src/**/*.test.ts"],
  },
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
