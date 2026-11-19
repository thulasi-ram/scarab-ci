// Render a RunEvent (EventKind) into a short human line for the activity feed.
// `kind` is either a bare string (unit variant) or a single-key tagged object.
import type { RunEvent } from "./api/client";
import { duration } from "./fmt";

export function describeEvent(e: RunEvent): string {
  const k = e.kind;
  if (typeof k === "string") {
    return k === "RunCreated" ? "Run created" : k;
  }
  const tag = Object.keys(k)[0];
  const v = k[tag] ?? {};
  const s = (x: unknown) => String(x ?? "");
  switch (tag) {
    case "RunTransitioned":
      return `Run ${s(v.from)} → ${s(v.to)}`;
    case "StepTransitioned":
      return `${s(v.step)}: ${s(v.from)} → ${s(v.to)}`;
    case "AttemptStarted":
      return `${s(v.step)} — attempt started`;
    case "AttemptFinished":
      return v.failure
        ? `${s(v.step)} — attempt failed (${s(v.failure)})`
        : `${s(v.step)} — attempt finished`;
    case "GateReleased":
      return `${s(v.step)} — gate released`;
    case "GateResolved":
      return `${s(v.step)} — ${gateResolvedSummary(v)}`;
    case "StepSkipped":
      return `${s(v.step)} — skipped (${s(v.reason)})`;
    case "RunRerunRequested":
      return `${s(v.target)} reran${v.by ? ` by ${s(v.by)}` : ""} — new version`;
    case "StepRetryRequested":
      return `${s(v.target)} retried${v.by ? ` by ${s(v.by)}` : ""} — same version`;
    case "AttemptReadopted":
      return `${s(v.step)} — re-adopted after control-plane restart`;
    // Keeping a run's Workspace Snapshots past the retention window
    // costs storage, so both directions are recorded and both name who asked.
    case "RunSnapshotsPinned":
      return `Workspace snapshots pinned${v.by ? ` by ${s(v.by)}` : ""} — kept past the retention window`;
    case "RunSnapshotsUnpinned":
      return `Workspace snapshots unpinned${v.by ? ` by ${s(v.by)}` : ""} — back to the retention window`;
    // Fan-in collisions (ticket 2e1a458): the merge overwrote paths — the
    // documented last-input-wins rule made loud, not an error. `count` is authoritative;
    // `sample` is a bounded excerpt (≤8 on k8s), so name the first paths only.
    case "WorkspaceInputCollisions":
      return `${s(v.step)} — ${collisionSummary(v)}`;
    case "StepInfraCondition":
      return `${s(v.step)} — ${infraSummary(v)}`;
    // The dead-letter reason is the ONE place the diagnosis is durably written
    // ("step `build`: Unschedulable: 0/3 nodes are available…"). It used to fall
    // through to `default` and render as the bare tag, so the run that most
    // needed explaining was the one that explained nothing.
    case "RunDeadLettered":
      return `Run dead-lettered — ${s(v.reason)}`;
    // The run's creation failed partway; the reason is the creation error.
    case "RunCreationFailed":
      return `Run creation failed — ${s(v.reason)}`;
    // A plain line the server wrote when it created the run, such as why a
    // path filter in `when:` was not applied. Other Raw payloads (the trigger,
    // the follow-up list) keep their tag.
    case "Raw":
      return typeof v.note === "string" ? v.note : tag;
    default:
      return tag;
  }
}

/** The human line for a GateResolved payload, step omitted: the journalled
 * verdict of one gate wait — passed or failed, with its reason, and who
 * rejected it when a human (or an external gate's token) did. */
function gateResolvedSummary(v: Record<string, unknown>): string {
  const reason = v.reason ? ` — ${String(v.reason)}` : "";
  if (v.passed) return `gate passed${reason}`;
  return `gate ${v.by ? `rejected by ${String(v.by)}` : "failed"}${reason}`;
}

/** The human line for a WorkspaceInputCollisions payload, step omitted:
 * count + the first sampled paths (the full list lives in the fetch
 * container's own log — the event is the rail's summary, not the ledger). */
function collisionSummary(v: Record<string, unknown>): string {
  const count = Number(v.count ?? 0);
  const sample = Array.isArray(v.sample) ? (v.sample as Array<Record<string, unknown>>) : [];
  const firsts = sample
    .slice(0, 2)
    .map((c) => String(c?.path ?? ""))
    .filter(Boolean);
  const paths = firsts.length ? `: ${firsts.join(", ")}${count > firsts.length ? ", …" : ""}` : "";
  return `${count} workspace input collision${count === 1 ? "" : "s"} — last input wins${paths}`;
}

