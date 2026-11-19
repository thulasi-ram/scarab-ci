// Attempt-grain scoping + try mappers — the pure helpers behind
// StepPane / AttemptsDropdown: take-window filtering, scoped/of-record try
// resolution, and the per-try outcome/tone copy.
import { describe, it, expect } from "vitest";
import {
  windowAttemptList,
  scopedAttempt,
  stepNotRun,
  ofRecordIndexOf,
  stripTries,
  causeSuffix,
  causeText,
  tryTitle,
  tryOutcome,
  tryTone,
  type AttemptLike,
  type FilmstripTry,
} from "./attempts";
import { deriveTakes, replayTake } from "./takes";
import { twoTakeRun, started, finished, retryRequested } from "./event-fixtures";

const a = (id: string, outcome = "succeeded", failed = false): AttemptLike => ({
  id,
  outcome,
  failed,
});

describe("windowAttemptList", () => {
  const list = [a("a1"), a("a2"), a("a3")];

  it("a take window keeps exactly its attempt ids", () => {
    expect(windowAttemptList(list, ["a2"], null).map((x) => x.id)).toEqual(["a2"]);
  });

  it("an empty window means the step didn't run in this take", () => {
    expect(windowAttemptList(list, [], null)).toEqual([]);
  });

  it("without a window, the frontier ≤-filter is the fallback", () => {
    expect(windowAttemptList(list, null, "a2").map((x) => x.id)).toEqual(["a1", "a2"]);
  });

  it("no window and no frontier is unscoped", () => {
    expect(windowAttemptList(list, null, null)).toEqual(list);
  });

  it("a window takes precedence over the frontier", () => {
    expect(windowAttemptList(list, ["a3"], "a1").map((x) => x.id)).toEqual(["a3"]);
  });
});

describe("scopedAttempt", () => {
  const list = [a("a1"), a("a2"), a("a3")];

  it("resolves an explicit selection", () => {
    expect(scopedAttempt(list, "a1")?.id).toBe("a1");
  });

  it("falls back to the latest attempt when the wanted id is absent or null", () => {
    expect(scopedAttempt(list, "a9")?.id).toBe("a3");
    expect(scopedAttempt(list, null)?.id).toBe("a3");
  });

  it("is null when nothing ran", () => {
    expect(scopedAttempt([], "a1")).toBeNull();
  });
});

describe("stepNotRun", () => {
  it("a selected step with zero windowed tries didn't run in this version", () => {
    expect(stepNotRun({ id: "b" }, [])).toBe(true);
  });
  it("no selection is not 'not run'", () => {
    expect(stepNotRun(null, [])).toBe(false);
  });
  it("any windowed try means it ran", () => {
    expect(stepNotRun({ id: "b" }, [a("a1")])).toBe(false);
  });
});

describe("ofRecordIndexOf", () => {
  it("points at the of-record try within the windowed list", () => {
    expect(ofRecordIndexOf([a("a1", "succeeded"), a("a2", "failed", true)])).toBe(0);
    expect(ofRecordIndexOf([a("a1", "succeeded"), a("a2", "succeeded")])).toBe(1);
  });
  it("-1 when nothing is of record", () => {
    expect(ofRecordIndexOf([a("a1", "failed", true)])).toBe(-1);
    expect(ofRecordIndexOf([])).toBe(-1);
  });
});

describe("stripTries", () => {
  const events = twoTakeRun();
  const takes = deriveTakes(events);

  it("windows to the selected take and re-numbers tries per-take", () => {
    const latest = replayTake(events, takes, takes[1]);
    const bAttempts: AttemptLike[] = [
      a("a1", "failed", true),
      a("a2", "succeeded"),
      a("a3", "succeeded"),
    ];
    const tries = stripTries(events, "b", bAttempts, latest.windowAttempts["b"] ?? []);
    expect(tries).toHaveLength(1);
    expect(tries[0].id).toBe("a3");
    expect(tries[0].index).toBe(0); // numbered per-take, not globally
    expect(tries[0].cause?.kind).toBe("rerun");
  });

  it("an empty window renders no tries (carried-forward step)", () => {
    const latest = replayTake(events, takes, takes[1]);
    expect(stripTries(events, "a", [a("a1")], latest.windowAttempts["a"] ?? [])).toEqual([]);
  });

  it("unscoped: carries causes, superseded and shadowed flags off the event log", () => {
    const cTries = stripTries(events, "c", [a("a1", "superseded"), a("a2", "running")], null);
    expect(cTries.map((t) => t.cause?.kind)).toEqual(["initial", "cascade"]);
    expect(cTries[0].superseded).toBe(true); // cut short by the rerun of b
    expect(cTries[1].superseded).toBe(false);

    const bTries = stripTries(
      events,
      "b",
      [a("a1", "failed", true), a("a2", "succeeded"), a("a3", "succeeded")],
      null,
    );
    expect(bTries.map((t) => t.shadowed)).toEqual([false, true, false]); // a2 shadowed by a3
    expect(bTries.map((t) => t.outcome)).toEqual(["failed", "succeeded", "succeeded"]);
  });
});

