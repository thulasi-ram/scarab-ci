// Org-level Environments, as the UI has to reason about
// them. Pure — no DOM, no fetch — so the Settings panel and its tests agree on
// what a form sends.
//
// The whole-Environment PUT carries its display name and description;
// protection, eligibility and privilege grants each have their own write. The
// PUT is a FULL replace, so `toPutBody` always sends every field.
//
// The name is fixed once created: pipeline YAML and the OIDC subject
// (`…/env/<name>/…`) use it. The display name is what headings, lists and
// pickers show; where the name matters it is shown beside it in mono.
//
// The server enforces every rule here; mirroring them only lets the form say
// what is wrong before the round trip. Where the two could disagree the server
// wins, and its 400/409 text is shown verbatim.
import {
  rulesToForm,
  splitList,
  toEligibilityBody,
  toProtectionBody,
  type EligibilityLike,
  type ProtectionBody,
  type RulesForm,
  type RulesLike,
} from "./environment-overlays";
import { orgHref } from "./settings-nav";

/** The editable fields. The name is the URL, not the body. */
export type OrgEnvironmentForm = {
  displayName: string;
  description: string;
};

/** What the PUT carries. `display_name: null` shows the name. */
export type OrgEnvironmentBody = {
  display_name: string | null;
  description: string;
};

/** The record shape these helpers read — structural, so the generated
 *  `OrgEnvironment` satisfies it without this module importing the schema. */
export type OrgEnvironmentLike = {
  name: string;
  display_name?: string | null;
  description: string;
};

type Built<T> = { body: T; error?: undefined } | { body?: undefined; error: string };

/** The longest display name the server accepts, in characters. */
export const MAX_DISPLAY_NAME = 200;

/** What people see for an Environment: its display name, or its name. */
export function shownName(env: { name: string; display_name?: string | null }): string {
  return env.display_name?.trim() || env.name;
}

/** The name, when it should be shown beside the display name (in mono):
 *  `null` when there is no display name or it is the name. */
export function nameBeside(env: { name: string; display_name?: string | null }): string | null {
  return shownName(env) === env.name ? null : env.name;
}

/** One line for a picker option or a plain-text label: "Production (EU) · prod",
 *  or the name alone. */
export function environmentLabel(env: { name: string; display_name?: string | null }): string {
  const beside = nameBeside(env);
  return beside === null ? env.name : `${shownName(env)} · ${beside}`;
}

/** Why `displayName` cannot be saved, or `null`. Blank is fine: it shows the
 *  name. */
export function displayNameError(displayName: string): string | null {
  if (/[\u0000-\u001f\u007f-\u009f]/.test(displayName)) {
    return "A display name cannot contain control characters.";
  }
  if ([...displayName.trim()].length > MAX_DISPLAY_NAME) {
    return `A display name is at most ${MAX_DISPLAY_NAME} characters.`;
  }
  return null;
}

/** Why `name` cannot name an Environment, or `null` if it can — the server's
 *  rule, checked on the name exactly as typed (never trimmed: the name is the
 *  key a pipeline's `environment:` uses, case and spaces included). Permissive on purpose: any
 *  name a pipeline can deploy to must be registrable, so spaces, `/` and any
 *  length are fine. Only empty, `.`/`..` (a URL cannot address them) and
 *  control characters are refused. */
export function nameError(name: string): string | null {
  if (name.length === 0) return "A name is required.";
  if (name === "." || name === "..") return `"${name}" cannot name an environment.`;
  if (/[\u0000-\u001f\u007f-\u009f]/.test(name)) return "A name cannot contain control characters.";
  return null;
}

/** The PUT body for `form`. Both are trimmed; a blank display name is
 *  `null` (show the name). */
export function toPutBody(form: OrgEnvironmentForm): { body: OrgEnvironmentBody } {
  return {
    body: { display_name: form.displayName.trim() || null, description: form.description.trim() },
  };
}

