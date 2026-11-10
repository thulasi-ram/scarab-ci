// The org policy editor, as the UI has to reason about it. Pure: no DOM, no
// fetch, so the page and its tests agree.
//
// - The form a CEL Policy is edited in, and the body it saves.
// - Where each finding of the checker belongs on the page: a field, a rule's
//   `when` (with its line and column), or the Policy as a whole.
// - Field-name completion in a rule's `when`, from the checkpoint's input
//   schema as the server serves it.
// - The Policy as YAML, for Copy YAML and the history diff.
// - The counts and replay results in words.
import type { PolicyBody } from "./policies";
import { splitEntries } from "./policies";

// ── The form ─────────────────────────────────────────────────────────────────

export type RuleForm = { when: string; message: string };

export type PolicyForm = {
  name: string;
  checkpoint: string;
  /** Comma- or line-separated globs. */
  projects: string;
  exceptProjects: string;
  environments: string;
  /** `run.create` only. */
  triggerKinds: string[];
  mode: "audit" | "enforce";
  rules: RuleForm[];
};

/** A new Policy: `run.create`, one empty rule, and audit mode, so it records
 *  what it would deny before it denies anything. */
export function newPolicyForm(): PolicyForm {
  return {
    name: "",
    checkpoint: "run.create",
    projects: "",
    exceptProjects: "",
    environments: "",
    triggerKinds: [],
    mode: "audit",
    rules: [{ when: "", message: "" }],
  };
}

/** The form for a stored or pasted Policy. A rule's effect is not part of
 *  the form: only `deny` is evaluated. */
export function policyFormOf(body: PolicyBody): PolicyForm {
  return {
    name: body.name ?? "",
    checkpoint: body.checkpoint ?? "run.create",
    projects: (body.match?.projects ?? []).join(", "),
    exceptProjects: (body.match?.except_projects ?? []).join(", "),
    environments: (body.match?.environments ?? []).join(", "),
    triggerKinds: [...(body.match?.trigger_kinds ?? [])],
    mode: body.mode === "enforce" ? "enforce" : "audit",
    rules: (body.rules ?? []).map((r) => ({ when: r.when ?? "", message: r.message ?? "" })),
  };
}

/** The body the form saves. Every rule denies; `trigger_kinds` is sent at
 *  `run.create` only, where it is matched. */
export function policyBodyOf(form: PolicyForm): PolicyBody {
  return {
    name: form.name.trim(),
    checkpoint: form.checkpoint,
    match: {
      projects: splitEntries(form.projects),
      except_projects: splitEntries(form.exceptProjects),
      environments: splitEntries(form.environments),
      trigger_kinds: form.checkpoint === "run.create" ? [...form.triggerKinds] : [],
    },
    mode: form.mode,
    rules: form.rules.map((r) => ({ when: r.when, effect: "deny", message: r.message })),
  };
}

// ── Diagnostics ──────────────────────────────────────────────────────────────

/** A finding of the checker (`PolicyDiagnosticDto`). */
export type Diagnostic = {
  severity: string;
  at: string;
  line: number;
  column: number;
  expr: string;
  message: string;
};

export type FormField = "name" | "checkpoint" | "projects" | "exceptProjects" | "environments" | "triggerKinds";

export type RuleDiagnostics = { when: Diagnostic[]; message: Diagnostic[] };

export type PlacedDiagnostics = {
  fields: Partial<Record<FormField, Diagnostic[]>>;
  /** By rule index. */
  rules: Map<number, RuleDiagnostics>;
  /** About the Policy as a whole: not a Policy at all, no rules, too many. */
  general: Diagnostic[];
};

const MATCH_FIELDS: Record<string, FormField> = {
  projects: "projects",
  except_projects: "exceptProjects",
  environments: "environments",
  trigger_kinds: "triggerKinds",
};

/** Where each finding goes on the page, from its `at`: `name`,
 *  `checkpoint`, `match.<key>[i]`, `rules[i].when` (or `.message`,
 *  `.effect`), or the Policy as a whole. */
