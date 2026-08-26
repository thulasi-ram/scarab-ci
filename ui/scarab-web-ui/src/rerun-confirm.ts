// Copy derivations for the rerun/retry CONFIRM popover (git-bug 4afaa3e): the
// affordance names the WHOLE cascade before the click lands, from the engine's
// own plan — never a client-side DAG estimate that could drift.
//
// Everything here is a pure function over the plan (no DOM), per the no-DOM
// vitest tier. Design rules baked in:
// - ADR-0027: smart never means mysterious — the common case is ONE sentence,
//   the long/widened case an explicit grouped list.
// - Amendment F4: a gate member "pauses for approval" — the copy never claims
//   a gate will run.
// - Amendment F9: retention is named GENERICALLY ("past retention"), never a
//   day count — the flat configured number is not the number that expired a
//   profiled run's packs, and a wrong number is worse than none.
// - Honesty note (design §2): the set is the MAXIMAL scope; skip-if-unchanged
//   may skip a re-armed descendant, so the footnote says so.

import {
  isWidened,
  nameList,
  type PlannedStepInfo,
  type RerunPlan,
} from "./snapshot-retention";

/** Which button opened the popover. Retry stays in the current version
 * (ADR-0056: another attempt, no fork); rerun forks a new one. */
export type ConfirmKind = "rerun" | "retry";

/** The plan's step list, tolerant of a fixture/mock that predates `steps[]`. */
function stepsOf(plan: RerunPlan): PlannedStepInfo[] {
  return plan.steps ?? [];
}

/** Above this many steps the one-sentence form gives way to the grouped list
 * (design §3); any widening forces the list too — regeneration deserves rows. */
export const LIST_THRESHOLD = 4;

/** The popover's headline: what is being asked. */
export function confirmHeadline(kind: ConfirmKind, target: string): string {
  return kind === "retry" ? `Retry ${target}?` : `Rerun from ${target}?`;
}

/** The widened warning line under the headline, or null. Generic retention
 * wording (amendment F9): no day count, ever. */
export function confirmWarning(plan: RerunPlan | null | undefined): string | null {
  if (!plan || !isWidened(plan)) return null;
  return `Inputs past retention — this re-runs from ${nameList(plan.starts_from)}.`;
}

/** The one-sentence common case (the ticket's literal form):
 *  - "This re-runs `push`."
 *  - "This re-runs `push`, which also re-runs `deploy-staging`."
 *  - widened: "This re-runs from `clone`, which also re-runs `build` and `test`."
 *  - retry: "This retries `push` — another attempt in this version." (+cascade)
 */
export function confirmSentence(plan: RerunPlan, kind: ConfirmKind): string {
  const widened = isWidened(plan);
  const roots = widened ? plan.starts_from : [plan.target];
  const others = plan.invalidated.filter((s) => !roots.includes(s));
  const also = others.length > 0 ? `, which also re-runs ${nameList(others)}` : "";
  if (kind === "retry") {
    return `This retries ${plan.target} — another attempt in this version${
      also ? ` —${also.slice(1)}` : ""
    }.`;
  }
  if (widened) return `This re-runs from ${nameList(roots)}${also}.`;
  return `This re-runs ${plan.target}${also}.`;
}

/** The gate disclosure (amendment F4): "Pauses for approval at `X`." for every
 * in-set gate — the plan never claims a gate will run. Null when none. */
export function gateNote(plan: RerunPlan | null | undefined): string | null {
  if (!plan) return null;
  const gates = stepsOf(plan)
    .filter((s) => s.is_gate)
    .map((s) => s.step);
  if (gates.length === 0) return null;
  return `Pauses for approval at ${nameList(gates)}.`;
}

/** Long cascades and any widening render the grouped list instead of (well,
 * under) the sentence. */
export function useExpandedList(plan: RerunPlan | null | undefined): boolean {
  if (!plan) return false;
  return isWidened(plan) || plan.invalidated.length > LIST_THRESHOLD;
}