// ── The Environment's own pages ──────────────────────────────────────────────
//
// An Environment is managed at `/orgs/<org>/settings/environments/<name>/manage`:
// six segments under a static `/orgs` first segment, with a static last
// segment, so it collides with neither `/:org/:repo/runs/:id` nor the
// four-segment settings sections. The name is one encoded segment: names may
// hold `/`, spaces and `%`. The addresses it had before, `…/<name>` and
// `…/<name>/edit`, redirect here.

const sectionHref = (org: string) => orgHref(org, "environments");

/** The Environments list in org settings. */
export const environmentsHref = (org: string) => sectionHref(org);

/** The manage page of one Environment. */
export const environmentHref = (org: string, name: string) =>
  `${sectionHref(org)}/${encodeURIComponent(name)}/manage`;

/** A destructive action goes ahead only when the exact name is typed — case
 *  and spaces included, as everywhere else the name is used. */
export const confirmsName = (typed: string, name: string) => name.length > 0 && typed === name;

// ── The Environment approval gate ────────────────────────────────────────────
//
// An Environment with approvers asks them on its approval gate: a `manual`
// gate with a Step after it that uses the Environment's authority (its
// secrets, a privilege grant, a k8s_overlay, a write forge token or the
// deploy mark). When a pipeline has no such gate, the Environment adds one to
// the Run, or refuses the Run (its missing-gate mode). The baseline also sets
// the added gate's expiry, how many approvers must approve, and how long an
// approval stays good for. These settings are the baseline's alone: a
// project overlay has none.

/** What Run creation does when no `manual` gate guards the Environment's
 *  Steps. */
export type MissingGateMode = "add" | "refuse";

/** The approval gate settings as the server serialises them. */
export type ApprovalGateLike = {
  missing_gate_mode: MissingGateMode;
  /** Seconds; absent: the added gate waits until approved or rejected. */
  added_gate_expiry_s?: number | null;
  /** Absent: every approver must approve. */
  required_approvals?: number | null;
  max_approval_age_s: number;
};

/** The server's default maximum approval age: 24 hours. */
export const DEFAULT_MAX_APPROVAL_AGE_S = 24 * 60 * 60;

/** The settings of a baseline that has never set them. */
export const DEFAULT_APPROVAL_GATE: ApprovalGateLike = {
  missing_gate_mode: "add",
  max_approval_age_s: DEFAULT_MAX_APPROVAL_AGE_S,
};

/** The approval gate settings as the form holds them: strings, possibly
 *  half typed. Blank means the default. */
export type ApprovalGateForm = {
  missingGateMode: MissingGateMode;
  /** Seconds; blank: no expiry. */
  addedGateExpiryS: string;
  /** Blank: every approver. */
  requiredApprovals: string;
  /** Seconds; blank: 24 hours. */
  maxApprovalAgeS: string;
};

/** The form for stored settings (or the defaults). */
export function approvalGateToForm(g?: ApprovalGateLike | null): ApprovalGateForm {
  const gate = g ?? DEFAULT_APPROVAL_GATE;
  return {
    missingGateMode: gate.missing_gate_mode,
    addedGateExpiryS: gate.added_gate_expiry_s != null ? String(gate.added_gate_expiry_s) : "",
    requiredApprovals: gate.required_approvals != null ? String(gate.required_approvals) : "",
    maxApprovalAgeS: String(gate.max_approval_age_s),
  };
}

/** The baseline PUT body (`…/environments/{env}/protection`): the four rules
 *  and the approval gate settings under `approval_gate`, the shape GET
 *  returns. The PUT replaces the whole baseline, so every field is always
 *  sent. */
export type BaselineBody = ProtectionBody & {
  approval_gate: {
    missing_gate_mode: MissingGateMode;
    added_gate_expiry_s: number | null;
    required_approvals: number | null;
    max_approval_age_s: number;
  };
};

/** A duration in seconds in words: "24 hours", "1 hour 30 minutes",
 *  "45 seconds". */
