// Copy and predicates for the **cold tier's** Workspace-Snapshot retention
// (ADR-0061 s5): the retention window a run's snapshots live under, the manual
// pin that holds it open, and — the part that matters most — what the rerun
// button is allowed to claim.
//
// ADR-0061 gives Workspace Snapshots two tiers with two policies. The workspace
// service (warm) is bounded by SPACE and evicts least-recently-used; it carries
// no promise, because a miss there is slower and never wrong. Object storage
// (cold) is bounded by TIME under a retention window, and *that* is the
// guarantee a user is given. Every string below describes the cold tier, and the
// pin copy says so explicitly rather than letting a reader assume a pin also
// pins a cache.
//
// ADR-0027's rule governs the rerun affordance: **smart never means
// mysterious.** A rerun whose inputs expired is a bigger action than the button
// implies — it walks upstream to regenerate them, in the limit back to `clone` —
// so the label has to change *before* the click, not explain itself afterwards.
//
// Pure derivations, no DOM: the no-DOM vitest tier covers them.

/** The run resource's `snapshot_retention` block. Structural, so the mock
 * fixtures and the generated API type both satisfy it. */
export type RetentionInfo = {
  retention_days: number;
  /** Epoch millis. Absent while the run is unfinished, and while pinned. */
  expires_at?: number | null;
  expired: boolean;
  pinned: boolean;
  pinned_by?: string | null;
  pinned_at?: number | null;
};

/** A rerun preview (`GET …/steps/{step}/rerun-plan`). */
export type RerunPlan = {
  target: string;
  invalidated: string[];
  /** Upstream steps dragged in because their snapshots expired. */
  widened: string[];
  /** Where the rerun effectively starts. */
  starts_from: string[];
  expired_inputs?: { consumer: string; produced_by: string; root: string }[];
};

/** Join ids the way prose does: `a`, `a and b`, `a, b and c`. */
export function nameList(ids: string[]): string {
  if (ids.length === 0) return "";
  if (ids.length === 1) return ids[0];
  return `${ids.slice(0, -1).join(", ")} and ${ids[ids.length - 1]}`;
}

/** Did expired snapshots widen this rerun beyond the step the user picked? */
export function isWidened(plan: RerunPlan | null | undefined): boolean {
  return !!plan && plan.widened.length > 0;
}

/** The rerun button's LABEL. The whole point of ADR-0061 s5's UI half: a widened
 * rerun must say which it is before it is confirmed, naming where it restarts
 * from — not "Rerun pipeline from this step", which would be false. */
export function rerunLabel(plan: RerunPlan | null | undefined): string {
  if (!isWidened(plan)) return "Rerun pipeline from this step";
  const from = nameList(plan!.starts_from);
  return `Inputs expired — this re-runs from ${from}`;
}

/** The rerun button's TITLE: the full scope, so the widening is legible before
 * the click. `days` is the run's retention window, for the "why". */
export function rerunTitle(
  plan: RerunPlan | null | undefined,
  step: string,
  days?: number,
): string {
  if (!isWidened(plan)) {
    return `rerun ${step} and everything downstream — forks a new version`;
  }
  const p = plan!;
  const window = days ? ` past the ${days}-day retention window` : "";
  return (
    `${step}'s input workspace snapshots are gone${window}, so rerunning ${step} alone ` +
    `is impossible. This re-runs ${nameList(p.starts_from)} first to regenerate them, ` +
    `then ${nameList(p.widened.filter((s) => !p.starts_from.includes(s)).concat(step))} — ` +
    `${p.invalidated.length} steps in a new version. Nothing is lost; it just costs the ` +
    `work again.`
  );
}

/** The one-line note under the toolbar when a rerun would widen. Same fact as
 * the title, short enough to be visible without hovering — a scope change the
 * user only discovers on hover is still a surprise. */
export function widenedNote(plan: RerunPlan | null | undefined): string | null {
  if (!isWidened(plan)) return null;
  const p = plan!;
  return (
    `${nameList(p.widened)} will re-run too — the workspace snapshots ${p.target} ` +
    `needs have expired and must be regenerated.`
  );
}

/** The run's retention promise, in one sentence. Deliberately says *expired*,
 * never *deleted*: the window lapsing is a promise ending, and GC is periodic —
 * the data often outlives it. A rerun resolves the difference for real. */
export function retentionNote(r: RetentionInfo, terminal: boolean): string {
  const d = r.retention_days;
  if (r.pinned) {
    const who = r.pinned_by ? ` by ${r.pinned_by}` : "";
    return `Workspace snapshots pinned${who} — kept past the ${d}-day window until unpinned.`;
  }
  if (!terminal) {
    return `Workspace snapshots are kept while this run is unfinished; the ${d}-day window starts when it finishes.`;
  }
  if (r.expired) {
    return `Workspace snapshots expired — kept for ${d} days after a run finishes. A rerun may have to re-run earlier steps.`;
  }
  return `Workspace snapshots kept for ${d} days after this run finished.`;
}

/** The pin toggle's label. */
export function pinLabel(r: RetentionInfo): string {
  return r.pinned ? "Unpin snapshots" : "Keep snapshots";
}

/** The pin toggle's title — and the one place the two-tier honesty lives. A pin
 * extends the time-bounded ARCHIVE. It cannot pin the size-bounded warm cache,
 * and promising otherwise would be the exact kind of quiet over-claim the
 * durability contract forbids. */
export function pinTitle(r: RetentionInfo): string {
  if (r.pinned) {
    return `stop keeping this run's workspace snapshots — they return to the ${r.retention_days}-day window and become collectable`;
  }
  return (
    `keep this run's workspace snapshots past the ${r.retention_days}-day window, for an investigation. ` +
    `Applies to the archive: the workspace service's cache is size-bounded and evicts on its ` +
    `own, which only ever makes a rerun slower, never wrong.`
  );
}
