import { describe, it, expect } from "vitest";
import {
  clock,
  clockLow,
  expiryPercent,
  isYankRefusal,
  originParts,
  pluginGateState,
  rerunScope,
  rerunYank,
  yankNote,
} from "./run-origin";

const lib = (name: string, hash: string, yanked = false) => ({ kind: "library", name, content_hash: hash.repeat(64 / hash.length), yanked });

describe("origin line", () => {
  it("walks from the Pipeline file through each invocation to the Step", () => {
    const parts = originParts("deploy", {
      id: "ship/smoke/probe",
      origin: [lib("acme/deploy-helm", "5d"), lib("acme/http-probe", "2c", true)],
    });
    expect(parts.map((p) => p.text)).toEqual([
      ".scarab/deploy.yaml",
      "ship",
      "lib:acme/deploy-helm",
      "smoke",
      "lib:acme/http-probe",
      "probe",
    ]);
    expect(parts[2]).toMatchObject({ hash: "5d5d5d5d5d5d", yanked: false });
    expect(parts[4]).toMatchObject({ yanked: true });
    expect(originParts("deploy", { id: "test" })).toEqual([]);
    expect(
      originParts("deploy", { id: "live", origin: [{ kind: "plugin", name: "argocd-prod/wait-live", content_hash: "e4".repeat(32) }] }).map((p) => p.text),
    ).toEqual([".scarab/deploy.yaml", "live", "plugin:argocd-prod/wait-live"]);
  });
});

describe("plugin gate", () => {
  it("reads the four states", () => {
    expect(pluginGateState({ status: "pending", plugin_gate: { plugin_state: "enabled" } })).toBe("waiting");
    expect(pluginGateState({ status: "pending", plugin_gate: { plugin_state: "killed" } })).toBe("killed");
    expect(pluginGateState({ status: "succeeded", gate_resolution: { passed: true } })).toBe("released");
    expect(pluginGateState({ status: "failed", gate_resolution: { passed: false, reason: "expired after 1800s" } })).toBe("expired");
    expect(pluginGateState({ status: "failed", gate_resolution: { passed: false, reason: "bad image" } })).toBe("rejected");
  });
  it("counts down and fills its bar", () => {
    expect(clock(23 * 60_000 + 41_000)).toBe("23:41");
    expect(clock(3_600_000 + 5_000)).toBe("1:00:05");
    expect(clock(-5)).toBe("0:00");
    expect(expiryPercent(0, 1000, 250)).toBe(25);
    expect(expiryPercent(0, 1000, 5000)).toBe(100);
    expect(expiryPercent(10, 10, 5)).toBe(100);
    expect(clockLow(4 * 60_000)).toBe(true);
    expect(clockLow(6 * 60_000)).toBe(false);
  });
});

describe("yanked versions on the Run page", () => {
  const steps = [
    { id: "checkout/git", needs: [], status: "succeeded" },
    { id: "test", needs: ["checkout/git"], status: "succeeded" },
    { id: "charts/git", needs: ["test"], status: "failed" },
    { id: "ship/helm", needs: ["charts/git"], status: "not_run" },
  ];
  const y = { name: "thulasi-ram/clone", content_hash: "a3c9e1f7b5d2".repeat(5) + "abcd", reason: "writes the token to the log", yanked_at: 0, steps: ["checkout/git", "charts/git"] };

  it("finds a Rerun that would launch a Step from a yanked version", () => {
    expect([...rerunScope("test", steps)]).toEqual(["test", "charts/git", "ship/helm"]);
    expect(rerunYank("test", steps, [y])).toBe(y);
    expect(rerunYank("ship/helm", steps, [y])).toBeNull();
    expect(rerunYank("test", steps, [])).toBeNull();
    expect(rerunYank(null, steps, [y])).toBeNull();
  });

  it("says what ran, what was refused and what to do", () => {
    const n = yankNote([y], steps, () => "14:05:02")!;
    expect(n).toMatch(/^lib:thulasi-ram\/clone a3c9e1f7b5d2 was yanked at 14:05:02: writes the token to the log\./);
    expect(n).toMatch(/checkout\/git had run\./);
    expect(n).toMatch(/charts\/git was refused at launch\./);
    expect(n).toMatch(/Start a new run/);
    expect(yankNote([], steps, String)).toBeNull();
    expect(isYankRefusal("step `charts/git` uses … which an org admin yanked")).toBe(true);
    expect(isYankRefusal("exit 1")).toBe(false);
  });
});
