// Which run statuses are terminal: the run page stops polling, drops the Live
// mark and swaps Cancel for New run on these.
import { describe, it, expect } from "vitest";
import { isTerminal } from "./api/client";

describe("isTerminal", () => {
  it("treats every finished status as terminal", () => {
    for (const s of ["succeeded", "failed", "cancelled"]) expect(isTerminal(s)).toBe(true);
  });

  // Regression: a dead-lettered run kept polling, showed Live and offered Cancel.
  it("treats a dead-lettered run as terminal", () => {
    expect(isTerminal("dead_lettered")).toBe(true);
  });

  it("keeps a live run live", () => {
    for (const s of ["pending", "running", "suspended"]) expect(isTerminal(s)).toBe(false);
  });
});
