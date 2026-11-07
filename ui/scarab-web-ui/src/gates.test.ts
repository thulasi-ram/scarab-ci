import { describe, it, expect } from "vitest";
import {
  isGateOpen,
  isApprovable,
  gateBlockers,
  approvableGates,
  gateStatus,
  gateStatusText,
  currentResolution,
  isRejectable,
  actionableGateIds,
  actionableGates,
  rejectReasonError,
  gateWaitHint,
  inboxRuns,
  inboxVerb,
  inboxDetail,
  addedByEnvironment,
  approvalNote,
  approvalReleaseIn,
  environmentGateLabel,
  isEnvironmentApprovalGate,
} from "./gates";
import { mockRuns } from "./mock";

const gate = (id: string, status: string, kind = "manual", needs: string[] = []) => ({
  id,
  status,
  gate: kind,
  needs,
});

describe("isGateOpen", () => {
  it("is false for a plain step, whatever its status", () => {
    // The discriminator is `gate`, not the status: a pending ordinary step is
    // pending because it has not started, which is not a decision to make.
    expect(isGateOpen({ status: "pending" })).toBe(false);
    expect(isGateOpen({ status: "running" })).toBe(false);
  });

  it("accepts every status a gate can legitimately hold", () => {
    // `pending` is what the server emits; `ready` is a real StepStatus variant;
    // `waiting` is mock-only fiction we tolerate so mock mode renders.
    for (const s of ["pending", "ready", "waiting"]) {
      expect(isGateOpen(gate("approve", s))).toBe(true);
    }
  });

  it("is false once the gate has settled", () => {
    // release_gate records Pending -> Succeeded, so a succeeded gate is exactly
    // the released one — re-offering it would 409.
    for (const s of ["succeeded", "failed", "skipped", "cancelled"]) {
      expect(isGateOpen(gate("approve", s))).toBe(false);
    }
  });
});

describe("isApprovable", () => {
  it("covers manual gates only", () => {
    // timer releases itself; external needs a signed token (ADR-0034). Offering
    // Approve on either would always fail.
    expect(isApprovable(gate("g", "pending", "manual"))).toBe(true);
    expect(isApprovable(gate("g", "pending", "timer"))).toBe(false);
    expect(isApprovable(gate("g", "pending", "external"))).toBe(false);
  });
});

describe("gateBlockers", () => {
  const statuses: Record<string, string> = {
    build: "succeeded",
    test: "running",
    lint: "failed",
  };
  const statusOf = (s: string) => statuses[s];

  it("is empty when every need succeeded — the only approvable state", () => {
    expect(gateBlockers(gate("g", "pending", "manual", ["build"]), statusOf)).toEqual([]);
  });

  it("names unmet needs, so 'not your turn yet' is distinguishable from 'yours'", () => {
    expect(gateBlockers(gate("g", "pending", "manual", ["build", "test"]), statusOf)).toEqual([
      "test",
    ]);
  });

  it("counts a FAILED need as blocking, not as resolved", () => {
    // Only `succeeded` clears a need. A failed upstream must never read as
    // "ready for your approval".
    expect(gateBlockers(gate("g", "pending", "manual", ["lint"]), statusOf)).toEqual(["lint"]);
  });

  it("treats an unknown need as blocking", () => {
    expect(gateBlockers(gate("g", "pending", "manual", ["ghost"]), statusOf)).toEqual(["ghost"]);
  });
});

describe("approvableGates", () => {
  it("returns EVERY open manual gate, not just the first", () => {
    // The multi-gate case this UI exists for: a fan-out where two branches each
    // end in their own approval. A run-level action could not express this.
    const steps = [
      gate("approve-eu", "pending"),
      gate("build", "succeeded", "manual"),
      gate("approve-us", "ready"),
      { id: "compile", status: "pending" },
      gate("auto", "pending", "timer"),
    ];
    expect(approvableGates(steps).map((s) => s.id)).toEqual(["approve-eu", "approve-us"]);
  });

  it("is empty for a run with no gates", () => {
    expect(approvableGates([{ id: "a", status: "running" }])).toEqual([]);
  });
});

// --- slice b, criterion 5: verdicts and reject -----------------------------

const resolved = (passed: boolean, over: Record<string, unknown> = {}) => ({
  id: "live",
  status: passed ? "succeeded" : "failed",
  gate: "external",
  needs: ["bump"],
  attempt_list: [{ id: "a1" }],
  gate_resolution: {
    passed,
    reason: passed ? "smoke tests green" : "ImagePullBackOff",
    attempt: "a1",
  },
  ...over,
});