export function durationText(seconds: number): string {
  if (seconds <= 0) return "0 seconds";
  const units: [number, string][] = [
    [3600, "hour"],
    [60, "minute"],
    [1, "second"],
  ];
  const parts: string[] = [];
  let left = seconds;
  for (const [size, name] of units) {
    const n = Math.floor(left / size);
    if (n > 0) {
      parts.push(`${n} ${name}${n === 1 ? "" : "s"}`);
      left -= n * size;
    }
    if (parts.length === 2) break;
  }
  return parts.join(" ");
}

/** A whole number of seconds above 0, or `null` for blank; `undefined` when
 *  it does not parse. */
function positiveSeconds(raw: string): number | null | undefined {
  const t = raw.trim();
  if (t === "") return null;
  const n = Number(t);
  return Number.isInteger(n) && n > 0 ? n : undefined;
}

/** Why the required approvals cannot be saved, or `null`. `approvers` is the
 *  number of baseline approvers the form names. */
export function requiredApprovalsError(raw: string, approvers: number): string | null {
  const t = raw.trim();
  if (t === "") return null;
  if (approvers === 0) return "Add approvers first.";
  const n = Number(t);
  if (!Number.isInteger(n) || n < 1 || n > approvers) {
    return approvers === 1
      ? "With one approver, the only count is 1."
      : `A whole number from 1 to ${approvers}, the number of approvers.`;
  }
  return null;
}

/** The baseline body for the form's rules and approval gate settings, or the
 *  first reason it cannot be sent.
 *
 *  The wait timer counts on the approval gate, so it needs approvers: with
 *  none, the body carries 0 (the server refuses anything else). */
export function toBaselineBody(rules: RulesForm, gate: ApprovalGateForm): Built<BaselineBody> {
  const protection = toProtectionBody(rules);
  if (protection.error !== undefined) return { error: protection.error };
  const p = protection.body;
  const required = requiredApprovalsError(gate.requiredApprovals, p.approvers.length);
  if (required !== null) return { error: `Required approvals: ${required}` };
  const expiry = positiveSeconds(gate.addedGateExpiryS);
  if (expiry === undefined) {
    return { error: "The added gate's expiry must be a whole number of seconds above 0, or blank." };
  }
  const maxAge = positiveSeconds(gate.maxApprovalAgeS);
  if (maxAge === undefined) {
    return { error: "The maximum approval age must be a whole number of seconds above 0." };
  }
  const requiredN = gate.requiredApprovals.trim() === "" ? null : Number(gate.requiredApprovals.trim());
  return {
    body: {
      ...p,
      wait_timer_s: p.approvers.length === 0 ? 0 : p.wait_timer_s,
      approval_gate: {
        missing_gate_mode: gate.missingGateMode,
        added_gate_expiry_s: expiry,
        required_approvals: requiredN,
        max_approval_age_s: maxAge ?? DEFAULT_MAX_APPROVAL_AGE_S,
      },
    },
  };
}

/** The warning an Environment with a wait timer and no approvers shows, or
 *  `null`. The timer waits on the approval gate, which needs approvers. */
export function idleTimerWarning(rules: { approvers: readonly string[]; wait_timer_s: number }): string | null {
  if (rules.approvers.length > 0 || rules.wait_timer_s <= 0) return null;
  return `The ${durationText(rules.wait_timer_s)} wait timer does nothing until this environment has approvers: it waits on the approval gate, which only an environment with approvers has.`;
}

/** What the manage page's form holds: the display name, the description, the
 *  baseline and its approval gate settings, eligibility. */
export type EnvironmentEditForm = {
  displayName: string;
  description: string;
  baseline: RulesForm;
  gate: ApprovalGateForm;
  allProjects: boolean;
  /** Comma-separated project-name patterns. */
  patterns: string;
};

/** The stored record as the manage page reads it. Structural, like
 *  `OrgEnvironmentLike`. */
