import { describe, it, expect } from "vitest";
import {
  imageError,
  installationChoices,
  installNameError,
  runtimeBadge,
  shortDigest,
  stateBadge,
  suggestEnvironment,
  targetOwners,
  toInstallBody,
  withSource,
} from "./plugins";

const REPO = "ghcr.io/acme/scarab-plugin-argocd";
const DIGEST = "4f2a1c9e7b3d".padEnd(64, "0");
const IMAGE = `${REPO}@sha256:${DIGEST}`;

describe("installNameError", () => {
  it("accepts a DNS-1123 label and refuses everything else", () => {
    expect(installNameError("argocd-prod")).toBeNull();
    expect(installNameError("a".repeat(63))).toBeNull();
    expect(installNameError("")).toMatch(/required/);
    expect(installNameError("ArgoCD")).toMatch(/not a valid name/);
    expect(installNameError("-argo")).toMatch(/not a valid name/);
    expect(installNameError("argo-")).toMatch(/not a valid name/);
    expect(installNameError("a".repeat(64))).toMatch(/not a valid name/);
  });

  it("refuses a name already installed", () => {
    expect(installNameError("argocd", ["argocd"])).toMatch(/already installed/);
  });
});

describe("imageError", () => {
  it("stays quiet on an empty field", () => {
    expect(imageError("", REPO)).toBeNull();
  });

  it("insists on a digest, not a tag", () => {
    expect(imageError(`${REPO}:v1.2.0`, REPO)).toMatch(/@sha256:/);
    expect(imageError(IMAGE, REPO)).toBeNull();
  });

  it("refuses a short or uppercase digest", () => {
    expect(imageError(`${REPO}@sha256:abc`, REPO)).toMatch(/64 lowercase hex/);
    expect(imageError(`${REPO}@sha256:${DIGEST.toUpperCase()}`, REPO)).toMatch(/64 lowercase hex/);
  });

  it("refuses another repository — that is a third-party image", () => {
    expect(imageError(`${REPO}-evil@sha256:${DIGEST}`, REPO)).toMatch(/third-party/);
  });
});

describe("toInstallBody", () => {
  const form = { plugin: "argocd", name: "argocd-prod", image: IMAGE, config: {} };

  it("sends the form, omitting config when every value is blank", () => {
    expect(toInstallBody({ ...form, config: { ARGOCD_UI_URL: "  " } }, { repository: REPO })).toEqual({
      body: { plugin: "argocd", name: "argocd-prod", image: IMAGE },
    });
  });

  it("sends only the config values that are set, trimmed", () => {
    const r = toInstallBody(
      { ...form, config: { ARGOCD_UI_URL: " https://argo.example ", OWNER_ANNOTATION: "" } },
      { repository: REPO },
    );
    expect(r.body?.config).toEqual({ ARGOCD_UI_URL: "https://argo.example" });
  });

  it("reports the first problem: plugin, then name, then image", () => {
    expect(toInstallBody({ ...form, plugin: "" }).error).toMatch(/Pick a plugin/);
    expect(toInstallBody({ ...form, name: "Bad" }).error).toMatch(/not a valid name/);
    expect(toInstallBody({ ...form, image: "" }).error).toMatch(/image is required/);
    expect(toInstallBody({ ...form, image: REPO }, { repository: REPO }).error).toMatch(/@sha256:/);
  });
});

describe("badges", () => {
  it("calls a killed installation out as danger", () => {
    expect(stateBadge("killed").tone).toBe("danger");
    expect(stateBadge("enabled").tone).toBe("ok");
  });

  it("maps the runtime vocabulary, unknown values muted rather than healthy", () => {
    expect(runtimeBadge("running")).toMatchObject({ label: "running", tone: "ok" });
    expect(runtimeBadge("probing").tone).toBe("motion");
    expect(runtimeBadge("pending").tone).toBe("motion");
    expect(runtimeBadge("probe_failed")).toMatchObject({ label: "probe failed", tone: "danger" });
    expect(runtimeBadge("error").tone).toBe("danger");
    expect(runtimeBadge("stopped").tone).toBe("muted");
    expect(runtimeBadge("hibernating")).toMatchObject({ label: "hibernating", tone: "muted" });
  });
});

describe("shortDigest", () => {
  it("shortens an image's digest, or a bare one", () => {
    expect(shortDigest(IMAGE)).toBe("sha256:4f2a1c9e7b3d");
    expect(shortDigest(DIGEST)).toBe("sha256:4f2a1c9e7b3d");
  });

  it("leaves a tag-pinned image alone", () => {
    expect(shortDigest(`${REPO}:dev`)).toBe(`${REPO}:dev`);
  });
});

describe("unmapped grouping → source", () => {
  const prod = {
    name: "production",
    description: "customer-facing",
    deployment_deadline_ms: 30 * 60_000,
    deployment_sources: [{ installation: "argocd", vendor_grouping: "production" }],
  };

  it("suggests the Environment named like the grouping, and nothing else", () => {
    expect(suggestEnvironment("production", [prod])).toBe("production");
    expect(suggestEnvironment("Production", [prod])).toBe("production");
    expect(suggestEnvironment("prod", [prod])).toBeNull();
  });

  it("appends the source and keeps everything the Environment had", () => {
    expect(withSource(prod, "argocd-us", "production")).toEqual({
      body: {
        description: "customer-facing",
        deployment_deadline_ms: 30 * 60_000,
        deployment_sources: [
          { installation: "argocd", vendor_grouping: "production" },
          { installation: "argocd-us", vendor_grouping: "production" },
        ],
      },
    });
  });

  it("refuses a source the Environment already lists", () => {
    expect(withSource(prod, "argocd", "production").error).toMatch(/already lists/);
  });
});

describe("installationChoices", () => {
  it("lists installations, marking killed ones and keeping an uninstalled current value", () => {
    expect(
      installationChoices(
        [
          { name: "argocd-us", state: "killed" },
          { name: "argocd", state: "enabled" },
        ],
        "flux",
      ),
    ).toEqual([
      { value: "flux", label: "flux (not installed)" },
      { value: "argocd", label: "argocd" },
      { value: "argocd-us", label: "argocd-us (killed)" },
    ]);
  });
});

describe("targetOwners", () => {
  it("indexes owners by source/target, skipping targets with none reported", () => {
    const owners = targetOwners([
      { source: "argocd", target_id: "orders-api", owner_projects: ["acme/orders-api"] },
      { source: "argocd", target_id: "messaging", owner_projects: [] },
    ]);
    expect(owners.get("argocd/orders-api")).toEqual(["acme/orders-api"]);
    expect(owners.has("argocd/messaging")).toBe(false);
  });
});
