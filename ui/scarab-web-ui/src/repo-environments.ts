// The repository's Environments tab: one table, a row per org Environment
// this repository may use (or still holds rules in). Pure, no DOM and no
// fetch, so the table and its tests agree on every cell and sentence.
//
// The row's Approvers, Refs and Wait cells read the server's `effective`
// block (the baseline merged with this repository's overlay); the merge is
// never redone here. The expanded row sets the two layers side by side:
// what the org baseline sets and what this repository adds on top.
import type { RulesLike } from "./environment-overlays";
import { durationText } from "./org-environments";

/** An Environment as this repository sees it. Structural, so the generated
 *  `ProjectEnvironment` satisfies it. */
export type RepoEnvLike = {
  eligible: boolean;
  baseline: RulesLike;
  overlay?: RulesLike | null;
  effective: {
    approvers: readonly string[];
    allowed_ref_layers: readonly (readonly string[])[];
    wait_timer_s: number;
    require_reason: boolean;
    approval_gate?: { required_approvals?: number | null } | null;
  };
  privilege_grant?: {
    privileged_images: readonly unknown[];
    permit_k8s_overlay: boolean;
  } | null;
};

/** A ref as people say it: `refs/heads/main` → `main`, `refs/tags/v*` → `v*`.
 *  Anything else is shown as written. */
