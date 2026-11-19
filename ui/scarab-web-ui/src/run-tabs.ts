// Pure helpers behind the Run page's run tabs (Artifacts, Activity) and the
// step pane's try tabs: which activity a filter shows and how much it hides,
// which artifact copies a filter shows and how they group by step, how the try
// tabs fold the older tries into a menu, and the tries the List view nests
// under a step. No DOM, so the vitest suite covers them (run-tabs.test.ts).
import type { RunEvent } from "./api/client";
import { failureWords, statusWord } from "./run-page";

// ── Activity ────────────────────────────────────────────────────────────────

/** The Activity tab's filter: `steps` (Run and steps, the default) hides the
 * cleared Pod conditions; `infra` shows only the infrastructure events;
 * `all` shows everything. */
export type ActivityFilter = "steps" | "infra" | "all";

/** What one event is to the Activity filter:
 * - `active`: a Pod condition holding a step's current try back right now;
 * - `cleared`: any other Pod condition event (its onset or its close, once the
 *   condition is over, or replaced by a newer try);
 * - `infra`: another infrastructure event (a control-plane re-adoption);
 * - `step`: everything else. */
export type ActivityClass = "step" | "infra" | "active" | "cleared";

function tagOf(e: RunEvent): { tag: string; v: Record<string, unknown> } | null {
  const k = e.kind as unknown;
  if (!k || typeof k !== "object") return null;
  const tag = Object.keys(k as Record<string, unknown>)[0];
  if (!tag) return null;
  return { tag, v: ((k as Record<string, unknown>)[tag] ?? {}) as Record<string, unknown> };
}

/** Classify one event by its kind. `active` holds the conditions holding a step
 * back now, by step (the onset's attempt and time); an onset that matches is
 * `active`, every other Pod condition event is `cleared`. The warning category
 * plays no part: a workspace collision or a note stays a step event. */
export function activityClass(
  e: RunEvent,
  active: Record<string, { attempt: string; since: number }>,
): ActivityClass {
  const k = tagOf(e);
  if (!k) return "step";
  if (k.tag === "StepInfraCondition") {
    const step = String(k.v.step ?? "");
    const a = active[step];
    const onset = k.v.held_ms === undefined || k.v.held_ms === null;
    return onset && a && a.attempt === String(k.v.attempt ?? "") && a.since === e.at ? "active" : "cleared";
  }
  if (k.tag === "AttemptReadopted") return "infra";
  return "step";
}

const inFilter = (c: ActivityClass, f: ActivityFilter): boolean =>
  f === "all" ? true : f === "infra" ? c !== "step" : c !== "cleared";

export type ActivityView = {
  /** The events the filter shows, newest first, with their class. */
  rows: { event: RunEvent; cls: ActivityClass }[];
  /** How many events each filter shows. */
  counts: Record<ActivityFilter, number>;
  /** Cleared Pod condition events the current filter hides. */
  hiddenCleared: number;
};

/** The Activity tab for one filter. Display only: the caller's event list is
 * not changed, so everything else on the page still reads all of it. */
export function activityView(
  events: RunEvent[],
  active: Record<string, { attempt: string; since: number }>,
  filter: ActivityFilter,
): ActivityView {
  const classed = events.map((event) => ({ event, cls: activityClass(event, active) }));
  const count = (f: ActivityFilter) => classed.filter((r) => inFilter(r.cls, f)).length;
  const rows = classed.filter((r) => inFilter(r.cls, filter)).reverse();
  return {
    rows,
    counts: { steps: count("steps"), infra: count("infra"), all: count("all") },
    hiddenCleared: filter === "steps" ? classed.filter((r) => r.cls === "cleared").length : 0,
  };
}

const events = (n: number) => (n === 1 ? "event" : "events");

/** The line under the Activity rows: the cleared conditions the filter hides
 * and, on an older version, the events after it. Either part is null when
 * there is nothing to say. */
