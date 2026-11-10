// Plugin installations, as the UI has to reason about them. Pure — no DOM,
// no fetch — so the Settings › Plugins panel and its tests agree on what a
// form sends and what a badge says.
//
// A PluginInstallation is an org's approval of exactly (image digest,
// manifest). Every plugin installs the same way: the admin names the
// manifest's git source, reads what it asks for, and installs an image pinned
// by digest. Who publishes it changes nothing. The server may still refuse an
// install (a 403 or 409), and its text is shown verbatim. These helpers only
// let the form say what is wrong before the round trip — where the two could
// disagree the server wins.
import { pinnedSourceError } from "./libraries";

/** A PluginInstallation name: a DNS-1123 label, as the server checks it. */
const DNS1123_LABEL = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;
const SHA256_HEX = /^[0-9a-f]{64}$/;

/** Why `source` is not a plugin manifest source
 *  (`<host>/<owner>/<repo>//<path>@<commit>`, the path `.json`, `.yaml` or
 *  `.yml`), or `null`. An empty field is not an error yet. */
export function manifestSourceError(source: string): string | null {
  return pinnedSourceError(source, {
    ext: /\.(json|ya?ml)$/,
    example: "scarab-plugin//manifest.json",
    hint: "The manifest is a repo-relative .json, .yaml or .yml path.",
  });
}

/** The manifest fetch form as the dialog holds it. */
export type ManifestFetchForm = {
  source: string;
  /** A ForgeConnection id; empty = read anonymously. */
  connection: string;
};

/** What `POST …/plugin-fetch` carries — structurally the generated
 *  `FetchPluginManifestRequest`. */
export type ManifestFetchBody = { source: string; connection?: string };

