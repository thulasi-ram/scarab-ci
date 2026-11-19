// The run tabs' pure helpers: what the Activity filter shows and hides, which
// artifact copies show and how they group, the try tabs' folding, and the
// tries the List view nests under a step.
import { describe, expect, it } from "vitest";
import {
  activityClass,
  activityHiddenNote,
  activityView,
  artifactFilterCounts,
  artifactGroups,
  artifactsInScope,
  copyMark,
  foldTries,
  isTriggerEvent,
  nestedTries,
  pluralNoun,
  resolutionWords,
  shownArtifacts,
  triggerLine,
  tryDurationMs,
  tryWords,
} from "./run-tabs";
import { activeInfraConditions } from "./events";
import { ev, finished, infraCondition, readoptedEv, started, transitioned } from "./event-fixtures";

describe("activity filter", () => {
  // build's first try waited for a node and the condition cleared; its second
  // try is waiting for a node right now. test had a workspace collision.
  const log = [
    ev(0, { Raw: { trigger: { event: { kind: "push", sha: "abc" } } } }),
    started(1, "build", "a1"),
    infraCondition(2, "build", "a1", "Unschedulable"),
    infraCondition(3, "build", "a1", "Unschedulable", 1000),
    finished(4, "build", "a1", "step"),
    started(5, "build", "a2"),
    infraCondition(6, "build", "a2", "Unschedulable"),
    ev(7, { WorkspaceInputCollisions: { step: "test", count: 2, sample: [] } }),
    readoptedEv(8, "build", "a2"),
    transitioned(9, "test", "running", "succeeded"),
  ];
  const active = activeInfraConditions(log);

  it("classifies by event kind: an open condition is active, the rest of the conditions cleared", () => {
    expect(log.map((e) => activityClass(e, active))).toEqual([
      "step",
      "step",
      "cleared",
      "cleared",
      "step",
      "step",
      "active",
      // A collision is a warning but not a Pod condition: it stays a step event.
      "step",
      "infra",
      "step",
    ]);
  });

  it("an onset whose try was replaced is cleared even without a closing event", () => {
    const evs = [started(1, "b", "a1"), infraCondition(2, "b", "a1", "Unschedulable"), started(3, "b", "a2")];
    expect(activityClass(evs[1], activeInfraConditions(evs))).toBe("cleared");
  });

  it("the default hides the cleared conditions only and says how many", () => {
    const v = activityView(log, active, "steps");
    expect(v.rows).toHaveLength(8);
    expect(v.rows.some((r) => r.cls === "cleared")).toBe(false);
    expect(v.rows.some((r) => r.cls === "active")).toBe(true);
    expect(v.hiddenCleared).toBe(2);
  });

  it("rows are newest first", () => {
    const v = activityView(log, active, "all");
    expect(v.rows.map((r) => r.event.at)).toEqual([9, 8, 7, 6, 5, 4, 3, 2, 1, 0]);
  });

  it("Infrastructure shows conditions and re-adoptions; Everything shows all", () => {
    const infra = activityView(log, active, "infra");
    expect(infra.rows.map((r) => r.event.at)).toEqual([8, 6, 3, 2]);
    expect(infra.hiddenCleared).toBe(0);
    expect(infra.counts).toEqual({ steps: 8, infra: 4, all: 10 });
    expect(activityView(log, active, "all").hiddenCleared).toBe(0);
  });

  it("with nothing active (an older version) every condition is cleared", () => {
    const v = activityView(log, {}, "steps");
    expect(v.hiddenCleared).toBe(3);
  });

  it("the hidden note composes the cleared count with the later-events count", () => {
    expect(activityHiddenNote(36, 0)).toEqual({ cleared: "36 cleared Pod condition events hidden", later: null });
    expect(activityHiddenNote(1, 4)).toEqual({
      cleared: "1 cleared Pod condition event hidden",
      later: "4 later events hidden by this version",
    });
    expect(activityHiddenNote(0, 0)).toEqual({ cleared: null, later: null });
  });
});

describe("trigger line", () => {
  it("finds the trigger among Raw events", () => {
    expect(isTriggerEvent(ev(0, { Raw: { trigger: { event: { kind: "push" } } } }))).toBe(true);
    expect(isTriggerEvent(ev(0, { Raw: { note: "path filter not applied" } }))).toBe(false);
    expect(isTriggerEvent(ev(0, "RunCreated"))).toBe(false);
  });

  it("reads as the trigger, branch, commit and actor", () => {
    expect(
      triggerLine({ words: "PR #2949", branch: "feature/x", sha: "15f2297b9c0d1e2f", actor: "thulasi" }),
    ).toBe("Triggered by PR #2949 · feature/x · 15f2297b · thulasi");
    expect(triggerLine({ words: "push", branch: "abc", sha: "abc" })).toBe("Triggered by push · abc");
    expect(triggerLine({})).toBe("Triggered by an event");
  });
});

