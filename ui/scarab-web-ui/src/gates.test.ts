import { describe, it, expect } from "vitest";
import { isGateOpen, isApprovable, gateBlockers, approvableGates } from "./gates";

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