export type EditableEnvironmentLike = OrgEnvironmentLike & {
  protection: RulesLike & { approval_gate?: ApprovalGateLike | null };
  eligibility: EligibilityLike;
};

/** The edit form for a stored Environment. */
export function toEditForm(env: EditableEnvironmentLike): EnvironmentEditForm {
  return {
    displayName: env.display_name ?? "",
    description: env.description,
    baseline: rulesToForm(env.protection),
    gate: approvalGateToForm(env.protection.approval_gate),
    allProjects: env.eligibility.all_projects,
    patterns: (env.eligibility.projects ?? []).join(", "),
  };
}

/** Does the form name any approver? The wait timer and the required
 *  approvals need one. */
export const formHasApprovers = (form: RulesForm) => splitList(form.approvers).length > 0;

/** One write a Save makes. Each part has its own endpoint, so a Save sends
 *  only the parts that changed — an unchanged baseline is never re-sent. */
export type EnvironmentWrite =
  | { part: "details"; body: OrgEnvironmentBody }
  | { part: "baseline"; body: BaselineBody }
  | { part: "eligibility"; body: { all_projects: boolean; projects: string[] } };

const sameList = (a: readonly string[], b: readonly string[]) =>
  a.length === b.length && a.every((x, i) => x === b[i]);

/**
 * The writes that turn `env` into `form`, in the order they are sent
 * (display name and description, baseline, eligibility), or the error that stops the Save
 * before any is sent. Compared on the bodies the server would store, so
 * re-ordering spaces or commas alone sends nothing.
 */
export function planSave(
  env: EditableEnvironmentLike,
  form: EnvironmentEditForm,
): { writes: EnvironmentWrite[]; error?: undefined } | { writes?: undefined; error: string } {
  const displayProblem = displayNameError(form.displayName);
  if (displayProblem !== null) return { error: displayProblem };
  const protection = toBaselineBody(form.baseline, form.gate);
  if (protection.error !== undefined) return { error: protection.error };
  const eligibility = toEligibilityBody(form.allProjects, form.patterns);
  if (eligibility.error !== undefined) return { error: eligibility.error };

  const writes: EnvironmentWrite[] = [];
  const details = toPutBody({ displayName: form.displayName, description: form.description }).body;
  // The stored values are compared trimmed too: one stored with edge spaces
  // (set through the API) is not a change the form made.
  if (
    details.description !== env.description.trim() ||
    details.display_name !== (env.display_name?.trim() || null)
  ) {
    writes.push({ part: "details", body: details });
  }
  const p = protection.body;
  const was = env.protection;
  const wasGate = was.approval_gate ?? DEFAULT_APPROVAL_GATE;
  if (
    !sameList(p.approvers, was.approvers) ||
    !sameList(p.allowed_refs, was.allowed_refs) ||
    // With no approvers the timer is not editable and the body carries 0;
    // a stored timer alone is not a change the form made.
    (p.approvers.length > 0 && p.wait_timer_s !== was.wait_timer_s) ||
    p.require_reason !== was.require_reason ||
    p.approval_gate.missing_gate_mode !== wasGate.missing_gate_mode ||
    p.approval_gate.added_gate_expiry_s !== (wasGate.added_gate_expiry_s ?? null) ||
    p.approval_gate.required_approvals !== (wasGate.required_approvals ?? null) ||
    p.approval_gate.max_approval_age_s !== wasGate.max_approval_age_s
  ) {
    writes.push({ part: "baseline", body: p });
  }
  const e = eligibility.body;
  const wasAll = env.eligibility.all_projects;
  if (e.all_projects !== wasAll || (!wasAll && !sameList(e.projects, env.eligibility.projects ?? []))) {
    writes.push({ part: "eligibility", body: e });
  }
  return { writes };
}

/** How each part is named when a Save reports what did or did not save. */
export const PART_LABEL: Record<EnvironmentWrite["part"], string> = {
  details: "display name and description",
  baseline: "baseline",
  eligibility: "list of projects that may use it",
};