export function activityHiddenNote(
  hiddenCleared: number,
  later: number,
): { cleared: string | null; later: string | null } {
  return {
    cleared: hiddenCleared > 0 ? `${hiddenCleared} cleared Pod condition ${events(hiddenCleared)} hidden` : null,
    later: later > 0 ? `${later} later ${events(later)} hidden by this version` : null,
  };
}

/** Whether an event is the run's trigger (a `Raw` payload carrying it). */
export function isTriggerEvent(e: RunEvent): boolean {
  const k = tagOf(e);
  if (!k || k.tag !== "Raw") return false;
  const t = k.v.trigger as Record<string, unknown> | undefined;
  return !!t && typeof t === "object" && !!t.event;
}

/** The trigger event in words: "Triggered by PR #2949 · feature/x · 15f2297b
 * · thulasi". `words` is the trigger as the run header names it. */
export function triggerLine(t: {
  words?: string | null;
  branch?: string | null;
  sha?: string | null;
  actor?: string | null;
}): string {
  const parts = [`Triggered by ${t.words || "an event"}`];
  if (t.branch && t.branch !== t.sha) parts.push(t.branch);
  if (t.sha) parts.push(t.sha.slice(0, 8));
  if (t.actor) parts.push(t.actor);
  return parts.join(" · ");
}

// ── Artifacts ───────────────────────────────────────────────────────────────

/** The Artifacts tab's filter: `record` shows one copy per file, `all` every
 * copy, earlier tries' included. */
export type ArtifactFilter = "record" | "all";

type ArtifactLike = { name: string; step: string; attempt: string; of_record: boolean };

const attemptNo = (id: string): number => parseInt(id.replace(/^a/, ""), 10) || 0;

/**
 * The copies a filter shows. `record`: the copy of record of each file, and
 * for a file that has none (every try that wrote it failed) its newest copy,
 * so a failed step's report is not hidden. `all`: every copy. Input order is
 * kept.
 */
export function shownArtifacts<A extends ArtifactLike>(rows: A[], filter: ArtifactFilter): A[] {
  if (filter === "all") return rows;
  const withRecord = new Set(rows.filter((a) => a.of_record).map((a) => a.name));
  const newest = new Map<string, A>();
  for (const a of rows) {
    if (withRecord.has(a.name)) continue;
    const cur = newest.get(a.name);
    if (!cur || attemptNo(a.attempt) >= attemptNo(cur.attempt)) newest.set(a.name, a);
  }
  return rows.filter((a) => (withRecord.has(a.name) ? a.of_record : newest.get(a.name) === a));
}

/** The rows in scope: one step's copies when the tab is narrowed to it, else
 * all of them. Narrow BEFORE `shownArtifacts`: of record is decided per file
 * name across the run, so a step whose file shares its name with another
 * step's file of record would otherwise show nothing. */
export function artifactsInScope<A extends ArtifactLike>(rows: A[], onlyStep: string | null): A[] {
  return onlyStep === null ? rows : rows.filter((a) => a.step === onlyStep);
}

/** What a copy that is not of record says about itself: "earlier copy" when
 * its try passed, "from a failed try" when it failed, and "from a try that did
 * not finish" when the try was superseded or cancelled. `outcome` is the
 * try's outcome when known; without it, a copy whose try did not pass reads
 * as failed. */
export function copyMark(
  a: { succeeded: boolean },
  outcome?: string | null,
): { text: string; tone: "earlier" | "failed" } {
  if (a.succeeded || outcome === "succeeded") return { text: "earlier copy", tone: "earlier" };
  if (outcome === "superseded" || outcome === "cancelled")
    return { text: "from a try that did not finish", tone: "failed" };
  return { text: "from a failed try", tone: "failed" };
}

/** How many rows each filter shows. */
export function artifactFilterCounts(rows: ArtifactLike[]): Record<ArtifactFilter, number> {
  return { record: shownArtifacts(rows, "record").length, all: rows.length };
}