describe("try mappers (AttemptsDropdown copy)", () => {
  const t = (o: Partial<FilmstripTry>): FilmstripTry => ({
    id: "a1",
    index: 0,
    outcome: "",
    failed: false,
    superseded: false,
    shadowed: false,
    readopted: false,
    ...o,
  });

  // Regression: a human Retry read "auto-retry", and any rerun read "you
  // reran" whoever pressed it.
  it("causeText says who or what started the try, in plain words", () => {
    expect(causeText({ kind: "initial" })).toBe("first run");
    expect(causeText({ kind: "auto-retry" })).toBe("automatic retry");
    expect(causeText({ kind: "manual-retry", by: "a.kim" })).toBe("retried by a.kim");
    expect(causeText({ kind: "manual-retry", by: null })).toBe("manual retry");
    expect(causeText({ kind: "rerun", by: "a.kim" })).toBe("rerun by a.kim");
    expect(causeText({ kind: "rerun", by: null })).toBe("rerun");
    expect(causeText({ kind: "cascade", by: "a.kim", upstream: "build", via: "rerun" })).toBe(
      "rerun of build",
    );
    expect(causeText({ kind: "cascade", by: null, upstream: "build", via: "retry" })).toBe(
      "retry of build",
    );
    expect(causeText(undefined)).toBe("");
  });

  it("causeSuffix prefixes the separator, and is empty for an unknown cause", () => {
    expect(causeSuffix({ kind: "auto-retry" })).toBe(" · automatic retry");
    expect(causeSuffix(undefined)).toBe("");
  });

  it("tryTitle is `try N` + cause", () => {
    expect(tryTitle(t({ index: 2, cause: { kind: "rerun", by: "a.kim" } }))).toBe(
      "try 3 · rerun by a.kim",
    );
    expect(tryTitle(t({ index: 0, cause: { kind: "initial" } }))).toBe("try 1 · first run");
    expect(tryTitle(t({ index: 0 }))).toBe("try 1");
  });

  it("stripTries carries the manual retry's actor through to the title", () => {
    const evs = [
      started(1000, "b", "a1"),
      finished(2000, "b", "a1", "step"),
      retryRequested(3000, "b", [], "a.kim"),
      started(4000, "b", "a2"),
    ];
    const tries = stripTries(evs, "b", [a("a1", "failed", true), a("a2", "running")], null);
    expect(tries.map(tryTitle)).toEqual(["try 1 · first run", "try 2 · retried by a.kim"]);
  });

  it("tryOutcome prefers the backend's authoritative outcome", () => {
    expect(tryOutcome(t({ outcome: "running" }))).toBe("● running");
    expect(tryOutcome(t({ outcome: "succeeded" }))).toBe("✓ succeeded");
    expect(tryOutcome(t({ outcome: "failed", failure: "step" }))).toBe("✗ failed · step");
    expect(tryOutcome(t({ outcome: "failed" }))).toBe("✗ failed");
    expect(tryOutcome(t({ outcome: "superseded" }))).toBe("⊘ superseded");
    expect(tryOutcome(t({ outcome: "cancelled" }))).toBe("⊘ cancelled");
  });

  it("tryOutcome shows an unknown failure class (a newer server's) as its token", () => {
    expect(tryOutcome(t({ outcome: "failed", failure: "brand-new-class" }))).toBe(
      "✗ failed · brand-new-class",
    );
  });

  it("tryOutcome falls back to derived flags on old servers (no outcome)", () => {
    expect(tryOutcome(t({ superseded: true }))).toBe("⊘ superseded");
    expect(tryOutcome(t({ failed: true, failure: "infra" }))).toBe("✗ failed · infra");
    expect(tryOutcome(t({}))).toBe("✓ succeeded");
  });

  it("tryTone maps outcome → accent, with the same fallback", () => {
    expect(tryTone(t({ outcome: "running" }))).toBe("running");
    expect(tryTone(t({ outcome: "succeeded" }))).toBe("emerald");
    expect(tryTone(t({ outcome: "failed" }))).toBe("danger");
    expect(tryTone(t({ outcome: "superseded" }))).toBe("copper");
    expect(tryTone(t({ outcome: "cancelled" }))).toBe("copper");
    expect(tryTone(t({ superseded: true }))).toBe("copper");
    expect(tryTone(t({ failed: true }))).toBe("danger");
    expect(tryTone(t({}))).toBe("emerald");
  });
});
