// ─────────────────────────────────────────────────────────────────────────
// REPRESENTATIVE CATALOG — the repo-first surfaces (repos list, per-repo run
// provenance, environments) have no backend yet: the forge adapter, RepoStore,
// and run repo/commit/trigger metadata are the next backend slice (see the UI
// audit). Until they land, these screens are driven by this ONE module so the
// full repo → runs → run experience is navigable and matches the design.
//
// SWAP PLAN: replace `listRepos`/`repoRuns`/`environments` with `/v1/repos` +
// enriched `RunSummaryDto` calls; delete `enrichProvenance`. Run DETAIL already
// uses the real API (DAG/events/logs/restart) — only listing + provenance are
// representative here.
// ─────────────────────────────────────────────────────────────────────────

export type TriggerKind = "push" | "pull_request" | "tag" | "manual" | "cron";

export type Provenance = {
  org: string;
  repo: string;
  branch: string;
  sha: string;
  message: string;
  trigger: TriggerKind;
  prNumber?: number;
  tag?: string;
  author: string;
  duration: string;
};

export type RepoCard = {
  org: string;
  name: string;
  defaultBranch: string;
  lastStatus: RunFacet;
  spark: RunFacet[];
};

export type RunFacet = "succeeded" | "failed" | "running" | "pending";

export const ORG = "acme";

export const TRIGGER_GLYPH: Record<TriggerKind, string> = {
  push: "⬆",
  pull_request: "⑃",
  tag: "⌾",
  manual: "✋",
  cron: "⏱",
};

// deterministic small hash so a run id maps to stable representative provenance
function hash(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

const BRANCHES = ["main", "main", "main", "feat/checkout", "fix/outbox", "release/2.3"];
const MESSAGES = [
  "fix: retry idempotency on outbox drain",
  "feat: checkout summary card",
  "chore: bump sqlx to 0.8",
  "refactor: fold scheduler admission",
  "test: gate timer auto-release",
  "perf: batch SKIP LOCKED lease claim",
  "docs: ADR-0037 log source wiring",
  "fix: redact secrets in streamed logs",
];
const AUTHORS = ["t.ram", "a.kumar", "s.lee", "dependabot"];
const TRIGGERS: TriggerKind[] = ["push", "push", "push", "pull_request", "tag", "manual"];

/** Stable representative provenance for a real run id (until forge lands). */
export function enrichProvenance(id: string, repo = "api"): Provenance {
  const h = hash(id);
  const trigger = TRIGGERS[h % TRIGGERS.length];
  const branch =
    trigger === "tag" ? "main" : trigger === "manual" ? "main" : BRANCHES[h % BRANCHES.length];
  const sha = id.replace(/-/g, "").slice(0, 7);
  const secs = 20 + (h % 160);
  const dur = secs < 60 ? `${secs}s` : `${Math.floor(secs / 60)}m${(secs % 60).toString().padStart(2, "0")}s`;
  return {
    org: ORG,
    repo,
    branch,
    sha,
    message: MESSAGES[h % MESSAGES.length],
    trigger,
    prNumber: trigger === "pull_request" ? 60 + (h % 40) : undefined,
    tag: trigger === "tag" ? `v2.${h % 6}.${h % 10}` : undefined,
    author: AUTHORS[h % AUTHORS.length],
    duration: dur,
  };
}

export function listRepos(): RepoCard[] {
  const spark = (seed: string): RunFacet[] => {
    const facets: RunFacet[] = ["succeeded", "succeeded", "failed", "succeeded", "running", "succeeded", "succeeded"];
    const h = hash(seed);
    return facets.map((f, i) => (((h >> i) & 7) === 0 ? "failed" : f));
  };
  return [
    { org: ORG, name: "api", defaultBranch: "main", lastStatus: "running", spark: spark("api") },
    { org: ORG, name: "web", defaultBranch: "main", lastStatus: "succeeded", spark: spark("web") },
    { org: ORG, name: "billing", defaultBranch: "main", lastStatus: "succeeded", spark: spark("billing") },
    { org: ORG, name: "infra", defaultBranch: "main", lastStatus: "failed", spark: spark("infra") },
  ];
}

export type EnvRow = {
  name: string;
  approvers: number;
  wait: string;
  allowed: string;
  current: string;
  history: { when: string; version: string; by: string }[];
};

export function environments(_repo: string): EnvRow[] {
  return [
    {
      name: "production",
      approvers: 2,
      wait: "0s",
      allowed: "main only",
      current: "v2.2.9",
      history: [
        { when: "2h ago", version: "v2.2.9", by: "t.ram" },
        { when: "1d ago", version: "v2.2.8", by: "a.kumar" },
      ],
    },
    {
      name: "staging",
      approvers: 0,
      wait: "0s",
      allowed: "any ref",
      current: "3f9a2c1",
      history: [{ when: "12m ago", version: "3f9a2c1", by: "s.lee" }],
    },
  ];
}

/** Manual-run inputs (typed pipeline `inputs:` — representative until wired). */
export type InputSpec =
  | { key: string; type: "choice"; options: string[]; default: string }
  | { key: string; type: "boolean"; default: boolean }
  | { key: string; type: "string"; default: string };

export function manualInputs(_repo: string): InputSpec[] {
  return [
    { key: "environment", type: "choice", options: ["staging", "production"], default: "staging" },
    { key: "dry_run", type: "boolean", default: false },
    { key: "reason", type: "string", default: "" },
  ];
}
