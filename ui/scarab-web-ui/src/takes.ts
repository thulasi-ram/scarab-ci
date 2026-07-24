// Takes (ADR-0056): the run-level version lens, derived ENTIRELY client-side
// from the event log — no take is stored anywhere. A Take is the span of a
// run between two human interventions (`RunRerunRequested` events); Take N's
// view is a pure replay of the log up to its closing boundary, so a closed
// Take shows the run exactly as it stood the instant Rerun was pressed
// (snapshot-at-boundary). An attempt that straddles a boundary — started in
// Take N, still in flight when Rerun was pressed — shows as `superseded` when
// that Rerun re-armed its step (it was cut short), else honestly as `running`.
import type { RunEvent, Artifact } from "./api/client";
import type { OutcomeCounts, VersionRow } from "./components/VersionDropdown";
import { relTime } from "./fmt";

/** The tag + payload of a structured event, or null for unit variants. */
function kindOf(e: RunEvent): { tag: string; v: Record<string, unknown> } | null {
  if (typeof e.kind === "string") return null;
  const tag = Object.keys(e.kind)[0];
  return { tag, v: e.kind[tag] ?? {} };
}

export type Take = {
  /** 1-based take number. */
  n: number;
  /** Exclusive end index into the event array: the boundary event that CLOSED
   * this take, or events.length for the latest (open) take. */
  endIdx: number;
  /** The step whose rerun closed this take (null for the latest take). */
  closedByTarget: string | null;
  /** Who pressed the rerun that closed this take (null = unknown/latest). */
  closedBy: string | null;
  /** Timestamp of the closing boundary, or null for the latest take. */
  closedAt: number | null;
};

/** Split the event log at its `RunRerunRequested` boundaries. Always returns
 * at least one take (the whole log). Take N ends where rerun N happens. */
export function deriveTakes(events: RunEvent[]): Take[] {
  const takes: Take[] = [];
  events.forEach((e, i) => {
    const k = kindOf(e);
    if (k?.tag === "RunRerunRequested") {
      takes.push({
        n: takes.length + 1,
        endIdx: i,
        closedByTarget: (k.v.target as string) ?? null,
        closedBy: (k.v.by as string) ?? null,
        closedAt: e.at,
      });
    }
  });
  takes.push({
    n: takes.length + 1,
    endIdx: events.length,
    closedByTarget: null,
    closedBy: null,
    closedAt: null,
  });
  return takes;
}

export type TakeView = {
  /** Per-step status as of the boundary instant — replayed cumulatively from
   * run birth, so a step NOT re-run in this Take keeps its carried-forward
   * verdict (steps absent = never seen). */
  status: Record<string, string>;
  /** Per-step latest attempt id **within this Take's window** — the evidence
   * frontier the attempt-scoped reads (`?attempt=`) are keyed with. Absent for a
   * step not re-armed in this Take. */
  frontier: Record<string, string>;
  /** Per-step attempt COUNT **within this Take's window** (drives the ×N badge).
   * 0 for a step carried forward untouched (a partial rerun left it alone). */
  attempts: Record<string, number>;
  /** Per-step attempt ids started **within this Take's window**, in order — the
   * exact set the tries strip renders (so a carried-forward step shows none,
   * and a re-run step shows only THIS Take's tries, numbered per-Take). */
  windowAttempts: Record<string, string[]>;
};

/** Replay the event log up to `take`'s boundary. Pure function of the log —
 * the same replay the durable engine could do, done in the browser. */
