// Takes (ADR-0056) — the run-level version lens, derived entirely client-side
// from the event log. This seam is where the real bugs lived (take-scoped
// duration, take-scoped live attempts, per-try outcome mapping), so it gets
// the densest coverage of the no-DOM tier.
import { describe, it, expect } from "vitest";
import {
  deriveTakes,
  replayTake,
  attemptCauses,
  ofRecordAttemptId,
  attemptN,
  stepTiming,
  visibleArtifacts,
  bucketOf,
  tally,
  versionRows,
} from "./takes";
import type { Artifact } from "./api/client";
import {
  started,
  finished,
  transitioned,
  rerun,
  retryRequested,
  readoptedEv,
  twoTakeRun,
  TWO_TAKE_BOUNDARY_IDX,
} from "./event-fixtures";

describe("deriveTakes", () => {
  it("an empty event log is one open take", () => {
    const takes = deriveTakes([]);
    expect(takes).toHaveLength(1);
    expect(takes[0]).toEqual({
      n: 1,
      endIdx: 0,
      closedByTarget: null,
      closedBy: null,
      closedAt: null,
    });
  });

  it("a log with no rerun is one open take spanning everything", () => {
    const events = twoTakeRun().slice(0, TWO_TAKE_BOUNDARY_IDX); // drop the rerun + after
    const takes = deriveTakes(events);
    expect(takes).toHaveLength(1);
    expect(takes[0].n).toBe(1);
    expect(takes[0].endIdx).toBe(events.length);
    expect(takes[0].closedByTarget).toBeNull();
  });

  it("one rerun splits the log into a closed take + the open latest", () => {
    const events = twoTakeRun();
    const takes = deriveTakes(events);
    expect(takes).toHaveLength(2);
    // Take 1 ends AT the boundary event and records who/what/when closed it.
    expect(takes[0].endIdx).toBe(TWO_TAKE_BOUNDARY_IDX);
    expect(takes[0].closedByTarget).toBe("b");
    expect(takes[0].closedBy).toBe("a.kim");
    expect(takes[0].closedAt).toBe(8000);
    // The latest take is open: spans to the end, no closing boundary.
    expect(takes[1].n).toBe(2);
    expect(takes[1].endIdx).toBe(events.length);
    expect(takes[1].closedByTarget).toBeNull();
    expect(takes[1].closedAt).toBeNull();
  });

  it("multiple reruns yield consecutively numbered takes at each boundary", () => {
    const events = [
      ...twoTakeRun(),
      finished(12000, "c", "a2"),
      rerun(13000, "a", ["b", "c"], "j.lee"),
      started(14000, "a", "a2"),
    ];
    const takes = deriveTakes(events);
    expect(takes.map((t) => t.n)).toEqual([1, 2, 3]);
    expect(takes[0].closedByTarget).toBe("b");
    expect(takes[1].closedByTarget).toBe("a");
    expect(takes[1].closedBy).toBe("j.lee");
    expect(takes[2].closedByTarget).toBeNull();
  });

  it("a rerun pressed while a step is mid-flight still closes the take at that instant", () => {
    // c@a1 started at 7000 and never finished before the 8000 boundary.
    const takes = deriveTakes(twoTakeRun());
    expect(takes[0].closedAt).toBe(8000);
    expect(takes[0].endIdx).toBe(TWO_TAKE_BOUNDARY_IDX);
  });
});

