// Pure helpers behind the Run page's run tabs (Artifacts, Activity) and the
// step pane's try tabs: which activity a filter shows and how much it hides,
// which artifact copies a filter shows and how they group by step, the try
// tabs' order, and the tries the List view nests under a step. No DOM, so the vitest suite covers them (run-tabs.test.ts).
import type { RunEvent } from "./api/client";
import { aboutDuration, failureWords, statusWord, stepDisplayName } from "./run-page";
import { attemptCauses, attemptPlaces, type AttemptCause } from "./takes";
import { causeText } from "./attempts";
import { eventParts, infraReasonText, NODE_WAIT_REASONS } from "./events";
import { duration } from "./fmt";
import { isGateExpiry } from "./gates";

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

/**
 * The step transitions that repeat what the try before them said: a step that
 * moved to `succeeded` or `failed` right after its try finished the same way,
 * with no newer try of that step in between. Keyed by the transition's index,
 * valued by the finished try's index. A failed try whose step went back to
 * pending for a retry is not merged: both facts stay.
 */
export function mergedTransitions(events: RunEvent[]): Map<number, number> {
  const out = new Map<number, number>();
  const last = new Map<string, { idx: number; failed: boolean }>(); // step -> its last finished try
  events.forEach((e, i) => {
    const k = tagOf(e);
    if (!k) return;
    const step = String(k.v.step ?? "");
    if (!step) return;
    if (k.tag === "AttemptStarted") last.delete(step);
    else if (k.tag === "AttemptFinished") last.set(step, { idx: i, failed: !!k.v.failure });
    else if (k.tag === "StepTransitioned") {
      const to = String(k.v.to ?? "");
      const f = last.get(step);
      if (f && ((to === "succeeded" && !f.failed) || (to === "failed" && f.failed))) {
        out.set(i, f.idx);
        last.delete(step);
      }
    }
  });
  return out;
}

export type ActivityView = {
  /** The events the filter shows, newest first, with their class. `merged`
   * marks a finished try that also stands for its step's move to passed or
   * failed (`mergedTransitions`). */
  rows: { event: RunEvent; cls: ActivityClass; merged?: boolean }[];
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
  const merged = mergedTransitions(events);
  const into = new Set(merged.values());
  const classed = events
    .map((event, i) => ({ event, cls: activityClass(event, active), i }))
    .filter((r) => !merged.has(r.i))
    .map(({ event, cls, i }) => (into.has(i) ? { event, cls, merged: true } : { event, cls }));
  const count = (f: ActivityFilter) => classed.filter((r) => inFilter(r.cls, f)).length;
  const rows = classed.filter((r) => inFilter(r.cls, filter)).reverse();
  return {
    rows,
    counts: { steps: count("steps"), infra: count("infra"), all: count("all") },
    hiddenCleared: filter === "steps" ? classed.filter((r) => r.cls === "cleared").length : 0,
  };
}

const events = (n: number) => (n === 1 ? "event" : "events");

/** One Activity row in words: the step's display name ("deploy-preview · us",
 * the raw id in `stepId`) and what happened, with tries counted within their
 * version ("try 6 started (automatic retry)", "try 5 failed: step error",
 * "passed in 2m50s on try 6", "waiting for a node: 0/14 nodes are
 * available", "unschedulable for about 1m: … · cleared"). Built once per
 * event log; call the result per row. */
