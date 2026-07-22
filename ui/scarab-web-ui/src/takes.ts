// Takes (ADR-0056): the run-level version lens, derived ENTIRELY client-side
// from the event log — no take is stored anywhere. A Take is the span of a
// run between two human interventions (`RunRestartRequested` events); Take N's
// view is a pure replay of the log up to its closing boundary, so a closed
// Take shows the run exactly as it stood the instant Restart was pressed
// (snapshot-at-boundary). An attempt that straddles a boundary — started in
// Take N, finished later — honestly shows as running in Take N, with a
// "finished in Take M" affordance from `finishedInTake`.
import type { RunEvent } from "./api/client";

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
  /** The step whose restart closed this take (null for the latest take). */
  closedByTarget: string | null;
  /** Who pressed the restart that closed this take (null = unknown/latest). */
  closedBy: string | null;
  /** Timestamp of the closing boundary, or null for the latest take. */
  closedAt: number | null;
};

/** Split the event log at its `RunRestartRequested` boundaries. Always returns
 * at least one take (the whole log). Take N ends where restart N happens. */
export function deriveTakes(events: RunEvent[]): Take[] {
  const takes: Take[] = [];
  events.forEach((e, i) => {
    const k = kindOf(e);
    if (k?.tag === "RunRestartRequested") {
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
  /** Steps that were mid-flight at the boundary, mapped to the take number in
   * which their straddling attempt finished (the "finished in Take M →"
   * affordance), or 0 if it never finished. */
  finishedInTake: Record<string, number>;
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

  // Straddling attempts: still in flight at the boundary. Find the take in
  // which each one's AttemptFinished eventually landed.
  const finishedInTake: Record<string, number> = {};
  for (const [step, attempt] of inFlight) {
    finishedInTake[step] = 0;
    for (let i = take.endIdx; i < events.length; i++) {
      const k = kindOf(events[i]);
      if (
        k?.tag === "AttemptFinished" &&
        (k.v.step as string) === step &&
        String(k.v.attempt ?? "") === attempt
      ) {
        finishedInTake[step] = takes.find((t) => i < t.endIdx)?.n ?? takes.length;
        break;
      }
    }
  }

  return { status, frontier, attempts, windowAttempts, finishedInTake };
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
 *                  later RunRestartRequested). Distinct from failed/cancelled.
 * - `shadowed`   — succeeded attempts that are no longer the of-record latest
 *                  (a newer successful attempt replaced their role). */
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

  // A restart naming this step arms the NEXT attempt of it — as a `rerun` when
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
    if (k.tag === "RunRestartRequested" || k.tag === "StepRetryRequested") {
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
