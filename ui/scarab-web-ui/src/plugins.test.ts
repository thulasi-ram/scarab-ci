import { describe, it, expect } from "vitest";
import {
  connectionBaseUrlError,
  credentialBadge,
  credentialError,
  imageError,
  ruleLabel,
  isInsecureHttp,
  toConnectionBody,
  installNameError,
  runtimeBadge,
  shortDigest,
  stateBadge,
  storageClaimError,
  storageMountPathError,
  toInstallBody,
  toStorageBody,
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

describe("connection binding", () => {
  const form = { base_url: "https://argocd.acme.example", credential: "tok", ca_pem: "" };

  it("accepts an https destination and drops a blank CA", () => {
    expect(toConnectionBody(form)).toEqual({
      body: { base_url: "https://argocd.acme.example", credential: "tok" },
    });
  });

  it("refuses what the server refuses", () => {
    expect(connectionBaseUrlError("")).toBeNull();
    expect(connectionBaseUrlError("argocd.acme.example")).toMatch(/absolute URL/);
    expect(connectionBaseUrlError("ftp://argocd.acme.example")).toMatch(/http or https/);
    expect(connectionBaseUrlError("https://u:p@argocd.acme.example")).toMatch(/user or password/);
    expect(connectionBaseUrlError("https://argocd.acme.example/?x=1")).toMatch(/query/);
    expect(connectionBaseUrlError("https://argocd.acme.example/#x")).toMatch(/query or fragment/);
    expect(connectionBaseUrlError("https://argocd.acme.example/argo")).toBeNull();
  });

  it("always needs the credential, as a bearer token", () => {
    expect(toConnectionBody({ ...form, credential: "" }).error).toMatch(/every save sends it again/);
    expect(toConnectionBody({ ...form, credential: "two words" }).error).toMatch(/no spaces/);
    expect(credentialError("a\nb")).toMatch(/no spaces/);
  });

  it("flags plain http and refuses a CA on it", () => {
    expect(isInsecureHttp("http://argocd.svc:8080")).toBe(true);
    expect(isInsecureHttp("https://argocd.svc")).toBe(false);
    expect(toConnectionBody({ ...form, base_url: "http://argocd.svc" }).body).toBeDefined();
    const pem = "-----BEGIN CERTIFICATE-----\nMII…\n-----END CERTIFICATE-----";
    expect(toConnectionBody({ ...form, base_url: "http://argocd.svc", ca_pem: pem }).error).toMatch(
      /https destination only/,
    );
    expect(toConnectionBody({ ...form, ca_pem: "not pem" }).error).toMatch(/PEM/);
    expect(toConnectionBody({ ...form, ca_pem: pem }).body?.ca_pem).toBe(pem);
  });

  it("shows a read rule with the only query keys it allows", () => {
    expect(ruleLabel({ method: "GET", path: "/api/version", query: [] })).toBe("GET /api/version");
    expect(ruleLabel({ method: "GET", path: "/api/v1/applications/*", query: ["appNamespace", "project"] })).toBe(
      "GET /api/v1/applications/* ?appNamespace&project",
    );
  });

  it("says set / not set / not bound — and never anything of the credential", () => {
    expect(credentialBadge({ configured: false, credential_set: false }).label).toBe("not bound");
    expect(credentialBadge({ configured: true, credential_set: true })).toMatchObject({
      label: "credential set",
      tone: "ok",
    });
    expect(credentialBadge({ configured: true, credential_set: false }).tone).toBe("danger");
  });
});

describe("storage (ADR-0074 §7)", () => {
  it("takes a claim name the way Kubernetes names a PVC", () => {
    expect(storageClaimError("argocd-state")).toBeNull();
    expect(storageClaimError("state.v2")).toBeNull();
    expect(storageClaimError("")).toMatch(/Enter/);
    for (const bad of ["Data", "-data", "data-", "data..v2", "data_v2", "a/b"]) {
      expect(storageClaimError(bad), bad).toMatch(/Not a claim name/);
    }
  });

  it("refuses a mount path that is relative, the root, or over a runtime mount", () => {
    expect(storageMountPathError("")).toBeNull();
    expect(storageMountPathError("/data")).toBeNull();
    expect(storageMountPathError("/var/lib/argocd/")).toBeNull();
    expect(storageMountPathError("/tmpdata")).toBeNull();
    expect(storageMountPathError("data")).toMatch(/absolute/);
    expect(storageMountPathError("/")).toMatch(/cannot be \//);
    expect(storageMountPathError("/data/../tmp")).toMatch(/parts/);
    for (const bad of ["/tmp", "/tmp/x", "/var/run/secrets", "/var", "/etc", "/proc/self"]) {
      expect(storageMountPathError(bad), bad).toMatch(/overlaps/);
    }
  });

  it("omits a blank mount path so the server's default applies", () => {
    expect(toStorageBody({ claim: " argocd-state ", mount_path: " " })).toEqual({
      body: { claim: "argocd-state" },
    });
    expect(toStorageBody({ claim: "argocd-state", mount_path: "/srv" })).toEqual({
      body: { claim: "argocd-state", mount_path: "/srv" },
    });
    expect(toStorageBody({ claim: "", mount_path: "/srv" }).error).toMatch(/Enter/);
    expect(toStorageBody({ claim: "argocd-state", mount_path: "/tmp" }).error).toMatch(/overlaps/);
  });
});
