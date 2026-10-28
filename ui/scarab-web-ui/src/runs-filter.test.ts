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