describe("replayTake", () => {
  const events = twoTakeRun();
  const takes = deriveTakes(events);

  it("closed take: re-armed in-flight attempt shows superseded; carried verdicts survive", () => {
    const view = replayTake(events, takes, takes[0]);
    expect(view.status["a"]).toBe("succeeded");
    // b was re-armed by the rerun but HAD a verdict in this take → keeps it.
    expect(view.status["b"]).toBe("succeeded");
    // c was in flight when Rerun was pressed and was re-armed → cut short.
    expect(view.status["c"]).toBe("superseded");
  });

  it("closed take: frontier/attempts/windowAttempts cover the whole first window", () => {
    const view = replayTake(events, takes, takes[0]);
    expect(view.frontier).toEqual({ a: "a1", b: "a2", c: "a1" });
    expect(view.attempts).toEqual({ a: 1, b: 2, c: 1 });
    expect(view.windowAttempts).toEqual({ a: ["a1"], b: ["a1", "a2"], c: ["a1"] });
  });

  it("latest take: window opens at the boundary — only THIS take's attempts count", () => {
    const view = replayTake(events, takes, takes[1]);
    // a was carried forward untouched: verdict kept, but ZERO attempts here.
    expect(view.status["a"]).toBe("succeeded");
    expect(view.attempts["a"]).toBeUndefined();
    expect(view.windowAttempts["a"]).toBeUndefined();
    // b and c re-ran: per-take attempt sets, not summed across takes.
    expect(view.windowAttempts["b"]).toEqual(["a3"]);
    expect(view.windowAttempts["c"]).toEqual(["a2"]);
    expect(view.attempts).toMatchObject({ b: 1, c: 1 });
    expect(view.frontier).toEqual({ b: "a3", c: "a2" });
    expect(view.status["c"]).toBe("running");
  });

  it("closed take: a re-armed step that never ran (and carries no verdict) is not_run", () => {
    const evs = [
      started(1000, "a", "a1"),
      finished(2000, "a", "a1"),
      transitioned(2100, "a", "running", "succeeded"),
      rerun(3000, "d", ["e"], "a.kim"),
      started(4000, "d", "a1"),
    ];
    const ts = deriveTakes(evs);
    const view = replayTake(evs, ts, ts[0]);
    expect(view.status["d"]).toBe("not_run");
    expect(view.status["e"]).toBe("not_run");
    expect(view.status["a"]).toBe("succeeded");
  });

  it("closed take: an in-flight step NOT re-armed genuinely straddles — stays running", () => {
    const evs = [
      started(1000, "a", "a1"), // never finishes, not re-armed
      started(2000, "b", "a1"),
      finished(3000, "b", "a1"),
      transitioned(3100, "b", "running", "succeeded"),
      rerun(4000, "b", [], "a.kim"),
      started(5000, "b", "a2"),
    ];
    const ts = deriveTakes(evs);
    const view = replayTake(evs, ts, ts[0]);
    expect(view.status["a"]).toBe("running");
    expect(view.status["b"]).toBe("succeeded"); // re-armed but had a verdict
  });

  it("pre-amendment boundary (no invalidated array) skips the re-arm classification", () => {
    const evs = [
      started(1000, "a", "a1"), // in flight at the boundary
      { version: 1, run: "run-1", at: 2000, kind: { RunRerunRequested: { target: "a" } } },
      started(3000, "a", "a2"),
    ];
    const ts = deriveTakes(evs);
    const view = replayTake(evs, ts, ts[0]);
    // No superseded/not_run rewrite on old logs — today's honest "running".
    expect(view.status["a"]).toBe("running");
  });
});

