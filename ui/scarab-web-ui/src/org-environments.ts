// Org-level Environments (ADR-0071, MMP step 1), as the UI has to reason about
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
