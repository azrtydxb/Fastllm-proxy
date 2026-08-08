import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Built into web/dist/ (relative to this file) by the Dockerfile's `node`
// stage, then embedded into the Rust binary by rust-embed
// (src/control/ui.rs) — see that module's doc comment for the whole story,
// including why a missing/empty dist/ must not fail `cargo build`.
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "dist",
    // Emptied so a renamed chunk from a previous build cannot linger and get
    // embedded alongside the current one. That also deletes `dist/.gitkeep` —
    // the one tracked file in here, and the thing that lets `cargo build`
    // work without ever running Node — so `npm run build` recreates it
    // afterwards. Without that, every UI build showed up as a deleted file in
    // `git status` and was one careless `git add -A` away from breaking the
    // Node-free build for everyone.
    emptyOutDir: true,
  },
  // The UI and the admin API it calls are served from the same origin (the
  // admin port, control::api::serve) in production, so no CORS or base-URL
  // configuration is needed there. In dev (`npm run dev`), proxy admin API
  // calls to a real fastllm-proxy running `--role all` on the default admin
  // port so `fetch("/admin/...")` works unmodified in both modes.
  server: {
    proxy: {
      "/admin": "http://127.0.0.1:4001",
      "/login": "http://127.0.0.1:4001",
      "/logout": "http://127.0.0.1:4001",
    },
  },
});