/** The body for `form`, or the first reason it cannot be sent. */
export function toManifestFetchBody(
  form: ManifestFetchForm,
): { body: ManifestFetchBody; error?: undefined } | { body?: undefined; error: string } {
  const source = form.source.trim().replace(/^https:\/\//, "");
  if (!source) return { error: "A source is required." };
  const problem = manifestSourceError(source);
  if (problem) return { error: problem };
  const connection = form.connection.trim();
  return { body: connection ? { source, connection } : { source } };
}

/** Why `name` cannot name a new installation, or `null` if it can. `taken`
 *  is the org's current installations; a RETIRED name (uninstalled once) is
 *  invisible here, so the server's 409 is the only answer to that one. */
export function installNameError(name: string, taken: readonly string[] = []): string | null {
  if (name.length === 0) return "A name is required.";
  if (!DNS1123_LABEL.test(name)) {
    return `"${name}" is not a valid name: lowercase letters, digits and "-", at most 63, starting and ending with a letter or digit.`;
  }
  if (taken.includes(name)) return `${name} is already installed — names are never reused.`;
  return null;
}

/**
 * Why `image` cannot be installed, or `null` if it can (or is still empty —
 * the form stays unsubmittable then, without shouting at a blank field).
 *
 * A plugin installs only by digest: `<repository>@sha256:<64 lowercase hex>`,
 * from any repository. A tag can change under the same name, and what an
 * admin approves is the digest.
 */
export function imageError(image: string): string | null {
  if (image.length === 0) return null;
  const at = image.indexOf("@sha256:");
  if (at < 0) return "Pin the image by digest: append @sha256:<digest> — a tag can change under you.";
  const repo = image.slice(0, at);
  const digest = image.slice(at + "@sha256:".length);
  if (repo === "" || /\s/.test(repo)) return "Name the image repository before @sha256:.";
  if (!SHA256_HEX.test(digest)) return "The digest must be 64 lowercase hex characters.";
  return null;
}

/** The install form as the dialog holds it. */
export type InstallForm = {
  name: string;
  image: string;
  /** Values for the manifest's `config_keys`; blank ones are not sent. */
  config: Record<string, string>;
  /** Blank claim = no storage. */
  storage: StorageForm;
};

/** The reviewed manifest an install names: what `POST …/plugin-fetch`
 *  returned, and the connection it was read through. */
export type ReviewedManifest = {
  source: string;
  manifest_sha256: string;
  connection?: string;
};

/** What `POST /v1/orgs/{org}/plugins` carries — structurally the generated
 *  `InstallPluginRequest`. */
export type InstallBody = {
  name: string;
  image: string;
  source: string;
  manifest_sha256: string;
  connection?: string;
  config?: Record<string, string>;
  storage?: StorageBody;
};

/** The body for `form` against the manifest just reviewed, or the first
 *  reason it cannot be sent. */
export function toInstallBody(
  form: InstallForm,
  reviewed: ReviewedManifest | null,
  opts: { taken?: readonly string[] } = {},
): { body: InstallBody; error?: undefined } | { body?: undefined; error: string } {
  if (!reviewed) return { error: "Read the manifest and review it first." };
  const nameProblem = installNameError(form.name, opts.taken);
  if (nameProblem) return { error: nameProblem };
  const image = form.image.trim();
  if (!image) return { error: "An image is required." };
  const imageProblem = imageError(image);
  if (imageProblem) return { error: imageProblem };
  let storage: StorageBody | undefined;
  if (form.storage.claim.trim() !== "" || form.storage.mount_path.trim() !== "") {
    const built = toStorageBody(form.storage);
    if (!built.body) return { error: built.error };
    storage = built.body;
  }
  const config = Object.fromEntries(
    Object.entries(form.config)
      .map(([k, v]) => [k, v.trim()] as const)
      .filter(([, v]) => v !== ""),
  );
  return {
    body: {
      name: form.name,
      image,
      source: reviewed.source,
      manifest_sha256: reviewed.manifest_sha256,
      ...(reviewed.connection ? { connection: reviewed.connection } : {}),
      ...(Object.keys(config).length > 0 ? { config } : {}),
      ...(storage ? { storage } : {}),
    },
  };
}

// ── Manifest review ──────────────────────────────────────────────────────
// What the install dialog shows of a fetched manifest before an admin
// approves it: every capability, connection slot, config key and gate, and
// the org's installations of the same plugin.

/** What a capability lets the plugin do, as the review shows it. */
const CAPABILITY: Record<string, string> = {
  use_connection: "send reads through the broker on its connections",
  resolve_gates: "pass or fail the gates it defines",
  approve_gates: "approve gates",
  report_deployments: "report deployments",
  subscribe_events: "subscribe to events",
  offer_actions: "offer actions",
  contribute_ui: "contribute UI",
};

/** A gate input or output: `name: type`, `?` when a Pipeline may omit it. */
export function gateFieldLabel(f: { name: string; type: string; required: boolean }): string {
  return `${f.name}: ${f.type}${f.required ? "" : "?"}`;
}

/** An installation of the same plugin, and whether it approved this exact
 *  manifest. */
export type InstalledView = { name: string; image: string; sameManifest: boolean };

/** A fetched manifest as the review lists it. */
export type ManifestReview = {
  plugin: string;
  capabilities: { key: string; label: string }[];
  connections: { name: string; auth: string; rules: string[] }[];
  configKeys: string[];
  gates: { name: string; description: string; inputs: string[]; outputs: string[] }[];
  installed: InstalledView[];
};

/** The review of `fetched`: everything its manifest asks for, in the
 *  manifest's order, and the org's installations of the same plugin. */
export function manifestReview(fetched: {
  manifest: {
    plugin: string;
    capabilities: readonly string[];
    connections: readonly {
      name: string;
      auth_schemes: readonly string[];
      read_rules: readonly { method: string; path: string; query: readonly string[] }[];
    }[];
    config_keys: readonly string[];
    gates: readonly {
      name: string;
      description: string;
      inputs: readonly { name: string; type: string; required: boolean }[];
      outputs: readonly { name: string; type: string; required: boolean }[];
    }[];
  };
  manifest_sha256: string;
  installed: readonly { name: string; image: string; manifest_sha256: string }[];
}): ManifestReview {
  const m = fetched.manifest;
  return {
    plugin: m.plugin,
    capabilities: m.capabilities.map((key) => ({ key, label: CAPABILITY[key] ?? key.replace(/_/g, " ") })),
    connections: m.connections.map((c) => ({
      name: c.name,
      auth: c.auth_schemes.join(", "),
      rules: c.read_rules.map(ruleLabel),
    })),
    configKeys: [...m.config_keys],
    gates: m.gates.map((g) => ({
      name: g.name,
      description: g.description,
      inputs: g.inputs.map(gateFieldLabel),
      outputs: g.outputs.map(gateFieldLabel),
    })),
    installed: fetched.installed.map((i) => ({
      name: i.name,
      image: i.image,
      sameManifest: i.manifest_sha256 === fetched.manifest_sha256,
    })),
  };
}

/** A name for a new installation: the plugin's own name, or `<plugin>-2`,
 *  `-3`, … while that is taken. */
export function suggestedInstallName(plugin: string, taken: readonly string[]): string {
  if (!taken.includes(plugin)) return plugin;
  for (let n = 2; ; n++) {
    const name = `${plugin}-${n}`;
    if (!taken.includes(name)) return name;
  }
}

/** A badge's colour, in the StatusBadge palette's terms: `ok` = a healthy
 *  terminal, `motion` = in progress, `danger` = needs an operator, `muted` =
 *  not running and not a fault. */
export type BadgeTone = "ok" | "motion" | "danger" | "muted";
export type Badge = { label: string; tone: BadgeTone; title: string };

/** Whether core accepts the installation's broker calls. */
export function stateBadge(state: string): Badge {
  return state === "killed"
    ? {
        label: "killed",
        tone: "danger",
        title: "Killed: its broker calls are refused until an admin enables it again.",
      }
    : { label: "enabled", tone: "ok", title: "Enabled: its broker calls are accepted." };
}

const RUNTIME: Record<string, Badge> = {
  pending: { label: "pending", tone: "motion", title: "Waiting for the plugin runtime to start it." },
  probing: {
    label: "probing",
    tone: "motion",
    title: "Running the isolation probe: the plugin must reach the broker and nothing else.",
  },
  running: { label: "running", tone: "ok", title: "Running, and its isolation probe passed." },
  probe_failed: {
    label: "probe failed",
    tone: "danger",
    title:
      "Held by the isolation probe: it failed (this cluster does not enforce the plugin's NetworkPolicy), or none has passed for 12 hours. The message says which.",
  },
  error: { label: "error", tone: "danger", title: "The plugin runtime could not run it." },
  stopped: { label: "stopped", tone: "muted", title: "Not running." },
};

/** What the plugin runtime last observed of the workload. Unknown values
 *  (a newer server) read muted rather than healthy. */
export function runtimeBadge(status: string): Badge {
  return RUNTIME[status] ?? { label: status.replace(/_/g, " "), tone: "muted", title: status };
}

/** `…@sha256:4f2a1c9e7b3d…` → `sha256:4f2a1c9e7b3d`; a bare hex digest is
 *  shortened the same way. Anything without a digest is returned as is. */
export function shortDigest(imageOrDigest: string, chars = 12): string {
  const at = imageOrDigest.indexOf("sha256:");
  const hex = at >= 0 ? imageOrDigest.slice(at + "sha256:".length) : imageOrDigest;
  if (!/^[0-9a-f]{12,}$/.test(hex)) return imageOrDigest;
  return `sha256:${hex.slice(0, chars)}`;
}

/** The repository part of `<repository>@sha256:<digest>`. */
export function imageRepository(image: string): string {
  const at = image.indexOf("@");
  return at >= 0 ? image.slice(0, at) : image;
}

// --- PluginConnections ----------------------------------------
// An admin binds each manifest connection slot to the ONE destination the
// broker may send the plugin's requests to, and the credential it adds. The
// credential is write-only: the server never returns it, so the form only
// ever says "set" / "not set", and every save sends it again — a new
// destination never inherits a credential entered for another.

/** The bind form as the panel holds it. */
export type ConnectionForm = {
  base_url: string;
  /** Always typed afresh; never prefilled (the server cannot give it back). */
  credential: string;
  /** PEM certificates trusted INSTEAD of the platform roots; blank = none. */
  ca_pem: string;
};

/** What `PUT …/plugins/{name}/connections/{slot}` carries — structurally the
 *  generated `PutPluginConnectionRequest`. */
export type ConnectionBody = { base_url: string; credential: string; ca_pem?: string };

/** Why `url` cannot be a connection's destination, or `null` if it can (or
 *  is still empty). Mirrors the server: an absolute http(s) URL with a host,
 *  no user/password, query or fragment. */
export function connectionBaseUrlError(url: string): string | null {
  const raw = url.trim();
  if (raw === "") return null;
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    return "Not an absolute URL — it is https://host[:port][/prefix].";
  }
  if (u.protocol !== "https:" && u.protocol !== "http:") return "The destination must be http or https.";
  if (!u.hostname) return "The destination needs a host.";
  if (u.username || u.password) return "No user or password in the URL — the credential goes in its own field.";
  if (u.search || u.hash || raw.includes("?") || raw.includes("#")) {
    return "No query or fragment: the plugin's own request supplies those.";
  }
  return null;
}