describe("gateStatus", () => {
  it("shows a passed verdict with its reason", () => {
    expect(gateStatus(resolved(true))).toEqual({
      tone: "passed",
      text: "passed: smoke tests green",
    });
  });

  it("shows a failed verdict with its reason", () => {
    expect(gateStatus(resolved(false))).toEqual({ tone: "failed", text: "failed: ImagePullBackOff" });
  });

  it("names who rejected a gate", () => {
    const s = resolved(false, {
      gate: "manual",
      gate_resolution: { passed: false, reason: "wrong build", attempt: "a1", by: "j.lee" },
    });
    expect(gateStatusText(s)).toBe("failed: wrong build (rejected by j.lee)");
  });

  it("has nothing to say for a plain step or an open manual gate", () => {
    expect(gateStatus({ status: "running" })).toBeNull();
    expect(gateStatus(gate("approve", "pending"))).toBeNull();
  });
});

describe("currentResolution", () => {
  it("drops a verdict the gate was re-armed past", () => {
    // A retry minted a2 and the gate waits again: the a1 verdict is history.
    const retried = resolved(false, { status: "pending", attempt_list: [{ id: "a1" }, { id: "a2" }] });
    expect(currentResolution(retried)).toBeNull();
    // A rerun put it back to pending before minting: still not current.
    expect(currentResolution(resolved(true, { status: "pending" }))).toBeNull();
    // A settled gate whose verdict closed a1 but whose latest attempt is a2.
    expect(currentResolution(resolved(true, { attempt_list: [{ id: "a1" }, { id: "a2" }] }))).toBeNull();
  });

  it("keeps the verdict of a settled gate's latest attempt", () => {
    expect(currentResolution(resolved(true))?.passed).toBe(true);
  });
});

describe("isRejectable", () => {
  const statuses: Record<string, string> = { bump: "succeeded", test: "running" };
  const statusOf = (s: string) => statuses[s];

  it("covers every gate kind once its needs succeeded", () => {
    for (const kind of ["manual", "timer", "external", "plugin"]) {
      expect(isRejectable(gate("g", "pending", kind, ["bump"]), statusOf)).toBe(true);
    }
  });

  it("is false while a need is unfinished — the server 409s 'not reached yet'", () => {
    expect(isRejectable(gate("g", "pending", "external", ["test"]), statusOf)).toBe(false);
  });

  it("is false for a settled gate and for a plain step", () => {
    expect(isRejectable(gate("g", "failed", "manual", ["bump"]), statusOf)).toBe(false);
    expect(isRejectable({ status: "pending", needs: ["bump"] }, statusOf)).toBe(false);
  });
});

describe("actionableGates", () => {
  it("lists every open gate a human can approve or reject, timer and external included", () => {
    const steps = [
      gate("approve", "pending"),
      gate("wait", "pending", "timer"),
      gate("live", "pending", "external"),
      gate("done", "succeeded", "external"),
      { id: "build", status: "pending" },
    ];
    expect(actionableGates(steps).map((s) => s.id)).toEqual(["approve", "wait", "live"]);
  });
});

describe("actionableGateIds", () => {
  it("is the same ids, in step order, across polls that replace every step object", () => {
    // Two polls of one run: fresh objects, same gates. The gate bar keys its
    // rows by these, so an open Reject form survives the second poll.
    const poll = () => [gate("approve", "pending"), gate("live", "pending", "external")];
    const first = poll();
    const second = poll();
    expect(first[1]).not.toBe(second[1]);
    expect(actionableGateIds(first)).toEqual(["approve", "live"]);
    expect(actionableGateIds(second)).toEqual(actionableGateIds(first));
  });
});

describe("rejectReasonError", () => {
  it("requires a reason once trimmed", () => {
    expect(rejectReasonError("")).toBe("a reason is required");
    expect(rejectReasonError("   ")).toBe("a reason is required");
    expect(rejectReasonError(" wrong build ")).toBeNull();
  });

  it("bounds it in bytes, as the server does", () => {
    expect(rejectReasonError("x".repeat(1024))).toBeNull();
    expect(rejectReasonError("x".repeat(1025))).toBe("at most 1024 bytes");
    // 342 three-byte characters = 1026 bytes, though only 342 chars.
    expect(rejectReasonError("€".repeat(342))).toBe("at most 1024 bytes");
  });
});

describe("gateWaitHint", () => {
  const statusOf = (s: string) => (s === "bump" ? "succeeded" : "running");

  it("says what an external gate waits for once reached", () => {
    expect(gateWaitHint(gate("live", "pending", "external", ["bump"]), statusOf)).toBe(
      "waiting for an external release",
    );
  });

  it("names unmet needs first", () => {
    expect(gateWaitHint(gate("live", "pending", "external", ["test"]), statusOf)).toBe("waiting on test");
  });

  it("says a plugin gate waits for its plugin, which no person can pass", () => {
    expect(gateWaitHint(gate("live", "pending", "plugin", ["bump"]), statusOf)).toBe("waiting for its plugin");
    expect(isApprovable(gate("live", "pending", "plugin"))).toBe(false);
  });
});

// --- git-bug c489346: the dashboard inbox follows reached gates -------------

