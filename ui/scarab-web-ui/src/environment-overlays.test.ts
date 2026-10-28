import { describe, it, expect } from "vitest";
import {
  eligibilityLabel,
  isPermissive,
  refLayersLabel,
  reportBanner,
  rulesToForm,
  tightenable,
  toEligibilityBody,
  toGrantBody,
  toProtectionBody,
} from "./environment-overlays";

const blank = () => rulesToForm(null);

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
    });
  });

  it("refuses a negative or fractional wait", () => {
    expect(toProtectionBody({ ...blank(), waitTimerS: "-1" }).error).toBeDefined();
    expect(toProtectionBody({ ...blank(), waitTimerS: "1.5" }).error).toBeDefined();
  });
});

describe("labels", () => {
  it("a permissive layer is exactly the migration's default", () => {
    const permissive = toProtectionBody(blank()).body!;
    expect(isPermissive(permissive)).toBe(true);
    expect(isPermissive({ ...permissive, require_reason: true })).toBe(false);
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