export function placeDiagnostics(ds: readonly Diagnostic[]): PlacedDiagnostics {
  const out: PlacedDiagnostics = { fields: {}, rules: new Map(), general: [] };
  const toField = (f: FormField, d: Diagnostic) => (out.fields[f] ??= []).push(d);
  for (const d of ds) {
    const rule = /^rules\[(\d+)\]\.(when|message|effect)/.exec(d.at);
    if (rule) {
      const i = Number(rule[1]);
      const slot = out.rules.get(i) ?? { when: [], message: [] };
      // An effect finding is shown with the rule's condition: the effect is
      // fixed to deny on this page.
      (rule[2] === "message" ? slot.message : slot.when).push(d);
      out.rules.set(i, slot);
      continue;
    }
    const m = /^match\.(\w+)/.exec(d.at);
    if (m && MATCH_FIELDS[m[1]!]) {
      toField(MATCH_FIELDS[m[1]!]!, d);
    } else if (d.at === "name" || d.at === "checkpoint") {
      toField(d.at, d);
    } else {
      out.general.push(d);
    }
  }
  return out;
}

/** A finding in one line: where in the rule, the sub-expression, and what
 *  is wrong. */
export function diagnosticText(d: Diagnostic): string {
  if (d.line === 0) return d.message;
  const expr = d.expr ? ` ${d.expr}:` : "";
  return `line ${d.line}, column ${d.column}:${expr} ${d.message}`;
}

/** The first error of a field, as a Field's error line. */
export function fieldError(ds: readonly Diagnostic[] | undefined): string | null {
  const e = ds?.find((d) => d.severity === "error");
  return e ? e.message : null;
}

/** Whether a check found no error. */
export const hasErrors = (ds: readonly Diagnostic[]) => ds.some((d) => d.severity === "error");

// ── Completion in a rule's `when` ────────────────────────────────────────────

/** A type in a served checkpoint schema (`{kind, of}`). */
export type SchemaType =
  | { kind: "bool" | "int" | "uint" | "double" | "string" | "bytes" | "null" | "timestamp" | "duration" | "dyn" }
  | { kind: "enum"; of: string }
  | { kind: "struct"; of: string }
  | { kind: "list"; of: { elem: SchemaType; cap: number } }
  | { kind: "map"; of: { key: SchemaType; value: SchemaType; cap: number } };

export type SchemaField = { name: string; ty: SchemaType; optional: boolean; doc: string };

export type CheckpointSchema = {
  variables: SchemaField[];
  structs: Record<string, { fields: SchemaField[] }>;
  enums: Record<string, { variants: string[] }>;
};

export type Completion = {
  /** What is inserted. */
  label: string;
  /** The type, in words. */
  detail: string;
  doc: string;
  /** Read it with `has()` (a field) or `!= null` (a variable). */
  optional: boolean;
};

/** A type in words: `string`, `list<Step>`, `Trigger`, `GateKind`. */
export function typeLabel(t: SchemaType): string {
  switch (t.kind) {
    case "list":
      return `list<${typeLabel(t.of.elem)}>`;
    case "map":
      return `map<${typeLabel(t.of.key)}, ${typeLabel(t.of.value)}>`;
    case "struct":
    case "enum":
      return t.of;
    default:
      return t.kind;
  }
}

/** A field's documentation, as the editor shows it: decision record
 *  references are left out. */
export function cleanDoc(doc: string): string {
  return doc.replace(/\s*\(ADR-\d{4}[^)]*\)/g, "").trim();
}

const MACROS = ["exists", "all", "exists_one"];
const STRING_FUNCTIONS = ["startsWith", "endsWith", "contains", "matches"];

/** The type `path` (`trigger`, `steps`, `s.origin` with `s` bound) names, or
 *  `undefined`. */
function resolve(
  schema: CheckpointSchema,
  bound: Map<string, SchemaType>,
  path: string[],
): SchemaType | undefined {
  const [head, ...rest] = path;
  let t: SchemaType | undefined = bound.get(head!) ?? schema.variables.find((v) => v.name === head)?.ty;
  for (const seg of rest) {
    if (t?.kind !== "struct") return undefined;
    t = schema.structs[t.of]?.fields.find((f) => f.name === seg)?.ty;
  }
  return t;
}

