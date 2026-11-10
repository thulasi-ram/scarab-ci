import { describe, it, expect } from "vitest";
import {
  appendJournalPage,
  pluginChips,
  pluginManageHref,
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
  gateFieldLabel,
  manifestReview,
  manifestSourceError,
  runtimeBadge,
  shortDigest,
  stateBadge,
  storageClaimError,
  storageMountPathError,
  suggestedInstallName,
  toInstallBody,
  toManifestFetchBody,
  toStorageBody,
  imageRepositoryError,
  manifestImageFact,
  sourceLabel,
  toUpgradeBody,
  toUpgradeFetchBody,
  upgradeDiffCounts,
  upgradeDiffView,
  upgradeStepUpLine,
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

const COMMIT = "6e1d0c9b8a7f6e5d4c3b2a1f0e9d8c7b6a5f4e3d";
const SOURCE = `github.com/acme/scarab-plugin-argocd//plugin.json@${COMMIT}`;
const SHA = "b7e3".padEnd(64, "9");

describe("imageError", () => {
  it("is quiet while empty, and wants a digest", () => {
    expect(imageError("")).toBeNull();
    expect(imageError(`${REPO}:v1.2.0`)).toMatch(/@sha256:/);
    expect(imageError(IMAGE)).toBeNull();
  });

  it("refuses a short or upper-case digest, and a missing repository", () => {
    expect(imageError(`${REPO}@sha256:abc`)).toMatch(/64 lowercase hex/);
    expect(imageError(`${REPO}@sha256:${DIGEST.toUpperCase()}`)).toMatch(/64 lowercase hex/);
    expect(imageError(`@sha256:${DIGEST}`)).toMatch(/repository/);
  });

  it("accepts any repository pinned by digest", () => {
    expect(imageError(`registry.example.com/team/argocd@sha256:${DIGEST}`)).toBeNull();
    expect(imageError(`argocd@sha256:${DIGEST}`)).toBeNull();
  });
});

describe("manifest source", () => {
  it("wants a .json, .yaml or .yml file pinned to a full commit", () => {
    expect(manifestSourceError("")).toBeNull();
    expect(manifestSourceError(SOURCE)).toBeNull();
    expect(manifestSourceError(`git.acme.dev/platform/plugins//argocd/plugin.yml@${COMMIT}`)).toBeNull();
    expect(manifestSourceError(`github.com/acme/p//plugin.yaml@${COMMIT}`)).toBeNull();
    expect(manifestSourceError("github.com/acme/p//plugin.json@main")).toMatch(/full commit id/);
    expect(manifestSourceError(`github.com/acme/p//plugin.toml@${COMMIT}`)).toMatch(/\.json, \.yaml or \.yml/);
    expect(manifestSourceError(`github.com/acme/p/plugin.json@${COMMIT}`)).toMatch(/\/\//);
  });

  it("sends the connection only when one is picked", () => {
    expect(toManifestFetchBody({ source: ` https://${SOURCE} `, connection: "" })).toEqual({ body: { source: SOURCE } });
    expect(toManifestFetchBody({ source: SOURCE, connection: "gh-acme" })).toEqual({
      body: { source: SOURCE, connection: "gh-acme" },
    });
    expect(toManifestFetchBody({ source: "", connection: "" }).error).toMatch(/required/);
  });
});

describe("toInstallBody", () => {
  const form = { name: "argocd-prod", image: IMAGE, config: {}, storage: { claim: "", mount_path: "" } };
  const reviewed = { source: SOURCE, manifest_sha256: SHA };

  it("sends the reviewed source and hash, omitting config and storage when blank", () => {
    expect(toInstallBody({ ...form, config: { ARGOCD_UI_URL: "  " } }, reviewed)).toEqual({
      body: { name: "argocd-prod", image: IMAGE, source: SOURCE, manifest_sha256: SHA },
    });
  });

  it("sends the connection the manifest was read through", () => {
    expect(toInstallBody(form, { ...reviewed, connection: "gh-acme" }).body?.connection).toBe("gh-acme");
  });

  it("sends only the config values that are set, trimmed, and the storage when named", () => {
    const r = toInstallBody(
      {
        ...form,
        config: { ARGOCD_UI_URL: " https://argo.example ", OWNER_ANNOTATION: "" },
        storage: { claim: "argocd-state", mount_path: "" },
      },
      reviewed,
    );
    expect(r.body?.config).toEqual({ ARGOCD_UI_URL: "https://argo.example" });
    expect(r.body?.storage).toEqual({ claim: "argocd-state" });
  });

  it("reports the first problem: no review, then name, image and storage", () => {
    expect(toInstallBody(form, null).error).toMatch(/review it first/);
    expect(toInstallBody({ ...form, name: "Bad" }, reviewed).error).toMatch(/not a valid name/);
    expect(toInstallBody({ ...form, image: "" }, reviewed).error).toMatch(/image is required/);
    expect(toInstallBody({ ...form, image: REPO }, reviewed).error).toMatch(/@sha256:/);
    expect(toInstallBody({ ...form, storage: { claim: "", mount_path: "/data" } }, reviewed).error).toMatch(
      /claim's name/i,
    );
    expect(toInstallBody({ ...form, name: "argocd" }, reviewed, { taken: ["argocd"] }).error).toMatch(
      /already installed/,
    );
  });
});

describe("manifestReview", () => {
  const fetched = {
    manifest_sha256: SHA,
    manifest: {
      plugin: "argocd",
      capabilities: ["use_connection", "resolve_gates", "some_new_thing"],
      connections: [
        {
          name: "argocd",
          auth_schemes: ["bearer"],
          read_rules: [
            { method: "GET", path: "/api/version", query: [] },
            { method: "GET", path: "/api/v1/applications/*", query: ["appNamespace", "project"] },
          ],
        },
      ],
      config_keys: ["ARGOCD_UI_URL"],
      gates: [
        {
          name: "wait-live",
          description: "Pass when the Application is synced.",
          inputs: [
            { name: "app", type: "string", required: true },
            { name: "timeout", type: "number", required: false },
          ],
          outputs: [{ name: "revision", type: "string", required: true }],
        },
      ],
    },
    installed: [
      { name: "argocd", image: IMAGE, manifest_sha256: SHA },
      { name: "argocd-us", image: IMAGE, manifest_sha256: "91c0".padEnd(64, "2") },
    ],
  };

  it("lists everything the manifest asks for, in its order", () => {
    const r = manifestReview(fetched);
    expect(r.plugin).toBe("argocd");
    expect(r.capabilities.map((c) => c.key)).toEqual(["use_connection", "resolve_gates", "some_new_thing"]);
    // A capability this UI does not know still shows, by its key.
    expect(r.capabilities[2]!.label).toBe("some new thing");
    expect(r.connections).toEqual([
      {
        name: "argocd",
        auth: "bearer",
        rules: ["GET /api/version", "GET /api/v1/applications/* ?appNamespace&project"],
      },
    ]);
    expect(r.configKeys).toEqual(["ARGOCD_UI_URL"]);
    expect(r.gates).toEqual([
      {
        name: "wait-live",
        description: "Pass when the Application is synced.",
        inputs: ["app: string", "timeout: number?"],
        outputs: ["revision: string"],
      },
    ]);
  });

  it("says which installations approved this exact manifest", () => {
    expect(manifestReview(fetched).installed).toEqual([
      { name: "argocd", image: IMAGE, sameManifest: true },
      { name: "argocd-us", image: IMAGE, sameManifest: false },
    ]);
  });

  it("labels a gate field and suggests a free installation name", () => {
    expect(gateFieldLabel({ name: "app", type: "string", required: true })).toBe("app: string");
    expect(suggestedInstallName("argocd", [])).toBe("argocd");
    expect(suggestedInstallName("argocd", ["argocd", "argocd-2"])).toBe("argocd-3");
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

describe("the list and manage pages", () => {
  it("tags a killed installation killed, and any other by what its runtime reports", () => {
    expect(pluginChips({ state: "killed", runtime_status: "stopped" }).map((b) => b.label)).toEqual(["killed"]);
    expect(pluginChips({ state: "enabled", runtime_status: "running" }).map((b) => [b.label, b.tone])).toEqual([
      ["running", "ok"],
    ]);
    expect(pluginChips({ state: "enabled", runtime_status: "probe_failed" }).map((b) => [b.label, b.tone])).toEqual([
      ["probe failed", "danger"],
    ]);
  });
  it("links an installation's manage page by its id", () => {
    expect(pluginManageHref("acme", { id: "4f2a1c9e7b" })).toBe("/orgs/acme/settings/plugins/4f2a1c9e7b/manage");
  });
});

describe("the recorded source and the manifest's image", () => {
  const REPO = "ghcr.io/acme/argocd";
  const digest = (c: string) => `@sha256:${c.repeat(64)}`;
  it("says when the source was not recorded", () => {
    expect(sourceLabel({ source: null })).toBe("source not recorded");
    expect(sourceLabel({ source: "github.com/a/b//m.json@x" })).toBe("github.com/a/b//m.json@x");
  });
  it("flags a manifest that names no image, and states the one that does", () => {
    expect(manifestImageFact({ image: null }).warn).toBe(true);
    expect(manifestImageFact({}).text).toMatch(/does not name its image/);
    expect(manifestImageFact({ image: REPO })).toEqual({ text: `runs only an image of ${REPO}`, warn: false });
  });
  it("refuses an image of another repository than the one the manifest names", () => {
    expect(imageRepositoryError(`${REPO}${digest("a")}`, REPO)).toBeNull();
    expect(imageRepositoryError(`ghcr.io/evil/argocd${digest("a")}`, REPO)).toMatch(/only images of ghcr.io\/acme\/argocd/);
    expect(imageRepositoryError(`ghcr.io/evil/argocd${digest("a")}`, null)).toBeNull();
    // A malformed image is imageError's to report.
    expect(imageRepositoryError("ghcr.io/evil/argocd:v1", REPO)).toBeNull();
  });
});

describe("the upgrade dialog", () => {
  const commit = "c".repeat(40);
  const recorded = `github.com/acme/argocd//manifest.json@${"1".repeat(40)}`;
  it("reads the recorded source at a bare commit, or another whole source", () => {
    expect(toUpgradeFetchBody({ ref: commit, connection: "", image: "" }, recorded)).toEqual({
      body: { commit },
    });
    const other = `github.com/acme/other//manifest.json@${commit}`;
    expect(toUpgradeFetchBody({ ref: `https://${other}`, connection: "forge-1", image: "" }, recorded)).toEqual({
      body: { source: other, connection: "forge-1" },
    });
  });
  it("needs the whole source when none was recorded, and checks the image shape", () => {
    expect(toUpgradeFetchBody({ ref: commit, connection: "", image: "" }, null).error).toMatch(/not recorded/);
    expect(toUpgradeFetchBody({ ref: "", connection: "", image: "" }, null).error).toMatch(/source/);
    expect(toUpgradeFetchBody({ ref: commit, connection: "", image: "x:latest" }, recorded).error).toMatch(/digest/);
    expect(toUpgradeFetchBody({ ref: commit, connection: "", image: `x@sha256:${"a".repeat(64)}` }, recorded)).toEqual({
      body: { commit, image: `x@sha256:${"a".repeat(64)}` },
    });
  });
  it("sends the reviewed hashes and refuses an image the new manifest does not name", () => {
    const reviewed = {
      source: `${recorded.slice(0, -40)}${commit}`,
      connection: null,
      manifest_sha256: "n".repeat(64),
      current_manifest_sha256: "o".repeat(64),
      manifest: { image: "ghcr.io/acme/argocd" },
    };
    expect(toUpgradeBody("x", null).error).toMatch(/review/);
    expect(toUpgradeBody(`ghcr.io/evil/argocd@sha256:${"a".repeat(64)}`, reviewed).error).toMatch(/only images/);
    expect(toUpgradeBody(`ghcr.io/acme/argocd@sha256:${"a".repeat(64)}`, reviewed)).toEqual({
      body: {
        image: `ghcr.io/acme/argocd@sha256:${"a".repeat(64)}`,
        source: reviewed.source,
        manifest_sha256: reviewed.manifest_sha256,
        expected_current_sha256: reviewed.current_manifest_sha256,
      },
    });
  });
  it("lists the diff's non-empty groups in a fixed order and sums them", () => {
    const g = (added: string[], removed: string[], unchanged: string[]) => ({ added, removed, unchanged });
    const groups = upgradeDiffView({
      image: g([], [], ["ghcr.io/acme/argocd"]),
      read_rules: g(["argocd: GET /api/v1/account"], ["argocd: GET /api/version"], ["argocd: GET /x"]),
      capabilities: g([], [], ["use_connection"]),
      gates: g([], [], []),
    });
    expect(groups.map((x) => x.key)).toEqual(["capabilities", "read_rules", "image"]);
    expect(groups[1]).toEqual({
      key: "read_rules",
      title: "Read rules",
      added: ["argocd: GET /api/v1/account"],
      removed: ["argocd: GET /api/version"],
      unchanged: ["argocd: GET /x"],
    });
    expect(upgradeDiffCounts(groups)).toEqual({ added: 1, removed: 1, unchanged: 3 });
  });
  it("says whether a passkey is needed from the server's answer", () => {
    expect(upgradeStepUpLine({ needs_step_up: true })).toMatch(/passkey/);
    expect(upgradeStepUpLine({ needs_step_up: false })).toMatch(/no passkey/);
  });
});
