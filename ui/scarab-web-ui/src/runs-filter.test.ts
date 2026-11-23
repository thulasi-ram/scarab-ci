import { describe, it, expect } from "vitest";
import { runEnvironmentOptions, runListFilter } from "./runs-filter";

describe("runEnvironmentOptions", () => {
  it("keeps every Project Environment when the loaded runs are one Environment's", () => {
    // The server already filtered to production: the rows alone would leave
    // production as the only choice.
    const runs = [{ environment: "production" }, { environment: "production" }];
    expect(runEnvironmentOptions(["staging", "production", "qa"], runs, "production")).toEqual([
      "production",
      "qa",
      "staging",
    ]);
  });

  it("adds Environments seen only on runs, and skips runs without one", () => {
    const runs = [{ environment: "legacy" }, { environment: null }, {}, { environment: "" }];
    expect(runEnvironmentOptions(["production"], runs, "")).toEqual(["legacy", "production"]);
  });

  it("keeps a selected Environment that neither list has", () => {
    expect(runEnvironmentOptions([], [], "from-a-link")).toEqual(["from-a-link"]);
  });

  it("is empty for a Project that has never deployed and has no Environments", () => {
    expect(runEnvironmentOptions([], [{ environment: null }], "")).toEqual([]);
  });
});

describe("runListFilter", () => {
  it("sends nothing when no filter is on (an empty environment is a 400)", () => {
    expect(runListFilter("", false)).toEqual({});
  });

  it("sends deploy only as true — the toggle off means every run, not the non-deploys", () => {
    expect(runListFilter("", true)).toEqual({ deploy: true });
    expect("deploy" in runListFilter("production", false)).toBe(false);
  });

  it("combines the Environment and the Deploys toggle", () => {
    expect(runListFilter("production", true)).toEqual({ environment: "production", deploy: true });
  });
});

import { barePipeline, mergeRefused } from "./runs-filter";

describe("refused rows in the Runs list", () => {
  const runs = [
    { id: "r2", created_at: 200 },
    { id: "r1", created_at: 100 },
  ];
  const refused = [
    { id: 3, created_at: 300, actor: "j.lee", pipeline: ".scarab/release.yaml" },
    { id: 2, created_at: 150, actor: "a.kim", pipeline: ".scarab/ci.yml" },
    { id: 1, created_at: 50, actor: "a.kim", pipeline: ".scarab/ci.yaml" },
  ];
  const ids = (rows: ReturnType<typeof mergeRefused>) =>
    rows.map((r) => (r.kind === "run" ? (r.run as unknown as { id: string }).id : `x${(r.refused as unknown as { id: number }).id}`));
  const base = { status: "all", author: "", pipeline: "", runOnly: false };

  it("interleaves refusals newest first, within the loaded window", () => {
    expect(ids(mergeRefused(runs, refused, base))).toEqual(["x3", "r2", "x2", "r1"]);
    expect(ids(mergeRefused(runs, refused, { ...base, status: "refused" }))).toEqual(["x3", "x2", "x1"]);
    expect(ids(mergeRefused(runs, refused, { ...base, status: "failed" }))).toEqual(["r2", "r1"]);
    expect(ids(mergeRefused([], refused, base))).toEqual(["x3", "x2", "x1"]);
  });

  it("applies the author and pipeline filters, and leaves refusals out for a Run-only filter", () => {
    expect(ids(mergeRefused(runs, refused, { ...base, author: "a.kim" }))).toEqual(["r2", "x2", "r1"]);
    expect(ids(mergeRefused(runs, refused, { ...base, pipeline: "release" }))).toEqual(["x3", "r2", "r1"]);
    expect(ids(mergeRefused(runs, refused, { ...base, runOnly: true }))).toEqual(["r2", "r1"]);
    expect(barePipeline(".scarab/ci.yml")).toBe("ci");
  });

  it("keeps only refused cron triggers under the scheduled filter", () => {
    const withCron = [{ ...refused[0]!, trigger_kind: "cron" }, { ...refused[1]!, trigger_kind: "push" }];
    expect(ids(mergeRefused(runs, withCron, { ...base, scheduled: true }))).toEqual(["x3", "r2", "r1"]);
  });
});

