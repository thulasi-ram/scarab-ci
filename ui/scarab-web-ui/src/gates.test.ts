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
  actionableGates,
  rejectReasonError,
  gateWaitHint,
} from "./gates";

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

// --- slice b, criterion 5: verdicts, waits, holds, reject ------------------

const waitingDeploy = (over: Record<string, unknown> = {}) => ({
  id: "live",
  status: "pending",
  gate: "deployment",
  needs: ["bump"],
  attempt_list: [{ id: "a1" }],
  deployment_gate: {
    target_source: "argocd",
    target_id: "web",
    deployment_status: "in_progress",
    latest_label: "sync wave 3",
  },
  ...over,
});

const resolved = (passed: boolean, over: Record<string, unknown> = {}) => ({
  id: "live",
  status: passed ? "succeeded" : "failed",
  gate: "deployment",
  needs: ["bump"],
  attempt_list: [{ id: "a1" }],
  gate_resolution: {
    passed,
    reason: passed ? "v1 live on `argocd`/`web`: Healthy" : "ImagePullBackOff",
    attempt: "a1",
  },
  ...over,
});

describe("gateStatus", () => {
  it("says what a waiting DeploymentGate waits on: target, status, label", () => {
    expect(gateStatus(waitingDeploy())).toEqual({
      tone: "waiting",
      text: 'waiting on argocd/web (in progress, "sync wave 3")',
    });
  });

  it("names the status alone when the source has no label yet", () => {
    const s = waitingDeploy({
      deployment_gate: { target_source: "argocd", target_id: "web", deployment_status: "requested" },
    });
    expect(gateStatusText(s)).toBe("waiting on argocd/web (requested)");
  });

  it("says so when the requester's Deployment is not in the record yet", () => {
    const s = waitingDeploy({ deployment_gate: { target_source: "argocd", target_id: "web" } });
    expect(gateStatusText(s)).toBe("waiting on argocd/web (no Deployment recorded yet)");
  });

  it("shows a passed verdict with its reason", () => {
    expect(gateStatus(resolved(true))).toEqual({
      tone: "passed",
      text: "passed: v1 live on `argocd`/`web`: Healthy",
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

  it("shows a hold with one `held:` prefix, not two", () => {
    // The server's reason already reads "held: waiting for …".
    const s = {
      id: "bump",
      status: "pending",
      held: "held: waiting for deployment concurrency on `argocd`/`web` (1/1 in flight)",
    };
    expect(gateStatus(s)).toEqual({
      tone: "held",
      text: "held: waiting for deployment concurrency on `argocd`/`web` (1/1 in flight)",
    });
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
    for (const kind of ["manual", "timer", "external", "deployment"]) {
      expect(isRejectable(gate("g", "pending", kind, ["bump"]), statusOf)).toBe(true);
    }
  });

  it("is false while a need is unfinished — the server 409s 'not reached yet'", () => {
    expect(isRejectable(gate("g", "pending", "deployment", ["test"]), statusOf)).toBe(false);
  });

  it("is false for a settled gate and for a plain step", () => {
    expect(isRejectable(gate("g", "failed", "manual", ["bump"]), statusOf)).toBe(false);
    expect(isRejectable({ status: "pending", needs: ["bump"] }, statusOf)).toBe(false);
  });

  it("never makes a DeploymentGate approvable", () => {
    expect(isApprovable(gate("g", "pending", "deployment"))).toBe(false);
  });
});

describe("actionableGates", () => {
  it("lists every open gate a human can approve or reject, timer and deployment included", () => {
    const steps = [
      gate("approve", "pending"),
      gate("wait", "pending", "timer"),
      gate("live", "pending", "deployment"),
      gate("done", "succeeded", "deployment"),
      { id: "build", status: "pending" },
    ];
    expect(actionableGates(steps).map((s) => s.id)).toEqual(["approve", "wait", "live"]);
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

  it("names a DeploymentGate's target and Deployment", () => {
    expect(gateWaitHint(waitingDeploy(), statusOf)).toBe(
      'waiting on argocd/web (in progress, "sync wave 3")',
    );
  });

  it("names unmet needs first", () => {
    expect(gateWaitHint(waitingDeploy({ needs: ["test"] }), statusOf)).toBe("waiting on test");
  });
});
