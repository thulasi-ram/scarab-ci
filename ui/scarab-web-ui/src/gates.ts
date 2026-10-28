import { absTime } from "./fmt";

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
  /** `manual` / `timer` / `external` / `deployment` when this step is a gate,
   * else absent. */
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

// --- Verdicts, waits and holds (slice b, criterion 5) ----------------------
// The server says, per step, how a gate's latest wait resolved
// (`gate_resolution`, the journal's GateResolved), what a waiting
// DeploymentGate waits on (`deployment_gate`, display only), and why a deploy
// step is held by per-DeploymentTarget concurrency (`held`). These turn those
// facts into the one line the DAG node and the step pane both show.

/** The kinds a human can REJECT (fail with a reason): every gate kind. A
 * DeploymentGate cannot be approved — core passes it from the Deployment
 * record — but rejecting one is how a human gives up before its deadline. */
const REJECTABLE_KINDS: ReadonlySet<string> = new Set(["manual", "timer", "external", "deployment"]);

/** A gate verdict as the server journals it (`GateResolutionDto`). */
export type GateResolutionLike = {
  passed: boolean;
  reason?: string | null;
  /** The gate Attempt the verdict closed. */
  attempt: string;
  /** Who rejected it, for a rejection by request. */
  by?: string | null;
};

/** A waiting DeploymentGate's target and Deployment (`DeploymentGateDto`). */
export type DeploymentGateLike = {
  target_source: string;
  target_id: string;
  /** The Deployment's OWN status (its Environment's baseline deadline over
   * its lifetime) — not the gate's clock: a retried gate waits afresh while
   * this can already read `stalled`. */
  deployment_status?: string | null;
  /** When the gate itself fails `stalled` (epoch ms), on the engine's clock:
   * its wait's window plus its Project's effective deadline. */
  fails_at?: number | null;
  latest_label?: string | null;
  /** The server could not read the Deployment record for this view: the
   * Deployment fields are absent because they are unknown. */
  record_unreadable?: boolean | null;
};

export type GateStatusLike = GateLike & {
  attempt_list?: { id: string }[];
  gate_resolution?: GateResolutionLike | null;
  deployment_gate?: DeploymentGateLike | null;
  held?: string | null;
  /** Set on a `plugin_action` step (ADR-0074 §5): the plugin, the action and
   * the plugin's latest `pending` label for the latest attempt. */
  plugin_action?: PluginActionLike | null;
};

/** A `plugin_action` step's view: which plugin action it waits on and what
 * the plugin last said. */
export type PluginActionLike = {
  plugin: string;
  action: string;
  label?: string | null;
};

/** The tone a status line renders with, and its text. */
export type GateStatus = { tone: "waiting" | "passed" | "failed" | "held"; text: string };

/** The verdict that speaks for the gate as it is NOW, or null.
 *
 * The server sends the LATEST `GateResolved`, which outlives a re-arm: a retry
 * or rerun puts the gate back to `pending` (and later mints a new Attempt), and
 * showing the old "passed" then would lie. So a verdict counts only while the
 * gate is settled (`succeeded`/`failed`) AND it closed the gate's latest
 * Attempt. */
export function currentResolution(s: GateStatusLike): GateResolutionLike | null {
  const r = s.gate_resolution;
  if (!r || !s.gate) return null;
  if (s.status !== "succeeded" && s.status !== "failed") return null;
  const last = s.attempt_list?.[s.attempt_list.length - 1];
  if (last && last.id !== r.attempt) return null;
  return r;
}

/** `in_progress` → `in progress`. */
const pretty = (status: string): string => status.replace(/_/g, " ");

/** A concurrency hold's reason as the server words it starts with `held:`;
 * the line adds that prefix itself, so it is not doubled. */
const holdReason = (held: string): string => held.replace(/^held:\s*/i, "").trim();

/** The one status line for a step's gate or hold, or null when there is
 * nothing to say beyond its status:
 *
 *   held: <reason>                       a deploy step held by concurrency
 *   passed: <reason>                     a settled gate's verdict
 *   failed: <reason> (rejected by <who>) a settled gate's verdict
 *   waiting on <source/id> (<status>, "<label>")   a waiting DeploymentGate
 *   waiting on <source/id> (Deployment: <status> · gate fails at <T>)
 *                                        ... when the server knows the gate's own deadline
 *
 * The Deployment's status and the gate's deadline are two clocks: a retried
 * gate waits afresh on a Deployment that already reads `stalled`, so when the
 * server sends `fails_at` the line says whose status it is and when the GATE
 * fails (git-bug ec84c5d). `fmtTime` renders that instant (absolute time by
 * default; tests pass a fixed one).
 *
 * A waiting DeploymentGate whose record could not be read says exactly that —
 * never "no Deployment recorded yet", which would be false.
 */
