// Longest-path DAG layering (extracted from Dag.tsx) — the layout answers
// "which dependency column does each step sit in".
import { describe, it, expect } from "vitest";
import { layerSteps } from "./dag-layout";

const s = (id: string, ...needs: string[]) => ({ id, needs });
const ids = (cols: { id: string }[][]) => cols.map((c) => c.map((x) => x.id));

describe("layerSteps", () => {
  it("a linear chain gets one column per step", () => {
    expect(ids(layerSteps([s("a"), s("b", "a"), s("c", "b")]))).toEqual([["a"], ["b"], ["c"]]);
  });

  it("a diamond keeps the join one column right of its DEEPEST dependency", () => {
    const cols = ids(layerSteps([s("a"), s("b", "a"), s("c", "a"), s("d", "b", "c")]));
    expect(cols).toEqual([["a"], ["b", "c"], ["d"]]);
  });

  it("multiple roots share column 0", () => {
    expect(ids(layerSteps([s("a"), s("b"), s("c", "a", "b")]))).toEqual([["a", "b"], ["c"]]);
  });

  it("longest path wins over shortest: a shortcut edge doesn't pull a step left", () => {
    // d needs both a (depth 0) and c (depth 2) → column 3.
    const cols = ids(layerSteps([s("a"), s("b", "a"), s("c", "b"), s("d", "a", "c")]));
    expect(cols).toEqual([["a"], ["b"], ["c"], ["d"]]);
  });

  it("a need naming an unknown step is treated as an external root", () => {
    expect(ids(layerSteps([s("a", "ghost"), s("b", "a")]))).toEqual([["a"], ["b"]]);
  });

  it("a cycle terminates and still places every step", () => {
    const cols = layerSteps([s("a", "b"), s("b", "a"), s("c")]);
    const placed = cols.flat().map((x) => x.id).sort();
    expect(placed).toEqual(["a", "b", "c"]);
  });

  it("a self-loop terminates", () => {
    const cols = layerSteps([s("a", "a")]);
    expect(cols.flat().map((x) => x.id)).toEqual(["a"]);
  });

  it("empty input yields no columns", () => {
    expect(layerSteps([])).toEqual([]);
  });
});
