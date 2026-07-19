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
  /** Per-step status as of the boundary instant (steps absent = never seen). */
  status: Record<string, string>;
  /** Per-step latest attempt id as of the boundary — the evidence frontier the
   * attempt-scoped reads (`?attempt=`) are keyed with. */
  frontier: Record<string, string>;
  /** Per-step attempt COUNT as of the boundary (drives the ×N retry badge). */
  attempts: Record<string, number>;
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
  const inFlight = new Map<string, string>(); // step -> attempt currently running

  for (let i = 0; i < take.endIdx; i++) {
    const k = kindOf(events[i]);
    if (!k) continue;
    const step = (k.v.step as string) ?? null;
    switch (k.tag) {
      case "StepTransitioned":
        if (step) status[step] = String(k.v.to ?? "");
        break;
      case "AttemptStarted":
        if (step) {
          frontier[step] = String(k.v.attempt ?? "");
          attempts[step] = (attempts[step] ?? 0) + 1;
          inFlight.set(step, String(k.v.attempt ?? ""));
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

  return { status, frontier, attempts, finishedInTake };
}

/** The cause of one attempt of a step (drives the attempt-strip chip label):
 * `initial` (first attempt), `restart` (a human boundary immediately precedes
 * it and names the step in its invalidation set), or `retry` (the engine's
 * own doing). `readopted` marks attempts a resumed control plane adopted. */
export type AttemptCause = "initial" | "restart" | "retry";

export function attemptCauses(
  events: RunEvent[],
  step: string,
): { causes: Record<string, AttemptCause>; readopted: Set<string> } {
  const causes: Record<string, AttemptCause> = {};
  const readopted = new Set<string>();
  // A restart naming this step arms the NEXT attempt of it.
  let armedByRestart = false;
  let seen = 0;
  for (const e of events) {
    const k = kindOf(e);
    if (!k) continue;
    if (k.tag === "RunRestartRequested") {
      const invalidated = (k.v.invalidated as string[]) ?? [];
      if (invalidated.includes(step) || (k.v.target as string) === step) {
        armedByRestart = true;
      }
      continue;
    }
    if ((k.v.step as string) !== step) continue;
    if (k.tag === "AttemptStarted") {
      const id = String(k.v.attempt ?? "");
      seen += 1;
      causes[id] = seen === 1 ? "initial" : armedByRestart ? "restart" : "retry";
      armedByRestart = false;
    } else if (k.tag === "AttemptReadopted") {
      readopted.add(String(k.v.attempt ?? ""));
    }
  }
  return { causes, readopted };
}