/**
 * Group rows by the step that published them, in the run's step order (a step
 * the run does not list comes last, in first-seen order). `onlyStep` keeps one
 * step's group. Within a group, rows keep their order.
 */
export function artifactGroups<A extends ArtifactLike>(
  rows: A[],
  stepOrder: string[],
  onlyStep: string | null = null,
): { step: string; rows: A[] }[] {
  const groups = new Map<string, A[]>();
  for (const a of rows) {
    if (onlyStep !== null && a.step !== onlyStep) continue;
    const g = groups.get(a.step);
    if (g) g.push(a);
    else groups.set(a.step, [a]);
  }
  const rank = (s: string) => {
    const i = stepOrder.indexOf(s);
    return i < 0 ? stepOrder.length : i;
  };
  return [...groups.entries()]
    .map(([step, rs], seen) => ({ step, rows: rs, seen }))
    .sort((a, b) => rank(a.step) - rank(b.step) || a.seen - b.seen)
    .map(({ step, rows: rs }) => ({ step, rows: rs }));
}

// ── Try tabs ────────────────────────────────────────────────────────────────

/** The try tabs' order: every try, newest first. The row scrolls sideways
 * when they do not fit. */
export function tabTries<T>(tries: T[]): T[] {
  return [...tries].reverse();
}

/** "Tries", or "Waits" for a gate's tries. */
export function pluralNoun(noun: string): string {
  return noun === "Wait" ? "Waits" : "Tries";
}

/** How a gate's wait was resolved, as the server reports it for the gate's
 * latest wait (`gate_resolution`). */
export type WaitResolution = {
  passed: boolean;
  reason?: string | null;
  /** Who released or rejected it, when a person or a plugin did. */
  by?: string | null;
  /** The wait (attempt id) it resolved. */
  attempt: string;
};

/** A resolved wait in words: "approved by a.kim", "rejected by a.kim: not
 * today", "expired". Null when the resolution names no one and did not expire
 * (a timer that ran out), so the plain outcome reads instead. */
export function resolutionWords(r: WaitResolution): string | null {
  const reason = r.reason?.trim();
  if (r.passed) return r.by ? `approved by ${r.by}` : null;
  if (r.by) return `rejected by ${r.by}${reason ? `: ${reason}` : ""}`;
  if (reason && /expire/i.test(reason)) return "expired";
  return null;
}

/** One try in words, numbered from 1: "Try 3 · passed", "Try 2 · failed: step
 * error", "Wait 1 · running". A gate's wait with a `resolution` for that wait
 * says how it was resolved: "Wait 1 · approved by a.kim". */
export function tryWords(
  n: number,
  t: { id?: string; outcome: string; failure?: string | null },
  noun = "Try",
  resolution?: WaitResolution | null,
): string {
  const resolved = resolution && t.id !== undefined && resolution.attempt === t.id ? resolutionWords(resolution) : null;
  const what =
    resolved ??
    (t.outcome === "failed"
      ? `failed: ${failureWords(t.failure)}`
      : t.outcome === "succeeded"
        ? "passed"
        : statusWord(t.outcome).toLowerCase());
  return `${noun} ${n} · ${what}`;
}

/** How long a try took, or has run so far; null without a start. */
export function tryDurationMs(
  t: { outcome: string; startedAt?: number | null; finishedAt?: number | null },
  now: number,
): number | null {
  if (t.startedAt == null) return null;
  if (t.finishedAt != null) return Math.max(0, t.finishedAt - t.startedAt);
  return t.outcome === "running" ? Math.max(0, now - t.startedAt) : null;
}

/** The tries the List view nests under a step: newest first, and only when
 * there is more than one. */
export function nestedTries<T>(tries: T[]): T[] {
  return tries.length > 1 ? [...tries].reverse() : [];
}
