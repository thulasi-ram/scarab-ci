// The Run page's pure helpers: matrix labels, root failures, the summary
// counts, the default selection, the URL state, the graph node's meta line,
// the band fit and the step header's words.
import { describe, expect, it } from "vitest";
import {
  aboutDuration,
  clampRunUrl,
  defaultSelection,
  failureBox,
  fitBand,
  matrixHeading,
  nodeMeta,
  notLatestNote,
  parseMatrixId,
  parseRunUrl,
  rootFailures,
  runSummary,
  runUrlParams,
  segmentByMatrix,
  stepName,
  tryStatusWords,
} from "./run-page";

describe("parseMatrixId", () => {
  it("splits a cell id into its step, keys and values", () => {
    expect(parseMatrixId("plan[cluster=treebo-production,phase=aws-cluster]")).toEqual({
      base: "plan",
      keys: ["cluster", "phase"],
      values: ["treebo-production", "aws-cluster"],
    });
  });

  it("keeps a matrixed invoke's coordinate in the base", () => {
    expect(parseMatrixId("ship[svc=api]/probe[n=1]")).toEqual({ base: "ship[svc=api]/probe", keys: ["n"], values: ["1"] });
  });

  it("is null for a step that is not a matrix cell", () => {
    expect(parseMatrixId("build")).toBeNull();
    expect(parseMatrixId("ship[svc=api]/probe")).toBeNull();
  });

  // The compiler does not escape values, so each of these makes the split a
  // guess; the page shows the full id instead.
  it.each([
    ["a comma in a value", "plan[msg=a,b]"],
    ["an equals sign in a value", "plan[expr=a=b]"],
    ["a closing bracket in a value", "plan[x=a]b]"],
    ["an opening bracket in a value", "plan[x=a[y=b]"],
    ["an empty value", "plan[x=]"],
    ["an empty coordinate", "plan[]"],
    ["a repeated key", "plan[x=1,x=2]"],
    ["no step name", "[x=1]"],
  ])("is null for %s", (_why, id) => {
    expect(parseMatrixId(id)).toBeNull();
  });
});

describe("segmentByMatrix", () => {
  const ids = [
    "git",
    "plan[cluster=a,phase=x]",
    "plan[cluster=a,phase=y]",
    "plan[cluster=b,phase=x]",
    "apply[cluster=a]",
    "report",
  ];

  it("groups consecutive cells of one matrix under one heading, values only", () => {
    const segs = segmentByMatrix(ids, (id) => id);
    expect(segs.map((s) => (s.matrix ? matrixHeading(s.matrix) : null))).toEqual([
      null,
      "plan · matrix · cluster · phase",
      "apply · matrix · cluster",
      null,
    ]);
    expect(segs[1].items.map((i) => i.label)).toEqual(["a · x", "a · y", "b · x"]);
    expect(segs[0].items[0].label).toBe("git");
  });

  it("does not merge cells whose keys disagree, and labels a null id as its own segment", () => {
    const segs = segmentByMatrix(["plan[a=1]", "plan[b=2]", null], (id) => id);
    expect(segs).toHaveLength(3);
    expect(segs[2]).toEqual({ matrix: null, items: [{ item: null, label: "" }] });
  });

  it("falls back to the full id for a cell that does not parse", () => {
    const segs = segmentByMatrix(["plan[msg=a,b]"], (id) => id);
    expect(segs[0].matrix).toBeNull();
    expect(segs[0].items[0].label).toBe("plan[msg=a,b]");
  });
});

describe("stepName", () => {
  it("names a cell by its step and values, any other step by its id", () => {
    expect(stepName("plan[cluster=a,phase=x]")).toEqual({ base: "plan", cell: "a · x" });
    expect(stepName("build")).toEqual({ base: "build", cell: null });
  });
});

