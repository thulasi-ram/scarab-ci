import { describe, it, expect } from "vitest";
import {
  eligibilityLabel,
  isPermissive,
  overlayToForm,
  refLayersLabel,
  reportBanner,
  rulesToForm,
  tightenable,
  toEligibilityBody,
  toOverlayBody,
  toGrantBody,
  toProtectionBody,
  type OverlayForm,
} from "./environment-overlays";

const blank = (): OverlayForm => ({ ...rulesToForm(null), gateDeadlineMinutes: "" });

describe("concurrency: blank is no limit, never coerced to 1", () => {
  it("sends null for a blank field", () => {
    expect(toProtectionBody(blank()).body?.deployment_concurrency).toBeNull();
  });

  it("refuses 0 and fractions instead of turning them into 1", () => {
    for (const concurrency of ["0", "-1", "1.5", "two"]) {
      const r = toProtectionBody({ ...blank(), concurrency });
      expect(r.error, concurrency).toMatch(/blank for no limit/);
    }
  });

  it("keeps a real limit", () => {
    expect(toProtectionBody({ ...blank(), concurrency: "3" }).body?.deployment_concurrency).toBe(3);
  });

  it("round-trips a stored null back to a blank field", () => {
    const form = rulesToForm({
      approvers: [],
      allowed_refs: [],
      wait_timer_s: 0,
      require_reason: false,
      deployment_concurrency: null,
    });
    expect(form.concurrency).toBe("");
  });
});

describe("rules form <-> body", () => {
  it("splits, trims and de-duplicates the lists", () => {
    const r = toProtectionBody({
      ...blank(),
      approvers: " github:alice, ,github:bob, github:alice",
      allowedRefs: "refs/heads/main,refs/tags/*",
      requireReason: true,
      waitTimerS: "30",
    });
    expect(r.body).toEqual({
      approvers: ["github:alice", "github:bob"],
      allowed_refs: ["refs/heads/main", "refs/tags/*"],
      wait_timer_s: 30,
      require_reason: true,
      deployment_concurrency: null,
    });
  });

  it("refuses a negative or fractional wait", () => {
    expect(toProtectionBody({ ...blank(), waitTimerS: "-1" }).error).toBeDefined();
    expect(toProtectionBody({ ...blank(), waitTimerS: "1.5" }).error).toBeDefined();
  });

  it("an overlay's gate deadline is minutes in the form, ms on the wire, blank = none", () => {
    expect(toOverlayBody(blank()).body?.deployment_gate_deadline_ms).toBeNull();
    expect(toOverlayBody({ ...blank(), gateDeadlineMinutes: "15" }).body?.deployment_gate_deadline_ms).toBe(
      900_000,
    );
    expect(toOverlayBody({ ...blank(), gateDeadlineMinutes: "0" }).error).toBeDefined();
    expect(overlayToForm({ ...toOverlayBody(blank()).body!, deployment_gate_deadline_ms: 900_000 }).gateDeadlineMinutes).toBe(
      "15",
    );
  });
});

describe("labels", () => {
  it("a permissive layer is exactly the migration's default", () => {
    const permissive = toProtectionBody(blank()).body!;
    expect(isPermissive(permissive)).toBe(true);
    expect(isPermissive({ ...permissive, require_reason: true })).toBe(false);
    expect(isPermissive({ ...permissive, deployment_concurrency: 1 })).toBe(false);
  });

  it("ref layers are ANDed, never intersected", () => {
    expect(refLayersLabel([[], []])).toBe("any ref");
    // [main] and [tags/*] intersect to [] — which would wrongly read "any ref".
    expect(refLayersLabel([["refs/heads/main"], ["refs/tags/*"]])).toBe(
      "refs/heads/main  AND  refs/tags/*",
    );
    expect(refLayersLabel([[], ["refs/heads/main", "refs/heads/release/*"]])).toBe(
      "refs/heads/main | refs/heads/release/*",
    );
  });

  it("an empty eligibility list admits no project", () => {
    expect(eligibilityLabel({ all_projects: false, projects: [] })).toBe("no project yet");
    expect(eligibilityLabel({ all_projects: true, projects: [] })).toBe("all projects");
    expect(toEligibilityBody(true, "web, api").body).toEqual({ all_projects: true, projects: [] });
    expect(toEligibilityBody(false, "web, team-*").body).toEqual({
      all_projects: false,
      projects: ["web", "team-*"],
    });
  });
});

describe("tightenable", () => {
  it("offers eligible environments and ones already holding an overlay, by name", () => {
    const envs = [
      { name: "staging", eligible: true, overlay: null },
      { name: "prod", eligible: false, overlay: { approvers: [] } },
      { name: "qa", eligible: false, overlay: null },
      { name: "dev", eligible: true },
    ];
    expect(tightenable(envs).map((e) => e.name)).toEqual(["dev", "prod", "staging"]);
  });
});

describe("reportBanner", () => {
  const line = (environment: string, acked: boolean, permissive: boolean, exists = true) => ({
    environment,
    acknowledged_at: acked ? 1 : null,
    permissive_baseline: permissive,
    environment_exists: exists,
  });

  it("counts open lines and permissive baselines separately", () => {
    expect(reportBanner([line("prod", false, true), line("qa", true, true)])).toBe(
      "1 migrated environment to review · 2 with a permissive baseline — set a baseline so the place is protected, not only each project",
    );
  });

  it("says nothing once everything is reviewed and protected", () => {
    expect(reportBanner([line("prod", true, false)])).toBeNull();
    expect(reportBanner([])).toBeNull();
  });

  it("does not ask for a baseline on a deleted environment", () => {
    expect(reportBanner([line("gone", true, false, false)])).toBeNull();
  });
});

describe("toGrantBody", () => {
  it("drops blank rows and normalises capabilities", () => {
    const r = toGrantBody(
      [
        { digest: " sha256:ab12 ", privileged: false, capabilities: "net_admin, SYS_TIME" },
        { digest: "", privileged: false, capabilities: "" },
      ],
      true,
    );
    expect(r.body).toEqual({
      privileged_images: [
        { image_digest: "sha256:ab12", privileged: false, capabilities: ["NET_ADMIN", "SYS_TIME"] },
      ],
      permit_k8s_overlay: true,
    });
  });

  it("refuses a tag or a duplicated digest", () => {
    expect(toGrantBody([{ digest: "nginx:latest", privileged: true, capabilities: "" }], false).error).toMatch(
      /sha256/,
    );
    const twice = { digest: "sha256:aa", privileged: true, capabilities: "" };
    expect(toGrantBody([twice, twice], false).error).toMatch(/twice/);
  });
});
