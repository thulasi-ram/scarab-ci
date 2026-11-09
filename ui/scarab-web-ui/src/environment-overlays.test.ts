import { describe, it, expect } from "vitest";
import {
  eligibilityLabel,
  isPermissive,
  refLayersLabel,
  reportBanner,
  rulesToForm,
  tightenable,
  STAR_ONLY_PATTERN_ERROR,
  toEligibilityBody,
  toGrantBody,
  toProtectionBody,
  unprotectedBanner,
  unprotectedButOpen,
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

  it("refuses a pattern of only * and keeps one of two patterns that differ in case", () => {
    expect(toEligibilityBody(false, "web, *").error).toBe(STAR_ONLY_PATTERN_ERROR);
    expect(toEligibilityBody(false, " ** ").error).toBe(STAR_ONLY_PATTERN_ERROR);
    expect(toEligibilityBody(false, "Web-*, web-*, *-api").body).toEqual({
      all_projects: false,
      projects: ["Web-*", "*-api"],
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
  const line = (acked: boolean) => ({ acknowledged_at: acked ? 1 : null });

  it("counts the lines still to review", () => {
    expect(reportBanner([line(false), line(true), line(false)])).toBe("2 migrated environments to review");
  });

  it("says nothing once every line is reviewed", () => {
    expect(reportBanner([line(true)])).toBeNull();
    expect(reportBanner([])).toBeNull();
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

describe("unprotected environments", () => {
  type Rules = { approvers: string[]; allowed_refs: string[]; wait_timer_s: number; require_reason: boolean };
  const permissive: Rules = { approvers: [], allowed_refs: [], wait_timer_s: 0, require_reason: false };
  const guarded: Rules = { ...permissive, approvers: ["github:alice"] };
  const env = (protection: Rules, all_projects: boolean, projects: string[] = []) => ({
    protection,
    eligibility: { all_projects, projects },
  });

  it("flags a permissive baseline only when some project may deploy", () => {
    expect(unprotectedButOpen(env(permissive, true))).toBe(true);
    expect(unprotectedButOpen(env(permissive, false, ["web-*"]))).toBe(true);
    expect(unprotectedButOpen(env(permissive, false))).toBe(false);
    expect(unprotectedButOpen(env(guarded, true))).toBe(false);
  });

  it("counts every open environment, not only migrated ones", () => {
    expect(
      unprotectedBanner([env(permissive, true), env(permissive, false, ["api"]), env(guarded, true)]),
    ).toBe(
      "2 environments take deploys with no baseline rule: only each project's own overlay protects them. Set a baseline on each.",
    );
    expect(unprotectedBanner([env(permissive, true)])).toBe(
      "1 environment takes deploys with no baseline rule: only each project's own overlay protects it. Set a baseline on it.",
    );
  });

  it("says nothing when every open environment has a baseline", () => {
    expect(unprotectedBanner([env(guarded, true), env(permissive, false)])).toBeNull();
    expect(unprotectedBanner([])).toBeNull();
  });
});
