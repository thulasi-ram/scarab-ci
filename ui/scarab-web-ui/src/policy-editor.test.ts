import { describe, expect, it } from "vitest";
import {
  applyCompletion,
  cleanDoc,
  completionsAt,
  diagnosticText,
  fieldError,
  newPolicyForm,
  placeDiagnostics,
  policyBodyOf,
  policyFormOf,
  policyYaml,
  replayRunHref,
  replaySummary,
  windowCountLabel,
  type CheckpointSchema,
  type Diagnostic,
} from "./policy-editor";
import checkpoints from "./mock-policy-checkpoints.json";

// The server's own `GET /v1/policy-checkpoints` answer, captured for mock
// mode: completion is tested against the real schemas.
const schemaOf = (c: string) =>
  checkpoints.find((x) => x.checkpoint === c)!.schema as unknown as CheckpointSchema;
const runCreate = schemaOf("run.create");
const stepAdmit = schemaOf("step.admit");

const diag = (at: string, extra: Partial<Diagnostic> = {}): Diagnostic => ({
  severity: "error",
  at,
  line: 0,
  column: 0,
  expr: "",
  message: `${at} is wrong`,
  ...extra,
});

describe("the policy form", () => {
  it("starts a new policy in audit mode at run.create with one empty rule", () => {
    expect(newPolicyForm()).toMatchObject({ checkpoint: "run.create", mode: "audit", rules: [{ when: "", message: "" }] });
  });

  it("saves every rule as a deny and sends trigger kinds at run.create only", () => {
    const form = {
      ...newPolicyForm(),
      name: " no-forks ",
      projects: "web-*, api",
      triggerKinds: ["pull_request"],
      rules: [{ when: "trigger.fork", message: "No forks." }],
    };
    expect(policyBodyOf(form)).toEqual({
      name: "no-forks",
      checkpoint: "run.create",
      match: { projects: ["web-*", "api"], except_projects: [], environments: [], trigger_kinds: ["pull_request"] },
      mode: "audit",
      rules: [{ when: "trigger.fork", effect: "deny", message: "No forks." }],
    });
    expect(policyBodyOf({ ...form, checkpoint: "step.admit" }).match!.trigger_kinds).toEqual([]);
  });

  it("reads a stored body back into the same form", () => {
    const form = { ...newPolicyForm(), name: "x", environments: "production", mode: "enforce" as const };
    expect(policyFormOf(policyBodyOf(form))).toEqual(form);
  });
});

describe("placeDiagnostics", () => {
  it("puts each finding at its field, its rule, or the policy as a whole", () => {
    const placed = placeDiagnostics([
      diag("name"),
      diag("match.except_projects[1]"),
      diag("match.trigger_kinds"),
      diag("rules[1].when", { line: 2, column: 7 }),
      diag("rules[1].effect"),
      diag("rules[0].message"),
      diag("rules"),
      diag(""),
    ]);
    expect(placed.fields.name).toHaveLength(1);
    expect(placed.fields.exceptProjects).toHaveLength(1);
    expect(placed.fields.triggerKinds).toHaveLength(1);
    expect(placed.rules.get(1)!.when.map((d) => d.at)).toEqual(["rules[1].when", "rules[1].effect"]);
    expect(placed.rules.get(0)!.message).toHaveLength(1);
    expect(placed.general.map((d) => d.at)).toEqual(["rules", ""]);
  });

  it("writes a CEL finding with its line and column, and a field's first error", () => {
    expect(diagnosticText(diag("rules[0].when", { line: 2, column: 5, expr: "pipeline.nope", message: "no field" }))).toBe(
      "line 2, column 5: pipeline.nope: no field",
    );
    expect(diagnosticText(diag("name", { message: "bad name" }))).toBe("bad name");
    expect(fieldError([diag("name", { severity: "warning" }), diag("name", { message: "first error" })])).toBe(
      "first error",
    );
    expect(fieldError(undefined)).toBeNull();
  });
});