export function replayTake(events: RunEvent[], takes: Take[], take: Take): TakeView {
  const status: Record<string, string> = {};
  const frontier: Record<string, string> = {};
  const attempts: Record<string, number> = {};
  const windowAttempts: Record<string, string[]> = {};
  const inFlight = new Map<string, string>(); // step -> attempt currently running

  // This Take's window opens at the previous Take's closing boundary (0 for
  // Take 1). Status replays cumulatively from birth (a step untouched by this
  // Take keeps its carried verdict), but attempts/frontier are scoped to the
  // window — so the latest Take reads THIS Take's tries, not every Take's summed
  // (the pre-2026-07-22 bug counted from index 0 for every Take).
  const startIdx = take.n > 1 ? (takes[take.n - 2]?.endIdx ?? 0) : 0;

  for (let i = 0; i < take.endIdx; i++) {
    const k = kindOf(events[i]);
    if (!k) continue;
    const step = (k.v.step as string) ?? null;
    const inWindow = i >= startIdx;
    switch (k.tag) {
      case "StepTransitioned":
        if (step) status[step] = String(k.v.to ?? "");
        break;
      case "AttemptStarted":
        if (step) {
          const attempt = String(k.v.attempt ?? "");
          if (inWindow) {
            frontier[step] = attempt;
            attempts[step] = (attempts[step] ?? 0) + 1;
            (windowAttempts[step] ??= []).push(attempt);
          }
          inFlight.set(step, attempt);
          // Admission's ready→running claim is not a logged transition — the
          // attempt start IS the "running" fact; the verdict arrives as a
          // later StepTransitioned.
          status[step] = "running";
        }
        break;
      case "AttemptFinished":
        if (step) inFlight.delete(step);
        break;
    }
  }

  // Closed take: the boundary event that closed it (a RunRerunRequested) re-arms
  // some steps for the NEXT take. From THIS take's snapshot: a re-armed step that
  // was still in flight was cut short → `superseded`; a re-armed step that never
  // ran in this window (and carries no succeeded/failed verdict) simply never ran
  // → `not_run`; a re-armed step it DID run keeps its verdict. An in-flight step
  // NOT re-armed genuinely straddles the boundary and stays `running`. On
  // pre-amendment logs (no `invalidated`/`target`) skip this → today's behavior.
  if (take.endIdx < events.length) {
    const bk = kindOf(events[take.endIdx]);
    const target = bk?.v.target as string | undefined;
    const invalidated = bk?.v.invalidated as string[] | undefined;
    if (typeof target === "string" && Array.isArray(invalidated)) {
      const rearmed = new Set<string>([target, ...invalidated]);
      for (const step of rearmed) {
        if (inFlight.has(step)) status[step] = "superseded";
        else if ((attempts[step] ?? 0) === 0 && !["succeeded", "failed"].includes(status[step] ?? ""))
          status[step] = "not_run";
        // else: keep the step's carried verdict.
      }
    }
  }

  return { status, frontier, attempts, windowAttempts };
}

/** The "of-record" attempt id for a step from its ordered attempt list: the one
 * whose Outputs/Artifacts a consumer should see. Prefers the LAST succeeded
 * attempt; failing that, the last attempt with a real verdict (not superseded /
 * running / cancelled); null when nothing is worth showing of-record. */
export function ofRecordAttemptId(
  list: { id: string; outcome: string; failed: boolean }[],
): string | null {
  for (let i = list.length - 1; i >= 0; i--) {
    if (list[i].outcome === "succeeded") return list[i].id;
  }
  for (let i = list.length - 1; i >= 0; i--) {
    const a = list[i];
    if (!a.failed && !["superseded", "running", "cancelled"].includes(a.outcome)) return a.id;
  }
  return null;
}

/** Why one try (Attempt) of a step exists (ADR-0056 amendment):
 * - `initial`  — the step's first-ever execution.
 * - `retry`    — the engine auto-retrying a not-yet-succeeded step (within
 *                budget); same history row, no fork.
 * - `rerun`    — a human reran THIS step (it was the rerun target): a fork.
 * - `cascade`  — a human reran an ANCESTOR and this step was dragged along by
 *                smart invalidation (ADR-0027) — "you did one thing; the rest
 *                followed" (points back at the rerun with "⟵ …").
 * The discriminator is the step's state + who acted, all read off the event
 * log — nothing is stored. */
export type AttemptCause = "initial" | "retry" | "rerun" | "cascade";

/** Attempt-grain derivations for one step, all from the event log:
 * - `causes`     — per-attempt cause (above).
 * - `readopted`  — attempts a resumed control plane re-adopted (a visibility
 *                  marker, never a new execution).
 * - `superseded` — attempts CUT SHORT while running by a rerun of an ancestor
 *                  (started, never finished, then the step was re-armed by a
 *                  later RunRerunRequested). Distinct from failed/cancelled.
 * - `shadowed`   — succeeded attempts that are no longer the of-record latest
 *                  (a newer successful attempt replaced their role).
 * NOTE: the backend's `AttemptDto.outcome` is now authoritative for
 * running/succeeded/failed/superseded (the fan reads it directly); this
 * derivation stays as the fallback for older events/servers, and `shadowed`
 * (an of-record lens over successes) remains client-derived here. */