/** The iteration variables bound before the cursor: `steps.exists(s, …`
 *  binds `s` to a Step. Scopes are not tracked: a variable stays bound to
 *  the end of the rule. */
function boundVariables(schema: CheckpointSchema, before: string): Map<string, SchemaType> {
  const bound = new Map<string, SchemaType>();
  const re = /([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)\.(?:exists_one|exists|all)\(\s*([A-Za-z_]\w*)\s*,/g;
  for (let m = re.exec(before); m; m = re.exec(before)) {
    const list = resolve(schema, bound, m[1]!.split("."));
    if (list?.kind === "list") bound.set(m[2]!, list.of.elem);
  }
  return bound;
}

/** Whether the cursor is inside a string literal. */
function inString(before: string): boolean {
  let quote: string | null = null;
  for (let i = 0; i < before.length; i++) {
    const c = before[i]!;
    if (quote) {
      if (c === "\\") i++;
      else if (c === quote) quote = null;
    } else if (c === "'" || c === '"') {
      quote = c;
    }
  }
  return quote !== null;
}

/**
 * What can be typed at `cursor` in `text`: after `trigger.`, the fields of
 * the trigger; after a list, the macros; after a string, its functions; at
 * the start of a name, the checkpoint's variables and the iteration
 * variables in scope. `from` is where the partial name starts, so a pick
 * replaces it. Nothing inside a string literal, and nothing before a name
 * is begun.
 */
export function completionsAt(
  schema: CheckpointSchema,
  text: string,
  cursor: number,
): { from: number; items: Completion[] } {
  const before = text.slice(0, cursor);
  const none = { from: cursor, items: [] };
  if (inString(before)) return none;
  const m = /((?:[A-Za-z_]\w*\.)*)([A-Za-z_]\w*)?$/.exec(before);
  if (!m) return none;
  const path = m[1] ? m[1].slice(0, -1).split(".") : [];
  const prefix = m[2] ?? "";
  const from = cursor - prefix.length;
  // A member access on something that is not a name (`f(x).`) has no
  // completions here.
  const start = cursor - m[0].length;
  if (path.length > 0 && start > 0 && /[\w)\]]/.test(before[start - 1]!)) return none;
  const bound = boundVariables(schema, before);
  const keep = (items: Completion[]) => ({
    from,
    items: items.filter((c) => c.label.startsWith(prefix) && c.label !== prefix),
  });

  if (path.length === 0) {
    if (prefix === "") return none;
    const vars: Completion[] = [...bound].map(([name, ty]) => ({
      label: name,
      detail: typeLabel(ty),
      doc: "the element the macro is iterating over",
      optional: false,
    }));
    for (const v of schema.variables) {
      vars.push({ label: v.name, detail: typeLabel(v.ty), doc: cleanDoc(v.doc), optional: v.optional });
    }
    return keep(vars);
  }

  const t = resolve(schema, bound, path);
  if (!t) return none;
  if (t.kind === "struct") {
    return keep(
      (schema.structs[t.of]?.fields ?? []).map((f) => ({
        label: f.name,
        detail: typeLabel(f.ty),
        doc: cleanDoc(f.doc),
        optional: f.optional,
      })),
    );
  }
  if (t.kind === "list") {
    return keep(
      MACROS.map((name) => ({
        label: name,
        detail: `macro over ${typeLabel(t.of.elem)}`,
        doc: `${name}(x, condition)`,
        optional: false,
      })),
    );
  }
  if (t.kind === "string") {
    return keep(
      STRING_FUNCTIONS.map((name) => ({
        label: name,
        detail: "string function",
        doc: name === "matches" ? "matches('literal pattern')" : `${name}('text')`,
        optional: false,
      })),
    );
  }
  return none;
}

/** `text` with the completion `label` put in at `from`, replacing the
 *  partial name up to `cursor`; and where the cursor goes after it. */
export function applyCompletion(text: string, from: number, cursor: number, label: string): [string, number] {
  return [text.slice(0, from) + label + text.slice(cursor), from + label.length];
}

