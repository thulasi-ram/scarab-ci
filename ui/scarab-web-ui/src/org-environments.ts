// Org-level Environments, as the UI has to reason about
// them. Pure — no DOM, no fetch — so the Settings panel and its tests agree on
// what a form sends.
//
// The whole-Environment PUT carries only its description; protection,
// eligibility and privilege grants each have their own write. The PUT is a
// FULL replace, so `toPutBody` always sends every field.
//
// The server enforces every rule here; mirroring them only lets the form say
// what is wrong before the round trip. Where the two could disagree the server
// wins, and its 400/409 text is shown verbatim.
import {
  rulesToForm,
  toEligibilityBody,
  toProtectionBody,
  type EligibilityLike,
  type ProtectionBody,
  type RulesForm,
  type RulesLike,
} from "./environment-overlays";

/** The editable fields. The name is the URL, not the body. */
export type OrgEnvironmentForm = {
  description: string;
};

/** What the PUT carries. */
export type OrgEnvironmentBody = {
  description: string;
};

/** The record shape these helpers read — structural, so the generated
 *  `OrgEnvironment` satisfies it without this module importing the schema. */
export type OrgEnvironmentLike = {
  name: string;
  description: string;
};

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

/** The form for an existing Environment (or a fresh one). */
export function toForm(env?: OrgEnvironmentLike): OrgEnvironmentForm {
  return { description: env?.description ?? "" };
}

/** The PUT body for `form`. The description is trimmed. */
export function toPutBody(form: OrgEnvironmentForm): { body: OrgEnvironmentBody } {
  return { body: { description: form.description.trim() } };
}

// ── The Environment's own pages ──────────────────────────────────────────────
//
// An Environment is viewed at `/orgs/<org>/settings/environments/<name>`
// (read-only) and changed at `…/<name>/edit`. Five and six segments under a
// static `/orgs` first segment, so neither can collide with `/:org/:repo`,
// `/:org/:repo/runs/:id` or the four-segment settings sections. The name is
// one encoded segment: names may hold `/`, spaces and `%`.

const sectionHref = (org: string) => `/orgs/${encodeURIComponent(org)}/settings/environments`;

/** The Environments list in org settings. */
export const environmentsHref = (org: string) => sectionHref(org);

/** The read-only page of one Environment. */
export const environmentHref = (org: string, name: string) =>
  `${sectionHref(org)}/${encodeURIComponent(name)}`;

/** The edit page of one Environment. */
export const environmentEditHref = (org: string, name: string) => `${environmentHref(org, name)}/edit`;

/** The Environment name in a route param. The router hands over the raw,
 *  still-encoded path segment; a malformed escape is taken as typed. */
export function nameFromParam(raw: string): string {
  try {
    return decodeURIComponent(raw);
  } catch {
    return raw;
  }
}

/** A destructive action goes ahead only when the exact name is typed — case
 *  and spaces included, as everywhere else the name is used. */
export const confirmsName = (typed: string, name: string) => name.length > 0 && typed === name;

/** What the edit page holds: the description, the baseline, eligibility. */
export type EnvironmentEditForm = {
  description: string;
  baseline: RulesForm;
  allProjects: boolean;
  /** Comma-separated project-name patterns. */
  patterns: string;
};

/** The stored record as the edit page reads it. Structural, like
 *  `OrgEnvironmentLike`. */
export type EditableEnvironmentLike = OrgEnvironmentLike & {
  protection: RulesLike;
  eligibility: EligibilityLike;
};

/** The edit form for a stored Environment. */
export function toEditForm(env: EditableEnvironmentLike): EnvironmentEditForm {
  return {
    description: env.description,
    baseline: rulesToForm(env.protection),
    allProjects: env.eligibility.all_projects,
    patterns: (env.eligibility.projects ?? []).join(", "),
  };
}

/** One write a Save makes. Each part has its own endpoint, so a Save sends
 *  only the parts that changed — an unchanged baseline is never re-sent. */
export type EnvironmentWrite =
  | { part: "description"; body: OrgEnvironmentBody }
  | { part: "baseline"; body: ProtectionBody }
  | { part: "eligibility"; body: { all_projects: boolean; projects: string[] } };

const sameList = (a: readonly string[], b: readonly string[]) =>
  a.length === b.length && a.every((x, i) => x === b[i]);

/**
 * The writes that turn `env` into `form`, in the order they are sent
 * (description, baseline, eligibility), or the error that stops the Save
 * before any is sent. Compared on the bodies the server would store, so
 * re-ordering spaces or commas alone sends nothing.
 */
export function planSave(
  env: EditableEnvironmentLike,
  form: EnvironmentEditForm,
): { writes: EnvironmentWrite[]; error?: undefined } | { writes?: undefined; error: string } {
  const protection = toProtectionBody(form.baseline);
  if (protection.error !== undefined) return { error: protection.error };
  const eligibility = toEligibilityBody(form.allProjects, form.patterns);
  if (eligibility.error !== undefined) return { error: eligibility.error };

  const writes: EnvironmentWrite[] = [];
  const description = toPutBody({ description: form.description }).body;
  if (description.description !== env.description) {
    writes.push({ part: "description", body: description });
  }
  const p = protection.body;
  const was = env.protection;
  if (
    !sameList(p.approvers, was.approvers) ||
    !sameList(p.allowed_refs, was.allowed_refs) ||
    p.wait_timer_s !== was.wait_timer_s ||
    p.require_reason !== was.require_reason
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
