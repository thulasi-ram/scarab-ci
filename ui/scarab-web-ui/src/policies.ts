// The org Policies page, as the UI has to reason about it. Pure: no DOM, no
// fetch, so the page and its tests agree.
//
// An org's rules live in two forms. CEL Policies are written as rules over a
// checkpoint's input, in the policy editor (`policy-editor.ts`). Typed settings are rules with a form of their
// own next to the feature they govern: an Environment's rules, the library and
// plugin lists, API token scopes. The page lists those read-only, each with a
// link to where it is edited.
//
// Also here: the Run page's view of a Run's decisions, and the library and
// plugin list forms (one Policy per list, at the org or one project).
import type { components } from "./api/schema";
import { orgHref } from "./settings-nav";

type Schemas = components["schemas"];

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

/** A stored Policy (`PolicyDto`), its authored body read as {@link PolicyBody}
 *  (the schema leaves it opaque). */
export type StoredPolicyLike = Omit<Schemas["PolicyDto"], "policy"> & { policy: PolicyBody };

/** One Policy's part in a recorded decision. `mode` is the Policy's mode
 *  when the decision was made. */
export type PolicyResultLike = {
  policy_id: string;
  name?: string;
  level?: string;
  mode?: string;
  outcome: { kind: string; messages?: string[]; message?: string; gate?: string; fronts?: string[] };
};

/** A recorded decision (`PolicyDecisionRecordDto`), its `decision` read as far
 *  as the page reads it (the schema leaves it opaque). */
export type DecisionRecordLike = Omit<Schemas["PolicyDecisionRecordDto"], "decision"> & {
  decision: {
    checkpoint?: string;
    input?: unknown;
    results?: PolicyResultLike[];
    outcome?: { kind: string; messages?: string[] };
  };
};

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

/** The anchor of the Library policies panel on the Libraries section and of
 *  the Plugin policies panel on the Plugins section. */
export const LIBRARY_POLICIES_ANCHOR = "library-policies";
export const PLUGIN_POLICIES_ANCHOR = "plugin-policies";
/** The anchors those panels had before, kept so links sent with them still
 *  scroll to the panel. */
export const OLD_LIBRARY_POLICIES_ANCHOR = "library-lists";
export const OLD_PLUGIN_POLICIES_ANCHOR = "plugin-lists";

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

/** The library and plugin policy rows, by name. An unknown setting kind is
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
        href: known
          ? `${orgHref(org, known.section)}#${known.section === "plugins" ? PLUGIN_POLICIES_ANCHOR : LIBRARY_POLICIES_ANCHOR}`
          : orgHref(org, "policies"),
        sectionLabel: known?.section === "plugins" ? "Plugins" : known ? "Libraries" : "Policies",
      };
    })
    .sort((a, b) => a.name.localeCompare(b.name));
}

/** A decision's verdict in words. */
export function decisionVerdict(d: DecisionRecordLike): "allowed" | "denied" {
  return d.decision.outcome?.kind === "deny" ? "denied" : "allowed";
}

// ── A Run's decisions, on the Run page ───────────────────────────────────────

/** One Policy's part in a decision, as the Run page shows it. */
export type RunPolicyOutcome = "denied" | "would_deny" | "allowed" | "requires";

export type RunPolicyResult = {
  policyId: string;
  name: string;
  /** `org`, `project`, or empty for a built-in rule. */
  level: string;
  outcome: RunPolicyOutcome;
  /** The rule's static messages, or the error a failed rule recorded. */
  messages: string[];
};

/** One recorded decision of a Run, with the Policies its `match` selected. */
export type RunDecisionView = {
  id: string;
  at: number;
  checkpoint: string;
  step: string | null;
  attempt: number | null;
  verdict: "denied" | "allowed";
  results: RunPolicyResult[];
  /** The input the rules saw, as recorded; `null` when none was stored. */
  input: unknown;
};

/** A recorded outcome as the Run page names it. A rule that failed denies in
 *  enforce mode and is recorded as a would-deny in audit mode. */
export function runOutcome(r: PolicyResultLike): { outcome: RunPolicyOutcome; messages: string[] } | null {
  const o = r.outcome ?? { kind: "" };
  const audit = r.mode === "audit";
  switch (o.kind) {
    case "denied":
      return { outcome: "denied", messages: o.messages ?? [] };
    case "would_deny":
      return { outcome: "would_deny", messages: o.messages ?? [] };
    case "error":
      return { outcome: audit ? "would_deny" : "denied", messages: o.message ? [o.message] : [] };
    case "requires":
      return { outcome: "requires", messages: o.messages ?? [] };
    case "gate_added":
      return {
        outcome: "allowed",
        messages: o.gate ? [`added the approval gate ${o.gate} before ${(o.fronts ?? []).join(", ")}`] : [],
      };
    case "passed":
      return { outcome: "allowed", messages: [] };
    default:
      // `not_matched`, or a kind this page does not know.
      return null;
  }
}

/** A Run's decisions as the Run page lists them, oldest first: each with
 *  the Policies its `match` selected. A decision no Policy matched is left
 *  out, so a Run no Policy applies to shows nothing. */
export function runDecisionViews(decisions: readonly DecisionRecordLike[]): RunDecisionView[] {
  const out: RunDecisionView[] = [];
  for (const d of decisions) {
    const results: RunPolicyResult[] = [];
    for (const r of d.decision.results ?? []) {
      const o = runOutcome(r);
      if (!o) continue;
      results.push({ policyId: r.policy_id, name: r.name ?? r.policy_id, level: r.level ?? "", ...o });
    }
    if (results.length === 0) continue;
    out.push({
      id: d.id,
      at: d.at,
      checkpoint: d.decision.checkpoint ?? "",
      step: d.step_id ?? null,
      attempt: d.attempt ?? null,
      verdict: decisionVerdict(d),
      results,
      input: d.decision.input ?? null,
    });
  }
  return out.sort((a, b) => a.at - b.at);
}

