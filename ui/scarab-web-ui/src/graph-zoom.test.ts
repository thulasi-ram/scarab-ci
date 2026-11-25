import { describe, expect, it } from "vitest";
import { clearGraphZoom, readGraphZoom, writeGraphZoom } from "./graph-zoom";

function memory() {
  const m = new Map<string, string>();
  return {
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
    removeItem: (k: string) => void m.delete(k),
  };
}

describe("the remembered graph zoom", () => {
  it("reads back what was written, and nothing after a reset", () => {
    const s = memory();
    expect(readGraphZoom(s)).toBeNull();
    writeGraphZoom(1.2 * 1.2, s);
    expect(readGraphZoom(s)).toBe(1.44);
    clearGraphZoom(s);
    expect(readGraphZoom(s)).toBeNull();
  });

  it("ignores a stored value that is not a zoom", () => {
    const s = memory();
    for (const bad of ["", "abc", "0", "9", "NaN"]) {
      s.setItem("scarab-graph-zoom", bad);
      expect(readGraphZoom(s)).toBeNull();
    }
  });

  it("opens at 100% when storage refuses", () => {
    const refusing = {
      getItem: () => {
        throw new Error("denied");
      },
      setItem: () => {
        throw new Error("denied");
      },
      removeItem: () => {
        throw new Error("denied");
      },
    };
    expect(() => writeGraphZoom(1.2, refusing)).not.toThrow();
    expect(readGraphZoom(refusing)).toBeNull();
    expect(() => clearGraphZoom(refusing)).not.toThrow();
  });
});