describe("completionsAt", () => {
  const labels = (schema: CheckpointSchema, text: string, cursor = text.length) =>
    completionsAt(schema, text, cursor).items.map((c) => c.label);

  it("offers the checkpoint's variables once a name is begun", () => {
    expect(labels(runCreate, "tr")).toEqual(["trigger"]);
    expect(labels(runCreate, "")).toEqual([]);
    expect(labels(stepAdmit, "st")).toEqual(["step"]);
  });

  it("offers a struct's fields after a dot, marking optional ones", () => {
    const got = completionsAt(runCreate, "trigger.", 8);
    expect(got.from).toBe(8);
    expect(got.items.map((c) => c.label)).toContain("ref");
    expect(got.items.find((c) => c.label === "ref")!.optional).toBe(true);
    expect(labels(runCreate, "trigger.re")).toEqual(["ref", "reason"]);
  });

  it("binds a macro's iteration variable to the list's element", () => {
    expect(labels(runCreate, "steps.")).toEqual(["exists", "all", "exists_one"]);
    expect(labels(runCreate, "steps.exists(s, s.orig")).toEqual(["origin"]);
    expect(labels(runCreate, "steps.exists(s, s.origin.exists(o, o.ki")).toEqual(["kind"]);
    expect(labels(runCreate, "steps.exists(s, s")).toContain("steps");
  });

  it("offers string functions after a string field", () => {
    expect(labels(runCreate, "pipeline.name.st")).toEqual(["startsWith"]);
  });

  it("offers nothing inside a string literal or after an unknown name", () => {
    expect(labels(runCreate, "trigger.ref == 'tr")).toEqual([]);
    expect(labels(runCreate, "nope.")).toEqual([]);
  });

  it("replaces the partial name when one is picked", () => {
    expect(applyCompletion("trigger.re && x", 8, 10, "ref")).toEqual(["trigger.ref && x", 11]);
  });

  it("leaves decision record references out of a field's doc", () => {
    expect(cleanDoc("The Principal linked to `actor` (ADR-0069), when there is one.")).toBe(
      "The Principal linked to `actor`, when there is one.",
    );
  });
});

describe("policyYaml", () => {
  it("writes a kind: policy file with a multi-line condition as a block", () => {
    const body = policyBodyOf({
      ...newPolicyForm(),
      name: "no-forks",
      environments: "production",
      rules: [{ when: "trigger.fork &&\n  pipeline.deploy", message: "A fork may not deploy." }],
    });
    expect(policyYaml(body)).toBe(
      [
        "kind: policy",
        'name: "no-forks"',
        'checkpoint: "run.create"',
        "mode: audit",
        "match:",
        '  environments: ["production"]',
        "rules:",
        "  - when: |-",
        "      trigger.fork &&",
        "        pipeline.deploy",
        "    effect: deny",
        '    message: "A fork may not deploy."',
        "",
      ].join("\n"),
    );
  });

  it("quotes a one-line condition and writes a typed setting's list", () => {
    const yaml = policyYaml({
      name: "libs",
      checkpoint: "run.create",
      setting: { libraries_allowed: { sources: ["github.com/acme/*"] } },
    });
    expect(yaml).toContain('setting:\n  libraries_allowed:\n    sources: ["github.com/acme/*"]\n');
    expect(yaml).not.toContain("match:");
  });
});

describe("counts and replay in words", () => {
  it("counts denies for an enforced policy and would-denies for an audit one", () => {
    const c = { matched: 9, denied: 3, would_deny: 4 };
    expect(windowCountLabel(c, "enforce", 7)).toBe("denied 3 in 7 days");
    expect(windowCountLabel(c, "audit", 7)).toBe("would deny 4 in 7 days");
    expect(windowCountLabel(undefined, "audit", 7)).toBe("would deny 0 in 7 days");
  });

  it("summarises a replay and links each change to its Run", () => {
    expect(replaySummary({ checkpoint: "step.admit", read: 40, skipped: 2, changed: [1, 2] })).toBe(
      "40 recent step.admit decisions read · 2 skipped (no recorded input) · 2 would change",
    );
    expect(replaySummary({ checkpoint: "run.create", read: 1, skipped: 0, changed: [] })).toBe(
      "1 recent run.create decision read · no outcome changes",
    );
    expect(replayRunHref("acme", { project: "web app", run_id: "r1" })).toBe("/acme/web%20app/runs/r1");
    expect(replayRunHref("acme", { project: "web", run_id: null })).toBeNull();
  });
});
