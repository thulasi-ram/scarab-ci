// Org-level Environments (ADR-0071, MMP step 1), as the UI has to reason about
// them. Pure — no DOM, no fetch — so the Settings panel, the Deployments page
// and their tests agree on what a form sends and what a pill says.
//
// An org Environment is a name, its EnvironmentDeploymentDeadline and its
// EnvironmentDeploymentSources. The PUT is a FULL replace: a field left out is
// reset to its default, not kept — so `toPutBody` always sends every field,
// and a form can never quietly drop the deadline someone set.
//
// The server enforces every rule here; mirroring them only lets the form say
// what is wrong before the round trip. Where the two could disagree the server
// wins, and its 400/409 text is shown verbatim.

/** The deadline bounds, in minutes (the server's are 60 000 ms .. 7 days). */
export const MIN_DEADLINE_MINUTES = 1;
export const MAX_DEADLINE_MINUTES = 7 * 24 * 60;
/** The server's default (`DEFAULT_DEPLOYMENT_DEADLINE_MS`, one hour): what a
 *  new Environment starts with, and what an unregistered name gets. */
export const DEFAULT_DEADLINE_MINUTES = 60;

const MINUTE_MS = 60_000;

/** One source row as the form holds it — possibly half typed. */
export type SourceRow = { installation: string; vendor_grouping: string };

/** The editable fields. The name is the URL, not the body. */
export type OrgEnvironmentForm = {
  description: string;
  /** Minutes; a decimal is allowed so a deadline set in odd milliseconds
   *  through the API survives an edit in the UI. */
  deadlineMinutes: number;
  sources: SourceRow[];
};

/** What the PUT carries — structurally the generated request type. */
export type OrgEnvironmentBody = {
  description: string;
  deployment_deadline_ms: number;
  deployment_sources: SourceRow[];
};

/** The record shape these helpers read — structural, so the generated
 *  `OrgEnvironment` satisfies it without this module importing the schema. */
export type OrgEnvironmentLike = {
  name: string;
  description: string;
  deployment_deadline_ms: number;
  deployment_sources: readonly SourceRow[];
};

/** A PluginInstallation name: a DNS-1123 label, as the server checks it. */
const INSTALLATION = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;

/** Why `name` cannot name an Environment, or `null` if it can. Permissive on
 *  purpose (the server's rule): every per-repo Environment name must be
 *  registrable. */
export function nameError(name: string): string | null {
  if (name.length === 0) return "A name is required.";
  if ([...name].length > 100) return "Names are at most 100 characters.";
  if (/[\/\s\u0000-\u001f\u007f]/.test(name)) return "A name cannot contain `/`, spaces or control characters.";
  return null;
}

/** The form for an existing Environment (or a fresh one). */
export function toForm(env?: OrgEnvironmentLike): OrgEnvironmentForm {
  if (!env) return { description: "", deadlineMinutes: DEFAULT_DEADLINE_MINUTES, sources: [] };
  return {
    description: env.description,
    deadlineMinutes: env.deployment_deadline_ms / MINUTE_MS,
    sources: env.deployment_sources.map((s) => ({ ...s })),
  };
}

/**
 * The PUT body for `form`, or the first reason it cannot be sent.
 *
 * Trims every text field. A row left entirely blank is an untouched "add
 * source" row and is dropped; a row with only one half filled is an error —
 * guessing the other half would route reports somewhere nobody chose. A
 * source listed twice is an error (the server refuses it too).
 */
export function toPutBody(
  form: OrgEnvironmentForm,
): { body: OrgEnvironmentBody; error?: undefined } | { body?: undefined; error: string } {
  const minutes = form.deadlineMinutes;
  if (!Number.isFinite(minutes)) return { error: "The deadline must be a number of minutes." };
  const ms = Math.round(minutes * MINUTE_MS);
  if (ms < MIN_DEADLINE_MINUTES * MINUTE_MS || ms > MAX_DEADLINE_MINUTES * MINUTE_MS) {
    return { error: `The deadline must be between 1 minute and 7 days (${MAX_DEADLINE_MINUTES} minutes).` };
  }

  const sources: SourceRow[] = [];
  const seen = new Set<string>();
  for (const row of form.sources) {
    const installation = row.installation.trim();
    const vendor_grouping = row.vendor_grouping.trim();
    if (!installation && !vendor_grouping) continue;
    if (!installation) return { error: `Source "${vendor_grouping}" needs an installation.` };
    if (!vendor_grouping) return { error: `Source "${installation}" needs a vendor grouping.` };
    if (!INSTALLATION.test(installation)) {
      return {
        error: `"${installation}" is not an installation name: lowercase letters, digits and "-", at most 63, starting and ending with a letter or digit.`,
      };
    }
    const key = JSON.stringify([installation, vendor_grouping]);
    if (seen.has(key)) return { error: `${installation} / ${vendor_grouping} is listed twice.` };
    seen.add(key);
    sources.push({ installation, vendor_grouping });
  }

  return {
    body: {
      description: form.description.trim(),
      deployment_deadline_ms: ms,
      deployment_sources: sources,
    },
  };
}

/** A deadline in words: `45 min`, `1 h`, `2 h 30 min`, `7 d`. */
export function deadlineLabel(ms: number): string {
  const totalMinutes = Math.round(ms / MINUTE_MS);
  const days = Math.floor(totalMinutes / (24 * 60));
  const hours = Math.floor((totalMinutes % (24 * 60)) / 60);
  const minutes = totalMinutes % 60;
  const parts = [days && `${days} d`, hours && `${hours} h`, minutes && `${minutes} min`].filter(Boolean);
  return parts.length ? parts.join(" ") : `${ms} ms`;
}

/** One Environment pill on the Deployments page. `registered` is `null` when
 *  the org's Environments could not be read (not wired, or no access) — then
 *  nothing is marked rather than everything being called unregistered. */
export type EnvPill = { name: string; registered: boolean | null; title: string };

/**
 * The Deployments page's Environment pills: every registered org Environment
 * (so one with nothing deployed yet can still be picked), every name the
 * Deployment record has, and the one the URL picked — a sorted union.
 *
 * A name only the record knows is marked: it has no org Environment, so its
 * Deployments stall by the default deadline and no source is routed to it.
 */
export function envPills(
  orgEnvs: readonly Pick<OrgEnvironmentLike, "name" | "deployment_deadline_ms">[] | null | undefined,
  boardEnvNames: readonly string[],
  picked?: string,
): EnvPill[] {
  const registered = new Map((orgEnvs ?? []).map((e) => [e.name, e.deployment_deadline_ms]));
  const names = new Set<string>([...registered.keys(), ...boardEnvNames]);
  if (picked) names.add(picked);
  return [...names]
    .sort((a, b) => a.localeCompare(b))
    .map((name) => {
      if (!orgEnvs) return { name, registered: null, title: name };
      const deadline = registered.get(name);
      return deadline === undefined
        ? {
            name,
            registered: false,
            title: `${name} — unregistered · default deadline (${deadlineLabel(DEFAULT_DEADLINE_MINUTES * MINUTE_MS)})`,
          }
        : { name, registered: true, title: `${name} — stalls after ${deadlineLabel(deadline)}` };
    });
}