// ── YAML ─────────────────────────────────────────────────────────────────────

/** A scalar as YAML: a string double-quoted (JSON's quoting is YAML's),
 *  anything else as JSON, which is YAML's flow style. */
const scalar = (v: unknown) => JSON.stringify(v);

/** A string over several lines as a literal block at `indent`, when it can
 *  be one; else quoted. */
function text(v: string, indent: string): string {
  if (v.includes("\n") && !/^\s/.test(v) && !v.endsWith("\n")) {
    return `|-\n${v
      .split("\n")
      .map((l) => (l === "" ? "" : `${indent}${l}`))
      .join("\n")}`;
  }
  return scalar(v);
}

/** The Policy as a `kind: policy` YAML file, the form the editor's Paste
 *  YAML reads back. Empty `match` lists are left out. */
export function policyYaml(body: PolicyBody): string {
  const lines = ["kind: policy", `name: ${scalar(body.name)}`, `checkpoint: ${scalar(body.checkpoint)}`];
  if (body.mode) lines.push(`mode: ${body.mode}`);
  const match = Object.entries(body.match ?? {}).filter(([, v]) => Array.isArray(v) && v.length > 0);
  if (match.length > 0) {
    lines.push("match:");
    for (const [k, v] of match) lines.push(`  ${k}: ${scalar(v)}`);
  }
  if (body.rules) {
    lines.push("rules:");
    for (const r of body.rules) {
      lines.push(`  - when: ${text(r.when, "      ")}`);
      lines.push(`    effect: ${typeof r.effect === "string" ? r.effect : scalar(r.effect)}`);
      lines.push(`    message: ${scalar(r.message)}`);
    }
  }
  if (body.setting) {
    lines.push("setting:");
    for (const [kind, fields] of Object.entries(body.setting)) {
      lines.push(`  ${kind}:`);
      for (const [k, v] of Object.entries(fields)) lines.push(`    ${k}: ${scalar(v)}`);
    }
  }
  return lines.join("\n") + "\n";
}

// ── Counts and replay, in words ──────────────────────────────────────────────

/** A Policy's counts over the window (`PolicyCountsDto`). */
export type WindowCounts = { matched: number; denied: number; would_deny: number };

/** The list's "denied / would deny N in 7 days": an audit-mode Policy
 *  counts what it would have denied, an enforced one what it denied. */
export function windowCountLabel(c: WindowCounts | undefined, mode: string | undefined, days: number): string {
  const n = mode === "audit" ? (c?.would_deny ?? 0) : (c?.denied ?? 0);
  return `${mode === "audit" ? "would deny" : "denied"} ${n} in ${days} days`;
}

export const VERDICT_LABEL: Record<string, string> = {
  allow: "allowed",
  deny: "denied",
  would_deny: "would deny (audit)",
};

/** A replay's summary line. */
export function replaySummary(r: { checkpoint: string; read: number; skipped: number; changed: unknown[] }): string {
  const parts = [`${r.read} recent ${r.checkpoint} decision${r.read === 1 ? "" : "s"} read`];
  if (r.skipped > 0) parts.push(`${r.skipped} skipped (no recorded input)`);
  parts.push(r.changed.length === 0 ? "no outcome changes" : `${r.changed.length} would change`);
  return parts.join(" · ");
}

/** The Run page a replayed decision is about, or `null` when it has no Run
 *  (a refused `run.create`). */
export function replayRunHref(
  org: string,
  c: { project?: string | null; run_id?: string | null },
): string | null {
  if (!c.project || !c.run_id) return null;
  return `/${encodeURIComponent(org)}/${encodeURIComponent(c.project)}/runs/${c.run_id}`;
}

/** The editor's URL for a Policy, or for a new one. */
export const policyEditHref = (org: string, id?: string) =>
  `/orgs/${encodeURIComponent(org)}/settings/policies/${id ? encodeURIComponent(id) : "new"}`;

export const policyHistoryHref = (org: string, id: string) => `${policyEditHref(org, id)}?tab=history`;