describe("inboxRuns", () => {
  it("includes a RUNNING run whose gate is reached while a sibling still runs", () => {
    const running = { id: "r1", status: "running", waiting_gates: [{ step: "approve", kind: "manual", since: 1 }] };
    expect(inboxRuns([running])).toEqual([running]);
  });

  it("leaves out a running run with no waiting gate, and terminal runs", () => {
    expect(
      inboxRuns([
        { status: "running" },
        { status: "running", waiting_gates: [] },
        { status: "succeeded" },
      ]),
    ).toEqual([]);
  });

  it("still lists a suspended run an older server sends without waiting_gates", () => {
    expect(inboxRuns([{ status: "suspended" }])).toHaveLength(1);
  });

  it("over the mock: both suspended runs and the running one with a reached gate", () => {
    const ids = inboxRuns(mockRuns).map((r) => r.id);
    expect(ids).toEqual([
      "01984f10a1b24c7e8f0012ab34cd56ef",
      "01984f22bb334d8e9a1122cd44ef6600",
      "01984f35cc445e9fab2233de55f07711",
    ]);
  });
});

describe("inbox row text", () => {
  const g = (step: string, kind: string) => ({ step, kind, since: 0 });

  it("approves a manual gate and says the run is still running beside it", () => {
    const r = { status: "running", waiting_gates: [g("approve", "manual")] };
    expect(inboxVerb(r)).toBe("approve");
    expect(inboxDetail(r)).toBe("gate approve (manual) waiting · other steps still running");
  });

  it("reviews a wait no human approves, and names every waiting gate", () => {
    const r = { status: "suspended", waiting_gates: [g("live", "external"), g("soak", "timer")] };
    expect(inboxVerb(r)).toBe("review");
    expect(inboxDetail(r)).toBe("gates live (external), soak (timer) waiting");
  });

  it("keeps the old wording for a suspended run without waiting_gates", () => {
    const r = { status: "suspended" };
    expect(inboxVerb(r)).toBe("approve");
    expect(inboxDetail(r)).toBe("suspended on a gate · waiting for a decision");
  });
});

describe("Environment approval gates", () => {
  const prod = { name: "production", display_name: "Production" };
  const authored = { ...gate("approve", "pending", "manual", ["build"]), environment_approval_gate: true };
  const added = {
    ...gate("environment-approval", "pending", "manual", ["build"]),
    environment_approval_gate: true,
    origin: [{ kind: "environment", name: "production" }],
  };
  const plain = { ...gate("qa", "pending", "manual", ["build"]), environment_approval_gate: false };
  const statusOf = (s: string) => (s === "build" ? "succeeded" : "pending");

  it("names the Environment by its display name and its name, and the one that added a gate", () => {
    expect(environmentGateLabel(authored, prod)).toBe("Environment approval gate · Production (production)");
    expect(environmentGateLabel(added, prod)).toBe(
      "Environment approval gate · added by Production (production)",
    );
    // A display name cannot pass prod off as another Environment.
    expect(environmentGateLabel(authored, { name: "production", display_name: "Staging" })).toBe(
      "Environment approval gate · Staging (production)",
    );
    expect(environmentGateLabel(authored, { name: "production", display_name: "production" })).toBe(
      "Environment approval gate · production",
    );
    expect(environmentGateLabel(authored, { name: "production" })).toBe(
      "Environment approval gate · production",
    );
    expect(addedByEnvironment(added)).toBe("production");
    expect(addedByEnvironment(authored)).toBeNull();
  });

  it("gives a plain gate no label and no mention of approvers", () => {
    expect(isEnvironmentApprovalGate(plain)).toBe(false);
    expect(environmentGateLabel(plain, prod)).toBeNull();
    expect(gateWaitHint(plain, statusOf)).toBe("waiting for a decision");
    expect(gateWaitHint(authored, statusOf)).toBe("waiting for the environment's approvers");
  });

  it("only a manual gate is one, whatever the mark says", () => {
    expect(isEnvironmentApprovalGate({ ...authored, gate: "external" })).toBe(false);
  });

  it("counts down to the release once the approvals are in, and not after", () => {
    const timed = { ...added, approval_release_at: 10_000 };
    expect(approvalReleaseIn(added, 0)).toBeNull();
    expect(approvalReleaseIn(timed, 4_000)).toBe(6_000);
    expect(approvalReleaseIn(timed, 12_000)).toBe(0);
    expect(approvalReleaseIn({ ...timed, status: "succeeded" }, 4_000)).toBeNull();
    expect(gateWaitHint(timed, statusOf)).toBe("approvals in · waiting for the wait timer");
  });

  it("words a 202 that did not release the gate as recorded, never as failed", () => {
    const at = (ms: number) => `t${ms}`;
    expect(approvalNote({ released: true }, at)).toBe("released");
    expect(approvalNote({ released: false, approvals: ["a"], releases_at: 99 }, at)).toBe(
      "approval recorded · the gate releases at t99, when the wait timer runs out",
    );
    expect(approvalNote({ released: false, approvals: ["a"] }, at)).toBe("1 approval recorded · awaiting more");
  });
});