/** The human line for a StepInfraCondition payload, step omitted.
 *
 * The onset and the close are the same variant distinguished by `held_ms`, and
 * they read differently on purpose: the onset is a live warning, the close is a
 * finished episode. Duration leads over the observation count because "stuck in
 * ContainerCreating for 41m" is what an operator acts on — the count is an
 * artifact of the backend's own backoff schedule. */
function infraSummary(v: Record<string, unknown>): string {
  const reason = String(v.reason ?? "");
  const message = v.message ? `: ${String(v.message)}` : "";
  const held = typeof v.held_ms === "number" ? v.held_ms : null;
  if (held === null) return `${reason}${message}`;
  return `${reason} cleared after ${duration(0, held)}${message}`;
}

/** Split an event into its optional step id and a human message with the step
 * omitted — so the rail can render the step name as a styled mono chip. */
export function eventParts(e: RunEvent): { step: string | null; text: string } {
  const k = e.kind;
  if (typeof k === "string") return { step: null, text: describeEvent(e) };
  const tag = Object.keys(k)[0];
  const v = k[tag] ?? {};
  const step = (v.step as string | undefined) ?? null;
  const s = (x: unknown) => String(x ?? "");
  if (!step) return { step: null, text: describeEvent(e) };
  switch (tag) {
    case "StepTransitioned":
      return { step, text: `${s(v.from)} → ${s(v.to)}` };
    case "AttemptStarted":
      return { step, text: "attempt started" };
    case "AttemptFinished":
      return { step, text: v.failure ? `attempt failed (${s(v.failure)})` : "attempt finished" };
    case "GateReleased":
      return { step, text: "gate released" };
    case "GateResolved":
      return { step, text: gateResolvedSummary(v) };
    case "StepSkipped":
      return { step, text: `skipped (${s(v.reason)})` };
    case "AttemptReadopted":
      return { step, text: "re-adopted after control-plane restart" };
    case "WorkspaceInputCollisions":
      return { step, text: collisionSummary(v) };
    case "StepInfraCondition":
      return { step, text: infraSummary(v) };
    default:
      return { step, text: describeEvent(e) };
  }
}

/** The activity-rail category for an event — drives its glyph and colour. `err`
 * covers a failed attempt (the retry story) and a run/step that ended failed.
 * `take` is a human rerun — the Take boundary; `recover` is a
 * control-plane crash re-adoption — durability made visible. */
export type EventCat = "info" | "ok" | "run" | "err" | "gate" | "take" | "recover" | "warning";

export function eventCategory(e: RunEvent): EventCat {
  const k = e.kind;
  if (typeof k === "string") return "info";
  const tag = Object.keys(k)[0];
  const v = k[tag] ?? {};
  const to = String(v.to ?? "");
  switch (tag) {
    case "RunTransitioned":
    case "StepTransitioned":
      return to === "succeeded"
        ? "ok"
        : to === "failed" || to === "cancelled"
          ? "err"
          : to === "running"
            ? "run"
            : "info";
    case "AttemptStarted":
      return "run";
    case "AttemptFinished":
      return v.failure ? "err" : "ok";
    case "GateReleased":
      return "gate";
    case "GateResolved":
      return v.passed ? "gate" : "err";
    case "RunRerunRequested":
      return "take";
    case "StepRetryRequested":
      // A human "again", but NOT a Take boundary — a re-execution trigger.
      return "run";
    case "AttemptReadopted":
      return "recover";
    // Fan-in collisions are semantics working as documented — but semantics
    // an author may not have intended, so they warrant a WARNING mark, never
    // the invisible `info` fall-through (the ticket's not-silent bar) and
    // never `err` (nothing failed).
    case "WorkspaceInputCollisions":
      return "warning";
    // An infra condition is a WARNING while it holds and plain info once it has
    // cleared: a Pod that could not schedule for two minutes and then ran is a
    // fact, not an alarm. Neither is `err` — nothing has failed yet, and the
    // failure (if it comes) has its own event.
    case "StepInfraCondition":
      return v.held_ms === undefined || v.held_ms === null ? "warning" : "info";
    // The system could not obtain a verdict — the operator signal, and squarely
    // an error. It previously fell through to `info`, rendering the most serious
    // outcome a run has as the quietest thing on the rail.
    case "RunDeadLettered":
    case "RunCreationFailed":
      return "err";
    // A note says something did not happen as written (a path filter that
    // could not be applied): a warning, since nothing failed.
    case "Raw":
      return typeof v.note === "string" ? "warning" : "info";
    // Everything else — including the snapshot pin/unpin — is `info`.
    // Those two used to have their own arms returning `info`, which was dead code
    // AND untestable: no input could tell the arm from this fall-through, so the
    // test that "covered" them was pinning this line. The decision they encoded
    // still holds and is what `default` means here: a category is a claim that
    // something *ran*, and only the cases above get to make it.
    default:
      return "info";
  }
}

