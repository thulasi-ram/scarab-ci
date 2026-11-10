// Playwright tier (test-strategy Phase 3): EXACTLY 2 specs against the mock
// mode (`dev:mock`, VITE_SCARAB_MOCK=1) — no server, no DB, the acme fixture.
// Text/role-based assertions only; NO pixel/screenshot snapshots. Chromium
// only: the escape classes this tier catches (boot breakage, wiring between
// derivations and the rendered DOM) are not browser-specific.
import { defineConfig, devices } from "@playwright/test";

// PLAYWRIGHT_PORT lets parallel runs in different worktrees use their own
// server. Never reuse a running server: on a busy port the run fails instead
// of testing another branch's code.
const PORT = Number(process.env.PLAYWRIGHT_PORT ?? 4173);

export default defineConfig({
  testDir: "e2e",
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  reporter: "list",
  use: {
    baseURL: `http://localhost:${PORT}`,
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: {
    // The mock-mode Vite dev server — same entry `just ui mock` uses. A pinned
    // strict port so a stray dev server on 5173 never gets picked up silently.
    command: `npm run dev:mock -- --port ${PORT} --strictPort`,
    url: `http://localhost:${PORT}`,
    reuseExistingServer: false,
    timeout: 60_000,
  },
});