/** Plain http: the credential would cross the network in the clear. Allowed
 *  (an in-cluster Service, say), never silently. */
export function isInsecureHttp(url: string): boolean {
  return url.trim().toLowerCase().startsWith("http://");
}

export const INSECURE_HTTP_WARNING =
  "Plain http: the broker sends the credential unencrypted to this destination. Use it only for a destination inside a network you trust.";

/** Why `credential` cannot be sent as a bearer token, or `null`. */
export function credentialError(credential: string): string | null {
  const t = credential.trim();
  if (t === "") return "Enter the credential — every save sends it again, so a destination never keeps one entered for another.";
  if (!/^[\x21-\x7e]+$/.test(t)) return "A bearer token is visible ASCII with no spaces.";
  return null;
}

/** The body for `form`, or the first reason it cannot be sent. */
export function toConnectionBody(
  form: ConnectionForm,
): { body: ConnectionBody; error?: undefined } | { body?: undefined; error: string } {
  const base_url = form.base_url.trim();
  if (!base_url) return { error: "Enter the destination's base URL." };
  const urlProblem = connectionBaseUrlError(base_url);
  if (urlProblem) return { error: urlProblem };
  const credProblem = credentialError(form.credential);
  if (credProblem) return { error: credProblem };
  const ca = form.ca_pem.trim();
  if (ca && isInsecureHttp(base_url)) return { error: "A CA applies to an https destination only." };
  if (ca && !ca.includes("-----BEGIN CERTIFICATE-----")) {
    return { error: "The CA must be PEM certificates (-----BEGIN CERTIFICATE-----…)." };
  }
  return {
    body: { base_url, credential: form.credential.trim(), ...(ca ? { ca_pem: ca } : {}) },
  };
}