describe("rootFailures", () => {
  it("keeps the failed steps whose needs all passed", () => {
    const steps = [
      { id: "a", status: "succeeded", needs: [] },
      { id: "b", status: "failed", needs: ["a"] },
      { id: "c", status: "failed", needs: ["b"] },
      { id: "d", status: "failed", needs: [] },
      { id: "e", status: "succeeded", needs: ["a"] },
    ];
    expect(rootFailures(steps)).toEqual(["b", "d"]);
  });

  it("counts a need the run does not list as passed", () => {
    expect(rootFailures([{ id: "x", status: "failed", needs: ["gone"] }])).toEqual(["x"]);
  });
});

describe("runSummary", () => {
  it("counts running steps apart from the ones an infra condition holds back", () => {
    const steps = [
      { id: "git", status: "succeeded" },
      { id: "p1", status: "running" },
      { id: "p2", status: "running" },
      { id: "p3", status: "running" },
      { id: "p4", status: "running" },
    ];
    const s = runSummary(steps, { p3: "waiting for a node", p4: "waiting for a node" });
    expect(s).toMatchObject({ total: 5, running: 2, passed: 1, failed: 0, roots: [] });
    expect(s.held).toEqual([{ text: "waiting for a node", count: 2 }]);
  });

  it("names root failures for a failed run", () => {
    const s = runSummary(
      [
        { id: "a", status: "failed", needs: [] },
        { id: "b", status: "failed", needs: ["a"] },
      ],
      {},
    );
    expect(s.failed).toBe(2);
    expect(s.roots).toEqual(["a"]);
  });
});

describe("defaultSelection", () => {
  const steps = [
    { id: "clone", status: "succeeded", needs: [] },
    { id: "build", status: "failed", needs: ["clone"] },
    { id: "test", status: "running", needs: ["clone"] },
    { id: "lint", status: "running", needs: ["clone"] },
  ];

  it("opens on a gate waiting for a decision first", () => {
    expect(defaultSelection({ steps, gates: ["approve"], held: ["lint"] })).toBe("approve");
  });

  it("then the first root failure", () => {
    expect(defaultSelection({ steps, gates: [], held: ["lint"] })).toBe("build");
  });

  it("then a step an infra condition holds back, then the first running step", () => {
    const live = steps.map((s) => (s.id === "build" ? { ...s, status: "succeeded" } : s));
    expect(defaultSelection({ steps: live, gates: [], held: ["lint"] })).toBe("lint");
    expect(defaultSelection({ steps: live, gates: [], held: [] })).toBe("test");
  });

  it("then the last finished step, then the first step", () => {
    const done = [
      { id: "a", status: "succeeded" },
      { id: "b", status: "succeeded" },
      { id: "c", status: "skipped" },
    ];
    expect(defaultSelection({ steps: done, gates: [], held: [] })).toBe("b");
    expect(defaultSelection({ steps: [{ id: "a", status: "pending" }], gates: [], held: [] })).toBe("a");
    expect(defaultSelection({ steps: [], gates: [], held: [] })).toBeNull();
  });
});

describe("parseRunUrl", () => {
  it("reads well-formed params", () => {
    expect(parseRunUrl({ step: "3", v: "2", try: "1", tab: "activity", ptab: "source" })).toEqual({
      step: 3,
      v: 2,
      try: 1,
      tab: "activity",
      ptab: "source",
    });
  });

  it("drops malformed values instead of failing", () => {
    expect(
      parseRunUrl({ step: "build", service: "-1", v: "0", try: "1.5", tab: "pipeline", ptab: "origin" }),
    ).toEqual({});
  });

  it("takes the first of a repeated param", () => {
    expect(parseRunUrl({ step: ["2", "5"] })).toEqual({ step: 2 });
  });
});

