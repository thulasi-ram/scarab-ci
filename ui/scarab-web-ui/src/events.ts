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
    default:
      return { step, text: describeEvent(e) };
  }
}

/** The activity-rail category for an event — drives its glyph and colour. `err`
 * covers a failed attempt (the retry story) and a run/step that ended failed. */
export type EventCat = "info" | "ok" | "run" | "err" | "gate";

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
};
