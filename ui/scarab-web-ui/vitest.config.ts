// No-DOM test tier (test-strategy Phase 3): the suite covers pure derivation
// modules only (takes/attempts/dag-layout/events/logs), so it runs in a plain
// node environment — no jsdom, no component rendering.
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