describe("clampRunUrl", () => {
  const ctx = {
    steps: 4,
    services: 1,
    versions: 3,
    triesOf: (step: number, v: number) => (step === 1 ? v : 1),
  };

  it("keeps params that exist in the run", () => {
    expect(clampRunUrl({ step: 1, v: 2, try: 2, ptab: "outputs" }, ctx)).toEqual({ step: 1, v: 2, try: 2, ptab: "outputs" });
  });

  it("drops a try past the version's tries, and a step past the end", () => {
    expect(clampRunUrl({ step: 1, v: 1, try: 4 }, ctx)).toEqual({ step: 1, v: 1 });
    expect(clampRunUrl({ step: 9, try: 1 }, ctx)).toEqual({});
  });

  it("drops the latest version (its absence means latest) and one past it", () => {
    expect(clampRunUrl({ v: 3 }, ctx)).toEqual({});
    expect(clampRunUrl({ v: 7 }, ctx)).toEqual({});
  });

  it("counts tries in the latest version when v is absent", () => {
    expect(clampRunUrl({ step: 1, try: 3 }, ctx)).toEqual({ step: 1, try: 3 });
  });

  it("keeps a service until the list loads, then drops one past the end; a step wins", () => {
    expect(clampRunUrl({ service: 5 }, { ...ctx, services: null })).toEqual({ service: 5 });
    expect(clampRunUrl({ service: 5 }, ctx)).toEqual({});
    expect(clampRunUrl({ service: 0, step: 0 }, ctx)).toEqual({ step: 0 });
  });
});

describe("runUrlParams", () => {
  it("writes every key, undefined for one to remove", () => {
    expect(runUrlParams({ step: 0, try: 2 })).toEqual({
      step: "0",
      service: undefined,
      v: undefined,
      try: "2",
      tab: undefined,
      ptab: undefined,
    });
  });

  it("round-trips through parseRunUrl", () => {
    const s = { service: 1, v: 2, tab: "artifacts" as const, ptab: "workspace" as const };
    const params = runUrlParams(s);
    expect(parseRunUrl(params)).toEqual(s);
  });
});

describe("nodeMeta", () => {
  const now = 1_000_000;

  it("names the try that passed after earlier failures, replacing ×N", () => {
    expect(nodeMeta({ status: "succeeded", tries: 3, durationMs: 131_000 }, now)).toEqual({
      text: "passed on try 3 · 2m11s",
      tone: null,
    });
  });

  it("counts failed tries when every try failed", () => {
    expect(nodeMeta({ status: "failed", tries: 2, failedTries: 2, durationMs: 121_000 }, now)).toEqual({
      text: "failed 2 of 2 tries · 2m01s",
      tone: "failed",
    });
  });

  it("reads a single try plainly", () => {
    expect(nodeMeta({ status: "succeeded", tries: 1, durationMs: 156_000 }, now).text).toBe("passed · 2m36s");
    expect(nodeMeta({ status: "failed", tries: 1, durationMs: 126_000 }, now).text).toBe("failed · 2m06s");
  });

  it("says how long an infra condition has held a running step, in the warning tone", () => {
    const m = nodeMeta(
      { status: "running", tries: 1, note: { tone: "warning", text: "waiting for a node", since: now - 70_000 } },
      now,
    );
    expect(m).toEqual({ text: "waiting for a node · 1m10s", tone: "warning" });
  });

  it("shows a running clock, a gate and a verdict note", () => {
    expect(nodeMeta({ status: "running", tries: 1, runningSince: now - 72_000 }, now).text).toBe("running · 1m12s");
    expect(nodeMeta({ status: "waiting", tries: 0, gate: "manual" }, now).text).toBe("gate · manual");
    expect(nodeMeta({ status: "waiting", tries: 0, gate: "manual", approvalGate: true }, now).text).toBe(
      "gate · approval",
    );
    expect(nodeMeta({ status: "failed", tries: 1, note: { tone: "failed", text: "refused · yanked" } }, now)).toEqual({
      text: "refused · yanked",
      tone: "failed",
    });
  });
});

describe("fitBand", () => {
  it("draws at 100% when the graph fits the width, however tall it is", () => {
    expect(fitBand({ bandW: 1000, contentW: 700, contentH: 900, maxH: 480, pad: 24 })).toEqual({
      scale: 1,
      bandH: 480,
      pannable: true,
    });
    expect(fitBand({ bandW: 1000, contentW: 700, contentH: 200, maxH: 480, pad: 24 })).toEqual({
      scale: 1,
      bandH: 248,
      pannable: false,
    });
  });

  it("shrinks a wider graph, but not below 75%, then pans", () => {
    const a = fitBand({ bandW: 848, contentW: 1000, contentH: 100, maxH: 480, pad: 24 });
    expect(a.scale).toBeCloseTo(0.8);
    expect(a.pannable).toBe(false);
    const b = fitBand({ bandW: 448, contentW: 1000, contentH: 100, maxH: 480, pad: 24 });
    expect(b.scale).toBe(0.75);
    expect(b.pannable).toBe(true);
  });
});

