// Leak checks (Org settings › Security): how many times each person may run
// "Check a leaked value" in an org, per rolling hour and per rolling day. Org
// Owners set the two limits; 0 in either turns the check off for the org.
// This module is the wording and the validation around them, with no DOM.

/** `GET`/`PUT /v1/orgs/{org}/security/leak-checks`. */
export type LeakCheckLimits = {
  per_hour: number;
  per_day: number;
  /** The most `per_hour` may be set to. */
  max_per_hour: number;
  /** The most `per_day` may be set to. */
  max_per_day: number;
};

/** Either limit at 0 turns the check off for the org. */
export function leakChecksOff(l: Pick<LeakCheckLimits, "per_hour" | "per_day">): boolean {
  return l.per_hour === 0 || l.per_day === 0;
}

const checks = (n: number) => `${n} ${n === 1 ? "check" : "checks"}`;

/** The limits in one line: `20 checks per person per hour · 40 per day`. */
export function limitsText(l: Pick<LeakCheckLimits, "per_hour" | "per_day">): string {
  if (leakChecksOff(l)) return "Turned off: no one in the org can run a check.";
  return `${checks(l.per_hour)} per person per hour · ${l.per_day} per day`;
}

/** What the match check says when the org has it turned off. */
export function leakChecksOffText(org: string): string {
  return `Leak checks are turned off for ${org}. An org Owner can turn them on in Settings › Security.`;
}

/** Only an Owner of the org may change the limits. */
export function canEditLeakChecks(s: { caller_role?: string | null } | null | undefined): boolean {
  return s?.caller_role === "owner";
}

/** One limit from its box: a whole number from 0 to `max`, or why not. */
function parseLimit(raw: string, max: number, what: string): number | string {
  const t = raw.trim();
  if (!/^\d+$/.test(t)) return `${what} must be a whole number from 0 to ${max}.`;
  const n = Number(t);
  if (n > max) return `${what} must be a whole number from 0 to ${max}.`;
  return n;
}

/**
 * The two boxes of the edit dialog as a request body, or a problem for each
 * box that is out of bounds. The server checks the same bounds and answers
 * `400 out_of_bounds` otherwise.
 */
export function validateLimits(
  hour: string,
  day: string,
  bounds: Pick<LeakCheckLimits, "max_per_hour" | "max_per_day">,
): { ok: true; value: { per_hour: number; per_day: number } } | { ok: false; hour?: string; day?: string } {
  const h = parseLimit(hour, bounds.max_per_hour, "Checks per hour");
  const d = parseLimit(day, bounds.max_per_day, "Checks per day");
  if (typeof h === "number" && typeof d === "number") return { ok: true, value: { per_hour: h, per_day: d } };
  return {
    ok: false,
    hour: typeof h === "string" ? h : undefined,
    day: typeof d === "string" ? d : undefined,
  };
}
