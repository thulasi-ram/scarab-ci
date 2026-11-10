import { describe, it, expect } from "vitest";
import {
  approversCell,
  deploySentence,
  layerRows,
  refsCell,
  repoChip,
  segmentsText,
  waitCell,
  waitShort,
  type RepoEnvLike,
} from "./repo-environments";

type Rules = { approvers: string[]; allowed_refs: string[]; wait_timer_s: number; require_reason: boolean };
const none: Rules = { approvers: [], allowed_refs: [], wait_timer_s: 0, require_reason: false };

// An Environment as the server would answer it: `effective` is the merge the
// server does (approvers union, ref layers ANDed, the longer wait, reason if
// either asks), written out by hand here.
function env(baseline: Partial<Rules>, overlay?: Partial<Rules>, extra: Partial<RepoEnvLike> = {}): RepoEnvLike {
  const b = { ...none, ...baseline };
  const o = overlay ? { ...none, ...overlay } : null;
  return {
    eligible: true,
    baseline: b,
    overlay: o,
    effective: {
      approvers: [...new Set([...b.approvers, ...(o?.approvers ?? [])])],
      allowed_ref_layers: o ? [b.allowed_refs, o.allowed_refs] : [b.allowed_refs],
      wait_timer_s: Math.max(b.wait_timer_s, o?.wait_timer_s ?? 0),
      require_reason: b.require_reason || (o?.require_reason ?? false),
      approval_gate: { required_approvals: null },
    },
    privilege_grant: null,
    ...extra,
  };
}

const sentence = (e: RepoEnvLike) => segmentsText(deploySentence(e));

describe("deploySentence", () => {
  it("names the refs, the approvals, the reason and the wait", () => {
    const e = env(
      { approvers: ["a", "b"], allowed_refs: ["refs/heads/main", "refs/tags/v*"], wait_timer_s: 300 },
      { approvers: ["c"], require_reason: true },
    );
    expect(sentence(e)).toBe("A deploy from `main` or a `v*` tag needs 3 approvals and a reason, then waits 5 minutes.");
  });

  it("says a deploy needs nothing when no rule protects it", () => {
    expect(sentence(env({}))).toBe("A deploy from any ref needs no approval.");
  });

  it("leaves out a wait timer that has no approvers to wait on", () => {
    expect(sentence(env({ wait_timer_s: 600, require_reason: true }))).toBe("A deploy from any ref needs a reason.");
  });

  it("counts the baseline's required approvals, plus each approver the overlay adds", () => {
    const e = env({ approvers: ["a", "b", "c"] }, { approvers: ["d"] });
    e.effective.approval_gate = { required_approvals: 1 };
    expect(sentence(e)).toBe("A deploy from any ref needs 2 approvals.");
  });

  it("reads two ref layers as a ref that must match both", () => {
    const e = env({ allowed_refs: ["refs/heads/main", "refs/heads/release/*"] }, { allowed_refs: ["refs/heads/release/*"] });
    expect(sentence(e)).toBe("A deploy from `main` or `release/*` that also matches `release/*` needs no approval.");
  });

  it("says deploys are refused once the repo is not eligible", () => {
    expect(sentence(env({ approvers: ["a"] }, { approvers: ["b"] }, { eligible: false }))).toBe(
      "Deploys from this repo are refused here.",
    );
  });
});

describe("row cells", () => {
  it("shortens refs and says Any when nothing restricts them", () => {
    expect(refsCell([[], []])).toBe("Any");
    expect(refsCell([["refs/heads/main", "refs/tags/v*"]])).toBe("main, v*");
    expect(refsCell([["refs/heads/main"], ["refs/tags/v*"]])).toBe("main and v*");
  });

  it("writes a wait short, and shows none when there are no approvers", () => {
    expect(waitShort(300)).toBe("5 min");
    expect(waitShort(90)).toBe("1 min 30 s");
    expect(waitShort(5400)).toBe("1 h 30 min");
    expect(waitCell(env({ approvers: ["a"], wait_timer_s: 300 }).effective).text).toBe("5 min");
    const idle = waitCell(env({ wait_timer_s: 600 }).effective);
    expect(idle.text).toBe("—");
    expect(idle.title).toMatch(/Does nothing/);
    expect(waitCell(env({ approvers: ["a"] }).effective)).toEqual({ text: "—" });
  });

  it("counts approvers, and says how many of them are needed", () => {
    expect(approversCell(env({}))).toBe("—");
    expect(approversCell(env({ approvers: ["a"] }, { approvers: ["b"] }))).toBe("2");
    const some = env({ approvers: ["a", "b", "c"] });
    some.effective.approval_gate = { required_approvals: 1 };
    expect(approversCell(some)).toBe("1 of 3");
  });
});

describe("repoChip", () => {
  it("sums up what the overlay adds", () => {
    expect(repoChip(env({ approvers: ["a"] }, { approvers: ["b"] }))).toEqual({ text: "+1 approver", warn: false });
    expect(repoChip(env({ approvers: ["a"] }, { require_reason: true }))).toEqual({ text: "tightened", warn: false });
    expect(repoChip(env({}, { approvers: ["b", "c"], wait_timer_s: 60 }))?.text).toBe("+2 approvers, tightened");
    // An approver the baseline already has, and a shorter wait, add nothing.
    expect(repoChip(env({ approvers: ["a"], wait_timer_s: 600 }, { approvers: ["a"], wait_timer_s: 60 }))?.text).toBe(
      "adds nothing",
    );
  });

  it("warns when no rule protects a deploy, or the repo is not eligible", () => {
    expect(repoChip(env({}))).toEqual({ text: "no rules", warn: true });
    // A wait timer alone protects nothing: it needs approvers.
    expect(repoChip(env({ wait_timer_s: 600 }))).toEqual({ text: "no rules", warn: true });
    expect(repoChip(env({}, { approvers: ["b"] }, { eligible: false }))).toEqual({ text: "not eligible", warn: true });
  });

  it("shows nothing when the baseline alone protects the environment", () => {
    expect(repoChip(env({ approvers: ["a"] }))).toBeNull();
  });
});

describe("layerRows", () => {
  it("sets the baseline beside what the overlay adds, and the repo's privileges", () => {
    const e = env(
      { approvers: ["ops-lead"], allowed_refs: ["refs/heads/main"] },
      { approvers: ["ops-lead", "j.lee"], wait_timer_s: 300, require_reason: true },
      { privilege_grant: { privileged_images: [{}], permit_k8s_overlay: true } },
    );
    expect(layerRows(e).map((r) => [r.rule, r.baseline, r.overlay])).toEqual([
      ["Approvers", "ops-lead", "j.lee"],
      ["Refs", "main", "—"],
      ["Wait", "—", "5 min"],
      ["Reason", "—", "Required"],
      ["Privileges", "—", "1 privileged image, raw k8s_overlay allowed"],
    ]);
  });

  it("reads — for every rule this repo adds when it has no overlay", () => {
    expect(layerRows(env({ require_reason: true })).map((r) => r.overlay)).toEqual(["—", "—", "—", "—", "—"]);
  });
});