describe("attemptCauses", () => {
  const events = twoTakeRun();

  it("classifies initial / auto-retry / rerun for the rerun target", () => {
    const { causes } = attemptCauses(events, "b");
    expect(causes).toEqual({ a1: "initial", a2: "retry", a3: "rerun" });
  });

  it("classifies the dragged-along descendant as cascade and supersedes its cut-short try", () => {
    const { causes, superseded } = attemptCauses(events, "c");
    expect(causes).toEqual({ a1: "initial", a2: "cascade" });
    expect(superseded).toEqual(new Set(["a1"]));
  });

  it("shadows earlier successes once a newer success is of record", () => {
    const { shadowed, superseded } = attemptCauses(events, "b");
    // b succeeded at a2 and again at a3 → a2 is shadowed; nothing superseded
    // (both b attempts had finished before the rerun).
    expect(shadowed).toEqual(new Set(["a2"]));
    expect(superseded).toEqual(new Set());
  });

  it("StepRetryRequested arms a retry cause (same take) and supersedes an in-flight try", () => {
    const evs = [
      started(1000, "b", "a1"), // in flight when the human retry lands
      retryRequested(2000, "b", []),
      started(3000, "b", "a2"),
    ];
    const { causes, superseded } = attemptCauses(evs, "b");
    expect(causes).toEqual({ a1: "initial", a2: "retry" });
    expect(superseded).toEqual(new Set(["a1"]));
  });

  it("marks re-adopted attempts (crash recovery is a marker, not a new try)", () => {
    const evs = [
      started(1000, "a", "a1"),
      readoptedEv(2000, "a", "a1"),
      finished(3000, "a", "a1"),
    ];
    const { causes, readopted } = attemptCauses(evs, "a");
    expect(readopted).toEqual(new Set(["a1"]));
    expect(causes).toEqual({ a1: "initial" });
  });
});

describe("ofRecordAttemptId", () => {
  const a = (id: string, outcome: string, failed = false) => ({ id, outcome, failed });

  it("the LAST succeeded attempt wins", () => {
    expect(ofRecordAttemptId([a("a1", "succeeded"), a("a2", "succeeded")])).toBe("a2");
    // A later failure does not displace an earlier success.
    expect(ofRecordAttemptId([a("a1", "succeeded"), a("a2", "failed", true)])).toBe("a1");
  });

  it("falls back to the last attempt with a real verdict when nothing succeeded", () => {
    // Old-server shape: outcome absent-ish, failed=false → still of record.
    expect(ofRecordAttemptId([a("a1", ""), a("a2", "superseded")])).toBe("a1");
  });

  it("failed / superseded / running / cancelled are never of record", () => {
    expect(
      ofRecordAttemptId([
        a("a1", "failed", true),
        a("a2", "superseded"),
        a("a3", "running"),
        a("a4", "cancelled"),
      ]),
    ).toBeNull();
  });

  it("an empty list has nothing of record", () => {
    expect(ofRecordAttemptId([])).toBeNull();
  });
});

describe("attemptN", () => {
  it("parses the numeric part of an attempt id, 0 when malformed", () => {
    expect(attemptN("a3")).toBe(3);
    expect(attemptN("a12")).toBe(12);
    expect(attemptN("bogus")).toBe(0);
  });
});

describe("stepTiming", () => {
  const events = twoTakeRun();

  it("unwindowed: first AttemptStarted → last AttemptFinished per step", () => {
    const m = stepTiming(events, null);
    expect(m["a"]).toEqual({ start: 1000, end: 2000 });
    expect(m["b"]).toEqual({ start: 3000, end: 10000 }); // spans all three tries
    expect(m["c"]).toEqual({ start: 7000 }); // still running — no end
  });

  it("take-windowed: only THIS take's attempts count (rerun must not grow durations)", () => {
    const takes = deriveTakes(events);
    const view = replayTake(events, takes, takes[1]);
    const m = stepTiming(events, view.windowAttempts);
    expect(m["b"]).toEqual({ start: 9000, end: 10000 }); // a3 only
    expect(m["c"]).toEqual({ start: 11000 }); // a2 only
    // a has no window entry (carried forward) → unfiltered, keeps its timing.
    expect(m["a"]).toEqual({ start: 1000, end: 2000 });
  });
});

