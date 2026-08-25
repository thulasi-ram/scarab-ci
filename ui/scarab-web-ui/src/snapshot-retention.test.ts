import { describe, it, expect } from "vitest";
import {
  isWidened,
  nameList,
  pinLabel,
  pinTitle,
  rerunLabel,
  rerunTitle,
  retentionNote,
  widenedNote,
  type RerunPlan,
  type RetentionInfo,
} from "./snapshot-retention";

const plan = (over: Partial<RerunPlan> = {}): RerunPlan => ({
  target: "test",
  invalidated: ["test"],
  widened: [],
  starts_from: ["test"],
  ...over,
});

/** The interesting case: a run reopened after its snapshots expired, so the
 * rerun of `test` has to walk back to `clone`. */
const widenedPlan = plan({
  invalidated: ["build", "clone", "test"],
  widened: ["build", "clone"],
  starts_from: ["clone"],
  expired_inputs: [{ consumer: "test", produced_by: "build", root: "abc" }],
});

const retention = (over: Partial<RetentionInfo> = {}): RetentionInfo => ({
  retention_days: 14,
  expires_at: 1_000_000,
  expired: false,
  pinned: false,
  ...over,
});

describe("nameList", () => {
  it("reads as prose, not as an array", () => {
    expect(nameList([])).toBe("");
    expect(nameList(["clone"])).toBe("clone");
    expect(nameList(["clone", "build"])).toBe("clone and build");
    expect(nameList(["a", "b", "c"])).toBe("a, b and c");
  });
});

describe("rerunLabel", () => {
  it("is the plain label when nothing widened", () => {
    expect(rerunLabel(plan())).toBe("Rerun pipeline from this step");
  });

  it("is the plain label while the preview has not loaded", () => {
    // A null plan must never render as "inputs expired" — an unknown answer is
    // not a scary answer.
    expect(rerunLabel(null)).toBe("Rerun pipeline from this step");
    expect(rerunLabel(undefined)).toBe("Rerun pipeline from this step");
  });

  it("says which it is, and names where it restarts from", () => {
    // ADR-0061 s5's literal requirement, and ADR-0027's "smart never means
    // mysterious": the button may not claim to rerun one step when it will
    // re-run the pipeline from clone.
    expect(rerunLabel(widenedPlan)).toBe("Inputs expired — this re-runs from clone");
  });

  it("names every starting point when the DAG forks", () => {
    expect(
      rerunLabel(
        plan({
          widened: ["fetch-a", "fetch-b"],
          starts_from: ["fetch-a", "fetch-b"],
          invalidated: ["fetch-a", "fetch-b", "test"],
        }),
      ),
    ).toBe("Inputs expired — this re-runs from fetch-a and fetch-b");
  });
});

describe("isWidened / widenedNote", () => {
  it("is false for an ordinary rerun and for no plan", () => {
    expect(isWidened(plan())).toBe(false);
    expect(isWidened(null)).toBe(false);
    expect(widenedNote(plan())).toBeNull();
  });

  it("states the widened scope without needing a hover", () => {
    // A scope change discovered only on hover is still a surprise.
    expect(widenedNote(widenedPlan)).toBe(
      "build and clone will re-run too — the workspace snapshots test needs have expired and must be regenerated.",
    );
  });
});

describe("rerunTitle", () => {
  it("keeps the ordinary explanation when nothing widened", () => {
    expect(rerunTitle(plan(), "test")).toBe(
      "rerun test and everything downstream — forks a new version",
    );
  });

  it("explains the widening, with the window as the reason", () => {
    const t = rerunTitle(widenedPlan, "test", 14);
    expect(t).toContain("14-day retention window");
    expect(t).toContain("re-runs clone first to regenerate them");
    expect(t).toContain("3 steps in a new version");
    // Honest about the cost and about what is NOT lost.
    expect(t).toContain("Nothing is lost");
  });

  it("omits the window when the run detail did not supply one", () => {
    expect(rerunTitle(widenedPlan, "test")).not.toContain("retention window");
  });
});

describe("retentionNote", () => {
  it("states the window for a settled run", () => {
    expect(retentionNote(retention(), true)).toBe(
      "Workspace snapshots kept for 14 days after this run finished.",
    );
  });

  it("does not promise an expiry for a run that has not finished", () => {
    // ADR-0050: a non-terminal run — including one suspended on a gate for
    // weeks — is never GC-eligible regardless of age.
    expect(retentionNote(retention({ expires_at: null }), false)).toBe(
      "Workspace snapshots are kept while this run is unfinished; the 14-day window starts when it finishes.",
    );
  });

  it("says EXPIRED, never DELETED", () => {
    // The window lapsing ends a promise; GC is periodic and content is shared,
    // so the bytes often outlive it. Claiming deletion would be a lie the
    // sweeper does not tell.
    const note = retentionNote(retention({ expired: true }), true);
    expect(note).toContain("expired");
    expect(note).not.toMatch(/delet/i);
  });

  it("reports a pin with its attribution and drops the expiry", () => {
    expect(retentionNote(retention({ pinned: true, pinned_by: "alice" }), true)).toBe(
      "Workspace snapshots pinned by alice — kept past the 14-day window until unpinned.",
    );
  });
});

describe("pin affordance", () => {
  it("toggles its label with the state", () => {
    expect(pinLabel(retention())).toBe("Keep snapshots");
    expect(pinLabel(retention({ pinned: true }))).toBe("Unpin snapshots");
  });

  it("is honest that a pin cannot pin the warm cache", () => {
    // The two tiers are separated precisely so only the time-bounded one carries
    // a promise. A pin that implied it also held a size-bounded LRU cache would
    // be over-claiming.
    const t = pinTitle(retention());
    expect(t).toContain("archive");
    expect(t).toContain("size-bounded");
    expect(t).toContain("slower, never wrong");
  });

  it("says what unpinning costs", () => {
    expect(pinTitle(retention({ pinned: true }))).toContain("collectable");
  });
});
