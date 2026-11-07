import { describe, it, expect } from "vitest";
import {
  appliesToEnvironments,
  appliesToProjects,
  countsOf,
  decisionVerdict,
  isCelPolicy,
  policyCounts,
  typedSettingRows,
  type DecisionRecordLike,
  type StoredPolicyLike,
} from "./policies";

const decision = (results: [string, string][], deny = false): DecisionRecordLike => ({
  id: "d",
  at: 0,
  decision: {
    checkpoint: "run.create",
    results: results.map(([policy_id, kind]) => ({ policy_id, outcome: { kind } })),
    outcome: deny ? { kind: "deny", messages: ["no"] } : { kind: "allow" },
  },
});

const stored = (id: string, policy: StoredPolicyLike["policy"], deleted = false): StoredPolicyLike => ({
  id,
  name: policy.name,
  level: "org",
  version: 1,
  updated_at: 0,
  deleted,
  policy,
});

describe("policyCounts", () => {
  it("counts matched, denied and would-deny per Policy, skipping not_matched", () => {
    const counts = policyCounts([
      decision([["a", "denied"], ["b", "not_matched"]], true),
      decision([["a", "passed"], ["b", "would_deny"]]),
      decision([["a", "error"], ["b", "passed"]], true),
    ]);
    expect(countsOf(counts, "a")).toEqual({ matched: 3, denied: 2, wouldDeny: 0 });
    expect(countsOf(counts, "b")).toEqual({ matched: 2, denied: 0, wouldDeny: 1 });
    expect(countsOf(counts, "c")).toEqual({ matched: 0, denied: 0, wouldDeny: 0 });
  });
});

describe("decisionVerdict", () => {
  it("reads the decision's outcome", () => {
    expect(decisionVerdict(decision([], true))).toBe("denied");
    expect(decisionVerdict(decision([]))).toBe("allowed");
  });
});

describe("applies to", () => {
  it("says all projects, or the patterns with their exceptions", () => {
    expect(appliesToProjects({ name: "p", checkpoint: "run.create" })).toBe("all projects");
    expect(
      appliesToProjects({
        name: "p",
        checkpoint: "run.create",
        match: { projects: ["web-*"], except_projects: ["web-legacy"] },
      }),
    ).toBe("web-*, except web-legacy");
    expect(appliesToEnvironments({ name: "p", checkpoint: "run.create" })).toBe("any");
    expect(
      appliesToEnvironments({ name: "p", checkpoint: "run.create", match: { environments: ["prod"] } }),
    ).toBe("prod");
  });
});

describe("CEL Policies and typed settings", () => {
  const cel = stored("1", { name: "cel", checkpoint: "run.create", rules: [] });
  const gone = stored("2", { name: "gone", checkpoint: "run.create", rules: [] }, true);
  const libs = stored("3", {
    name: "libs",
    checkpoint: "run.create",
    setting: { libraries_allowed: { sources: ["github.com/acme/*"] } },
  });
  const plugins = stored("4", {
    name: "gates",
    checkpoint: "run.create",
    setting: { plugins_allowed: { installations: ["argocd"] } },
  });

  it("lists live CEL Policies only", () => {
    expect([cel, gone, libs].filter(isCelPolicy).map((p) => p.name)).toEqual(["cel"]);
  });

  it("lists each library or plugin list with a link to its feature's section", () => {
    expect(typedSettingRows("acme", [cel, libs, plugins, gone])).toEqual([
      {
        id: "4",
        name: "gates",
        label: "Plugins whose gates may run",
        values: ["argocd"],
        appliesTo: "all projects",
        href: "/orgs/acme/settings/plugins",
        sectionLabel: "Plugins",
      },
      {
        id: "3",
        name: "libs",
        label: "Libraries allowed",
        values: ["github.com/acme/*"],
        appliesTo: "all projects",
        href: "/orgs/acme/settings/libraries",
        sectionLabel: "Libraries",
      },
    ]);
  });
});