export function attemptCauses(
  events: RunEvent[],
  step: string,
): {
  causes: Record<string, AttemptCause>;
  readopted: Set<string>;
  superseded: Set<string>;
  shadowed: Set<string>;
} {
  const causes: Record<string, AttemptCause> = {};
  const readopted = new Set<string>();
  const superseded = new Set<string>();
  const shadowed = new Set<string>();

  const started: string[] = []; // attempt ids of this step, in order
  const finished = new Set<string>(); // ids that got an AttemptFinished
  const succeeded: string[] = []; // ids that finished WITHOUT a failure, in order

  // A rerun naming this step arms the NEXT attempt of it — as a `rerun` when
  // this step IS the target, else as a `cascade` (dragged in via invalidated).
  let armedBy: AttemptCause | null = null;
  let seen = 0;
  for (const e of events) {
    const k = kindOf(e);
    if (!k) continue;
    // A rerun (Take fork) OR a retry (in-Take, ADR-0056 amendment 2026-07-22)
    // both re-arm the target + its dependent cascade. The target's next attempt
    // is a `rerun` (fork) or a `retry` (same-Take try); dragged descendants are
    // `cascade`. Either way, an attempt still in flight is cut short → superseded.
    if (k.tag === "RunRerunRequested" || k.tag === "StepRetryRequested") {
      const invalidated = (k.v.invalidated as string[]) ?? [];
      const target = k.v.target as string;
      if (target === step) armedBy = k.tag === "StepRetryRequested" ? "retry" : "rerun";
      else if (invalidated.includes(step)) armedBy = "cascade";
      else continue;
      for (const id of started) if (!finished.has(id)) superseded.add(id);
      continue;
    }
    if ((k.v.step as string) !== step) continue;
    if (k.tag === "AttemptStarted") {
      const id = String(k.v.attempt ?? "");
      seen += 1;
      causes[id] = seen === 1 ? "initial" : (armedBy ?? "retry");
      armedBy = null;
      started.push(id);
    } else if (k.tag === "AttemptFinished") {
      const id = String(k.v.attempt ?? "");
      finished.add(id);
      const failure = k.v.failure;
      if (failure === null || failure === undefined) succeeded.push(id);
    } else if (k.tag === "AttemptReadopted") {
      readopted.add(String(k.v.attempt ?? ""));
    }
  }

  // Of-record = the latest successful attempt; earlier successes are shadowed.
  for (let i = 0; i < succeeded.length - 1; i++) shadowed.add(succeeded[i]);

  return { causes, readopted, superseded, shadowed };
}

/** Numeric part of an attempt id (`a3` → 3) for as-of-boundary comparisons. */
export const attemptN = (id: string): number => parseInt(id.replace(/^a/, ""), 10) || 0;

/** Per-step wall-clock from the event log: first AttemptStarted → last
 * AttemptFinished. When `windowAttempts` (the selected Take's per-step attempt
 * windows) is given, only THIS take's attempts count — a step's duration is the
 * current take's wall-clock, not summed across takes (a rerun must not keep
 * growing the displayed step time). Extracted from RunDetail. */
export function stepTiming(
  events: RunEvent[],
  windowAttempts: Record<string, string[]> | null,
): Record<string, { start?: number; end?: number }> {
  const m: Record<string, { start?: number; end?: number }> = {};
  for (const e of events) {
    const k = kindOf(e);
    if (!k) continue;
    const step = (k.v.step as string | undefined) ?? undefined;
    if (!step) continue;
    // Restrict to the selected take's attempts: skip an AttemptStarted/
    // AttemptFinished whose attempt belongs to another take.
    const attempt = (k.v.attempt as string | undefined) ?? undefined;
    if (windowAttempts && attempt !== undefined) {
      const win = windowAttempts[step];
      if (win && !win.includes(attempt)) continue;
    }
    const t = (m[step] ??= {});
    if (k.tag === "AttemptStarted" && (t.start === undefined || e.at < t.start)) t.start = e.at;
    if (k.tag === "AttemptFinished" && (t.end === undefined || e.at > t.end)) t.end = e.at;
  }
  return m;
}

/** Artifact versions visible in a view: on the latest view (`tv` null) all of
 * them, server flags intact. While time-traveling, only versions from attempts
 * that existed as of the boundary — and of-record is recomputed within that
 * horizon (the server's flag is latest-global). Extracted from RunDetail. */