/** A read rule as the approval screen shows it: `GET /api/v1/applications/*
 *  ?appNamespace&project` — the method, the path pattern, and the only query
 *  keys a request under it may carry (none listed = no query at all). */
export function ruleLabel(r: { method: string; path: string; query: readonly string[] }): string {
  const base = `${r.method} ${r.path}`;
  return r.query.length > 0 ? `${base} ?${r.query.join("&")}` : base;
}

/** A connection slot's credential, as a badge: the only thing ever shown of
 *  it. A binding whose secret went missing reads "not set" too. */
export function credentialBadge(c: { configured: boolean; credential_set: boolean }): Badge {
  if (!c.configured) {
    return {
      label: "not bound",
      tone: "muted",
      title: "No destination or credential yet: the broker refuses the plugin's requests on this connection.",
    };
  }
  return c.credential_set
    ? { label: "credential set", tone: "ok", title: "Stored in the secret store. It is never shown again." }
    : {
        label: "credential not set",
        tone: "danger",
        title: "The binding's credential is missing from the secret store: bind it again.",
      };
}

// ── Storage ────────────────────────────────────────────────
// A plugin may mount ONE PersistentVolumeClaim that an operator created in
// the plugin namespace. Scarab mounts it; it never creates, backs up or reads
// it. The server is the authority on these rules; the form mirrors them so a
// typo is caught before the request.

/** Where the claim is mounted when no path is given. */
export const DEFAULT_STORAGE_MOUNT_PATH = "/data";

/** What the panel says about storage, wherever it shows it. */
export const STORAGE_NOTE =
  "Scarab mounts this claim read-write into the plugin's Pod. It does not create, back up or read it, and the data is outside what Scarab keeps.";

/** Paths the plugin runtime or the kubelet mounts: a storage mount path may
 *  not be one of them, under one, or above one. */
const RESERVED_MOUNT_PATHS = [
  "/tmp",
  "/var/run/secrets",
  "/run/secrets",
  "/etc/hosts",
  "/etc/hostname",
  "/etc/resolv.conf",
  "/dev",
  "/proc",
  "/sys",
];

/** Why `claim` is not a PersistentVolumeClaim name, or `null`. */
export function storageClaimError(claim: string): string | null {
  const c = claim.trim();
  if (c === "") return "Enter the PersistentVolumeClaim's name.";
  const label = /^[a-z0-9]([-a-z0-9]*[a-z0-9])?$/;
  if (c.length > 253 || !c.split(".").every((l) => label.test(l))) {
    return "Not a claim name: lowercase letters, digits, \"-\" and \".\", starting and ending with a letter or digit.";
  }
  return null;
}

/** Why `path` cannot be the mount path, or `null` (blank = the default). */
export function storageMountPathError(path: string): string | null {
  const p = path.trim().replace(/(.)\/+$/, "$1");
  if (p === "") return null;
  if (!p.startsWith("/")) return "The mount path must be absolute.";
  if (p === "/") return "The mount path cannot be /.";
  if (p.length > 256) return "The mount path is longer than 256 characters.";
  const parts = p.slice(1).split("/");
  if (parts.some((x) => x === "" || x === "." || x === "..")) {
    return "No empty, \".\" or \"..\" parts in the mount path.";
  }
  if (!parts.every((x) => /^[A-Za-z0-9._-]+$/.test(x))) {
    return "The mount path may hold only letters, digits, \".\", \"_\", \"-\" and \"/\".";
  }
  const within = (inner: string, outer: string) => inner === outer || inner.startsWith(`${outer}/`);
  const hit = RESERVED_MOUNT_PATHS.find((r) => within(p, r) || within(r, p));
  if (hit) return `The mount path overlaps ${hit}, which the plugin runtime or the kubelet mounts.`;
  return null;
}