export function activityWords(
  log: RunEvent[],
): (e: RunEvent, merged?: boolean) => { step: string | null; stepId: string | null; text: string } {
  const places = attemptPlaces(log);
  const starts = new Map<string, number>(); // "step attempt" -> started at
  for (const e of log) {
    const k = tagOf(e);
    if (k?.tag === "AttemptStarted") starts.set(`${String(k.v.step)} ${String(k.v.attempt)}`, e.at);
  }
  const causes = new Map<string, Record<string, AttemptCause>>();
  const causeOf = (step: string, attempt: string): AttemptCause | undefined => {
    if (!causes.has(step)) causes.set(step, attemptCauses(log, step).causes);
    return causes.get(step)![attempt];
  };
  return (e, merged = false) => {
    const k = tagOf(e);
    const base = eventParts(e);
    const stepId = base.step;
    const step = stepId ? stepDisplayName(stepId) : null;
    if (!k || !stepId) return { step, stepId, text: base.text };
    const v = k.v;
    const attempt = String(v.attempt ?? "");
    const n = places[stepId]?.[attempt]?.try;
    const tryN = n !== undefined ? `try ${n}` : "a try";
    const took = (): string | null => {
      const at = starts.get(`${stepId} ${attempt}`);
      return at !== undefined ? duration(at, e.at) : null;
    };
    switch (k.tag) {
      case "AttemptStarted": {
        const c = causeOf(stepId, attempt);
        const why = c && c.kind !== "initial" ? ` (${causeText(c)})` : "";
        return { step, stepId, text: `${tryN} started${why}` };
      }
      case "AttemptFinished": {
        const d = took();
        if (v.failure) {
          const what = failureWords(String(v.failure));
          return {
            step,
            stepId,
            text: merged ? `failed${d ? ` after ${d}` : ""} on ${tryN}: ${what}` : `${tryN} failed: ${what}`,
          };
        }
        return {
          step,
          stepId,
          text: merged ? `passed${d ? ` in ${d}` : ""} on ${tryN}` : `${tryN} passed${d ? ` in ${d}` : ""}`,
        };
      }
      case "StepTransitioned":
        return {
          step,
          stepId,
          text: `${statusWord(String(v.from ?? "")).toLowerCase()} → ${statusWord(String(v.to ?? "")).toLowerCase()}`,
        };
      case "AttemptReadopted":
        return { step, stepId, text: `${tryN} re-adopted after a control-plane restart` };
      case "StepInfraCondition": {
        const reason = String(v.reason ?? "");
        const message = v.message ? `: ${String(v.message)}` : "";
        if (typeof v.held_ms !== "number") return { step, stepId, text: `${infraReasonText(reason)}${message}` };
        const what = NODE_WAIT_REASONS.has(reason) ? "unschedulable" : infraReasonText(reason);
        return { step, stepId, text: `${what} for ${aboutDuration(v.held_ms)}${message} · cleared` };
      }
    }
    return { step, stepId, text: base.text };
  };
}

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
  /** Who released or rejected it: set by a reject (a person, or
   * `external:<step>` for an external gate's token) and by a plugin's verdict.
   * Absent on an approval, a timer and an expiry. */
  by?: string | null;
  /** The wait (attempt id) it resolved. */
  attempt: string;
  /** Who approved that wait, from the run's events ({@link waitApprovers}). */
  approvers?: string[];
};

// The reasons the engine journals on a pass that an approval caused: the
// Approve button (or an external gate's token) releases with "released"; an
// Environment approval gate whose approvals were in releases when its wait
// timer runs out.
const APPROVED_REASONS: ReadonlySet<string> = new Set([
  "released",
  "approved; the Environment's wait timer ran out",
]);

/** A resolved wait in words: "approved", "approved by a.kim", "released by
 * plugin:argocd", "rejected by a.kim: not today", "expired". Null for any
 * other verdict (a timer gate that ran out, a gate that cannot release), so
 * the plain outcome reads instead. */
export function resolutionWords(r: WaitResolution): string | null {
  const reason = r.reason?.trim() ?? "";
  if (r.passed) {
    // Only a plugin's verdict names who released a passing gate.
    if (r.by) return `released by ${r.by}`;
    if (!APPROVED_REASONS.has(reason)) return null;
    return r.approvers?.length ? `approved by ${r.approvers.join(", ")}` : "approved";
  }
  if (isGateExpiry(reason)) return "expired";
  if (r.by) return `rejected by ${r.by}${reason ? `: ${reason}` : ""}`;
  return null;
}

/** Whether an event starts a new wait of gate `step`: the gate went back to
 * pending, its open wait was superseded, or a rerun named it. The engine
 * counts a wait's approvals from the same boundary. */
function isGateRearm(e: RunEvent, step: string): boolean {
  const k = tagOf(e);
  if (!k) return false;
  switch (k.tag) {
    case "StepTransitioned":
      return k.v.step === step && k.v.to === "pending";
    case "AttemptSuperseded":
      return k.v.step === step;
    case "RunRerunRequested":
      return Array.isArray(k.v.invalidated) && k.v.invalidated.includes(step);
    default:
      return false;
  }
}

/** The people who approved the wait of gate `step` that `attempt` closed, in
 * approval order: the approvals after the gate's last re-arm before that
 * wait's verdict. Empty when the verdict is not in the events. */
export function waitApprovers(events: RunEvent[], step: string, attempt: string): string[] {
  let seen: string[] = [];
  for (const e of events) {
    if (isGateRearm(e, step)) {
      seen = [];
      continue;
    }
    const k = tagOf(e);
    if (!k || k.v.step !== step) continue;
    if (k.tag === "GateApproved" && typeof k.v.by === "string" && !seen.includes(k.v.by)) {
      seen = [...seen, k.v.by];
    } else if (k.tag === "GateResolved" && k.v.attempt === attempt) {
      return seen;
    }
  }
  return [];
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
