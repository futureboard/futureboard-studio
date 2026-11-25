// Futureboard Studio's Loudness & Visualizer page.
//
// Built to `dist/`, which `crates/SphereUIComponents/build.rs` embeds into the
// app; the Studio API server serves it from 127.0.0.1. In `astro dev` the API
// and the WebSocket are proxied to a running Studio with the API server on.
import { defineConfig } from "astro/config";
import react from "@astrojs/react";

const studio = process.env.FUTUREBOARD_API ?? "http://127.0.0.1:7471";

export default defineConfig({
  output: "static",
  integrations: [react()],
  build: {
    // One page, served by the app: keep styles in their own file.
    inlineStylesheets: "never",
    assets: "_astro",
  },
  vite: {
    server: {
      proxy: {
        "/api": studio,
        "/ws": { target: studio.replace(/^http/, "ws"), ws: true },
      },
    },
  },
});
