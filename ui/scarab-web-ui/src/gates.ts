// Gate predicates (ADR-0008), shared so the DAG's label and the approve action
// can never disagree about what "this gate is waiting" means.
//
// A gate launches no Pod. It holds its step status until something releases it,
// and `scheduler::release_gate` records the transition Pending -> Succeeded — so
// the server's own definition of "awaiting a decision" is the pair
// (is a gate, status is not yet terminal). `scheduler::GateNotPending` rejects
// approving anything else.
//
// Three statuses are accepted rather than just `pending`:
//   - `pending`  — what the server actually emits today (verified against a live
//                  suspended run).
//   - `ready`    — a real `StepStatus` variant, so a gate can legitimately hold
//                  it; matching only `pending` would silently miss that.
//   - `waiting`  — NOT a server status. No `Waiting` variant exists in
//                  `StepStatus`, and nothing in the engine emits it; it appears
//                  only in the UI mock fixtures. Tolerated so mock mode renders,
//                  and kept here (rather than rediscovered per call site) with
//                  this note attached.

/** The step shape these predicates need — structural, so both the DAG's own
 * `DagStep` and the API's `StepStatus` satisfy it. */
export type GateLike = {
  status: string;
  /** `manual` / `timer` / `external` when this step is a gate, else absent. */
  gate?: string | null;
  needs?: string[];
};

/** Is this step a gate that has not yet been released or settled? */
export function isGateOpen(s: GateLike): boolean {
  return (
    !!s.gate && (s.status === "pending" || s.status === "waiting" || s.status === "ready")
  );
}

/** Is this gate one a human can approve? Only `manual` gates are: a `timer`
 * gate releases itself and an `external` one is released by a signed token
 * (ADR-0034), so offering an Approve button for either would always 40x. */
export function isApprovable(s: GateLike): boolean {
  return isGateOpen(s) && s.gate === "manual";
}

/** Upstream ids that have not succeeded yet.
 *
 * An open gate means one of two quite different things, and conflating them is
 * what makes a gate UI lie: either it is genuinely your turn, or it is simply
 * not reachable yet because something it needs is still running. Naming the
 * unmet needs keeps those apart — and an empty list is the only state in which
 * approving is meaningful. */
export function gateBlockers(s: GateLike, statusOf: (step: string) => string | undefined): string[] {
  return (s.needs ?? []).filter((n) => statusOf(n) !== "succeeded");
}

/** Every gate in a run that a human could act on, in step order.
 *
 * Returns a LIST because a run can be suspended on several gates at once — a
 * fan-out where two branches each end in their own approval, say. Any
 * run-level "approve" affordance would be ambiguous about which one it meant. */
export function approvableGates<T extends GateLike>(steps: readonly T[]): T[] {
  return steps.filter(isApprovable);
}