export function visibleArtifacts(all: Artifact[], tv: TakeView | null): Artifact[] {
  if (!tv) return all;
  const rows = all.filter((a) => {
    if (!a.step) return true; // pre-ADR-0056 row: no provenance to judge
    const frontier = tv.frontier[a.step];
    return frontier !== undefined && attemptN(a.attempt) <= attemptN(frontier);
  });
  const ofRecord = new Map<string, number>();
  rows.forEach((a, i) => {
    if (a.succeeded) ofRecord.set(a.name, i);
  });
  return rows.map((a, i) => ({ ...a, of_record: ofRecord.get(a.name) === i }));
}

// --- Version rows (ADR-0056 amendment): the run history is a row per Rerun,
// never surfacing "Take"/"attempt". Take 1 is the "original run"; every later
// Take is named by the Rerun that OPENED it — the previous Take's closing
// target and time. Extracted from RunDetail. ---

/** The step whose rerun OPENED take `t` (null for the original run). */
export const openedBy = (takes: Take[], t: Take): string | null =>
  t.n <= 1 ? null : (takes[t.n - 2]?.closedByTarget ?? null);

/** Primary row copy: "original run" / "you reran <step>". */
export const rowLabel = (takes: Take[], t: Take): string => {
  const by = openedBy(takes, t);
  return by ? `you reran ${by}` : "original run";
};

/** When take `t` opened: the run's start for take 1, else the opening rerun. */
export const rowTime = (takes: Take[], t: Take, startedAt: number | null): number | null =>
  t.n <= 1 ? startedAt : (takes[t.n - 2]?.closedAt ?? null);

/** Who pressed the Rerun that opened this Take (the acting principal, `null`
 * when auth is off or for the original run). */
export const rowActor = (takes: Take[], t: Take): string | null =>
  t.n <= 1 ? null : (takes[t.n - 2]?.closedBy ?? null);

/** The row's second line: on the latest row show what opened it ("you reran b"),
 * then the actor and time — enough provenance without a banner. */
export const rowSub = (takes: Take[], t: Take, startedAt: number | null): string => {
  const parts: string[] = [];
  if (t.n === takes.length && t.n > 1) parts.push(rowLabel(takes, t));
  const who = rowActor(takes, t);
  if (who) parts.push(`by ${who}`);
  const at = rowTime(takes, t, startedAt);
  if (at) parts.push(relTime(at));
  return parts.join(" · ");
};

/** Map a step status into an outcome bucket for the rail's mini-summary. Only
 * the five named statuses get their own accent; everything else (pending,
 * skipped, ready, waiting, cancelled, …) falls to `other`. */
export function bucketOf(status: string): keyof OutcomeCounts {
  switch (status) {
    case "succeeded":
      return "succeeded";
    case "failed":
      return "failed";
    case "superseded":
      return "superseded";
    case "not_run":
      return "notRun";
    case "running":
      return "running";
    default:
      return "other";
  }
}

/** Tally step statuses into the per-bucket counts a version row displays. */
export function tally(statuses: string[]): OutcomeCounts {
  const c: OutcomeCounts = {
    succeeded: 0,
    failed: 0,
    superseded: 0,
    notRun: 0,
    running: 0,
    other: 0,
  };
  for (const s of statuses) c[bucketOf(s)] += 1;
  return c;
}

/** The version dropdown's rows: one per Take. The latest/open Take tallies the
 * caller's live statuses (backend-authoritative); a closed Take tallies its
 * snapshot-at-boundary replay. Takes are few — re-replaying per closed row each
 * render is cheap. `selectedN` null = viewing latest. */
export function versionRows(
  events: RunEvent[],
  takes: Take[],
  latestStatuses: string[],
  selectedN: number | null,
  startedAt: number | null,
): VersionRow[] {
  const latest = takes.length;
  const selN = selectedN ?? latest;
  return takes.map((t) => {
    const isLatest = t.n === latest;
    const statuses = isLatest
      ? latestStatuses
      : Object.values(replayTake(events, takes, t).status);
    return {
      n: t.n,
      label: isLatest ? "latest" : rowLabel(takes, t),
      sub: rowSub(takes, t, startedAt),
      summary: tally(statuses),
      isLatest,
      isSelected: selN === t.n,
    };
  });
}