/** One row of the expanded list. */
export type PlanRow = { step: string; note: string };
/** One group of the expanded list. */
export type PlanGroup = { title: string; rows: PlanRow[] };

/** Per-row note, from the engine's reason + attribution. Generic retention
 * wording only (amendment F9); gates append their pause (amendment F4). */
function rowNote(s: PlannedStepInfo, kind: ConfirmKind): string {
  let note: string;
  switch (s.reason) {
    case "target":
      note = kind === "retry" ? "the step you are retrying" : "the step you picked";
      break;
    case "regenerate":
      note = s.because_of
        ? `inputs past retention · regenerates data for ${s.because_of}`
        : "inputs past retention";
      break;
    case "regenerate_cascade":
      note = s.because_of ? `re-runs because ${s.because_of} re-runs` : "re-runs to regenerate";
      break;
    default:
      note = s.because_of ? `depends on ${s.because_of}` : "downstream of the target";
  }
  if (s.is_gate) note += " · pauses for approval";
  return note;
}

/** The grouped, execution-ordered list: expired regeneration first (when any),
 * then the rerun the user actually asked for. */
export function planGroups(plan: RerunPlan, kind: ConfirmKind): PlanGroup[] {
  const steps = stepsOf(plan);
  const regen = steps.filter(
    (s) => s.reason === "regenerate" || s.reason === "regenerate_cascade",
  );
  const asked = steps.filter((s) => s.reason === "target" || s.reason === "cascade");
  const groups: PlanGroup[] = [];
  if (regen.length > 0) {
    groups.push({
      title: `Regenerating expired inputs (${regen.length})`,
      rows: regen.map((s) => ({ step: s.step, note: rowNote(s, kind) })),
    });
  }
  groups.push({
    title:
      regen.length > 0
        ? `Then the ${kind} you asked for (${asked.length})`
        : `The ${kind} you asked for (${asked.length})`,
    rows: asked.map((s) => ({ step: s.step, note: rowNote(s, kind) })),
  });
  return groups;
}

/** The confirm button: the last thing under the cursor names the blast radius.
 * A missing plan (fetch failed) still allows confirm — disclosure must never
 * become prevention — but then the button claims no count. */
export function confirmLabel(plan: RerunPlan | null | undefined, kind: ConfirmKind): string {
  const verb = kind === "retry" ? "Retry" : "Rerun";
  if (!plan) return verb;
  const n = plan.invalidated.length;
  return `${verb} ${n} step${n === 1 ? "" : "s"}`;
}

/** The footnote: maximal-scope honesty (skip-if-unchanged may skip), plus the
 * version framing (rerun forks, retry does not — ADR-0056). */
export function confirmFootnote(plan: RerunPlan, kind: ConfirmKind): string {
  const n = plan.invalidated.length;
  const where = kind === "retry" ? "in this version" : "in a new version";
  return `Up to ${n} step${n === 1 ? "" : "s"} re-run ${where}. Steps whose inputs are unchanged are skipped.`;
}

/** The TOCTOU disclosure (design §5): the POST returns the EXECUTED plan; when
 * it is wider than the preview the user confirmed (a snapshot expired between
 * preview and click), say so — after the fact, but out loud. Null when nothing
 * diverged, or when there was no preview to diverge from. */
export function planDivergence(
  preview: RerunPlan | null | undefined,
  executed: RerunPlan | null | undefined,
  kind: ConfirmKind,
): string | null {
  if (!preview || !executed) return null;
  const promised = new Set(preview.invalidated);
  const extra = (executed.invalidated ?? []).filter((s) => !promised.has(s));
  if (extra.length === 0) return null;
  const verb = kind === "retry" ? "retry" : "rerun";
  return `The ${verb} widened while you confirmed: ${nameList(extra)} also re-run${
    extra.length === 1 ? "s" : ""
  }.`;
}
