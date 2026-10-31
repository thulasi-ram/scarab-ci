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
});
