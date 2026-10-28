// Org-level Environment governance (ADR-0071 §1-3, §8, §10) as the UI has to
// reason about it. Pure — no DOM, no fetch — so Settings › Environments, the
// repository's Environments tab and their tests agree on what a form sends
// and what a line says.
//
// Two layers carry the same four rules: an Environment's BASELINE (org
// admins) and a Project's tighten-only OVERLAY (that Project's admins). The
// server merges them into the EFFECTIVE rules; the UI never re-implements
// that merge — it shows the server's `effective` block — so the only logic
// here is form <-> body, and labels.

/** The four rules as the server serialises one layer. Structural, so the
 *  generated DTOs satisfy it without this module importing the schema. */
export type RulesLike = {
  approvers: readonly string[];
  allowed_refs: readonly string[];
  wait_timer_s: number;
  require_reason: boolean;
};

/** One rules layer as the form holds it — strings, possibly half typed. */
export type RulesForm = {
  /** Comma-separated Principal ids. */
  approvers: string;
  /** Comma-separated ref globs; blank = any ref. */
  allowedRefs: string;
  /** Seconds. */
  waitTimerS: string;
  requireReason: boolean;
};

/** The baseline PUT body (`…/environments/{env}/protection`), and an
 *  overlay's (`…/projects/{project}/overlay`). */
export type ProtectionBody = {
  approvers: string[];
  allowed_refs: string[];
  wait_timer_s: number;
  require_reason: boolean;
};

type Built<T> = { body: T; error?: undefined } | { body?: undefined; error: string };

/** A comma list, trimmed, blanks dropped, duplicates removed (in order). */
export function splitList(s: string): string[] {
  const out: string[] = [];
  for (const raw of s.split(",")) {
    const x = raw.trim();
    if (x && !out.includes(x)) out.push(x);
  }
  return out;
}

/** The form for a stored layer, or a blank (permissive) one. */
export function rulesToForm(rules?: RulesLike | null): RulesForm {
  return {
    approvers: (rules?.approvers ?? []).join(", "),
    allowedRefs: (rules?.allowed_refs ?? []).join(", "),
    waitTimerS: String(rules?.wait_timer_s ?? 0),
    requireReason: rules?.require_reason ?? false,
  };
}

/** The baseline (or overlay) body for `form`, or the first reason it cannot
 *  be sent. */
export function toProtectionBody(form: RulesForm): Built<ProtectionBody> {
  const wait = form.waitTimerS.trim() === "" ? 0 : Number(form.waitTimerS);
  if (!Number.isInteger(wait) || wait < 0) {
    return { error: "The wait timer must be a whole number of seconds (0 or more)." };
  }
  return {
    body: {
      approvers: splitList(form.approvers),
      allowed_refs: splitList(form.allowedRefs),
      wait_timer_s: wait,
      require_reason: form.requireReason,
    },
  };
}

/** The rules protect nothing: no approver, any ref, no wait, no reason — the
 *  baseline the migration gave every Environment (ADR-0071 §10 step 1).
 *  Shown as a warning: set a baseline. */
export function isPermissive(rules: RulesLike): boolean {
  return (
    rules.approvers.length === 0 &&
    rules.allowed_refs.length === 0 &&
    rules.wait_timer_s === 0 &&
    !rules.require_reason
  );
}

/** `any ref`, or each restricting layer's globs, joined — a ref must match
 *  EVERY layer (never their intersection, which would read "any ref" for
 *  `[main]` + `[tags/*]`). */
export function refLayersLabel(layers: readonly (readonly string[])[]): string {
  const restricting = layers.filter((l) => l.length > 0);
  if (restricting.length === 0) return "any ref";
  return restricting.map((l) => l.join(" | ")).join("  AND  ");
}

/** One-line summary of a layer's rules. */
export function rulesSummary(rules: RulesLike): string {
  const parts = [
    `${rules.approvers.length} approver${rules.approvers.length === 1 ? "" : "s"}`,
    refLayersLabel([rules.allowed_refs]),
  ];
  if (rules.wait_timer_s > 0) parts.push(`${rules.wait_timer_s}s wait`);
  if (rules.require_reason) parts.push("reason required");
  return parts.join(" · ");
}