/** The glyph shown in the rail node for a category. */
export const EVENT_GLYPH: Record<EventCat, string> = {
  info: "◆",
  ok: "✓",
  run: "●",
  // The failure mark the rest of the run page uses (the attempts menu, the
  // version menu, a failed artifact). A circular arrow here read as "retry".
  err: "✗",
  gate: "◷",
  take: "◈",
  recover: "⟲",
  warning: "△",
};

/** A condition still holding a step's attempt back: the onset was logged and
 * no closing event (the one carrying `held_ms`) has followed yet. */
export type ActiveInfraCondition = {
  attempt: string;
  reason: string;
  message?: string;
  /** When the condition started (the onset event's time). */
  since: number;
};

/** The infra condition holding each step's CURRENT attempt back right now, by
 * step id. A step whose latest attempt has no open condition is absent.
 *
 * The backend logs an onset (no `held_ms`) when a condition appears and a
 * closing event (with `held_ms`) when it clears, changes, or the attempt ends;
 * a change closes the old condition before it opens the new one. An attempt
 * that finished, or that a newer attempt replaced, holds nothing, even when a
 * closing event never arrived. */
export function activeInfraConditions(events: RunEvent[]): Record<string, ActiveInfraCondition> {
  const latest: Record<string, string> = {}; // step -> current attempt id
  const open: Record<string, ActiveInfraCondition> = {}; // step -> open condition
  for (const e of events) {
    const k = e.kind;
    if (typeof k === "string") continue;
    const tag = Object.keys(k)[0];
    const v = k[tag] ?? {};
    const step = v.step as string | undefined;
    if (!step) continue;
    const attempt = String(v.attempt ?? "");
    switch (tag) {
      case "AttemptStarted":
        latest[step] = attempt;
        delete open[step];
        break;
      case "AttemptFinished":
        if (open[step]?.attempt === attempt) delete open[step];
        break;
      case "StepInfraCondition": {
        const closing = v.held_ms !== undefined && v.held_ms !== null;
        if (closing) {
          if (open[step]?.attempt === attempt) delete open[step];
        } else if (latest[step] === undefined || latest[step] === attempt) {
          open[step] = {
            attempt,
            reason: String(v.reason ?? ""),
            ...(v.message ? { message: String(v.message) } : {}),
            since: e.at,
          };
        }
        break;
      }
    }
  }
  return open;
}

/** Reasons that mean the scheduler could not place the Pod on a node. */
export const NODE_WAIT_REASONS = new Set(["Unschedulable", "NotScheduled", "SchedulingGated"]);

/** How long the scheduler could not place one attempt's Pod, in ms: the sum of
 * its closed "waiting for a node" conditions. Null when there were none. An
 * open condition is not counted; the caller shows it as active instead. */
export function heldBeforeStart(events: RunEvent[], step: string, attempt: string): number | null {
  let total = 0;
  for (const e of events) {
    const k = e.kind;
    if (typeof k === "string") continue;
    const v = k.StepInfraCondition;
    if (!v || v.step !== step || String(v.attempt ?? "") !== attempt) continue;
    if (typeof v.held_ms !== "number" || !NODE_WAIT_REASONS.has(String(v.reason ?? ""))) continue;
    total += v.held_ms;
  }
  return total > 0 ? total : null;
}

/** An infra condition's reason token in plain words, for the graph node:
 * "waiting for a node" for a Pod the scheduler cannot place, and so on. An
 * unknown token is split into lower-case words ("SomeNewReason" → "some new
 * reason") rather than hidden. */
export function infraReasonText(reason: string): string {
  switch (reason) {
    case "Unschedulable":
    case "NotScheduled":
    case "SchedulingGated":
      return "waiting for a node";
    case "ContainerCreating":
    case "PodInitializing":
      return "starting its container";
    case "ImagePullBackOff":
    case "ErrImagePull":
      return "cannot pull its image";
    case "ErrImageNeverPull":
      return "image not on the node";
    case "InvalidImageName":
      return "invalid image name";
    case "CreateContainerConfigError":
    case "CreateContainerError":
      return "cannot create its container";
    case "CrashLoopBackOff":
      return "container keeps crashing";
  }
  const words = reason.replace(/([a-z0-9])([A-Z])/g, "$1 $2").toLowerCase().trim();
  return words || "waiting on infrastructure";
}