describe("visibleArtifacts", () => {
  const art = (o: Partial<Artifact> & { name: string }): Artifact => ({
    attempt: "a1",
    content_type: "application/octet-stream",
    of_record: false,
    size: 1,
    step: "b",
    succeeded: true,
    ...o,
  });

  it("latest view (no take view) passes artifacts through untouched", () => {
    const all = [art({ name: "bin", of_record: true })];
    expect(visibleArtifacts(all, null)).toBe(all);
  });

  it("time-travel: drops versions from attempts beyond the boundary frontier", () => {
    const events = twoTakeRun();
    const takes = deriveTakes(events);
    const tv = replayTake(events, takes, takes[0]); // frontier b→a2
    const rows = visibleArtifacts(
      [
        art({ name: "bin", attempt: "a2" }),
        art({ name: "bin", attempt: "a3", of_record: true }), // from take 2
      ],
      tv,
    );
    expect(rows.map((r) => r.attempt)).toEqual(["a2"]);
    // …and of-record is recomputed within the horizon (server flag was on a3).
    expect(rows[0].of_record).toBe(true);
  });

  it("time-travel: a step with no frontier in this take contributes nothing; provenance-less rows survive", () => {
    const events = twoTakeRun();
    const takes = deriveTakes(events);
    const tv = replayTake(events, takes, takes[0]);
    const rows = visibleArtifacts(
      [
        art({ name: "x.tar", step: "zz", attempt: "a1" }), // step never ran
        art({ name: "old.tar", step: "" }), // pre-ADR-0056: no provenance
      ],
      tv,
    );
    expect(rows.map((r) => r.name)).toEqual(["old.tar"]);
  });

  it("of-record within the horizon is the LAST succeeded version of each name", () => {
    const events = twoTakeRun();
    const takes = deriveTakes(events);
    const tv = replayTake(events, takes, takes[0]);
    const rows = visibleArtifacts(
      [
        art({ name: "bin", attempt: "a1", succeeded: true }),
        art({ name: "bin", attempt: "a2", succeeded: false }),
      ],
      tv,
    );
    expect(rows.map((r) => r.of_record)).toEqual([true, false]);
  });
});

describe("bucketOf / tally", () => {
  it("buckets the five named statuses and folds the rest into other", () => {
    expect(bucketOf("succeeded")).toBe("succeeded");
    expect(bucketOf("failed")).toBe("failed");
    expect(bucketOf("superseded")).toBe("superseded");
    expect(bucketOf("not_run")).toBe("notRun");
    expect(bucketOf("running")).toBe("running");
    expect(bucketOf("pending")).toBe("other");
    expect(bucketOf("skipped")).toBe("other");
  });

  it("tallies a status list into per-bucket counts", () => {
    expect(
      tally(["succeeded", "succeeded", "failed", "not_run", "running", "pending", "skipped"]),
    ).toEqual({ succeeded: 2, failed: 1, superseded: 0, notRun: 1, running: 1, other: 2 });
  });
});

describe("versionRows", () => {
  const events = twoTakeRun();
  const takes = deriveTakes(events);
  const liveStatuses = ["succeeded", "succeeded", "running"]; // a, b, c live

  it("one row per take: closed takes replay their snapshot, the latest tallies live statuses", () => {
    const rows = versionRows(events, takes, liveStatuses, null, 1000);
    expect(rows).toHaveLength(2);
    expect(rows[0].label).toBe("original run");
    expect(rows[0].isLatest).toBe(false);
    // Take 1 snapshot: a succeeded, b succeeded, c superseded.
    expect(rows[0].summary).toMatchObject({ succeeded: 2, superseded: 1, running: 0 });
    expect(rows[1].label).toBe("latest");
    expect(rows[1].isLatest).toBe(true);
    expect(rows[1].summary).toMatchObject({ succeeded: 2, running: 1 });
  });

  it("the latest row's sub names what opened it and who pressed it", () => {
    const rows = versionRows(events, takes, liveStatuses, null, 1000);
    expect(rows[1].sub).toContain("you reran b");
    expect(rows[1].sub).toContain("by a.kim");
  });

  it("selection: null selects the latest; a take number selects that row", () => {
    expect(versionRows(events, takes, liveStatuses, null, 1000).map((r) => r.isSelected)).toEqual(
      [false, true],
    );
    expect(versionRows(events, takes, liveStatuses, 1, 1000).map((r) => r.isSelected)).toEqual([
      true,
      false,
    ]);
  });
});