export function gateStatus(
  s: GateStatusLike,
  fmtTime: (ms: number) => string = absTime,
): GateStatus | null {
  if (s.held) return { tone: "held", text: `held: ${holdReason(s.held)}` };
  // A running plugin_action step waits on its plugin's answer, holding no Pod.
  const p = s.plugin_action;
  if (p && s.status === "running") {
    const what = `${p.plugin}/${p.action}`;
    const label = p.label?.trim();
    return {
      tone: "waiting",
      text: label ? `waiting on ${what}: ${label}` : `waiting on ${what} (no answer yet)`,
    };
  }
  if (!s.gate) return null;
  const r = currentResolution(s);
  if (r) {
    const reason = r.reason?.trim();
    if (r.passed) return { tone: "passed", text: reason ? `passed: ${reason}` : "passed" };
    const by = r.by ? ` (rejected by ${r.by})` : "";
    return { tone: "failed", text: `failed: ${reason || "no reason given"}${by}` };
  }
  const d = s.deployment_gate;
  if (d && isGateOpen(s)) {
    const status = d.deployment_status
      ? [pretty(d.deployment_status), d.latest_label ? `"${d.latest_label}"` : null]
          .filter(Boolean)
          .join(", ")
      : null;
    const detail = status
      ? d.fails_at != null
        ? `Deployment: ${status} · gate fails at ${fmtTime(d.fails_at)}`
        : status
      : d.record_unreadable
        ? "Deployment record could not be read"
        : "no Deployment recorded yet";
    return { tone: "waiting", text: `waiting on ${d.target_source}/${d.target_id} (${detail})` };
  }
  return null;
}

/** {@link gateStatus}'s text alone. */
export function gateStatusText(
  s: GateStatusLike,
  fmtTime: (ms: number) => string = absTime,
): string | null {
  return gateStatus(s, fmtTime)?.text ?? null;
}

/** Can a human reject this gate right now? Any gate kind, while it waits on a
 * verdict: open, and REACHED — every need succeeded. The server 409s a gate
 * whose needs are unmet ("not reached yet"), so offering Reject there would
 * always fail. */
export function isRejectable(s: GateLike, statusOf: (step: string) => string | undefined): boolean {
  return isGateOpen(s) && REJECTABLE_KINDS.has(s.gate ?? "") && gateBlockers(s, statusOf).length === 0;
}

/** The gates the run page's gate bar lists, in step order: every open gate a
 * human can act on — approve (manual) or reject (any kind). A blocked one is
 * listed too, with its actions disabled, so "not your turn yet" is visible. */
export function actionableGates<T extends GateLike>(steps: readonly T[]): T[] {
  return steps.filter((s) => isGateOpen(s) && REJECTABLE_KINDS.has(s.gate ?? ""));
}

/** {@link actionableGates} as step IDs — what the gate bar iterates. A poll
 * replaces every step object, so a list keyed by object rebuilds each row
 * (and re-mounts an open Reject form, losing its caret) once a poll; IDs are
 * strings, equal across polls, so a row lives as long as its gate is open. */
export function actionableGateIds(steps: readonly (GateLike & { id: string })[]): string[] {
  return actionableGates(steps).map((s) => s.id);
}

/** The server's bound on a reject reason, in UTF-8 bytes. */
export const REJECT_REASON_MAX_BYTES = 1024;

/** Why a reject reason would be refused (400), or null when it is fine. The
 * server trims it, then requires 1..=1024 bytes. */
export function rejectReasonError(reason: string): string | null {
  const t = reason.trim();
  if (!t) return "a reason is required";
  if (new TextEncoder().encode(t).length > REJECT_REASON_MAX_BYTES) {
    return `at most ${REJECT_REASON_MAX_BYTES} bytes`;
  }
  return null;
}

/** The gate bar's hint for an open gate: what it is waiting for. */
export function gateWaitHint(s: GateStatusLike, statusOf: (step: string) => string | undefined): string {
  const blockers = gateBlockers(s, statusOf);
  if (blockers.length > 0) return `waiting on ${blockers.join(", ")}`;
  switch (s.gate) {
    case "deployment":
      return gateStatusText(s) ?? "waiting on its Deployment";
    case "timer":
      return "waiting for its timer";
    case "external":
      return "waiting for an external release";
    default:
      return "waiting for a decision";
  }
}

// --- The dashboard's action inbox (git-bug c489346) --------------------------
// A waiting gate holds only its dependents (ADR-0070 §9): a Run whose gate is
// reached while an independent sibling still runs stays `running`. So "this
// run waits on a gate" is the server's `waiting_gates` (gate steps Pending,
// reached, with an open Attempt), never `status === "suspended"`.

/** A run's waiting gate as the run list sends it (`WaitingGateDto`). */
export type WaitingGateLike = { step: string; kind: string; since: number };

/** The run-list shape the inbox needs (`RunSummaryDto`). */
export type InboxRunLike = { status: string; waiting_gates?: WaitingGateLike[] | null };

/** The runs that wait on a gate, in list order. A `suspended` run counts too —
 * suspension means only gate waits remain, and an older server sends no
 * `waiting_gates`. */
export function inboxRuns<T extends InboxRunLike>(runs: readonly T[]): T[] {
  return runs.filter((r) => (r.waiting_gates?.length ?? 0) > 0 || r.status === "suspended");
}

/** The inbox row's verb: `approve` when a manual gate waits (the one kind a
 * human approves), else `review` (a rejectable timer/external/deployment
 * wait). */
export function inboxVerb(r: InboxRunLike): "approve" | "review" {
  const gates = r.waiting_gates ?? [];
  if (gates.length === 0) return "approve"; // suspended, gates unknown: as before
  return gates.some((g) => g.kind === "manual") ? "approve" : "review";
}

/** The inbox row's detail line: which gates wait, and whether the run is
 * still running beside them. */
export function inboxDetail(r: InboxRunLike): string {
  const gates = r.waiting_gates ?? [];
  if (gates.length === 0) return "suspended on a gate · waiting for a decision";
  const names = gates.map((g) => `${g.step} (${g.kind})`).join(", ");
  const what = `${gates.length === 1 ? "gate" : "gates"} ${names} waiting`;
  return r.status === "running" ? `${what} · other steps still running` : what;
}
