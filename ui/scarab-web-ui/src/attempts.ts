// Attempt-grain derivations (ADR-0056): pure helpers over a step's ordered
// attempt list + the event log — Take-window scoping, the scoped/of-record
// try resolution, and the filmstrip/dropdown try mappers. Extracted from
// StepPane / RunDetail / AttemptsDropdown so the seam where the real bugs
// lived (take-scoped attempts, per-try outcome mapping) is testable without
// a DOM.
import type { RunEvent } from "./api/client";
import { attemptCauses, attemptN, ofRecordAttemptId, type AttemptCause } from "./takes";

/** The attempt fields these derivations read (structural subset of AttemptDto). */
export type AttemptLike = {
  id: string;
  /** Backend-authoritative verdict: running|succeeded|failed|superseded|cancelled. */
  outcome: string;
  failed: boolean;
  failure?: string | null;
  /** The executor's human-readable cause (`attempts.failure_detail`) — WHAT
   * happened, where `failure` is only which retry policy applied (ADR-0068).
   * This field existed on the API for months and no view read it, which is why
   * a dead-lettered step showed "no output for this try" and nothing else. */
  failure_detail?: string | null;
};

/** The attempts a step pane's tabs are scoped to. Take-windowed in EVERY view
 * (ADR-0056 amendment 2026-07-24): when the caller hands a `window` (the
 * selected take's attempt-id set), keep only those tries — so a step carried
 * forward untouched (`[]`) reads as "didn't run in this version". The
 * `frontier` ≤-filter stays as the fallback for callers that don't pass a
 * window (snapshot-at-boundary honesty). */
export function windowAttemptList<A extends { id: string }>(
  list: A[],
  window: string[] | null | undefined,
  frontier: string | null | undefined,
): A[] {
  if (window) return list.filter((a) => window.includes(a.id));
  return frontier ? list.filter((a) => attemptN(a.id) <= attemptN(frontier)) : list;
}

/** The attempt every tab is scoped to, from an already-windowed list: the
 * wanted id (explicit selection, else the Take frontier) when present, else
 * the latest attempt; null when nothing ran. */
export function scopedAttempt<A extends { id: string }>(
  list: A[],
  want: string | null | undefined,
): A | null {
  if (!list.length) return null;
  return list.find((a) => a.id === want) ?? list[list.length - 1];
}

/** The selected step didn't run in the viewed version — there IS a step but
 * its (already-windowed) tries are empty. Both a time-travel `not_run` step
 * and a not-yet-launched step land here. */
export function stepNotRun(step: unknown, tries: readonly unknown[]): boolean {
  return !!step && tries.length === 0;
}

/** Index of the of-record attempt within an (already-windowed) list, -1 when
 * nothing is of record — drives the "of record · try N" coordinate note. */
export function ofRecordIndexOf(list: AttemptLike[]): number {
  const rec = ofRecordAttemptId(list);
  return rec ? list.findIndex((a) => a.id === rec) : -1;
}

/** One try of the SELECTED step, resolved from the event log — `stripTries`
 * produces these; the attempts dropdown renders them. The name is kept from
 * the former filmstrip so callers' type imports are unchanged. */
export type FilmstripTry = {
  id: string;
  /** 0-based order; the label is `try {index + 1}`. */
  index: number;
  cause?: AttemptCause;
  /** The backend's authoritative per-attempt verdict (AttemptDto.outcome):
   * `running | succeeded | failed | superseded | cancelled`. Preferred over the
   * `failed` bool so a running/superseded/cancelled try never renders green. */
  outcome: string;
  failed: boolean;
  failure?: string;
  /** The executor's human-readable cause, when it reported one (ADR-0068). */
  failureDetail?: string;
  /** Cut short by a rerun of an ancestor (started, never finished). */
  superseded: boolean;
  /** A success that a newer success replaced as of-record. */
  shadowed: boolean;
  /** Re-adopted after a control-plane restart (visibility marker, not a re-run). */
  readopted: boolean;
};

/** A step's tries as the attempts dropdown renders them, scoped to a Take's
 * attempt-id window (`win`; null = unscoped) with causes derived over the
 * caller's (boundary-truncated while time-traveling) event log. */
export function stripTries(
  events: RunEvent[],
  stepId: string,
  list: AttemptLike[],
  win: string[] | null,
): FilmstripTry[] {
  const visible = win ? list.filter((a) => win.includes(a.id)) : list;
  const c = attemptCauses(events, stepId);
  return visible.map((a, i) => ({
    id: a.id,
    index: i,
    cause: c.causes[a.id],
    // The backend's authoritative verdict — the fan reads this so it never
    // shows an abandoned attempt green; the derived flags below stay as
    // fallback.
    outcome: a.outcome,
    failed: a.failed,
    failure: a.failure ?? undefined,
    failureDetail: a.failure_detail ?? undefined,
    superseded: c.superseded.has(a.id),
    shadowed: c.shadowed.has(a.id),
    readopted: c.readopted.has(a.id),
  }));
}

/** Plain-english cause suffix (ADR-0056 amendment) — the machine's own retry vs
 * a human rerun of this step vs a rerun of an ancestor that dragged it along. */
export const causeSuffix = (c?: AttemptCause): string =>
  c === "rerun" ? " · you reran" : c === "cascade" ? " · ⟵ rerun" : c === "retry" ? " · auto-retry" : "";

/** The dropdown row's title: `try N` + its cause suffix. */
export const tryTitle = (t: FilmstripTry): string => `try ${t.index + 1}${causeSuffix(t.cause)}`;

/** Prefer the backend's authoritative `outcome` (AttemptDto.outcome) so a
 * still-running / superseded / cancelled try is never mislabelled green. Fall
 * back to the pre-fix superseded→failed→green derivation only when `outcome` is
 * absent (an old server), so nothing regresses. The leading glyph is baked in
 * (✓/✗/●/⊘) so `try N · {outcome}` reads as "try N · {glyph} {word}". */
export const tryOutcome = (t: FilmstripTry): string => {
  switch (t.outcome) {
    case "running":
      return "● running";
    case "succeeded":
      return "✓ succeeded";
    case "failed":
      return `✗ failed${t.failure ? ` · ${t.failure}` : ""}`;
    case "superseded":
      return "⊘ superseded";
    case "cancelled":
      return "⊘ cancelled";
  }
  if (t.superseded) return "⊘ superseded";
  if (t.failed) return `✗ failed${t.failure ? ` · ${t.failure}` : ""}`;
  return "✓ succeeded";
};

/** The tone class a try renders with (chip/row accent). */
export const tryTone = (t: FilmstripTry): string => {
  switch (t.outcome) {
    case "running":
      return "running";
    case "succeeded":
      return "emerald";
    case "failed":
      return "danger";
    case "superseded":
    case "cancelled":
      return "copper";
  }
  return t.superseded ? "copper" : t.failed ? "danger" : "emerald";
};
