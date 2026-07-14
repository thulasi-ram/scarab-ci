import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// Dev server for local UI testing (ADR-0028, 0036). The typed client calls the
// API at `/` (see src/api/client.ts), so proxy `/v1` (+ `/healthz`) to a locally
// running `scarab-server --executor local`. Override the target with
// SCARAB_API_URL when the server isn't on the default dev port.
const apiTarget = process.env.SCARAB_API_URL ?? "http://127.0.0.1:8899";

export default defineConfig({
  plugins: [solid()],
  server: {
    port: 5173,
    proxy: {
      "/v1": { target: apiTarget, changeOrigin: true },
      "/healthz": { target: apiTarget, changeOrigin: true },
    },
  },
});