describe("artifacts", () => {
  const art = (name: string, step: string, attempt: string, of_record: boolean) => ({
    name,
    step,
    attempt,
    of_record,
  });
  const rows = [
    art("junit.xml", "test", "a1", false),
    art("junit.xml", "test", "a3", true),
    art("dist.tgz", "build", "a1", true),
    // Every try that wrote report.html failed: no copy is of record.
    art("report.html", "e2e", "a1", false),
    art("report.html", "e2e", "a2", false),
  ];

  it("Of record keeps the copy of record, or the newest copy of a file with none", () => {
    expect(shownArtifacts(rows, "record")).toEqual([rows[1], rows[2], rows[4]]);
    expect(shownArtifacts(rows, "all")).toBe(rows);
    expect(artifactFilterCounts(rows)).toEqual({ record: 3, all: 5 });
  });

  it("groups by the publishing step in run order, unknown steps last", () => {
    const g = artifactGroups([...rows, art("x", "gone", "a1", true)], ["build", "test", "e2e"]);
    expect(g.map((x) => x.step)).toEqual(["build", "test", "e2e", "gone"]);
    expect(g[1].rows).toEqual([rows[0], rows[1]]);
  });

  it("keeps one step's group when filtered to it", () => {
    expect(artifactGroups(rows, ["build", "test", "e2e"], "test")).toEqual([{ step: "test", rows: [rows[0], rows[1]] }]);
    expect(artifactGroups(rows, ["build"], "nothing")).toEqual([]);
  });

  // Regression: narrowed to a step, the tab showed nothing (and the step
  // header counted a file the tab did not list) when another step's file of
  // the same name was the one of record.
  it("narrows to a step before picking the copy of record", () => {
    const two = [art("report.xml", "unit", "a1", false), art("report.xml", "e2e", "a1", true)];
    const unit = artifactsInScope(two, "unit");
    expect(shownArtifacts(unit, "record")).toEqual([two[0]]);
    expect(artifactFilterCounts(unit)).toEqual({ record: 1, all: 1 });
    expect(artifactFilterCounts(artifactsInScope(two, "e2e"))).toEqual({ record: 1, all: 1 });
    expect(artifactsInScope(two, null)).toBe(two);
  });

  it("says why a copy is not of record", () => {
    expect(copyMark({ succeeded: true })).toEqual({ text: "earlier copy", tone: "earlier" });
    expect(copyMark({ succeeded: false }, "failed")).toEqual({ text: "from a failed try", tone: "failed" });
    expect(copyMark({ succeeded: false }, "superseded").text).toBe("from a try that did not finish");
    expect(copyMark({ succeeded: false }, "cancelled").text).toBe("from a try that did not finish");
    expect(copyMark({ succeeded: false }).text).toBe("from a failed try");
  });
});

describe("try tabs", () => {
  const tries = (n: number) => Array.from({ length: n }, (_, i) => ({ id: `a${i + 1}` }));

  it("shows every try newest first up to four", () => {
    const f = foldTries(tries(3), "a1");
    expect(f.shown.map((t) => t.id)).toEqual(["a3", "a2", "a1"]);
    expect(f.earlier).toEqual([]);
    expect(f.selectedEarlier).toBe(false);
  });

  it("folds the older tries into the menu past four", () => {
    const f = foldTries(tries(6), "a6");
    expect(f.shown.map((t) => t.id)).toEqual(["a6", "a5", "a4", "a3"]);
    expect(f.earlier.map((t) => t.id)).toEqual(["a2", "a1"]);
    expect(f.selectedEarlier).toBe(false);
    expect(foldTries(tries(6), "a2").selectedEarlier).toBe(true);
    expect(foldTries(tries(6), null).selectedEarlier).toBe(false);
  });

  it("names a try's outcome", () => {
    expect(tryWords(3, { outcome: "succeeded" })).toBe("Try 3 · passed");
    expect(tryWords(2, { outcome: "failed", failure: "timeout" })).toBe("Try 2 · failed: timed out");
    expect(tryWords(4, { outcome: "running" })).toBe("Try 4 · running");
    expect(tryWords(1, { outcome: "failed", failure: "gate" }, "Wait")).toBe("Wait 1 · failed: gate failed");
  });

  it("says how a gate's wait was resolved, for the wait the resolution closed", () => {
    const wait = { id: "a2", outcome: "failed", failure: "gate" };
    const rejected = { passed: false, reason: "not today", by: "a.kim", attempt: "a2" };
    expect(tryWords(2, wait, "Wait", rejected)).toBe("Wait 2 · rejected by a.kim: not today");
    expect(tryWords(2, { id: "a2", outcome: "succeeded" }, "Wait", { passed: true, by: "a.kim", attempt: "a2" })).toBe(
      "Wait 2 · approved by a.kim",
    );
    expect(tryWords(2, wait, "Wait", { passed: false, reason: "expired after 1800s", attempt: "a2" })).toBe(
      "Wait 2 · expired",
    );
    // Another wait's resolution, or one that names no one, leaves the outcome.
    expect(tryWords(1, { id: "a1", outcome: "failed", failure: "gate" }, "Wait", rejected)).toBe(
      "Wait 1 · failed: gate failed",
    );
    expect(resolutionWords({ passed: true, reason: "timer elapsed", attempt: "a1" })).toBeNull();
  });

  it("names the plural of a try or a wait", () => {
    expect(pluralNoun("Try")).toBe("Tries");
    expect(pluralNoun("Wait")).toBe("Waits");
  });

  it("measures a try from its start and finish, or to now while it runs", () => {
    expect(tryDurationMs({ outcome: "failed", startedAt: 1000, finishedAt: 4000 }, 9000)).toBe(3000);
    expect(tryDurationMs({ outcome: "running", startedAt: 1000, finishedAt: null }, 9000)).toBe(8000);
    expect(tryDurationMs({ outcome: "superseded", startedAt: 1000, finishedAt: null }, 9000)).toBeNull();
    expect(tryDurationMs({ outcome: "running" }, 9000)).toBeNull();
  });
});

describe("nested tries in the List view", () => {
  it("lists tries newest first, only for a step with more than one", () => {
    expect(nestedTries(["a1"])).toEqual([]);
    expect(nestedTries(["a1", "a2", "a3"])).toEqual(["a3", "a2", "a1"]);
  });
});