/** What a Save says when the Environment is gone before its first write. */
export const DELETED_ELSEWHERE = "This environment was deleted by someone else.";

/**
 * Send a Save's `writes` in order, and say what went wrong, or `null`.
 *
 * Before the first write it asks `stillThere` whether the Environment still
 * exists. The details write is a create-or-replace, so a Save on a page
 * opened before another admin deleted the Environment would otherwise bring
 * it back with the default baseline and eligibility, and the delete would be
 * lost. A missing Environment stops the Save with nothing sent.
 *
 * Stops at the first write that fails and names the parts that did save.
 */
export async function runSave(
  writes: readonly EnvironmentWrite[],
  io: {
    stillThere: () => Promise<boolean>;
    send: (write: EnvironmentWrite) => Promise<unknown>;
  },
): Promise<string | null> {
  if (writes.length === 0) return null;
  try {
    if (!(await io.stillThere())) return DELETED_ELSEWHERE;
  } catch (e) {
    const why = e instanceof Error ? e.message : "the server did not answer";
    return `Could not check the environment before saving: ${why}.`;
  }
  const done: string[] = [];
  for (const w of writes) {
    try {
      await io.send(w);
      done.push(PART_LABEL[w.part]);
    } catch (e) {
      const why = e instanceof Error ? e.message : "the server refused it";
      const saved = done.length > 0 ? ` The ${done.join(" and ")} saved.` : "";
      return `Could not save the ${PART_LABEL[w.part]}: ${why}.${saved}`;
    }
  }
  return null;
}

// ── Delete ───────────────────────────────────────────────────────────────────
//
// A plain `DELETE …/environments/<name>` is refused (409) while a project
// holds an overlay, a privilege grant or a secret in the Environment. The
// refusal lists them. `?cascade=true` deletes them with the Environment.

/** The 409 body of a plain delete (structural). */
export type DeleteBlockersLike = {
  code: string;
  overlay_count: number;
  privilege_grant_count: number;
  secret_count: number;
  overlays: readonly string[];
  privilege_grants: readonly string[];
  secrets: readonly { project: string; keys: readonly string[] }[];
};

/** Is `x` the 409 body that lists what blocks a delete? */
export function isDeleteBlockers(x: unknown): x is DeleteBlockersLike {
  return (
    typeof x === "object" &&
    x !== null &&
    (x as { code?: unknown }).code === "environment_in_use" &&
    Array.isArray((x as { overlays?: unknown }).overlays)
  );
}

const counted = (count: number, one: string, many: string) =>
  `${count} ${count === 1 ? one : many}`;

/** "a, b and c". */
const andList = (xs: readonly string[]) =>
  xs.length <= 1 ? xs.join("") : `${xs.slice(0, -1).join(", ")} and ${xs[xs.length - 1]}`;

/** The confirming button once the blockers are known: "Delete the
 *  environment and its 1 overlay, 1 grant and 2 secrets". Kinds with none
 *  are left out. */
export function cascadeDeleteLabel(b: DeleteBlockersLike): string {
  const parts = [
    b.overlay_count > 0 ? counted(b.overlay_count, "overlay", "overlays") : null,
    b.privilege_grant_count > 0 ? counted(b.privilege_grant_count, "grant", "grants") : null,
    b.secret_count > 0 ? counted(b.secret_count, "secret", "secrets") : null,
  ].filter((p): p is string => p !== null);
  return parts.length === 0
    ? "Delete environment"
    : `Delete the environment and its ${andList(parts)}`;
}

/** One line per kind that blocks the delete, naming the projects (and each
 *  project's secret names). */
export function blockerLines(b: DeleteBlockersLike): string[] {
  const lines: string[] = [];
  if (b.overlays.length > 0) {
    lines.push(`Overlays of ${andList(b.overlays)}`);
  }
  if (b.privilege_grants.length > 0) {
    lines.push(`Privilege grants of ${andList(b.privilege_grants)}`);
  }
  for (const s of b.secrets) {
    lines.push(`Secrets of ${s.project}: ${s.keys.join(", ")}`);
  }
  return lines;
}

