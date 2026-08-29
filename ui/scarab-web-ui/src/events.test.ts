// Activity-rail event rendering: categorisation (glyph/colour) and the
// step-chip split, over representative event shapes.
import { describe, it, expect } from "vitest";
import { describeEvent, eventParts, eventCategory, EVENT_GLYPH } from "./events";
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

  it("names the snapshot pin and unpin, with their actor (ADR-0061 s5)", () => {
    // Keeping a run's Workspace Snapshots past the retention window costs storage, so the
    // rail records both directions and attributes both. Neither is an execution
    // event, so neither gets a louder category than `info`.
    expect(describeEvent(ev(1, { RunSnapshotsPinned: { by: "priya" } }))).toBe(
      "Workspace snapshots pinned by priya — kept past the retention window",
    );
    expect(describeEvent(ev(2, { RunSnapshotsUnpinned: { by: "priya" } }))).toBe(
      "Workspace snapshots unpinned by priya — back to the retention window",
    );
    // `by` is null when auth is off — the line must still read.
    expect(describeEvent(ev(3, { RunSnapshotsPinned: { by: null } }))).toBe(
      "Workspace snapshots pinned — kept past the retention window",
    );
  });

  it("does not give a retention decision an execution category", () => {
    // A category is a claim that something RAN — it picks the rail's glyph and
    // colour. A pin is a storage decision, so it must stay `info`.
    //
    // `info` is also `eventCategory`'s fall-through, which makes the naive
    // assertion (`toBe("info")` on a pin, alone) unfalsifiable: it passes whether
    // the kind is handled, unhandled, or misspelled. So assert the CONTRAST — a
    // real Take boundary in the same breath — which is what actually distinguishes
    // "deliberately quiet" from "eventCategory has stopped discriminating".
    const pin = eventCategory(ev(1, { RunSnapshotsPinned: { by: "priya" } }));
    const unpin = eventCategory(ev(2, { RunSnapshotsUnpinned: { by: "priya" } }));
    const take = eventCategory(rerun(3, "b", ["c"], "a.kim"));
    expect(take).toBe("take");
    expect(pin).toBe("info");
    expect(unpin).toBe("info");
    expect(pin).not.toBe(take);
    // And the glyph really is a distinct mark, not the same one twice — the rail
    // is the surface a reader scans, so two categories that render identically
    // would make the contrast above invisible where it matters.
    expect(EVENT_GLYPH[pin]).not.toBe(EVENT_GLYPH[take]);
  });
});

describe("WorkspaceInputCollisions (ticket 2e1a458)", () => {
  const collisions = (count: number, sample: Array<{ path: string; winner: string; loser: string }>) =>
    ev(1, { WorkspaceInputCollisions: { step: "e", attempt: "a1", count, sample } });

  it("is a WARNING — its own visible mark, never the info fall-through, never err", () => {
    const cat = eventCategory(collisions(1, [{ path: "shared.txt", winner: "c", loser: "b" }]));
    expect(cat).toBe("warning");
    // The not-silent bar: the mark must be distinct from the fall-through's,
    // or an unhandled tag would render identically and the arm would be
    // unfalsifiable (same trap the pin/unpin test documents).
    expect(EVENT_GLYPH[cat]).toBeTruthy();
    expect(EVENT_GLYPH[cat]).not.toBe(EVENT_GLYPH["info"]);
    // And it is NOT an error: nothing failed — last-wins is ADR-0007
    // semantics, the event only makes it loud.
    expect(cat).not.toBe("err");
  });

  it("renders the count and the first sampled paths, step split into the chip", () => {
    const parts = eventParts(
      collisions(3, [
        { path: "dist/app.js", winner: "c", loser: "b" },
        { path: "dist/app.css", winner: "c", loser: "b" },
        { path: "dist/index.html", winner: "c", loser: "b" },
      ]),
    );
    expect(parts.step).toBe("e");
    expect(parts.text).toBe(
      "3 workspace input collisions — last input wins: dist/app.js, dist/app.css, …",
    );
  });

  it("singular count, and a count that outruns its truncated sample stays authoritative", () => {
    expect(eventParts(collisions(1, [{ path: "shared.txt", winner: "c", loser: "b" }])).text).toBe(
      "1 workspace input collision — last input wins: shared.txt",
    );
    // The k8s transport caps the sample at 8; the count is the truth.
    const parts = eventParts(collisions(40, [{ path: "a.txt", winner: "c", loser: "b" }]));
    expect(parts.text).toContain("40 workspace input collisions");
    expect(parts.text).toContain("a.txt, …");
  });

  it("an empty sample (fully truncated transport) still reads", () => {
    const parts = eventParts(collisions(12, []));
    expect(parts.step).toBe("e");
    expect(parts.text).toBe("12 workspace input collisions — last input wins");
  });

  it("the whole-line form names the step", () => {
    expect(describeEvent(collisions(1, [{ path: "shared.txt", winner: "c", loser: "b" }]))).toBe(
      "e — 1 workspace input collision — last input wins: shared.txt",
    );
  });
});

// The infra-condition channel (ADR-0068). These events exist because a step
// whose Pod never started has an empty log stream by construction, so the
// activity rail is the only place its diagnosis can appear.
describe("StepInfraCondition", () => {
  const onset = (reason: string, message?: string) =>
    ev(1, { StepInfraCondition: { step: "build", attempt: "a1", reason, message } });
  const close = (reason: string, held_ms: number, observations: number) =>
    ev(2, { StepInfraCondition: { step: "build", attempt: "a1", reason, held_ms, observations } });

  it("an onset leads with the backend reason and its message", () => {
    expect(describeEvent(onset("Unschedulable", "0/3 nodes are available: 3 Insufficient cpu"))).toBe(
      "build — Unschedulable: 0/3 nodes are available: 3 Insufficient cpu",
    );
  });

  it("a reason with no message still reads", () => {
    expect(eventParts(onset("ContainerCreating")).text).toBe("ContainerCreating");
  });

  it("a close leads with how long it held, not how many polls saw it", () => {
    // Duration is what an operator acts on; the observation count is an artifact
    // of the backend's own backoff schedule.
    expect(eventParts(close("ImagePullBackOff", 252_000, 8)).text).toBe(
      "ImagePullBackOff cleared after 4m12s",
    );
  });

  it("splits the step into a chip like other step-scoped events", () => {
    expect(eventParts(onset("Unschedulable")).step).toBe("build");
  });

  it("warns while it holds and goes quiet once it has cleared", () => {
    // A Pod that could not schedule for two minutes and then ran is a fact, not
    // an alarm — and neither is `err`: nothing has failed yet.
    expect(eventCategory(onset("Unschedulable"))).toBe("warning");
    expect(eventCategory(close("Unschedulable", 120_000, 4))).toBe("info");
  });
});

describe("RunDeadLettered", () => {
  const dl = (reason: string) => ev(9, { RunDeadLettered: { reason } });

  it("renders the reason instead of the bare tag", () => {
    // This is the one place the diagnosis is durably written. It used to fall
    // through to `default` and render as "RunDeadLettered", so the run that most
    // needed explaining explained nothing.
    expect(describeEvent(dl("step `build`: Unschedulable: 0/3 nodes are available"))).toBe(
      "Run dead-lettered — step `build`: Unschedulable: 0/3 nodes are available",
    );
  });

  it("is an error, not the quietest thing on the rail", () => {
    expect(eventCategory(dl("whatever"))).toBe("err");
  });
});
