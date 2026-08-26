// The confirm popover's copy derivations (git-bug 4afaa3e) — pure functions,
// the no-DOM vitest tier. The popover component only lays these out.
import { describe, it, expect } from "vitest";
import {
  confirmFootnote,
  confirmHeadline,
  confirmLabel,
  confirmSentence,
  confirmWarning,
  gateNote,
  planDivergence,
  planGroups,
  useExpandedList,
  LIST_THRESHOLD,
} from "./rerun-confirm";
import { type RerunPlan } from "./snapshot-retention";

const plan = (over: Partial<RerunPlan> = {}): RerunPlan => ({
  target: "push",
  invalidated: ["push"],
  widened: [],
  starts_from: ["push"],
  expired_inputs: [],
  steps: [{ step: "push", reason: "target", is_gate: false }],
  ...over,
});

const cascadePlan = plan({
  invalidated: ["deploy-staging", "push"],
  steps: [
    { step: "push", reason: "target", is_gate: false },
    { step: "deploy-staging", reason: "cascade", because_of: "push", is_gate: false },
  ],
});

/** A run reopened past retention: rerunning `push` re-runs from `clone`. */
const widenedPlan = plan({
  invalidated: ["build", "clone", "deploy-staging", "push", "smoke-test"],
  widened: ["build", "clone"],
  starts_from: ["clone"],
  expired_inputs: [{ consumer: "push", produced_by: "build", root: "abc" }],
  steps: [
    { step: "clone", reason: "regenerate", because_of: "build", is_gate: false },
    { step: "build", reason: "regenerate", because_of: "push", is_gate: false },
    { step: "push", reason: "target", is_gate: false },
    { step: "deploy-staging", reason: "cascade", because_of: "push", is_gate: false },
    { step: "smoke-test", reason: "cascade", because_of: "deploy-staging", is_gate: false },
  ],
});

describe("confirmHeadline", () => {
  it("asks the question the button implies", () => {
    expect(confirmHeadline("rerun", "push")).toBe("Rerun from push?");
    expect(confirmHeadline("retry", "push")).toBe("Retry push?");
  });
});

describe("confirmSentence — the one-sentence common case", () => {
  it("names just the target when nothing cascades", () => {
    expect(confirmSentence(plan(), "rerun")).toBe("This re-runs push.");
  });

  it("names the cascade — retrying push may not silently re-deploy staging", () => {
    // The ticket's literal G1: the ordinary cascade was never named anywhere.
    expect(confirmSentence(cascadePlan, "rerun")).toBe(
      "This re-runs push, which also re-runs deploy-staging.",
    );
  });

  it("frames retry as another attempt in THIS version (no fork)", () => {
    expect(confirmSentence(cascadePlan, "retry")).toBe(
      "This retries push — another attempt in this version — which also re-runs deploy-staging.",
    );
    expect(confirmSentence(plan(), "retry")).toBe(
      "This retries push — another attempt in this version.",
    );
  });

  it("never claims a gate re-runs — gates live in the gate note only", () => {
    const p = plan({
      invalidated: ["approve-prod", "deploy", "push"],
      steps: [
        { step: "push", reason: "target", is_gate: false },
        { step: "approve-prod", reason: "cascade", because_of: "push", is_gate: true },
        { step: "deploy", reason: "cascade", because_of: "approve-prod", is_gate: false },
      ],
    });
    // "approve-prod" must NOT appear in the re-runs claim; gateNote carries it.
    expect(confirmSentence(p, "rerun")).toBe("This re-runs push, which also re-runs deploy.");
    expect(gateNote(p)).toBe("Pauses for approval at approve-prod.");
  });

  it("starts from the regeneration roots when widened", () => {
    expect(confirmSentence(widenedPlan, "rerun")).toBe(
      "This re-runs from clone, which also re-runs build, deploy-staging, push and smoke-test.",
    );
  });
});

describe("confirmWarning — the widened headline line", () => {
  it("is null for an ordinary plan and for no plan", () => {
    expect(confirmWarning(plan())).toBeNull();
    expect(confirmWarning(null)).toBeNull();
  });

  it("names retention generically — NEVER a day count (amendment F9)", () => {
    const w = confirmWarning(widenedPlan)!;
    expect(w).toBe("Inputs past retention — this re-runs from clone.");
    expect(w).not.toMatch(/\d/);
  });
});

describe("useExpandedList", () => {
  it("keeps the common case to one sentence", () => {
    expect(useExpandedList(plan())).toBe(false);
    expect(useExpandedList(cascadePlan)).toBe(false);
    expect(useExpandedList(null)).toBe(false);
  });

  it("expands long cascades and ANY widening", () => {
    const long = plan({
      invalidated: Array.from({ length: LIST_THRESHOLD + 1 }, (_, i) => `s${i}`),
    });
    expect(useExpandedList(long)).toBe(true);
    expect(useExpandedList(widenedPlan)).toBe(true);
  });
});

