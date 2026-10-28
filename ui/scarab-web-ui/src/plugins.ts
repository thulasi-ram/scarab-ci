// Plugin installations (ADR-0072), as the UI has to reason about them. Pure —
// no DOM, no fetch — so the Settings › Plugins panel and its tests agree on
// what a form sends and what a badge says.
//
// A PluginInstallation is an org's approval of exactly (image digest,
// manifest). The MMP installs first-party plugins only, pinned by digest; the
// server refuses anything else with a 403 whose text is shown verbatim. These
// helpers only let the form say what is wrong before the round trip — where
// the two could disagree the server wins.

/** A PluginInstallation name: a DNS-1123 label, as the server checks it. */
const DNS1123_LABEL = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;
const SHA256_HEX = /^[0-9a-f]{64}$/;

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
 * The MMP installs only by digest: `<repository>@sha256:<64 lowercase hex>`.
 * A tag can change under the same name, and what an admin approves is the
 * digest. With `repository` given, the image must be that catalog
 * repository — anything else is a third-party image, which is off.
 */
export function imageError(image: string, repository?: string): string | null {
  if (image.length === 0) return null;
  const at = image.indexOf("@sha256:");
  if (at < 0) return "Pin the image by digest: append @sha256:<digest> — a tag can change under you.";
  const repo = image.slice(0, at);
  const digest = image.slice(at + "@sha256:".length);
  if (repository && repo !== repository) {
    return `The image must be ${repository}@sha256:… — any other repository is a third-party image, and those are off.`;
  }
  if (!SHA256_HEX.test(digest)) return "The digest must be 64 lowercase hex characters.";
  return null;
}

/** The install form as the dialog holds it. */
export type InstallForm = {
  plugin: string;
  name: string;
  image: string;
  /** Values for the manifest's `config_keys`; blank ones are not sent. */
  config: Record<string, string>;
};

/** What `POST /v1/orgs/{org}/plugins` carries — structurally the generated
 *  `InstallPluginRequest`. */
export type InstallBody = {
  plugin: string;
  name: string;
  image: string;
  config?: Record<string, string>;
};

/** The body for `form`, or the first reason it cannot be sent. */
export function toInstallBody(
  form: InstallForm,
  opts: { repository?: string; taken?: readonly string[] } = {},
): { body: InstallBody; error?: undefined } | { body?: undefined; error: string } {
  if (!form.plugin) return { error: "Pick a plugin to install." };
  const nameProblem = installNameError(form.name, opts.taken);
  if (nameProblem) return { error: nameProblem };
  const image = form.image.trim();
  if (!image) return { error: "An image is required." };
  const imageProblem = imageError(image, opts.repository);
  if (imageProblem) return { error: imageProblem };
  const config = Object.fromEntries(
    Object.entries(form.config)
      .map(([k, v]) => [k, v.trim()] as const)
      .filter(([, v]) => v !== ""),
  );
  return {
    body: {
      plugin: form.plugin,
      name: form.name,
      image,
      ...(Object.keys(config).length > 0 ? { config } : {}),
    },
  };
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
    title: "The isolation probe failed: this cluster does not enforce the plugin's NetworkPolicy, so it is not run.",
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

// --- PluginConnections (ADR-0072 §5) ----------------------------------------
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