/** The storage form as the panel holds it. */
export type StorageForm = { claim: string; mount_path: string };

/** What `PUT …/plugins/{name}/storage` carries. */
export type StorageBody = { claim: string; mount_path?: string };

/** The body for `form`, or the first reason it cannot be sent. */
export function toStorageBody(
  form: StorageForm,
): { body: StorageBody; error?: undefined } | { body?: undefined; error: string } {
  const claimProblem = storageClaimError(form.claim);
  if (claimProblem) return { error: claimProblem };
  const pathProblem = storageMountPathError(form.mount_path);
  if (pathProblem) return { error: pathProblem };
  const mount_path = form.mount_path.trim();
  return { body: { claim: form.claim.trim(), ...(mount_path ? { mount_path } : {}) } };
}

// ── Broker journal ───────────────────────────────────────────────────────
// Every request a plugin sends to the CredentialBroker is journaled: one
// `refused` row when the broker answered it itself, or a `head` row (the
// upstream's response head arrived) and an `end` row (its body ended) when
// it was forwarded. Rows are kept 14 days. Query values are never returned,
// only the parameter names.

/** The phase filter as the panel holds it; `""` = every phase. */
export type JournalPhaseFilter = "" | "refused" | "head" | "end";
export type JournalFilter = { phase: JournalPhaseFilter; deniedOnly: boolean };

/** The query parameters of `GET …/plugins/{name}/broker-journal`. */
export type JournalParams = {
  limit?: number;
  before?: number;
  phase?: "refused" | "head" | "end";
  denied?: boolean;
};

export const JOURNAL_PAGE = 100;

/** The parameters for one page under `filter`; `before` continues from the
 *  previous page's `next_before`. Unset filters are not sent. */
export function journalParams(filter: JournalFilter, before?: number | null): JournalParams {
  return {
    limit: JOURNAL_PAGE,
    ...(before != null ? { before } : {}),
    ...(filter.phase ? { phase: filter.phase } : {}),
    ...(filter.deniedOnly ? { denied: true } : {}),
  };
}

/** `shown` followed by the rows of an older page, each id once (a page read
 *  twice, from a double click, adds nothing). */
export function appendJournalPage<T extends { id: number }>(shown: readonly T[], older: readonly T[]): T[] {
  const seen = new Set(shown.map((e) => e.id));
  return [...shown, ...older.filter((e) => !seen.has(e.id))];
}

/** A byte count for a table cell: `512 B`, `1.5 KiB`, `3.2 MiB`; `""` when
 *  the row has none. */
export function formatBytes(n: number | null | undefined): string {
  if (n == null) return "";
  if (n < 1024) return `${n} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let v = n / 1024;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u += 1;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[u]}`;
}

/** A journal row as the table shows it. */
export type JournalEntryView = {
  phase: string;
  tone: BadgeTone;
  /** The path as the plugin sent it, then the query's parameter names. */
  path: string;
  rule: string;
  status: string;
  bytes: string;
  duration: string;
  /** Why it was refused or cut short; `""` otherwise. */
  reason: string;
};

/** The table's view of `e`. A refused row and a cut stream read as danger;
 *  a 2xx as ok; anything else muted. */
export function journalEntryView(e: {
  phase: string;
  path: string;
  query_keys: readonly string[];
  rule?: string | null;
  status?: number | null;
  bytes?: number | null;
  duration_ms?: number | null;
  denied_reason?: string | null;
}): JournalEntryView {
  const reason = e.denied_reason ?? "";
  const tone: BadgeTone =
    e.phase === "refused" || reason !== ""
      ? "danger"
      : e.status != null && e.status >= 200 && e.status < 300
        ? "ok"
        : "muted";
  return {
    phase: e.phase,
    tone,
    path: e.query_keys.length > 0 ? `${e.path}?${e.query_keys.join("&")}` : e.path,
    rule: e.rule ?? "",
    status: e.status != null ? String(e.status) : "",
    bytes: formatBytes(e.bytes),
    duration: e.duration_ms != null ? `${e.duration_ms} ms` : "",
    reason,
  };
}

/** The empty-table text: plain when nothing is filtered. */
export function journalEmptyText(filter: JournalFilter): string {
  return filter.phase || filter.deniedOnly
    ? "No requests match these filters in the last 14 days."
    : "No requests recorded in the last 14 days.";
}
