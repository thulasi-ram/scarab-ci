import { describe, it, expect } from "vitest";
import {
  appliesToEnvironments,
  appliesToProjects,
  decisionVerdict,
  isCelPolicy,
  newSettingForm,
  runDecisionViews,
  runPolicySummary,
  settingBody,
  settingFormErrors,
  settingFormOf,
  splitEntries,
  typedSettingRows,
  type DecisionRecordLike,
  type StoredPolicyLike,
} from "./policies";

const decision = (results: [string, string, string?][], deny = false): DecisionRecordLike => ({
  id: "d",
  org: "acme",
  at: 0,
  decision: {
    checkpoint: "run.create",
    results: results.map(([policy_id, kind, mode = "enforce"]) => ({ policy_id, mode, outcome: { kind } })),
    outcome: deny ? { kind: "deny", messages: ["no"] } : { kind: "allow" },
  },
});

const stored = (id: string, policy: StoredPolicyLike["policy"], deleted = false): StoredPolicyLike => ({
  id,
  org: "acme",
  name: policy.name,
  level: "org",
  version: 1,
  content_hash: "h",
  created_at: 0,
  updated_at: 0,
  deleted,
  policy,
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
        href: "/orgs/acme/settings/plugins#plugin-lists",
        sectionLabel: "Plugins",
      },
      {
        id: "3",
        name: "libs",
        label: "Libraries allowed",
        values: ["github.com/acme/*"],
        appliesTo: "all projects",
        href: "/orgs/acme/settings/libraries#library-lists",
        sectionLabel: "Libraries",
      },
    ]);
  });
});

describe("runDecisionViews", () => {
  const record = (
    id: string,
    at: number,
    results: { policy_id: string; name?: string; mode?: string; outcome: Record<string, unknown> }[],
    extra: Partial<DecisionRecordLike> = {},
  ): DecisionRecordLike => ({
    id,
    org: "acme",
    at,
    ...extra,
    decision: {
      checkpoint: extra.step_id ? "step.admit" : "run.create",
      input: { project: { name: "web" } },
      results: results as DecisionRecordLike["decision"]["results"],
      outcome: results.some((r) => r.outcome.kind === "denied") ? { kind: "deny", messages: ["no"] } : { kind: "allow" },
    },
  });

  it("leaves out decisions no Policy matched, and the results that did not match", () => {
    const views = runDecisionViews([
      record("b", 2, [
        { policy_id: "p1", name: "one", outcome: { kind: "denied", messages: ["Release from main."] } },
        { policy_id: "p2", name: "two", outcome: { kind: "not_matched" } },
      ]),
      record("a", 1, [{ policy_id: "p2", name: "two", outcome: { kind: "not_matched" } }]),
    ]);
    expect(views.map((v) => v.id)).toEqual(["b"]);
    expect(views[0]!.results).toEqual([
      { policyId: "p1", name: "one", level: "", outcome: "denied", messages: ["Release from main."] },
    ]);
    expect(views[0]!.verdict).toBe("denied");
    expect(views[0]!.input).toEqual({ project: { name: "web" } });
  });

  it("names a failed rule by its mode, and sorts oldest first", () => {
    const views = runDecisionViews([
      record("late", 9, [{ policy_id: "a", mode: "audit", outcome: { kind: "error", message: "boom" } }], {
        step_id: "deploy",
        attempt: 2,
      }),
      record("early", 1, [{ policy_id: "e", mode: "enforce", outcome: { kind: "error", message: "boom" } }]),
    ]);
    expect(views.map((v) => [v.id, v.results[0]!.outcome])).toEqual([
      ["early", "denied"],
      ["late", "would_deny"],
    ]);
    expect(views[1]!.step).toBe("deploy");
    expect(views[1]!.attempt).toBe(2);
  });

  it("is empty for a Run with no decisions", () => {
    expect(runDecisionViews([])).toEqual([]);
  });

  it("summarises denied decisions and audit-mode would-denies", () => {
    const views = runDecisionViews([
      record("a", 1, [{ policy_id: "p", outcome: { kind: "would_deny", messages: ["x"] } }]),
      record("b", 2, [{ policy_id: "q", outcome: { kind: "denied", messages: ["y"] } }]),
    ]);
    expect(runPolicySummary(views)).toBe("2 decisions · 1 denied · 1 would deny in audit mode");
  });
});

describe("library and plugin list forms", () => {
  it("splits entries on lines and commas, dropping blanks and repeats", () => {
    expect(splitEntries(" github.com/acme/*\n\ngithub.com/x, github.com/acme/* ")).toEqual([
      "github.com/acme/*",
      "github.com/x",
    ]);
  });

  it("starts a library list in audit mode and a plugin list enforced", () => {
    expect(newSettingForm("libraries_allowed").mode).toBe("audit");
    expect(newSettingForm("plugins_allowed").mode).toBe("enforce");
    expect(newSettingForm("libraries_blocked").name).toBe("libraries-blocked");
  });

  it("builds an org body with its match and a project body without one", () => {
    const form = {
      ...newSettingForm("libraries_allowed"),
      entries: "github.com/acme/*",
      projects: "web-*",
      exceptProjects: "web-legacy",
    };
    expect(settingBody(form, "org")).toEqual({
      name: "libraries-allowed",
      checkpoint: "run.create",
      mode: "audit",
      setting: { libraries_allowed: { sources: ["github.com/acme/*"] } },
      match: { projects: ["web-*"], except_projects: ["web-legacy"], environments: [], trigger_kinds: [] },
    });
    expect(settingBody(form, "project").match).toBeUndefined();
  });

  it("reads a stored list back into the form", () => {
    const body = settingBody({ ...newSettingForm("plugins_allowed"), entries: "argocd\nflux" }, "org");
    expect(settingFormOf(body)).toMatchObject({ kind: "plugins_allowed", entries: "argocd\nflux", mode: "enforce" });
  });

  it("refuses what the server would: a bad name, no entries, a misplaced *", () => {
    expect(settingFormErrors({ ...newSettingForm("libraries_allowed"), name: "has space" })).toEqual({
      name: "1 to 100 letters, digits, -, _ or .",
      entries: "Add at least one entry.",
    });
    expect(
      settingFormErrors({ ...newSettingForm("libraries_allowed"), entries: "github.com/*/libs" }).entries,
    ).toMatch(/prefix ending in \*/);
    expect(settingFormErrors({ ...newSettingForm("plugins_allowed"), entries: "argo*" }).entries).toMatch(
      /no \*/,
    );
    expect(settingFormErrors({ ...newSettingForm("libraries_allowed"), entries: "github.com/acme/*" })).toEqual({});
  });
});