// ── Duplicate ────────────────────────────────────────────────────────────────
//
// `POST …/environments/<name>/duplicate` creates a new Environment with the
// old one's baseline, eligibility, every project's overlay and grant, and its
// secrets with their values. The old one is not changed. The answer lists
// what was not copied, which the page shows as a checklist.

/** What the duplicate POST carries. No display name: the server uses the old
 *  one's plus " (copy)". */
export type DuplicateBody = { name: string; display_name?: string };

/** The duplicate body, or why it cannot be sent. `taken` are the names
 *  already registered in the org. */
export function toDuplicateBody(
  name: string,
  displayName: string,
  taken: readonly string[],
): { body: DuplicateBody; error?: undefined } | { body?: undefined; error: string } {
  const problem = nameError(name);
  if (problem !== null) return { error: problem };
  if (taken.includes(name)) return { error: `${name} is already registered. Pick another name.` };
  const displayProblem = displayNameError(displayName);
  if (displayProblem !== null) return { error: displayProblem };
  const shown = displayName.trim();
  return { body: shown ? { name, display_name: shown } : { name } };
}

/** The parts of the duplicate's answer the checklist reads (structural). */
export type DuplicateResultLike = {
  environment: { name: string };
  copied: {
    overlays: string[];
    privilege_grants: string[];
    secrets: { project: string; keys: string[] }[];
  };
  not_copied: {
    policies: { id: string; name: string; project?: string | null; environments: string[] }[];
    projects_with_runs: string[];
    old_oidc_subject: string;
    new_oidc_subject: string;
    notes: string[];
  };
};

/** One line of the "what was copied" summary. */
export function copiedSummary(r: DuplicateResultLike): string[] {
  const n = (count: number, one: string, many: string) => `${count} ${count === 1 ? one : many}`;
  const secretCount = r.copied.secrets.reduce((sum, s) => sum + s.keys.length, 0);
  return [
    "baseline rules and the projects that may use it",
    n(r.copied.overlays.length, "project overlay", "project overlays"),
    n(r.copied.privilege_grants.length, "privilege grant", "privilege grants"),
    `${n(secretCount, "secret", "secrets")} with their values${
      r.copied.secrets.length > 0 ? ` (${r.copied.secrets.map((s) => s.project).join(", ")})` : ""
    }`,
  ];
}

/** One item to do by hand after a duplicate. `items` are the names it is about
 *  (shown in mono); empty when the item is a single instruction. */
export type ChecklistItem = { title: string; detail: string; items: string[] };

/** The checklist of what the duplicate did not copy, in the order to work
 *  through it. Policies and projects appear only when there are some; the
 *  OIDC subject always does, since Scarab cannot see cloud trust policies. */
export function duplicateChecklist(old: string, r: DuplicateResultLike): ChecklistItem[] {
  const fresh = r.environment.name;
  const out: ChecklistItem[] = [];
  if (r.not_copied.projects_with_runs.length > 0) {
    out.push({
      title: "Change the pipelines",
      detail: `These projects deployed into ${old}. Their pipelines deploy into ${fresh} only once their environment: says ${fresh}.`,
      items: r.not_copied.projects_with_runs,
    });
  }
  if (r.not_copied.policies.length > 0) {
    out.push({
      title: "Review the policies",
      detail: `These policies apply to ${old} and not to ${fresh}.`,
      items: r.not_copied.policies.map((p) => (p.project ? `${p.project} / ${p.name}` : p.name)),
    });
  }
  out.push({
    title: "Update cloud trust policies",
    detail: `A cloud role that trusts ${r.not_copied.old_oidc_subject} must also trust ${r.not_copied.new_oidc_subject} before a run in ${fresh} can assume it.`,
    items: [],
  });
  for (const note of r.not_copied.notes) out.push({ title: "Note", detail: note, items: [] });
  return out;
}
