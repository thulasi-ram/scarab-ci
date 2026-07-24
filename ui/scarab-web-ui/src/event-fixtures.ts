// TEST-ONLY event builders: RunEvent values shaped exactly as the events SSE
// serializes them (see api/client.ts `RunEvent` and the fixture in mock.ts —
// `kind` is a bare string for unit variants or a single-key tagged object).
// Imported by the *.test.ts suites only; never bundled into the app.
import type { RunEvent } from "./api/client";

export const ev = (at: number, kind: RunEvent["kind"]): RunEvent => ({
  version: 1,
  run: "run-1",
  at,
  kind,
});

export const started = (at: number, step: string, attempt: string): RunEvent =>
  ev(at, { AttemptStarted: { step, attempt } });

export const finished = (at: number, step: string, attempt: string, failure?: string): RunEvent =>
  ev(at, { AttemptFinished: { step, attempt, ...(failure ? { failure } : {}) } });

export const transitioned = (at: number, step: string, from: string, to: string): RunEvent =>
  ev(at, { StepTransitioned: { step, from, to } });

/** A human Rerun — the Take boundary. `invalidated` = the dependent cascade. */
export const rerun = (at: number, target: string, invalidated: string[], by?: string): RunEvent =>
  ev(at, { RunRerunRequested: { target, invalidated, ...(by ? { by } : {}) } });

/** A human Retry — another attempt in the CURRENT take, no fork. */
export const retryRequested = (at: number, target: string, invalidated: string[]): RunEvent =>
  ev(at, { StepRetryRequested: { target, invalidated } });

export const readoptedEv = (at: number, step: string, attempt: string): RunEvent =>
  ev(at, { AttemptReadopted: { step, attempt } });

/** A canonical two-take run, used across the suites:
 *
 *   take 1: a@a1 ok → b@a1 failed → b@a2 ok (auto-retry) → c@a1 in flight
 *   ── rerun b (cascade c) by a.kim at t=8000 ──
 *   take 2: b@a3 ok → c@a2 in flight
 *
 * The boundary catches c@a1 mid-run (the rerun re-arms it → superseded) and
 * leaves a untouched (carried forward).
 */
export function twoTakeRun(): RunEvent[] {
  return [
    started(1000, "a", "a1"),
    finished(2000, "a", "a1"),
    transitioned(2100, "a", "running", "succeeded"),
    started(3000, "b", "a1"),
    finished(4000, "b", "a1", "step"),
    transitioned(4100, "b", "running", "failed"),
    started(5000, "b", "a2"),
    finished(6000, "b", "a2"),
    transitioned(6100, "b", "running", "succeeded"),
    started(7000, "c", "a1"),
    rerun(8000, "b", ["c"], "a.kim"), // index 10 — closes take 1
    started(9000, "b", "a3"),
    finished(10000, "b", "a3"),
    transitioned(10100, "b", "running", "succeeded"),
    started(11000, "c", "a2"),
  ];
}

/** Index of the take-1-closing rerun inside `twoTakeRun()`. */
export const TWO_TAKE_BOUNDARY_IDX = 10;
