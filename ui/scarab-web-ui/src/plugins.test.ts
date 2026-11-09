import { describe, it, expect } from "vitest";
import {
  appendJournalPage,
  connectionBaseUrlError,
  formatBytes,
  journalEmptyText,
  journalEntryView,
  journalParams,
  credentialBadge,
  credentialError,
  imageError,
  ruleLabel,
  isInsecureHttp,
  toConnectionBody,
  installNameError,
  pluginPublisher,
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

  it("refuses a repository other than the catalog's, naming the one it wants", () => {
    const err = imageError(`${REPO}-evil@sha256:${DIGEST}`, REPO);
    expect(err).toContain(`${REPO}@sha256:`);
    expect(err).not.toMatch(/first-party|third-party/);
  });
});

describe("pluginPublisher", () => {
  it("reads the owner after the registry host, from a repository or a pinned image", () => {
    expect(pluginPublisher(REPO)).toBe("acme");
    expect(pluginPublisher("ghcr.io/thulasi-ram/scarab-plugin-argocd")).toBe("thulasi-ram");
    expect(pluginPublisher(IMAGE)).toBe("acme");
    expect(pluginPublisher("localhost:5000/acme/argocd@sha256:" + DIGEST)).toBe("acme");
    expect(pluginPublisher("localhost/acme/argocd")).toBe("acme");
  });

  it("reads a Docker Hub path without a host the same way", () => {
    expect(pluginPublisher("acme/argocd")).toBe("acme");
  });

  it("is empty when there is no owner segment", () => {
    expect(pluginPublisher("argocd")).toBe("");
    expect(pluginPublisher("ghcr.io/argocd")).toBe("");
    expect(pluginPublisher("")).toBe("");
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

describe("storage", () => {
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

describe("broker journal", () => {
  it("sends only the filters that are set, and the cursor once there is one", () => {
    expect(journalParams({ phase: "", deniedOnly: false })).toEqual({ limit: 100 });
    expect(journalParams({ phase: "refused", deniedOnly: true }, 41)).toEqual({
      limit: 100,
      before: 41,
      phase: "refused",
      denied: true,
    });
    expect(journalParams({ phase: "end", deniedOnly: false }, null)).toEqual({ limit: 100, phase: "end" });
  });

  it("appends an older page without repeating a row", () => {
    const shown = [{ id: 9 }, { id: 8 }];
    expect(appendJournalPage(shown, [{ id: 8 }, { id: 7 }]).map((e) => e.id)).toEqual([9, 8, 7]);
  });

  it("formats bytes in binary units, and nothing for a row without them", () => {
    expect(formatBytes(null)).toBe("");
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(1536)).toBe("1.5 KiB");
    expect(formatBytes(50 * 1024 * 1024)).toBe("50 MiB");
  });

  it("shows query names, never values, and marks refusals and cut streams", () => {
    const refused = journalEntryView({
      phase: "refused",
      path: "/api/v1/applications",
      query_keys: ["token", "project"],
      status: 403,
      denied_reason: "query",
    });
    expect(refused.path).toBe("/api/v1/applications?token&project");
    expect(refused.tone).toBe("danger");
    expect(refused.reason).toBe("query");
    expect(refused.bytes).toBe("");
    const cut = journalEntryView({
      phase: "end",
      path: "/api/v1/stream/applications",
      query_keys: [],
      status: 200,
      bytes: 2048,
      duration_ms: 1200,
      denied_reason: "cut: killed",
    });
    expect(cut.tone).toBe("danger");
    expect(cut.duration).toBe("1200 ms");
    const ok = journalEntryView({ phase: "head", path: "/api/version", query_keys: [], status: 200 });
    expect(ok.tone).toBe("ok");
    expect(ok.status).toBe("200");
  });

  it("says plainly when nothing was recorded, and when filters hide everything", () => {
    expect(journalEmptyText({ phase: "", deniedOnly: false })).toBe("No requests recorded in the last 14 days.");
    expect(journalEmptyText({ phase: "head", deniedOnly: false })).toBe(
      "No requests match these filters in the last 14 days.",
    );
  });
});
