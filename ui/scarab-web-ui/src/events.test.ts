// Activity-rail event rendering: categorisation (glyph/colour) and the
// step-chip split, over representative event shapes.
import { describe, it, expect } from "vitest";
import { describeEvent, eventParts, eventCategory } from "./events";
import { ev, started, finished, transitioned, rerun, retryRequested, readoptedEv } from "./event-fixtures";

describe("eventCategory", () => {
  it("unit variants (bare string kind) are info", () => {
    expect(eventCategory(ev(1, "RunCreated"))).toBe("info");
  });

  it("transitions categorise by destination status", () => {
    expect(eventCategory(transitioned(1, "b", "running", "succeeded"))).toBe("ok");
    expect(eventCategory(transitioned(1, "b", "running", "failed"))).toBe("err");
    expect(eventCategory(transitioned(1, "b", "pending", "cancelled"))).toBe("err");
    expect(eventCategory(transitioned(1, "b", "ready", "running"))).toBe("run");
    expect(eventCategory(transitioned(1, "b", "pending", "ready"))).toBe("info");
    expect(eventCategory(ev(1, { RunTransitioned: { from: "running", to: "succeeded" } }))).toBe(
      "ok",
    );
  });

  it("attempts: started is run; finished is ok unless it carries a failure", () => {
    expect(eventCategory(started(1, "b", "a1"))).toBe("run");
    expect(eventCategory(finished(2, "b", "a1"))).toBe("ok");
    expect(eventCategory(finished(2, "b", "a1", "step"))).toBe("err");
  });

  it("a human rerun is the take boundary; a retry is NOT (just a re-execution)", () => {
    expect(eventCategory(rerun(1, "b", ["c"]))).toBe("take");
    expect(eventCategory(retryRequested(1, "b", []))).toBe("run");
  });

  it("gate release and crash re-adoption get their own categories", () => {
    expect(eventCategory(ev(1, { GateReleased: { step: "deploy" } }))).toBe("gate");
    expect(eventCategory(readoptedEv(1, "b", "a1"))).toBe("recover");
  });
});

describe("describeEvent / eventParts", () => {
  it("names the run-created unit variant", () => {
    expect(describeEvent(ev(1, "RunCreated"))).toBe("Run created");
  });

  it("splits step-scoped events into a chip + message", () => {
    expect(eventParts(started(1, "build", "a1"))).toEqual({
      step: "build",
      text: "attempt started",
    });
    expect(eventParts(finished(1, "build", "a1", "step"))).toEqual({
      step: "build",
      text: "attempt failed (step)",
    });
    expect(eventParts(transitioned(1, "build", "ready", "running"))).toEqual({
      step: "build",
      text: "ready → running",
    });
    expect(eventParts(ev(1, { StepSkipped: { step: "deploy", reason: "filtered" } }))).toEqual({
      step: "deploy",
      text: "skipped (filtered)",
    });
  });

  it("a rerun (no step field) stays a whole-line message naming target and actor", () => {
    const parts = eventParts(rerun(1, "b", ["c"], "a.kim"));
    expect(parts.step).toBeNull();
    expect(parts.text).toBe("b reran by a.kim — new version");
  });

  it("names the workspace pin and unpin, with their actor (ADR-0061 s5)", () => {
    // Keeping a run's workspaces past the retention window costs storage, so the
    // rail records both directions and attributes both. Neither is an execution
    // event, so neither gets a louder category than `info`.
    expect(describeEvent(ev(1, { RunWorkspacePinned: { by: "priya" } }))).toBe(
      "Workspaces pinned by priya — kept past the retention window",
    );
    expect(describeEvent(ev(2, { RunWorkspaceUnpinned: { by: "priya" } }))).toBe(
      "Workspaces unpinned by priya — back to the retention window",
    );
    // `by` is null when auth is off — the line must still read.
    expect(describeEvent(ev(3, { RunWorkspacePinned: { by: null } }))).toBe(
      "Workspaces pinned — kept past the retention window",
    );
    expect(eventCategory(ev(1, { RunWorkspacePinned: { by: "priya" } }))).toBe("info");
  });
});