export const RUN_OUTCOME_LABEL: Record<RunPolicyOutcome, string> = {
  denied: "denied",
  would_deny: "would deny (audit)",
  allowed: "allowed",
  requires: "waiting on a requirement",
};

/** The Run page's one-line summary: how many decisions denied and how many
 *  results would have denied in audit mode. */
export function runPolicySummary(views: readonly RunDecisionView[]): string {
  const denied = views.filter((v) => v.verdict === "denied").length;
  const would = views.reduce((n, v) => n + v.results.filter((r) => r.outcome === "would_deny").length, 0);
  const parts = [`${views.length} decision${views.length === 1 ? "" : "s"}`];
  if (denied > 0) parts.push(`${denied} denied`);
  if (would > 0) parts.push(`${would} would deny in audit mode`);
  return parts.join(" · ");
}

// ── Library and plugin policies (typed settings) ─────────────────────────────

export type SettingKind = "libraries_allowed" | "libraries_blocked" | "plugins_allowed";

export const LIBRARY_KINDS: readonly SettingKind[] = ["libraries_allowed", "libraries_blocked"];
export const PLUGIN_KINDS: readonly SettingKind[] = ["plugins_allowed"];

/** What each list means, in a sentence, for the form's help line. */
export const SETTING_HELP: Record<SettingKind, string> = {
  libraries_allowed:
    "A Run is denied when a Step or gate comes from a library matching none of these sources.",
  libraries_blocked: "A Run is denied when a Step or gate comes from a library matching any of these sources.",
  plugins_allowed:
    "A Run is denied when it has a gate from a plugin installation not on this list.",
};

export const settingLabel = (kind: string) => SETTING_KINDS[kind]?.label ?? kind;

/** The list form, as the page edits it. Lists are one entry per line. */
export type SettingForm = {
  kind: SettingKind;
  name: string;
  entries: string;
  /** Org level only: the projects it applies to, and the exceptions. */
  projects: string;
  exceptProjects: string;
  mode: "audit" | "enforce";
};

/** The kind of a typed-setting Policy, or `undefined` for a CEL one. */
export function settingKindOf(p: PolicyBody): SettingKind | undefined {
  const kind = Object.keys(p.setting ?? {})[0];
  return kind && kind in SETTING_KINDS ? (kind as SettingKind) : undefined;
}

/** Entries split on new lines and commas, trimmed, empty and repeated ones
 *  dropped. */
export function splitEntries(text: string): string[] {
  const out: string[] = [];
  for (const raw of text.split(/[\n,]/)) {
    const x = raw.trim();
    if (x && !out.includes(x)) out.push(x);
  }
  return out;
}

/** A new list's form: a name from its kind and no entries. A library list
 *  starts in audit mode, so it records what it would deny before it denies
 *  anything. A plugin list starts enforced: with no enforced org list no
 *  plugin's gates run, so an audit-mode one would allow nothing. */
export function newSettingForm(kind: SettingKind): SettingForm {
  return {
    kind,
    name: kind.replace(/_/g, "-"),
    entries: "",
    projects: "",
    exceptProjects: "",
    mode: kind === "plugins_allowed" ? "enforce" : "audit",
  };
}

/** The form for a stored list. */
export function settingFormOf(p: PolicyBody): SettingForm {
  const kind = settingKindOf(p) ?? "libraries_allowed";
  const list = SETTING_KINDS[kind]!.list;
  return {
    kind,
    name: p.name,
    entries: (p.setting?.[kind]?.[list] ?? []).join("\n"),
    projects: (p.match?.projects ?? []).join(", "),
    exceptProjects: (p.match?.except_projects ?? []).join(", "),
    mode: p.mode === "audit" ? "audit" : "enforce",
  };
}

/** The Policy body the form saves. At the project level there is no
 *  `match`: a project's list applies to its own Runs only. */
export function settingBody(form: SettingForm, level: "org" | "project"): PolicyBody {
  const list = SETTING_KINDS[form.kind]!.list;
  const body: PolicyBody = {
    name: form.name.trim(),
    checkpoint: "run.create",
    mode: form.mode,
    setting: { [form.kind]: { [list]: splitEntries(form.entries) } },
  };
  if (level === "org") {
    body.match = {
      projects: splitEntries(form.projects),
      except_projects: splitEntries(form.exceptProjects),
      environments: [],
      trigger_kinds: [],
    };
  }
  return body;
}

/** What the server would refuse in the form, checked before sending: the
 *  name's characters, an empty list, and the pattern shapes. */
export function settingFormErrors(form: SettingForm): { name?: string; entries?: string } {
  const out: { name?: string; entries?: string } = {};
  if (!/^[A-Za-z0-9._-]{1,100}$/.test(form.name.trim())) {
    out.name = "1 to 100 letters, digits, -, _ or .";
  }
  const entries = splitEntries(form.entries);
  if (entries.length === 0) {
    out.entries = "Add at least one entry.";
  } else if (form.kind === "plugins_allowed") {
    const bad = entries.find((e) => e.includes("*"));
    if (bad) out.entries = `${bad}: an installation is named exactly, with no *.`;
  } else {
    const bad = entries.find((e) => e.indexOf("*") !== -1 && e.indexOf("*") !== e.length - 1);
    if (bad) out.entries = `${bad}: a pattern is an exact source or a prefix ending in *.`;
  }
  return out;
}
