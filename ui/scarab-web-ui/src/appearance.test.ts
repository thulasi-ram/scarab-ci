import { describe, it, expect } from "vitest";
import { PALETTES, parsePalette } from "./appearance";

describe("stored palette", () => {
  it("reads back mono", () => {
    expect(parsePalette("mono")).toBe("mono");
  });

  it("falls back to emerald for a missing, empty or unknown value", () => {
    expect(parsePalette(null)).toBe("emerald");
    expect(parsePalette(undefined)).toBe("emerald");
    expect(parsePalette("")).toBe("emerald");
    // Exact match only: the pre-paint guard in index.html compares the same way.
    expect(parsePalette("Mono")).toBe("emerald");
    expect(parsePalette("mono ")).toBe("emerald");
    expect(parsePalette("dark")).toBe("emerald");
  });

  it("offers every palette it can read back, emerald first", () => {
    expect(PALETTES.map((p) => p.id)).toEqual(["emerald", "mono"]);
    for (const p of PALETTES) expect(parsePalette(p.id)).toBe(p.id);
  });
});
