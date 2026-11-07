// The org Policies page, as the UI has to reason about it. Pure: no DOM, no
// fetch, so the page and its tests agree.
//
// An org's rules live in two forms. CEL Policies are written as rules over a
// checkpoint's input and stored with the policy API; the page lists them with
// how often they matched, denied and, in audit mode, would have denied. Typed
// settings are rules with a form of their own next to the feature they govern:
// an Environment's rules, the library and plugin lists, API token scopes. The
// page lists those read-only, each with a link to where it is edited.
import { orgHref } from "./settings-nav";

/** An authored Policy as the server serialises it (`PolicyDto.policy`). */
export type PolicyBody = {
  name: string;
  checkpoint: string;
  match?: {
    projects?: string[];
    except_projects?: string[];
    environments?: string[];
    trigger_kinds?: string[];
  };
  mode?: "enforce" | "audit";
  rules?: { when: string; effect: unknown; message: string }[];
  setting?: Record<string, Record<string, string[]>>;
};

/** A stored Policy (`PolicyDto`), as far as the page reads it. */
export type StoredPolicyLike = {
  id: string;
  name: string;
  level: string;
  project?: string | null;
  version: number;
  updated_at: number;
  deleted: boolean;
  policy: PolicyBody;
};

/** One Policy's part in a recorded decision. */
export type PolicyResultLike = { policy_id: string; outcome: { kind: string } };

/** A recorded decision (`PolicyDecisionRecordDto`), as far as the page reads it. */
export type DecisionRecordLike = {
  id: string;
  at: number;
  project?: string | null;
  run_id?: string | null;
  decision: { checkpoint?: string; results?: PolicyResultLike[]; outcome?: { kind: string; messages?: string[] } };
};

/** How often one Policy took part in the decisions read. */
export type PolicyCounts = {
  /** Its `match` selected the consultation. */
  matched: number;
  /** It denied (enforce mode), or its rule failed and so denied. */
  denied: number;
  /** In audit mode, it would have denied. */
  wouldDeny: number;
};

const ZERO: PolicyCounts = { matched: 0, denied: 0, wouldDeny: 0 };

/** Each Policy's counts over `decisions`, by Policy id. A Policy absent from
 *  every decision has none; read it with {@link countsOf}. */
export function policyCounts(decisions: readonly DecisionRecordLike[]): Map<string, PolicyCounts> {
  const out = new Map<string, PolicyCounts>();
  for (const d of decisions) {
    for (const r of d.decision.results ?? []) {
      const kind = r.outcome?.kind;
      if (!kind || kind === "not_matched") continue;
      const c = { ...(out.get(r.policy_id) ?? ZERO) };
      c.matched += 1;
      if (kind === "denied" || kind === "error") c.denied += 1;
      if (kind === "would_deny") c.wouldDeny += 1;
      out.set(r.policy_id, c);
    }
  }
  return out;
}

/** One Policy's counts, zero when it took part in none. */
export const countsOf = (counts: Map<string, PolicyCounts>, id: string): PolicyCounts =>
  counts.get(id) ?? ZERO;

/** A live CEL Policy (rules, not a typed setting). */
export const isCelPolicy = (p: StoredPolicyLike) => !p.deleted && Array.isArray(p.policy.rules);

/** A live typed-setting Policy (a library or plugin list). */
export const isTypedSettingPolicy = (p: StoredPolicyLike) => !p.deleted && !!p.policy.setting;

/** What a Policy's `match.projects` says, under the label "Applies to
 *  projects": every project, or the patterns, with any exceptions. */
export function appliesToProjects(p: PolicyBody): string {
  const projects = p.match?.projects ?? [];
  const except = p.match?.except_projects ?? [];
  const base = projects.length === 0 ? "all projects" : projects.join(", ");
  return except.length > 0 ? `${base}, except ${except.join(", ")}` : base;
}

/** What a Policy's `match.environments` says: any Run, or the names. */
export function appliesToEnvironments(p: PolicyBody): string {
  const envs = p.match?.environments ?? [];
  return envs.length === 0 ? "any" : envs.join(", ");
}

/** A typed setting's kind in words, and the section of org settings its
 *  feature lives in. */
const SETTING_KINDS: Record<string, { label: string; section: string; list: string }> = {
  libraries_allowed: { label: "Libraries allowed", section: "libraries", list: "sources" },
  libraries_blocked: { label: "Libraries blocked", section: "libraries", list: "sources" },
  plugins_allowed: { label: "Plugins whose gates may run", section: "plugins", list: "installations" },
};

/** One typed-setting Policy as the page lists it. */
export type TypedSettingRow = {
  id: string;
  name: string;
  label: string;
  values: string[];
  appliesTo: string;
  /** Where the feature it governs lives in org settings. */
  href: string;
  sectionLabel: string;
};

/** The library and plugin list rows, by name. An unknown setting kind is
 *  shown by its wire name, linking to the Policies page itself. */
export function typedSettingRows(org: string, policies: readonly StoredPolicyLike[]): TypedSettingRow[] {
  return policies
    .filter(isTypedSettingPolicy)
    .map((p) => {
      const [kind, body] = Object.entries(p.policy.setting!)[0] ?? ["", {}];
      const known = SETTING_KINDS[kind];
      const values = known ? (body[known.list] ?? []) : Object.values(body).flat();
      return {
        id: p.id,
        name: p.name,
        label: known?.label ?? kind,
        values,
        appliesTo: appliesToProjects(p.policy),
        href: orgHref(org, known?.section ?? "policies"),
        sectionLabel: known?.section === "plugins" ? "Plugins" : known ? "Libraries" : "Policies",
      };
    })
    .sort((a, b) => a.name.localeCompare(b.name));
}

/** A decision's verdict in words. */
export function decisionVerdict(d: DecisionRecordLike): "allowed" | "denied" {
  return d.decision.outcome?.kind === "deny" ? "denied" : "allowed";
}