export function shortRef(ref: string): string {
  return ref.replace(/^refs\/heads\//, "").replace(/^refs\/tags\//, "");
}

/** The Refs cell: `Any` when no layer restricts, else each restricting
 *  layer's refs, short. A ref must match every layer, so two layers read
 *  `main and release/*`. */
export function refsCell(layers: readonly (readonly string[])[]): string {
  const restricting = layers.filter((l) => l.length > 0);
  if (restricting.length === 0) return "Any";
  return restricting.map((l) => l.map(shortRef).join(", ")).join(" and ");
}

/** A wait in short form: `30 s`, `5 min`, `1 h`, `1 h 30 min`. */
export function waitShort(seconds: number): string {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;
  const parts: string[] = [];
  if (h > 0) parts.push(`${h} h`);
  if (m > 0) parts.push(`${m} min`);
  if (s > 0 && h === 0) parts.push(`${s} s`);
  return parts.join(" ") || "0 s";
}

/** The Wait cell, and what hovering it says. The timer waits on the approval
 *  gate, so with no approvers it does nothing and the cell reads `—`. */
export function waitCell(effective: RepoEnvLike["effective"]): { text: string; title?: string } {
  const wait = effective.wait_timer_s;
  if (wait <= 0) return { text: "—" };
  if (effective.approvers.length === 0) {
    return { text: "—", title: `Does nothing: the ${waitShort(wait)} timer needs approvers` };
  }
  return {
    text: waitShort(wait),
    title: "A minimum wait on the approval gate, after the approvals are in",
  };
}

/** How many approvals a deploy needs: the baseline's required count (every
 *  baseline approver when it sets none), plus each approver the overlay adds.
 *  An overlay approver who is also a baseline approver counts toward both. */
export function approvalsNeeded(env: RepoEnvLike): number {
  const base = env.baseline.approvers;
  const added = [...new Set(env.overlay?.approvers ?? [])];
  const required = Math.min(env.effective.approval_gate?.required_approvals ?? base.length, base.length);
  const overlap = added.filter((a) => base.includes(a)).length;
  return added.length + Math.max(0, required - overlap);
}

/** The Approvers cell: `—`, the count, or `1 of 3` when fewer approvals are
 *  needed than there are approvers. */
export function approversCell(env: RepoEnvLike): string {
  const total = env.effective.approvers.length;
  if (total === 0) return "—";
  const needed = approvalsNeeded(env);
  return needed < total ? `${needed} of ${total}` : String(total);
}

/** The approvers this repository's overlay adds to the baseline's. */
export function addedApprovers(env: RepoEnvLike): string[] {
  return [...new Set(env.overlay?.approvers ?? [])].filter((a) => !env.baseline.approvers.includes(a));
}

/** The overlay sets a rule other than an approver that the baseline does not
 *  already set at least as strictly. */
function tightensBeyondApprovers(env: RepoEnvLike): boolean {
  const o = env.overlay;
  if (!o) return false;
  return (
    o.allowed_refs.length > 0 ||
    o.wait_timer_s > env.baseline.wait_timer_s ||
    (o.require_reason && !env.baseline.require_reason)
  );
}

/** No rule protects a deploy: no approver, any ref, no reason. A wait timer
 *  alone protects nothing, since it needs approvers. */
export function protectsNothing(effective: RepoEnvLike["effective"]): boolean {
  return (
    effective.approvers.length === 0 &&
    effective.allowed_ref_layers.every((l) => l.length === 0) &&
    !effective.require_reason
  );
}

/** The This repo cell: a chip saying what this repository's overlay adds, a
 *  warning when nothing protects a deploy, or `null` (the baseline alone). */
export function repoChip(env: RepoEnvLike): { text: string; warn: boolean } | null {
  if (!env.eligible) return { text: "not eligible", warn: true };
  if (env.overlay) {
    const n = addedApprovers(env).length;
    const parts: string[] = [];
    if (n > 0) parts.push(`+${n} approver${n === 1 ? "" : "s"}`);
    if (tightensBeyondApprovers(env)) parts.push("tightened");
    return { text: parts.join(", ") || "adds nothing", warn: false };
  }
  if (protectsNothing(env.effective)) return { text: "no rules", warn: true };
  return null;
}

/** A piece of a sentence; `mono` pieces are refs, shown in the mono face. */
export type Seg = { text: string; mono?: boolean };

/** The sentence as plain text, mono pieces in backticks (for tests and
 *  titles). */
export function segmentsText(segs: readonly Seg[]): string {
  return segs.map((s) => (s.mono ? `\`${s.text}\`` : s.text)).join("");
}

/** One layer's refs as words: `main`, `release/*` or a `v*` tag. */
function refPhrase(layer: readonly string[]): Seg[] {
  const items: Seg[][] = layer.map((ref) =>
    ref.startsWith("refs/tags/")
      ? [{ text: "a " }, { text: shortRef(ref), mono: true }, { text: " tag" }]
      : [{ text: shortRef(ref), mono: true }],
  );
  const out: Seg[] = [];
  items.forEach((item, i) => {
    if (i > 0) out.push({ text: i === items.length - 1 ? " or " : ", " });
    out.push(...item);
  });
  return out;
}

/** What a deploy from this repository needs, in one sentence:
 *  "A deploy from `main` or a `v*` tag needs 3 approvals and a reason, then
 *  waits 5 minutes." A wait timer with no approvers is left out: it does
 *  nothing. */
export function deploySentence(env: RepoEnvLike): Seg[] {
  if (!env.eligible) return [{ text: "Deploys from this repo are refused here." }];
  const e = env.effective;
  const layers = e.allowed_ref_layers.filter((l) => l.length > 0);
  const out: Seg[] = [{ text: "A deploy from " }];
  if (layers.length === 0) out.push({ text: "any ref" });
  layers.forEach((layer, i) => {
    if (i > 0) out.push({ text: " that also matches " });
    out.push(...refPhrase(layer));
  });

  const n = e.approvers.length > 0 ? approvalsNeeded(env) : 0;
  const needs: string[] = [];
  if (n > 0) needs.push(`${n} approval${n === 1 ? "" : "s"}`);
  if (e.require_reason) needs.push("a reason");
  out.push({ text: ` needs ${needs.join(" and ") || "no approval"}` });
  if (n > 0 && e.wait_timer_s > 0) out.push({ text: `, then waits ${durationText(e.wait_timer_s)}` });
  out.push({ text: "." });

  // Merge neighbouring plain pieces, so the sentence is as few nodes as it
  // can be and tests compare whole words.
  return out.reduce<Seg[]>((acc, s) => {
    const last = acc[acc.length - 1];
    if (last && !last.mono && !s.mono) last.text += s.text;
    else acc.push({ ...s });
    return acc;
  }, []);
}

/** One row of the expanded table: a rule, what the org baseline sets and what
 *  this repository adds. `mono` when the values are ids or refs. */
export type LayerRow = { rule: string; baseline: string; overlay: string; mono: boolean };

/** The expanded row's table. The overlay column shows only what tightens the
 *  baseline: an approver the baseline already has, a shorter wait or a
 *  reason it already asks for adds nothing and reads `—`. Privileges are
 *  this repository's grant; the baseline sets none. */
export function layerRows(env: RepoEnvLike): LayerRow[] {
  const b = env.baseline;
  const o = env.overlay;
  const list = (xs: readonly string[]) => (xs.length > 0 ? xs.join(", ") : "—");
  const grant = env.privilege_grant;
  let privileges = "—";
  if (grant && (grant.privileged_images.length > 0 || grant.permit_k8s_overlay)) {
    const n = grant.privileged_images.length;
    const parts: string[] = [];
    if (n > 0) parts.push(`${n} privileged image${n === 1 ? "" : "s"}`);
    if (grant.permit_k8s_overlay) parts.push("raw k8s_overlay allowed");
    privileges = parts.join(", ");
  }
  return [
    { rule: "Approvers", baseline: list(b.approvers), overlay: list(addedApprovers(env)), mono: true },
    {
      rule: "Refs",
      baseline: b.allowed_refs.length > 0 ? b.allowed_refs.map(shortRef).join(", ") : "Any",
      overlay: o && o.allowed_refs.length > 0 ? o.allowed_refs.map(shortRef).join(", ") : "—",
      mono: true,
    },
    {
      rule: "Wait",
      baseline: b.wait_timer_s > 0 ? waitShort(b.wait_timer_s) : "—",
      overlay: o && o.wait_timer_s > b.wait_timer_s ? waitShort(o.wait_timer_s) : "—",
      mono: false,
    },
    {
      rule: "Reason",
      baseline: b.require_reason ? "Required" : "—",
      overlay: o?.require_reason && !b.require_reason ? "Required" : "—",
      mono: false,
    },
    { rule: "Privileges", baseline: "—", overlay: privileges, mono: false },
  ];
}
