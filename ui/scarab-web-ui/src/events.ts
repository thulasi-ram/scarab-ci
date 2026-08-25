// Render a RunEvent (EventKind) into a short human line for the activity feed.
// `kind` is either a bare string (unit variant) or a single-key tagged object.
import type { RunEvent } from "./api/client";

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
    case "StepSkipped":
      return `${s(v.step)} — skipped (${s(v.reason)})`;
    case "RunRerunRequested":
      return `${s(v.target)} reran${v.by ? ` by ${s(v.by)}` : ""} — new version`;
    case "StepRetryRequested":
      return `${s(v.target)} retried${v.by ? ` by ${s(v.by)}` : ""} — same version`;
    case "AttemptReadopted":
      return `${s(v.step)} — re-adopted after control-plane restart`;
    // ADR-0061 s5: keeping a run's Workspace Snapshots past the retention window
    // costs storage, so both directions are recorded and both name who asked.
    case "RunSnapshotsPinned":
      return `Workspace snapshots pinned${v.by ? ` by ${s(v.by)}` : ""} — kept past the retention window`;
    case "RunSnapshotsUnpinned":
      return `Workspace snapshots unpinned${v.by ? ` by ${s(v.by)}` : ""} — back to the retention window`;
    default:
      return tag;
  }
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
    case "StepSkipped":
      return { step, text: `skipped (${s(v.reason)})` };
    case "AttemptReadopted":
      return { step, text: "re-adopted after control-plane restart" };
    default:
      return { step, text: describeEvent(e) };
  }
}

/** The activity-rail category for an event — drives its glyph and colour. `err`
 * covers a failed attempt (the retry story) and a run/step that ended failed.
 * `take` is a human rerun — the Take boundary (ADR-0056); `recover` is a
 * control-plane crash re-adoption — durability made visible. */
export type EventCat = "info" | "ok" | "run" | "err" | "gate" | "take" | "recover";

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
    case "RunRerunRequested":
      return "take";
    case "StepRetryRequested":
      // A human "again", but NOT a Take boundary — a re-execution trigger.
      return "run";
    case "AttemptReadopted":
      return "recover";
    // Everything else — including the ADR-0061 s5 snapshot pin/unpin — is `info`.
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
  err: "↻",
  gate: "◷",
  take: "◈",
  recover: "⟲",
};