import { filterChips, inSegment, noMatchSentence, parseSegment, segmentCounts, segmentRows } from "./runs-filter";

describe("status segments", () => {
  const runs = [
    { id: "a", created_at: 900, status: "running" },
    { id: "b", created_at: 800, status: "pending" },
    { id: "c", created_at: 700, status: "suspended" },
    { id: "d", created_at: 600, status: "failed" },
    { id: "e", created_at: 500, status: "dead_lettered" },
    { id: "f", created_at: 400, status: "succeeded" },
  ];
  const refused = [
    { id: 2, created_at: 650, actor: "a.kim", pipeline: ".scarab/ci.yaml" },
    // Older than every loaded Run: the Refused segment lists it, All does not.
    { id: 1, created_at: 100, actor: "a.kim", pipeline: ".scarab/ci.yaml" },
  ];
  const f = { author: "", pipeline: "", runOnly: false };

  it("reads ?status=waiting, and falls back to all for anything else", () => {
    expect(parseSegment("waiting")).toBe("waiting");
    expect(parseSegment("")).toBe("all");
    expect(parseSegment("succeeded")).toBe("all");
  });

  it("Running takes pending Runs, Waiting suspended ones, Failed dead-lettered ones", () => {
    expect(inSegment("pending", "running")).toBe(true);
    expect(inSegment("suspended", "waiting")).toBe(true);
    expect(inSegment("suspended", "running")).toBe(false);
    expect(inSegment("dead_lettered", "failed")).toBe(true);
    expect(inSegment("cancelled", "failed")).toBe(false);
  });

  it("each count is the number of rows a click on its segment lists", () => {
    const counts = segmentCounts(runs, refused, f);
    expect(counts).toEqual({ all: 7, running: 2, waiting: 1, failed: 2, refused: 2 });
    for (const seg of ["all", "running", "waiting", "failed", "refused"] as const) {
      expect(segmentRows(runs, refused, seg, f)).toHaveLength(counts[seg]);
    }
  });

  it("a filter refusals cannot match leaves Refused at zero", () => {
    expect(segmentCounts(runs, refused, { ...f, runOnly: true }).refused).toBe(0);
  });
});

describe("filter chips", () => {
  const none = { pipeline: "", env: "", author: "", branch: "", pr: "", deploy: false, scheduled: false };

  it("names each filter that is on, in the panel's order, and cuts long values", () => {
    const chips = filterChips({ ...none, author: "j.rao", pipeline: "deploy-preview-environments-for-feature-branches", deploy: true });
    expect(chips.map((c) => [c.param, c.key, c.value])).toEqual([
      ["pipeline", "pipeline", "deploy-preview-environments…"],
      ["author", "author", "j.rao"],
      ["deploy", "deploys only", null],
    ]);
    expect(chips[0]!.full).toBe("deploy-preview-environments-for-feature-branches");
  });

  it("shows a PR as #N and nothing when no filter is on", () => {
    expect(filterChips({ ...none, pr: "213" }).map((c) => `${c.key} ${c.value}`)).toEqual(["PR #213"]);
    expect(filterChips(none)).toEqual([]);
  });
});

describe("the no-match sentence", () => {
  const none = { pipeline: "", env: "", author: "", branch: "", pr: "", deploy: false, scheduled: false };
  const read = (...a: Parameters<typeof noMatchSentence>) => noMatchSentence(...a).map((p) => p.text).join("");

  it("names the status, the switches and each filter, over the loaded window", () => {
    expect(read({ ...none, pipeline: "deploy-preview", author: "j.rao", deploy: true }, "failed", 50)).toBe(
      "No failed deploy Run of deploy-preview by j.rao in the latest 50 Runs.",
    );
    expect(read({ ...none, branch: "main", pr: "7" }, "refused", 1)).toBe(
      "No refused trigger on main for PR #7 in the latest 1 Run.",
    );
  });

  it("names no window when the server's filters left no Run at all", () => {
    expect(read({ ...none, env: "production" }, "all", 0)).toBe("No Run into production.");
  });

  it("sets the values in Mono", () => {
    expect(noMatchSentence({ ...none, author: "j.rao" }, "all", 3).filter((p) => p.t === "mono")).toEqual([
      { t: "mono", text: "j.rao" },
    ]);
  });
});
