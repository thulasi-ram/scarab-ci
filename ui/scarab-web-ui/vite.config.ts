import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// Dev server for local UI testing (ADR-0028, 0036). The typed client calls the
// API at `/` (see src/api/client.ts), so in dev we proxy `/v1` (+ `/healthz`)
// to a running scarab-server.
//
// That server's address is NOT ours to guess — which server, which port is
// dev-machine wiring we don't own — and a silent default just proxies to a dead
// port (the exact footgun that wastes time). So require `SCARAB_API_URL`
// explicitly, and fail loudly if it's missing.
//
// This is a DEV-SERVER concern only: it fires for `vite dev` (command ===
// "serve"), never for `vite build`. A production build is served same-origin
// with the API (relative `/v1`), so it needs no proxy and no API URL at all —
// which is why the throw is scoped to `serve`.
export default defineConfig(({ command }) => {
  let proxy;
  if (command === "serve") {
    const apiTarget = process.env.SCARAB_API_URL;
    if (!apiTarget) {
      throw new Error(
        "SCARAB_API_URL is required for the dev server — point it at a running " +
          "scarab-server, e.g. `SCARAB_API_URL=http://127.0.0.1:8080 npm run dev` " +
          "(or just `just ui`). Production builds are same-origin and don't need it.",
      );
    }
    proxy = {
      // `ws: true` so the debug-shell attach WebSocket upgrades through the proxy.
      "/v1": { target: apiTarget, changeOrigin: true, ws: true },
      "/healthz": { target: apiTarget, changeOrigin: true },
    };
  }
  return {
    plugins: [solid()],
    server: {
      // Baked ASCII scenes are imported from ../brand/ascii/generated (the
      // canonical committed output — not copied per-app); allow the parent.
      fs: { allow: ["../.."] },
      port: 5173,
      proxy,
    },
  };
});