describe("tryStatusWords", () => {
  const now = 10_000_000;
  it("says what the selected try did and for how long", () => {
    expect(tryStatusWords({ outcome: "succeeded", startedAt: 0, finishedAt: 131_000 }, now).text).toBe(
      "✓ Passed in 2m11s",
    );
    expect(
      tryStatusWords({ outcome: "failed", failure: "step", startedAt: 0, finishedAt: 126_000 }, now).text,
    ).toBe("✕ Failed after 2m06s: step error");
    expect(tryStatusWords({ outcome: "running", startedAt: now - 72_000 }, now).text).toBe("● Running for 1m12s");
  });

  it("says what an infra condition holds the try back on", () => {
    expect(tryStatusWords({ outcome: "running", startedAt: 0 }, now, "waiting for a node")).toEqual({
      text: "◷ Waiting for a node",
      tone: "warning",
    });
  });
});

describe("failureBox", () => {
  const tries = [
    { outcome: "failed", failure: "infra", failureDetail: "the node was drained", cause: "first run" },
    { outcome: "failed", failure: "step", failureDetail: "exit 1", cause: "automatic retry" },
    { outcome: "succeeded", cause: "retried by thulasi" },
  ];

  it("names a failed try's class and detail, and how long it waited for a node", () => {
    expect(failureBox({ tries, selected: 1, heldBeforeMs: 65_000 })).toEqual({
      tone: "failed",
      head: "Try 2 failed: step error",
      detail: "exit 1 · unschedulable for about 1m before it started",
    });
  });

  it("says a passing try came after earlier failures", () => {
    expect(failureBox({ tries, selected: 2 })).toEqual({
      tone: "info",
      head: "Passed on try 3 (retried by thulasi)",
      detail: "2 earlier tries failed in this version",
    });
    expect(failureBox({ tries: [{ outcome: "succeeded" }], selected: 0 })).toBeNull();
  });

  it("names the version and try a step that did not run was reused from", () => {
    expect(failureBox({ tries: [], selected: null, carried: { take: 2, try: 1 }, viewedTake: 3 })).toEqual({
      tone: "info",
      head: "Not run in version 3",
      detail: "Reused from version 2, try 1",
    });
    expect(failureBox({ tries: [], selected: null })).toBeNull();
  });

  it("shows an active condition in the warning tone, and a gate's waits as waits", () => {
    expect(
      failureBox({ tries: [{ outcome: "running" }], selected: 0, held: { text: "waiting for a node", message: "0/14 nodes" } }),
    ).toEqual({ tone: "warning", head: "Waiting for a node", detail: "0/14 nodes" });
    expect(failureBox({ tries: [{ outcome: "failed", failure: "gate" }], selected: 0, noun: "Wait" })?.head).toBe(
      "Wait 1 failed: gate failed",
    );
  });
});

describe("notLatestNote", () => {
  it("says what the newest try is doing when an older one is selected", () => {
    const tries = [{ outcome: "failed" }, { outcome: "failed" }, { outcome: "running" }];
    expect(notLatestNote(tries, 0)).toEqual({ text: "Not the latest try: try 3 is running", latest: 2 });
    expect(notLatestNote(tries, 2)).toBeNull();
    expect(notLatestNote(tries, null)).toBeNull();
  });
});

describe("aboutDuration", () => {
  it("rounds to the largest unit", () => {
    expect(aboutDuration(40_000)).toBe("about 40s");
    expect(aboutDuration(65_000)).toBe("about 1m");
    expect(aboutDuration(2 * 3_600_000)).toBe("about 2h");
  });
});
