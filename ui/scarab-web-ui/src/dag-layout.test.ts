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

import { idSegments, layoutGroups, stepGroups, type DagCell } from "./dag-layout";

const lib = (name: string, hash: string, yanked = false) => ({ kind: "library", name, content_hash: hash, yanked });

/** A layout as nested ids, for comparison: a box is `{key: [...cols]}`. */
function shape(cols: DagCell<{ id: string }>[][]): unknown {
  return cols.map((c) =>
    c.map((cell) => (cell.kind === "step" ? cell.step.id : { [cell.group.key]: shape(cell.cols) })),
  );
}

describe("invocation groups", () => {
  it("splits an id on / but keeps a matrix leg whole", () => {
    expect(idSegments("ship/smoke/probe")).toEqual(["ship", "smoke", "probe"]);
    expect(idSegments("build[os=linux/amd64]/push")).toEqual(["build[os=linux/amd64]", "push"]);
  });

  it("names each box a Step sits in, outermost first, and draws none for a plugin gate", () => {
    const g = stepGroups({
      id: "ship/smoke/probe",
      origin: [lib("acme/deploy-helm", "h1"), lib("acme/http-probe", "h2", true)],
    });
    expect(g.map((x) => [x.key, x.label, x.name, x.yanked])).toEqual([
      ["ship", "ship", "acme/deploy-helm", false],
      ["ship/smoke", "smoke", "acme/http-probe", true],
    ]);
    expect(stepGroups({ id: "live", origin: [{ kind: "plugin", name: "argocd-prod/wait-live", content_hash: "m" }] })).toEqual([]);
    // An origin longer than the id's invoke parts draws only what the id shows.
    expect(stepGroups({ id: "git", origin: [lib("x/y", "h")] })).toEqual([]);
  });

  it("lays out each invocation as one unit, its Steps inside, nested for an inner one", () => {
    const steps = [
      { id: "checkout/git", needs: [], origin: [lib("thulasi-ram/clone", "c")] },
      { id: "test", needs: ["checkout/git"] },
      { id: "build", needs: ["checkout/git"] },
      { id: "ship/render", needs: ["test", "build"], origin: [lib("acme/deploy-helm", "d")] },
      { id: "ship/helm", needs: ["ship/render"], origin: [lib("acme/deploy-helm", "d")] },
      { id: "ship/smoke/probe", needs: ["ship/helm"], origin: [lib("acme/deploy-helm", "d"), lib("acme/http-probe", "p")] },
      { id: "announce", needs: ["ship/smoke/probe"] },
    ];
    expect(shape(layoutGroups(steps))).toEqual([
      [{ checkout: [["checkout/git"]] }],
      ["test", "build"],
      [{ ship: [["ship/render"], ["ship/helm"], [{ "ship/smoke": [["ship/smoke/probe"]] }]] }],
      ["announce"],
    ]);
  });

  it("is plain layering when no Step comes from a library", () => {
    const steps = [
      { id: "a", needs: [] },
      { id: "b", needs: ["a"] },
    ];
    expect(shape(layoutGroups(steps))).toEqual([["a"], ["b"]]);
  });
});
