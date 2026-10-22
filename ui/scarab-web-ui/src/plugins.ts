// Plugin installations (ADR-0072, Deployments MMP d1), as the UI has to reason
// about them. Pure — no DOM, no fetch — so the Settings › Plugins panel, the
// Environment dialog's source picker, the Deployments board and their tests
// agree on what a form sends and what a badge says.
//
// A PluginInstallation is an org's approval of exactly (image digest,
// manifest). The MMP installs first-party plugins only, pinned by digest; the
// server refuses anything else with a 403 whose text is shown verbatim. These
// helpers only let the form say what is wrong before the round trip — where
// the two could disagree the server wins.
import { toForm, toPutBody, type OrgEnvironmentBody, type OrgEnvironmentLike } from "./org-environments";

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

/** Whether core accepts the installation's reports and broker calls. */
export function stateBadge(state: string): Badge {
  return state === "killed"
    ? {
        label: "killed",
        tone: "danger",
        title: "Killed: its reports and broker calls are refused until an admin enables it again.",
      }
    : { label: "enabled", tone: "ok", title: "Enabled: its reports and broker calls are accepted." };
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

/**
 * Which org Environment an unmapped grouping most likely belongs to: the one
 * whose name IS the grouping (exactly, then ignoring case — Argo projects
 * are commonly named after the environment). `null` when nothing matches:
 * guessing further would route reports somewhere nobody chose.
 */
export function suggestEnvironment(
  grouping: string,
  envs: readonly Pick<OrgEnvironmentLike, "name">[],
): string | null {
  const exact = envs.find((e) => e.name === grouping);
  if (exact) return exact.name;
  const folded = envs.find((e) => e.name.toLowerCase() === grouping.toLowerCase());
  return folded?.name ?? null;
}

/**
 * The full PUT body that adds `installation / grouping` as a source of
 * `env`, keeping everything else the Environment has (the PUT is a full
 * replace). An error when the source is already listed there.
 */
export function withSource(
  env: OrgEnvironmentLike,
  installation: string,
  grouping: string,
): { body: OrgEnvironmentBody; error?: undefined } | { body?: undefined; error: string } {
  const form = toForm(env);
  if (form.sources.some((s) => s.installation === installation && s.vendor_grouping === grouping)) {
    return { error: `${env.name} already lists ${installation} / ${grouping}.` };
  }
  return toPutBody({ ...form, sources: [...form.sources, { installation, vendor_grouping: grouping }] });
}

/** One option of the Environment dialog's installation picker. */
export type InstallationChoice = { value: string; label: string };

/**
 * The installation picker's options: every installation in the org (a killed
 * one is still a valid source — killing is not uninstalling), plus the row's
 * current value if it names something not installed, so an existing source
 * is shown rather than silently blanked (the server will refuse to save it).
 */
export function installationChoices(
  installed: readonly { name: string; state: string }[],
  current: string,
): InstallationChoice[] {
  const choices = installed
    .map((i) => ({ value: i.name, label: i.state === "killed" ? `${i.name} (killed)` : i.name }))
    .sort((a, b) => a.value.localeCompare(b.value));
  if (current && !installed.some((i) => i.name === current)) {
    choices.unshift({ value: current, label: `${current} (not installed)` });
  }
  return choices;
}

/** A DeploymentTarget's key on the board: `source/target_id`. */
export const targetKey = (source: string, id: string) => `${source}/${id}`;

/** Owners per target, from an Environment's reported DeploymentTargets. A
 *  target whose source has not reported owners is absent, not empty. */
export function targetOwners(
  targets: readonly { source: string; target_id: string; owner_projects: readonly string[] }[],
): Map<string, readonly string[]> {
  const owners = new Map<string, readonly string[]>();
  for (const t of targets) {
    if (t.owner_projects.length > 0) owners.set(targetKey(t.source, t.target_id), t.owner_projects);
  }
  return owners;
}

/**
 * The note a board row carries when its target's source now routes it
 * somewhere else — the display term **Moved** (ADR-0070 §3), not a status.
 * The row's history stays; what was live here is no longer what this
 * Environment runs.
 */
export function movedAwayNote(moved: { now?: string | null }): { label: string; title: string } {
  return moved.now
    ? {
        label: `moved — now: ${moved.now}`,
        title: `Its source now reports this target into ${moved.now}. Its history here stays; a request for it here will stall, because its reports land in ${moved.now}.`,
      }
    : {
        label: "moved — unmapped",
        title:
          "No environment lists its source's grouping any more, so its reports are dropped. Its history here stays.",
      };
}

/** What adding `installation / grouping` to `env` tells the admin: the
 *  targets already reported under it are not on the board yet — the plugin
 *  re-sends them when it next polls (within about a minute). */
export function sourceAddedNotice(env: string, installation: string, grouping: string): string {
  return `${env} now takes ${installation} / ${grouping}. Targets it already reports there appear on the board within ~60 s, as the plugin re-sends their current state.`;
}

/** An installation's use of its per-installation caps, as one line. `warn`
 *  from 90%: past a cap, a NEW target (or unmapped grouping) is refused. */
export function usageLine(u: {
  targets: number;
  max_targets: number;
  unmapped_groupings: number;
  max_unmapped_groupings: number;
}): { label: string; warn: boolean; title: string } {
  const n = (x: number) => x.toLocaleString("en-US");
  const warn = u.targets >= 0.9 * u.max_targets || u.unmapped_groupings >= 0.9 * u.max_unmapped_groupings;
  return {
    label: `${n(u.targets)} / ${n(u.max_targets)} targets · ${n(u.unmapped_groupings)} / ${n(u.max_unmapped_groupings)} unmapped groupings`,
    warn,
    title:
      "Per-installation caps: past one, a report about a NEW target (or a new unmapped grouping) is rejected; targets it already reports keep reporting.",
  };
}