/** An Environment's eligibility as the server serialises it. */
export type EligibilityLike = { all_projects: boolean; projects?: readonly string[] };

/** Eligibility in words. An empty list admits NO project (fail-closed). */
export function eligibilityLabel(e: EligibilityLike): string {
  if (e.all_projects) return "all projects";
  const projects = e.projects ?? [];
  if (projects.length === 0) return "no project yet";
  return projects.join(", ");
}

/** The eligibility PUT body from the form's toggle and pattern list. */
export function toEligibilityBody(
  allProjects: boolean,
  patterns: string,
): Built<{ all_projects: boolean; projects: string[] }> {
  if (allProjects) return { body: { all_projects: true, projects: [] } };
  return { body: { all_projects: false, projects: splitList(patterns) } };
}

/** The Environments a Project may tighten: the eligible ones, plus any it
 *  already holds an overlay in (editable after eligibility is withdrawn,
 *  as the server allows), sorted by name. */
export function tightenable<T extends { name: string; eligible: boolean; overlay?: unknown }>(
  envs: readonly T[],
): T[] {
  return envs
    .filter((e) => e.eligible || e.overlay != null)
    .slice()
    .sort((a, b) => a.name.localeCompare(b.name));
}

/** A split-report line as the server serialises it (the fields used here). */
export type ReportLineLike = {
  environment: string;
  acknowledged_at?: number | null;
  permissive_baseline: boolean;
  environment_exists: boolean;
};

/** The count banner over the split report: lines still to review, and how
 *  many of the Environments in it still have a permissive baseline. `null`
 *  when there is nothing to say. */
export function reportBanner(lines: readonly ReportLineLike[]): string | null {
  const open = lines.filter((l) => l.acknowledged_at == null).length;
  const permissive = lines.filter((l) => l.environment_exists && l.permissive_baseline).length;
  const parts: string[] = [];
  if (open > 0) parts.push(`${open} migrated environment${open === 1 ? "" : "s"} to review`);
  if (permissive > 0) {
    parts.push(
      `${permissive} with a permissive baseline — set a baseline so the place is protected, not only each project`,
    );
  }
  return parts.length ? parts.join(" · ") : null;
}

/** One whitelist row as the grant form holds it. */
export type ImageGrantRow = { digest: string; privileged: boolean; capabilities: string };

/** The privilege-grant PUT body. */
export type GrantBody = {
  privileged_images: { image_digest: string; privileged: boolean; capabilities: string[] }[];
  permit_k8s_overlay: boolean;
};

/** The privilege-grant body for the form, or the first reason it cannot be
 *  sent. A digest is `sha256:<hex>` — admission compares an image's
 *  `@sha256:…` to it exactly, so anything else could never match — and is
 *  listed once; an untouched blank row is dropped. Capabilities are
 *  upper-cased (`net_admin` → `NET_ADMIN`), the form Kubernetes expects. */
export function toGrantBody(rows: readonly ImageGrantRow[], permitK8sOverlay: boolean): Built<GrantBody> {
  const privileged_images: GrantBody["privileged_images"] = [];
  for (const row of rows) {
    const digest = row.digest.trim();
    if (!digest && !row.privileged && !row.capabilities.trim()) continue;
    if (!/^sha256:[0-9a-fA-F]+$/.test(digest)) {
      return { error: `"${digest || "(blank)"}" is not an image digest — write it as sha256:<hex>.` };
    }
    if (privileged_images.some((g) => g.image_digest === digest)) {
      return { error: `${digest} is listed twice.` };
    }
    privileged_images.push({
      image_digest: digest,
      privileged: row.privileged,
      capabilities: splitList(row.capabilities).map((c) => c.toUpperCase()),
    });
  }
  return { body: { privileged_images, permit_k8s_overlay: permitK8sOverlay } };
}