describe("planGroups — the expanded list", () => {
  it("groups regeneration first, then the rerun asked for, execution-ordered", () => {
    const groups = planGroups(widenedPlan, "rerun");
    expect(groups.map((g) => g.title)).toEqual([
      "Regenerating expired inputs (2)",
      "Then the rerun you asked for (3)",
    ]);
    expect(groups[0].rows.map((r) => r.step)).toEqual(["clone", "build"]);
    expect(groups[1].rows.map((r) => r.step)).toEqual(["push", "deploy-staging", "smoke-test"]);
  });

  it("annotates each row from the engine's reason + attribution — no day counts", () => {
    const groups = planGroups(widenedPlan, "rerun");
    const notes = Object.fromEntries(
      groups.flatMap((g) => g.rows.map((r) => [r.step, r.note])),
    );
    expect(notes["clone"]).toBe("inputs past retention · regenerates data for build");
    expect(notes["push"]).toBe("the step you picked");
    expect(notes["deploy-staging"]).toBe("depends on push");
    for (const note of Object.values(notes)) expect(note).not.toMatch(/\d+-day/);
  });

  it("names a regenerate-cascade member after the root that drags it in", () => {
    const p = plan({
      invalidated: ["base", "left", "right"],
      widened: ["base", "right"],
      starts_from: ["base"],
      steps: [
        { step: "base", reason: "regenerate", because_of: "left", is_gate: false },
        { step: "left", reason: "target", is_gate: false },
        { step: "right", reason: "regenerate_cascade", because_of: "base", is_gate: false },
      ],
    });
    const groups = planGroups(p, "rerun");
    const right = groups[0].rows.find((r) => r.step === "right")!;
    expect(right.note).toBe("re-runs because base re-runs");
  });
});

describe("gateNote — amendment F4", () => {
  it("is null when the set holds no gates", () => {
    expect(gateNote(widenedPlan)).toBeNull();
    expect(gateNote(null)).toBeNull();
  });

  it("says the run PAUSES at a gate — never that the gate runs", () => {
    const p = plan({
      invalidated: ["approve-prod", "deploy", "push"],
      steps: [
        { step: "push", reason: "target", is_gate: false },
        { step: "approve-prod", reason: "cascade", because_of: "push", is_gate: true },
        { step: "deploy", reason: "cascade", because_of: "approve-prod", is_gate: false },
      ],
    });
    expect(gateNote(p)).toBe("Pauses for approval at approve-prod.");
  });

  it("its row note carries the pause too", () => {
    const p = plan({
      invalidated: ["a", "b", "c", "d", "gate1"],
      steps: [
        { step: "a", reason: "target", is_gate: false },
        { step: "gate1", reason: "cascade", because_of: "a", is_gate: true },
        { step: "b", reason: "cascade", because_of: "gate1", is_gate: false },
        { step: "c", reason: "cascade", because_of: "b", is_gate: false },
        { step: "d", reason: "cascade", because_of: "c", is_gate: false },
      ],
    });
    const rows = planGroups(p, "rerun").flatMap((g) => g.rows);
    expect(rows.find((r) => r.step === "gate1")!.note).toBe(
      "depends on a · pauses for approval",
    );
  });
});

describe("confirmLabel — the button names the blast radius", () => {
  it("carries the count, singular and plural", () => {
    expect(confirmLabel(plan(), "rerun")).toBe("Rerun 1 step");
    expect(confirmLabel(widenedPlan, "rerun")).toBe("Rerun 5 steps");
    expect(confirmLabel(cascadePlan, "retry")).toBe("Retry 2 steps");
  });

  it("claims no count when the preview failed — confirm is still allowed", () => {
    // Unknown is not "expired"; disclosure must never become prevention.
    expect(confirmLabel(null, "rerun")).toBe("Rerun");
    expect(confirmLabel(undefined, "retry")).toBe("Retry");
  });
});

describe("confirmFootnote — maximal-scope honesty", () => {
  it("says 'up to' and that unchanged steps are skipped", () => {
    const f = confirmFootnote(widenedPlan, "rerun");
    expect(f).toBe(
      "Up to 5 steps re-run in a new version. Steps whose inputs are unchanged are skipped.",
    );
    expect(confirmFootnote(plan(), "retry")).toContain("in this version");
  });
});

describe("planDivergence — the TOCTOU toast", () => {
  it("is silent when the executed plan matches the preview", () => {
    expect(planDivergence(plan(), plan(), "rerun")).toBeNull();
  });

  it("is silent without a preview or without an executed body to compare", () => {
    expect(planDivergence(null, plan(), "rerun")).toBeNull();
    expect(planDivergence(plan(), null, "rerun")).toBeNull();
  });

  it("names exactly the steps that appeared between preview and confirm", () => {
    expect(planDivergence(cascadePlan, widenedPlan, "rerun")).toBe(
      "The rerun widened while you confirmed: build, clone and smoke-test also re-run.",
    );
  });

  it("singular form for one extra step", () => {
    const executed = plan({
      invalidated: ["build", "push"],
      widened: ["build"],
      starts_from: ["build"],
    });
    expect(planDivergence(plan(), executed, "retry")).toBe(
      "The retry widened while you confirmed: build also re-runs.",
    );
  });
});
