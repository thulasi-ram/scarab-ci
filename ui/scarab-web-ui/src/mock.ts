// Fixture / demo mode for the web UI. Gated behind VITE_SCARAB_MOCK=1
// (`just ui mock` / `npm run dev:mock`); patches window.fetch +
// window.EventSource to serve a fixed "acme" org so the UI renders with no
// server or DB. The fastest way to eyeball a UI change, and the source of the
// docs screenshots. Off by default — the guard in index.tsx never imports this
// module unless the flag is set, so it stays out of the production build.
//
// Neutral identity so nothing real leaks into screenshots: org=acme,
// author=a.kim, approver=j.lee; the `scarab` repo is kept (its build log
// references the crates), the rest are generic. Add `?theme=dark` (or `light`)
// to the URL to pin the theme, and `?palette=mono` (or `emerald`) to pin the
// palette (used for deterministic capture); otherwise the stored choices apply.
// The caller administers acme and globex, so the settings org dropdown has a
// choice; `?admin_orgs=acme` (any comma-separated list) makes them an admin of
// those orgs instead.

// Type-only imports — erased at compile time, so the install-before-App-loads
// ordering in index.tsx is unaffected. The fixtures typecheck against the
// generated OpenAPI types (the same contract the real server serves).
import type { components } from "./api/schema";
import { libraryRoute, libraryRunSummaries } from "./mock-libraries";
import { policyRoute } from "./mock-policies";
import { mockStepId, mockStepKey, withStepKeys } from "./mock-step-keys";
import { paginate } from "./org-page";

/** A run detail fixture. Its steps carry no key: the route that serves it
 * adds them ([`withStepKeys`]). */
type RunStatusDto = Omit<components["schemas"]["RunStatusResponse"], "steps"> & {
  steps: Omit<components["schemas"]["StepStatusDto"], "key">[];
};
type RunSummaryDto = components["schemas"]["RunSummaryDto"];
type ArtifactDto = components["schemas"]["ArtifactDto"];
type ServiceStatusDto = components["schemas"]["ServiceStatusDto"];
type OrgEnvironmentDto = components["schemas"]["OrgEnvironmentDto"];
type RulesDto = components["schemas"]["EnvironmentProtectionRulesDto"];
type OverlayDto = components["schemas"]["EnvironmentProjectOverlayDto"];
type GrantDto = components["schemas"]["EnvironmentProjectPrivilegeGrantDto"];
type ProjectEnvironmentDto = components["schemas"]["ProjectEnvironmentDto"];
type EnvironmentProjectDto = components["schemas"]["EnvironmentProjectDto"];
type PluginManifestDto = components["schemas"]["PluginManifestDto"];
type FetchedPluginManifestDto = components["schemas"]["FetchedPluginManifestDto"];
type FetchedPluginUpgradeDto = components["schemas"]["FetchedPluginUpgradeDto"];
type PluginInstallationDto = components["schemas"]["PluginInstallationDto"];
type PluginConnectionDto = components["schemas"]["PluginConnectionDto"];
type BrokerJournalEntryDto = components["schemas"]["BrokerJournalEntryDto"];
type ScheduleDto = components["schemas"]["ScheduleDto"];
type RefusedRunDto = components["schemas"]["RefusedRunDto"];
type RunManifestDto = components["schemas"]["RunManifestDto"];

const now = Date.now();
const ago = (ms: number) => now - ms;
const min = 60000;

const RUN_ID = "0190f8a2000071fb8c0011223344aabb";
// The RICH run — the Playwright walkthrough fixture: two versions (a
// RunRerunRequested boundary with superseded/shadowed attempts), a shared
// service + a docked sidecar, a pending manual gate, an artifact.
const RICH_RUN_ID = "0190f8a2000071fb8c0099887766ccdd";
// The GATES run: one Deploy Run showing every external gate
// state — waiting, passed, failed with a reason. At
// /acme/edge-gateway/runs/<id>.
const GATES_RUN_ID = "0190f8a2000071fb8c00d3e9107a1616";
// The ENVIRONMENT GATE run: a deploy into production with a plain QA gate
// and the approval gate production added. /acme/scarab/runs/<id>.
const ENV_GATE_RUN_ID = "0190f8a2000071fb8c00e7a9b0c1d2e3";
// The MATRIX run: a failed plan over a cluster × phase matrix. Two cells
// failed (one on both of its tries, after waiting for a node), so the summary
// line names two root failures. /acme/scarab/runs/<id>.
const MATRIX_RUN_ID = "0190f8a2000071fb8c00f00d4a7b1c33";
// The RETRIES run: a live run whose `unit` step passed on its third try and
// whose `e2e` step passed on its sixth, with cleared Pod conditions on
// earlier tries, two matrix cells waiting for a node right now, and artifacts
// that earlier tries also wrote. /acme/scarab/runs/<id>.
const RETRIES_RUN_ID = "0190f8a2000071fb8c00a11c3e5d7f34";

// ── Run pipeline launcher ─────────────────────────────────────────────────
// The manual catalog at any ref: `release` takes a spread of typed inputs,
// `deploy` a few, `docs` none, and one file that does not parse.
const MOCK_PIPELINE_SHA = "7c2e91d4b0a1f6e3c58d92a7b4e0f1c3d2a9b8e7";
const MOCK_REFS = [
  { name: "main", kind: "branch", short_sha: MOCK_PIPELINE_SHA.slice(0, 7) },
  { name: "feature/shared-postgres", kind: "branch", short_sha: "4be1f2a" },
  { name: "v0.8.0", kind: "tag", short_sha: "c0790f1" },
];
const mockCatalog = [
  { name: "ci", manual: true, api: false },
  { name: "release", manual: true, api: true },
  { name: "deploy", manual: true, api: false },
  { name: "docs", manual: true, api: false },
  { name: "nightly-bench", manual: false, api: false, error: "line 14: unknown key `timout` (did you mean `timeout`?)" },
];
const mockInputs: Record<string, unknown[]> = {
  release: [
    { name: "version", type: "string", required: true, description: "Semver of the release, without the leading v." },
    { name: "channel", type: "choice", required: true, options: ["stable", "beta", "nightly"], description: "Where the images are published." },
    { name: "platforms", type: "choice", required: false, default: "linux/amd64,linux/arm64", options: ["linux/amd64", "linux/arm64", "linux/amd64,linux/arm64"] },
    { name: "parallelism", type: "number", required: false, default: 4, description: "Build jobs run at once." },
    { name: "sign_images", type: "boolean", required: false, default: true, description: "Sign every pushed image with the release key." },
    { name: "dry_run", type: "boolean", required: false, default: false, description: "Build and test, push nothing." },
    { name: "notes", type: "string", required: false, default: "", description: "Extra text for the GitHub Release body." },
  ],
  deploy: [
    { name: "environment", type: "choice", required: true, options: ["staging", "production"] },
    { name: "image_tag", type: "string", required: true },
    { name: "canary", type: "boolean", required: false, default: false },
  ],
};

// ── /v1/me ────────────────────────────────────────────────────────────────
// An Owner of the one implicit org, so the Settings gear renders.
// The Owner role is global, so Avery is a global admin: the org dropdown in
// Organization settings ends with New organization… (2 waiting): the two
// connections below that serve no org wait on that page (git-bug 19b0a45). Avery
// administers two orgs, so the org dropdown in Organization settings offers a
// choice; globex has no connections or repositories of its own.
/** A mock Principal id (`p_` + 32 hex), fixed per handle so screenshots and
 *  links stay stable. The server mints random ones. */
function pid(handle: string): string {
  let h = 0x811c9dc5;
  let out = "";
  for (let round = 0; round < 4; round++) {
    for (const c of `${round}:${handle}`) h = Math.imul(h ^ c.charCodeAt(0), 0x01000193) >>> 0;
    out += h.toString(16).padStart(8, "0");
  }
  return `p_${out}`;
}

const me = {
  subject: pid("a.kim"),
  id: pid("a.kim"),
  handle: "a.kim",
  display_name: "Avery Kim",
  roles: ["Owner"],
  can_administer: true,
  global_admin: true,
  admin_orgs: ["acme", "globex"],
  token_mint_orgs: ["acme"],
  // Signed in two ways: the legacy GitHub provider and Okta.
  identities: [
    { provider: "default", handle: "a-kim" },
    { provider: "okta", handle: "a.kim@acme.example" },
  ],
};

// ── identity links ─────────────────────────────────────────────
// Three providers so the login page shows the multi-provider choice and the
// Account page has one left to link.
const authProviders = [
  { id: "default", name: "GitHub", default: true },
  { id: "okta", name: "Okta", default: false },
  { id: "forgejo", name: "Forgejo", default: false },
];
// Members and repository Access: an org Owner and an org admin; Jordan, an
// org member who also holds roles on two repositories; collaborators the
// forge gave a role on `scarab`, one of whom never signed in; and an org
// viewer. So the `scarab` repository's Access shows a role granted in Scarab,
// roles from the forge, and access through an org role. The list is mutable:
// grant, revoke and re-sync change it for the life of the page.
type MockBinding = components["schemas"]["BindingDto"];
const bindings: MockBinding[] = [
  { id: pid("a.kim"), handle: "a.kim", role: "owner", project: "", display_name: "Avery Kim", claimed: true, origin: "native" },
  { id: pid("m.diaz"), handle: "m.diaz", role: "admin", project: "", display_name: "Morgan Diaz", claimed: true, origin: "native" },
  { id: pid("j.lee"), handle: "j.lee", role: "member", project: "", display_name: "Jordan Lee", claimed: true, origin: "native" },
  { id: pid("j.lee"), handle: "j.lee", role: "admin", project: "orders-api", display_name: "Jordan Lee", claimed: true, origin: "native" },
  { id: pid("j.lee"), handle: "j.lee", role: "admin", project: "scarab", display_name: "Jordan Lee", claimed: true, origin: "native" },
  { id: pid("r.okafor"), handle: "r.okafor", role: "member", project: "scarab", claimed: false, origin: "import" },
  { id: pid("t.nguyen"), handle: "t.nguyen", role: "viewer", project: "scarab", display_name: "Taylor Nguyen", claimed: true, origin: "import" },
  { id: pid("okta:s.patel@acme.example"), handle: "okta:s.patel@acme.example", role: "viewer", project: "", display_name: "Sam Patel", claimed: true, origin: "native" },
];
// What the forge lists as each repository's collaborators, already mapped to
// roles (admin → admin, write → member, read → viewer). A re-sync of `scarab`
// raises Taylor to member and adds Priya; a repository not listed here has an
// empty listing, which, as on the server, changes nothing.
const forgeCollaborators: Record<string, MockBinding[]> = {
  scarab: [
    { id: pid("r.okafor"), handle: "r.okafor", role: "member", project: "scarab", claimed: false },
    { id: pid("t.nguyen"), handle: "t.nguyen", role: "member", project: "scarab", display_name: "Taylor Nguyen", claimed: true },
    { id: pid("p.ito"), handle: "p.ito", role: "viewer", project: "scarab", claimed: false },
  ],
};

// The server's re-sync, in the mock: seed or refresh the forge's rows, never
// a role granted in Scarab, and drop the forge's rows it no longer lists.
function mockResync(repo: string): MockBinding[] {
  const listed = forgeCollaborators[repo] ?? [];
  if (listed.length === 0) return [];
  for (const c of listed) {
    const row = bindings.find((b) => b.id === c.id && b.project === repo);
    if (!row) bindings.push({ ...c, origin: "import" });
    else if (row.origin === "import") row.role = c.role;
  }
  for (let i = bindings.length - 1; i >= 0; i--) {
    const b = bindings[i]!;
    if (b.project === repo && b.origin === "import" && !listed.some((c) => c.id === b.id)) {
      bindings.splice(i, 1);
    }
  }
  return listed.map(({ id, handle, role, project }) => ({ id, handle, role, project }));
}

function mockGrant(req: { subject?: string; role?: string; project?: string }) {
  const project = req.project ?? "";
  // The server takes a handle or an id and stores the id.
  const who = principals.find((x) => x.handle === req.subject || x.id === req.subject);
  const id = who?.id ?? pid(req.subject ?? "");
  const row = bindings.find((b) => b.id === id && b.project === project);
  if (row) {
    row.role = req.role ?? row.role;
    row.origin = "native";
  } else {
    bindings.push({
      id,
      handle: who?.handle ?? req.subject ?? "",
      role: req.role ?? "member",
      project,
      display_name: who?.display_name ?? null,
      claimed: who?.claimed ?? true,
      origin: "native",
    });
  }
}

const principals = [
  {
    id: pid("j.lee"),
    handle: "j.lee",
    display_name: "Jordan Lee",
    claimed: true,
    identities: [{ provider: "default", handle: "j.lee" }],
  },
  {
    id: pid("r.okafor"),
    handle: "r.okafor",
    display_name: null,
    claimed: false,
    identities: [{ provider: "default", handle: "r.okafor" }],
  },
  {
    id: pid("m.diaz"),
    handle: "m.diaz",
    display_name: "Morgan Diaz",
    claimed: true,
    identities: [{ provider: "default", handle: "m.diaz" }],
  },
  {
    id: pid("t.nguyen"),
    handle: "t.nguyen",
    display_name: "Taylor Nguyen",
    claimed: true,
    identities: [{ provider: "default", handle: "t.nguyen" }],
  },
  {
    id: pid("okta:s.patel@acme.example"),
    handle: "okta:s.patel@acme.example",
    display_name: "Sam Patel",
    claimed: true,
    identities: [{ provider: "okta", handle: "s.patel@acme.example" }],
  },
];

// ── /v1/repos (dashboard: first 4 → "most active" cards; count → "repos · 7")
const proj = (project: string, mins: number | null) => ({
  org: "acme",
  project,
  name: project,
  owner: "acme",
  repo_url: `https://github.com/acme/${project}`,
  connection_id: "gh-acme",
  last_run_at: mins == null ? null : ago(mins * 60000),
  unreachable_since: null as number | null,
});
const projects = [
  proj("scarab", 3),
  proj("orders-api", 21),
  proj("inventory-svc", 48),
  proj("edge-gateway", 120),
  proj("billing", 9),
  // Removed from the GitHub App's installation two days ago: kept, and
  // marked unreachable on its page, the org page and Connections.
  { ...proj("messaging", 300), unreachable_since: ago(2 * 24 * 60 * 60000) },
  proj("scarab-infra", null),
];

// ── /v1/runs (dashboard inbox = 2 suspended + 1 running with a reached gate) ─
const summary = (
  o: Partial<RunSummaryDto> & Pick<RunSummaryDto, "id" | "status">,
): RunSummaryDto => ({
  duration_ms: 0,
  created_at: ago(60000),
  deploy: false,
  ...o,
});
const runs = [
  summary({
    id: "01984f10a1b24c7e8f0012ab34cd56ef",
    status: "suspended",
    org: "acme",
    project: "scarab",
    created_at: ago(12 * 60000),
    waiting_gates: [{ step: "approve", key: mockStepKey("approve"), kind: "manual", since: ago(10 * 60000) }],
    waiting_for: { step: "approve", kind: "manual" },
  }),
  summary({
    id: "01984f22bb334d8e9a1122cd44ef6600",
    status: "suspended",
    org: "acme",
    project: "billing",
    created_at: ago(27 * 60000),
    waiting_gates: [{ step: "release", key: mockStepKey("release"), kind: "manual", since: ago(20 * 60000) }],
    waiting_for: { step: "release", kind: "environment_approval" },
  }),
  // git-bug c489346: a gate holds only its dependents, so this run is still
  // `running` (its `version` sibling runs) while its `approve` gate waits.
  summary({
    id: "01984f35cc445e9fab2233de55f07711",
    status: "running",
    org: "acme",
    project: "orders-api",
    created_at: ago(6 * 60000),
    waiting_gates: [{ step: "approve", key: mockStepKey("approve"), kind: "manual", since: ago(4 * 60000) }],
  }),
  // A running run with no reached gate: not in the inbox.
  summary({
    id: "01984f40dd556fa0bc3344ef66018822",
    status: "running",
    org: "acme",
    project: "inventory-svc",
    created_at: ago(2 * 60000),
  }),
];
/** The `/v1/runs` fixture, for no-DOM tests of what the dashboard derives. */
export { runs as mockRuns };

// ── per-repo run strips (dashboard cards) — status+duration_ms drive bars ────
// Tune succeeded/failed ratios to the "N% pass" shown per card.
// Headlines for the filler runs, so every row reads like a real one.
const STRIP_TITLES = [
  "Retry flaky upload in the artifact step",
  "Bump tokio to 1.41",
  "Cache cargo registry between runs",
  "Split integration tests by crate",
  "Pin the base image by digest",
  "Fix timezone in the nightly report",
  "Drop the unused staging secret",
];
const strip = (pattern: string, first = 120): { runs: RunSummaryDto[] } =>
  ({
    runs: pattern.split("").map((c, i) => ({
      id: `r${i}`,
      run_number: first - i,
      sha: (0x9e3779b1 * (first - i)).toString(16).padStart(8, "0").slice(-7) + "0".repeat(33),
      trigger_kind: "push",
      trigger_title: STRIP_TITLES[i % STRIP_TITLES.length],
      // Pushes land on main, with every fourth on a feature branch, by one
      // of three people, so a row names its branch and its author.
      git_ref: i % 4 === 1 ? "refs/heads/feat/cache-registry" : "refs/heads/main",
      actor: ["a.kim", "m.diaz", "j.rao"][i % 3],
      status: c === "x" ? "failed" : c === "c" ? "cancelled" : "succeeded",
      ...(c === "x" ? { failed_step: "test", failed_steps_count: 1 } : {}),
      duration_ms: 40000 + ((i * 37) % 11) * 22000,
      created_at: ago((i + 1) * 900000),
      // Every third run is a deploy — marked `deploy: true`, with an
      // `environment:` — so the runs list's pipeline/environment/deploys
      // filters have something to offer.
      pipeline: i % 3 === 2 ? "deploy" : "ci",
      deploy: i % 3 === 2,
      ...(i % 3 === 2 ? { environment: i % 2 ? "production" : "staging" } : {}),
    })),
  });
// Two of scarab's runs were started by its nightly schedule (trigger kind
// `cron`, titled with the expression, as the server records them), so the
// Runs tab's scheduled filter and the Schedules tab's Runs link have rows.
const scarabRuns = strip("ooxooooxooxooo");
for (const i of [3, 7]) {
  Object.assign(scarabRuns.runs[i]!, {
    trigger_kind: "cron",
    trigger_title: "0 2 * * *",
    pipeline: "nightly",
    actor: null,
  });
}
// The newest of scarab's runs, above the filler: pull request #131 pushed
// three times (each newer push cancelled the Run before it), a deploy held
// at its Environment's approval, a Run held at a manual gate, and a failed
// Run whose two plan cells failed. The last three open the run fixtures
// below; the pull request #131 Runs have no run page in the mock.
const pr131 = (o: Partial<RunSummaryDto> & Pick<RunSummaryDto, "id" | "status">): RunSummaryDto => ({
  trigger_kind: "pull_request",
  git_ref: "refs/pull/131/head",
  pr_number: 131,
  origin_pr_base: "main",
  origin_pr_head: "feat/shared-postgres",
  actor: "a.kim",
  trigger_title: "Wire shared postgres into the integration suite",
  duration_ms: 0,
  created_at: 0,
  pipeline: "ci",
  deploy: false,
  ...o,
});
scarabRuns.runs.unshift(
  pr131({
    id: "0190f8a2000071fb8c00c0ffee000003",
    status: "running",
    run_number: 155,
    sha: "e4d3c2b1a0f9e8d7c6b5a4f3e2d1c0b9a8f7e6d5",
    created_at: ago(90_000),
  }),
  pr131({
    id: "0190f8a2000071fb8c00c0ffee000002",
    status: "cancelled",
    run_number: 154,
    sha: "d3c2b1a0f9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4",
    duration_ms: 70_000,
    created_at: ago(4 * 60_000),
    replaced_by: "0190f8a2000071fb8c00c0ffee000003",
  }),
  pr131({
    id: "0190f8a2000071fb8c00c0ffee000001",
    status: "cancelled",
    run_number: 153,
    sha: "c2b1a0f9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b3",
    duration_ms: 95_000,
    created_at: ago(7 * 60_000),
    replaced_by: "0190f8a2000071fb8c00c0ffee000002",
  }),
  pr131({
    id: ENV_GATE_RUN_ID,
    status: "suspended",
    run_number: 152,
    trigger_kind: "push",
    git_ref: "refs/heads/main",
    pr_number: null,
    origin_pr_base: null,
    origin_pr_head: null,
    trigger_title: "Deploy 7c1e2d4 to production",
    sha: "7c1e2d4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d",
    created_at: ago(5 * 60_000),
    pipeline: "deploy",
    deploy: true,
    environment: "production",
    waiting_for: { step: "environment-approval", kind: "environment_approval" },
  }),
  pr131({
    id: RICH_RUN_ID,
    status: "suspended",
    run_number: 147,
    sha: "7c2e91d04b8a3f5e6d1c0b9a87654321fedcba98",
    created_at: ago(9 * 60_000),
    pipeline: "release",
    waiting_for: { step: "approve", kind: "manual" },
  }),
  pr131({
    id: MATRIX_RUN_ID,
    status: "failed",
    run_number: 33,
    git_ref: "refs/pull/2949/head",
    pr_number: 2949,
    origin_pr_base: "develop",
    origin_pr_head: "feature/cluster-module",
    actor: "thulasi",
    trigger_title: "Smoke-run the plan pipelines on the new cluster module",
    sha: "15f2297b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f",
    duration_ms: 11 * 60_000,
    created_at: ago(12 * 60_000),
    pipeline: "tofu-plan",
    failed_step: "plan[cluster=treebo-production,phase=aws-post-cluster]",
    failed_steps_count: 2,
  }),
);
const repoRuns: Record<string, ReturnType<typeof strip>> = {
  scarab: scarabRuns, // ~64% pass window near the older tail
  "orders-api": strip("oxooxooxooooo"),
  "inventory-svc": strip("oxoxooxxooxoo"), // ~50%
  "edge-gateway": strip("ooooooooxoooo"), // ~93%
  // matches its last_run_at: null above — the empty-state repo (ponder scene)
  "scarab-infra": { runs: [] },
};
// A gentler pass mix so the shown percentages land ~64/64/50/93 like the
// original: recompute deterministically by fixed strings above (close enough).

// ── run manifests (`GET /v1/runs/{id}/manifest`) ───────────────────────────
// The Pipeline file each run was created from, as recorded at creation. The
// matrix run invokes the clone library; the retries run predates recording,
// and any other run answers the same way.
const runManifests: Record<string, RunManifestDto> = {
  [RUN_ID]: {
    recorded: true,
    path: ".scarab/ci.yaml",
    sha: "3f9a1c2d9e8b7a6c5d4e3f2a1b0c9d8e4f5a6b7c",
    source: `name: ci
on:
  push: { branches: [main] }
  pull_request: {}
steps:
  - id: clone
    image: alpine/git:2.45.2
    command: [git, clone, --depth=1, "\${{ event.clone_url }}", /workspace/src]
  - id: build
    image: rust:1.97
    needs: [clone]
    command: [cargo, build, --release, --locked]
  - id: test
    image: rust:1.97
    needs: [build]
    command: [cargo, nextest, run, --workspace]
`,
    invoked: [],
  },
  [MATRIX_RUN_ID]: {
    recorded: true,
    path: ".scarab/tofu-plan.yaml",
    sha: "15f2297b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f",
    source: `name: tofu-plan
on:
  pull_request: { branches: [develop] }
steps:
  - id: checkout
    invoke: lib:thulasi-ram::scarab-ci::clone
    with: { depth: "1" }
  - id: plan
    image: ghcr.io/opentofu/opentofu:1.8.3
    needs: [checkout]
    matrix:
      cluster: [superhero-tools-common, treebo-production, treebo-staging]
      phase: [aws-cluster, aws-post-cluster]
    retry: { attempts: 2 }
    command:
      - sh
      - -c
      - |
        cd live/\${{ matrix.cluster }}/\${{ matrix.phase }}
        tofu init -input=false
        tofu plan -input=false -out=/scarab/artifacts/plan.bin
`,
    invoked: [
      {
        kind: "library",
        name: "thulasi-ram::scarab-ci::clone",
        source: "github.com/thulasi-ram/scarab-ci//libs/clone.yaml@ac8c661a4e0f3b2d9c7a5e1f0b6d4c2a8e9f7b13",
        content_hash: "9d4b1e7c3a2f8e6d5c4b3a2f1e0d9c8b7a6f5e4d3c2b1a0f9e8d7c6b5a4f3e2d",
        steps: ["checkout/git"],
      },
    ],
  },
  [RETRIES_RUN_ID]: { recorded: false, invoked: [] },
};

// ── run detail ──────────────────────────────────────────────────────────────
const runStatus: RunStatusDto = {
  id: RUN_ID,
  status: "running",
  run_number: 142,
  pipeline: "ci",
  trigger_title: "Add exponential backoff to step retries",
  origin_pr_base: null,
  params: {},
  // A live run is not on the retention clock at all (a non-terminal
  // run is never GC-eligible), so no expiry is promised and nothing is pinned.
  snapshot_retention: { retention_days: 14, expired: false, pinned: false },
  steps: [
    {
      id: "clone",
      status: "succeeded",
      attempts: 1,
      needs: [],
      attempt_list: [
        { id: "a1", started_at: ago(325000), finished_at: ago(298000), failed: false, outcome: "succeeded" },
      ],
    },
    {
      id: "build",
      status: "succeeded",
      attempts: 2,
      needs: ["clone"],
      attempt_list: [
        { id: "a1", started_at: ago(297000), finished_at: ago(260000), failed: true, failure: "step", outcome: "failed" },
        { id: "a2", started_at: ago(250000), finished_at: ago(190000), failed: false, outcome: "succeeded" },
      ],
    },
    {
      id: "test",
      status: "running",
      attempts: 1,
      needs: ["build"],
      attempt_list: [{ id: "a1", started_at: ago(190000), finished_at: null, failed: false, outcome: "running" }],
    },
  ],
};

// events (text/event-stream body) — trigger provenance + step timing.
const sse = (run: string) => (at: number, kind: unknown) =>
  `data: ${JSON.stringify({ version: 1, run, at, kind })}\n\n`;
const ev = sse(RUN_ID);
const eventsBody = [
  ev(ago(330000), {
    Raw: {
      trigger: {
        event: {
          kind: "pull_request",
          actor: "a.kim",
          branch: "main",
          ref: "refs/heads/feature",
          sha: "3f9a1c2d9e8b7a6c5d4e3f2a1b0c9d8e",
          repo: { owner: "acme", name: "scarab" },
          pr: 128,
        },
      },
    },
  }),
  ev(ago(325000), { AttemptStarted: { step: "clone", attempt: "a1" } }),
  ev(ago(298000), { AttemptFinished: { step: "clone", attempt: "a1" } }),
  ev(ago(297000), { AttemptStarted: { step: "build", attempt: "a1" } }),
  ev(ago(260000), { AttemptFinished: { step: "build", attempt: "a1", failure: "step" } }),
  ev(ago(250000), { AttemptStarted: { step: "build", attempt: "a2" } }),
  ev(ago(190000), { AttemptFinished: { step: "build", attempt: "a2" } }),
  ev(ago(190000), { AttemptStarted: { step: "test", attempt: "a1" } }),
].join("");

// ── the RICH run (Playwright walkthrough fixture) ───────────────────────────
// One run exercising every multi-take surface at once. Timeline:
//
//   version 1 (original run): clone@a1 ok → build@a1 ok → test@a1 in flight
//   ── a.kim reruns build (cascade: test, approve) at T-15m ─────────────────
//   version 2 (latest):       build@a2 failed → build@a3 ok (auto-retry)
//                             → test@a2 ok → approve gate waiting (manual)
//
// Evidence baked in: test@a1 is SUPERSEDED (in flight at the boundary, re-armed
// by the rerun); build@a1 is SHADOWED (an earlier success no longer of record);
// the latest take's build window is [a2, a3] → the attempts dropdown shows a
// failed try followed by a succeeding one; `postgres` is a SHARED service
// (peer node in the DAG services lane, dotted `uses` edge to test); `redis` is
// test's docked SIDECAR chip; the run is suspended on the manual `approve`
// gate; `build@a3` published one artifact of record.
const richRunStatus: RunStatusDto = {
  id: RICH_RUN_ID,
  status: "suspended",
  run_number: 147,
  pipeline: "release",
  trigger_title: "Wire shared postgres into the integration suite",
  origin_pr_base: null,
  // The generated schema types params opaquely; the values are plain JSON.
  // `channel: edge` is an option the pipeline has since removed, so a new run
  // with these inputs starts `channel` from its default.
  params: { version: "0.8.1", channel: "edge", parallelism: 8, sign_images: true, dry_run: false } as unknown as RunStatusDto["params"],
  // Suspended on a gate, so likewise never GC-eligible regardless of age
  // — but PINNED, to render the "keep this run's snapshots" state.
  snapshot_retention: {
    retention_days: 14,
    expired: false,
    pinned: true,
    pinned_by: "priya",
    pinned_at: ago(120000),
  },
  steps: [
    {
      id: "clone",
      status: "succeeded",
      attempts: 1,
      needs: [],
      attempt_list: [
        { id: "a1", started_at: ago(25 * min), finished_at: ago(24 * min + 45000), failed: false, outcome: "succeeded" },
      ],
    },
    {
      id: "build",
      status: "succeeded",
      attempts: 3,
      needs: ["clone"],
      attempt_list: [
        { id: "a1", started_at: ago(24 * min + 30000), finished_at: ago(23 * min + 20000), failed: false, outcome: "succeeded" },
        {
          id: "a2",
          started_at: ago(14 * min + 55000),
          finished_at: ago(13 * min + 50000),
          failed: true,
          failure: "step",
          outcome: "failed",
        },
        { id: "a3", started_at: ago(13 * min + 40000), finished_at: ago(12 * min + 30000), failed: false, outcome: "succeeded" },
      ],
    },
    {
      id: "test",
      status: "succeeded",
      attempts: 2,
      needs: ["build"],
      services: [{ index: 0, image: "redis:7.4", ports: [6379] }],
      uses: ["postgres"],
      attempt_list: [
        { id: "a1", started_at: ago(23 * min + 15000), finished_at: ago(15 * min), failed: false, outcome: "superseded" },
        { id: "a2", started_at: ago(12 * min + 25000), finished_at: ago(10 * min), failed: false, outcome: "succeeded" },
      ],
    },
    {
      id: "approve",
      status: "waiting",
      attempts: 0,
      needs: ["test"],
      gate: "manual",
      attempt_list: [],
    },
  ],
};

const rev = sse(RICH_RUN_ID);
const richEventsBody = [
  rev(ago(25 * min + 5000), {
    Raw: {
      trigger: {
        event: {
          kind: "pull_request",
          actor: "a.kim",
          branch: "main",
          ref: "refs/heads/feature/shared-postgres",
          sha: "7c2e91d04b8a3f5e6d1c0b9a87654321",
          repo: { owner: "acme", name: "scarab" },
          pr: 131,
        },
      },
    },
  }),
  // ── version 1 (original run) ──
  rev(ago(25 * min), { AttemptStarted: { step: "clone", attempt: "a1" } }),
  rev(ago(24 * min + 45000), { AttemptFinished: { step: "clone", attempt: "a1" } }),
  rev(ago(24 * min + 44000), { StepTransitioned: { step: "clone", from: "Running", to: "Succeeded" } }),
  rev(ago(24 * min + 30000), { AttemptStarted: { step: "build", attempt: "a1" } }),
  rev(ago(23 * min + 20000), { AttemptFinished: { step: "build", attempt: "a1" } }),
  rev(ago(23 * min + 19000), { StepTransitioned: { step: "build", from: "Running", to: "Succeeded" } }),
  rev(ago(23 * min + 15000), { AttemptStarted: { step: "test", attempt: "a1" } }),
  // ── the boundary: a human reran build; test (in flight) + approve cascade ──
  rev(ago(15 * min), {
    RunRerunRequested: { target: "build", invalidated: ["test", "approve"], by: "a.kim" },
  }),
  // ── version 2 (latest) ──
  rev(ago(14 * min + 55000), { AttemptStarted: { step: "build", attempt: "a2" } }),
  rev(ago(13 * min + 50000), { AttemptFinished: { step: "build", attempt: "a2", failure: "step" } }),
  rev(ago(13 * min + 49000), { StepTransitioned: { step: "build", from: "Running", to: "Ready" } }),
  rev(ago(13 * min + 40000), { AttemptStarted: { step: "build", attempt: "a3" } }),
  rev(ago(12 * min + 30000), { AttemptFinished: { step: "build", attempt: "a3" } }),
  rev(ago(12 * min + 29000), { StepTransitioned: { step: "build", from: "Running", to: "Succeeded" } }),
  rev(ago(12 * min + 25000), { AttemptStarted: { step: "test", attempt: "a2" } }),
  rev(ago(10 * min), { AttemptFinished: { step: "test", attempt: "a2" } }),
  rev(ago(10 * min - 1000), { StepTransitioned: { step: "test", from: "Running", to: "Succeeded" } }),
].join("");

// A status as the server's events carry it: the engine's names, `Succeeded`
// for the DTOs' `succeeded`.
const wireStatus = (s: string): string => s.replace(/(^|_)([a-z])/g, (_m, _u, c: string) => c.toUpperCase());

// ── the MATRIX run (summary line, matrix labels, retries) ───────────────────
// checkout `checkout/git` (the clone library's Step), then `plan` over
// cluster × phase. treebo-production ×
// aws-cluster failed twice (the engine retried it), and its second try waited
// about a minute for a node first; treebo-production × aws-post-cluster
// failed once. The other four cells passed and published a plan each.
const MX_CELLS: [string, string, "ok" | "fail" | "fail2"][] = [
  ["superhero-tools-common", "aws-cluster", "ok"],
  ["superhero-tools-common", "aws-post-cluster", "ok"],
  ["treebo-production", "aws-cluster", "fail2"],
  ["treebo-production", "aws-post-cluster", "fail"],
  ["treebo-staging", "aws-cluster", "ok"],
  ["treebo-staging", "aws-post-cluster", "ok"],
];
const mxId = (cluster: string, phase: string) => `plan[cluster=${cluster},phase=${phase}]`;
const MX_T0 = 12 * min; // the run started 12 minutes ago
const MX_PLAN_AT = MX_T0 - 61_000; // the plan cells start when checkout/git finishes
const matrixRunStatus: RunStatusDto = {
  id: MATRIX_RUN_ID,
  status: "failed",
  run_number: 33,
  pipeline: "tofu-plan",
  trigger_title: "Smoke-run the plan pipelines on the new cluster module",
  origin_pr_base: "develop",
  origin_pr_head: "feature/cluster-module",
  params: {},
  snapshot_retention: { retention_days: 14, expired: false, pinned: false },
  steps: [
    {
      id: "checkout/git",
      status: "succeeded",
      attempts: 1,
      needs: [],
      origin: [
        {
          kind: "library",
          name: "thulasi-ram::scarab-ci::clone",
          source: "github.com/thulasi-ram/scarab-ci//libs/clone.yaml@ac8c661a4e0f3b2d9c7a5e1f0b6d4c2a8e9f7b13",
          content_hash: "9d4b1e7c3a2f8e6d5c4b3a2f1e0d9c8b7a6f5e4d3c2b1a0f9e8d7c6b5a4f3e2d",
        },
      ],
      attempt_list: [
        { id: "a1", started_at: ago(MX_T0), finished_at: ago(MX_PLAN_AT), failed: false, outcome: "succeeded" },
      ],
    },
    ...MX_CELLS.map(([cluster, phase, how], i) => {
      const start = MX_PLAN_AT - 2000;
      const took = 150_000 + i * 6_000;
      const attempt_list =
        how === "ok"
          ? [{ id: "a1", started_at: ago(start), finished_at: ago(start - took), failed: false, outcome: "succeeded" }]
          : how === "fail"
            ? [
                {
                  id: "a1",
                  started_at: ago(start),
                  finished_at: ago(start - 121_000),
                  failed: true,
                  failure: "step",
                  failure_detail: "tofu plan exited 1: Error acquiring the state lock",
                  outcome: "failed",
                },
              ]
            : [
                {
                  id: "a1",
                  started_at: ago(start),
                  finished_at: ago(start - 48_000),
                  failed: true,
                  failure: "step",
                  failure_detail: "tofu init exited 1: registry.terraform.io timed out",
                  outcome: "failed",
                },
                {
                  id: "a2",
                  started_at: ago(start - 60_000),
                  finished_at: ago(start - 60_000 - 126_000),
                  failed: true,
                  failure: "step",
                  failure_detail: "tofu plan exited 1: Error acquiring the state lock",
                  outcome: "failed",
                },
              ];
      return {
        id: mxId(cluster, phase),
        status: how === "ok" ? "succeeded" : "failed",
        attempts: attempt_list.length,
        needs: ["checkout/git"],
        attempt_list,
      };
    }),
  ],
};
const mxv = sse(MATRIX_RUN_ID);
const matrixEventsBody = (() => {
  const out: string[] = [
    mxv(ago(MX_T0 + 3000), {
      Raw: {
        trigger: {
          event: {
            kind: "pull_request",
            actor: "thulasi",
            branch: "feature/cluster-module",
            sha: "15f2297b9c0d1e2f3a4b5c6d7e8f9a0b",
            repo: { owner: "acme", name: "scarab" },
            pr: 2949,
          },
        },
      },
    }),
    mxv(ago(MX_T0), { AttemptStarted: { step: "checkout/git", attempt: "a1" } }),
    mxv(ago(MX_PLAN_AT), { AttemptFinished: { step: "checkout/git", attempt: "a1" } }),
    mxv(ago(MX_PLAN_AT - 500), { StepTransitioned: { step: "checkout/git", from: "Running", to: "Succeeded" } }),
  ];
  const timed: [number, string][] = [];
  for (const s of matrixRunStatus.steps.slice(1)) {
    for (const a of s.attempt_list ?? []) {
      timed.push([a.started_at, mxv(a.started_at, { AttemptStarted: { step: s.id, attempt: a.id } })]);
      if (a.id === "a2") {
        // The retry waited about a minute for a node before it started.
        timed.push([
          a.started_at + 62_000,
          mxv(a.started_at + 62_000, {
            StepInfraCondition: {
              step: s.id,
              attempt: a.id,
              reason: "Unschedulable",
              message: "0/14 nodes are available: 12 node(s) had untolerated taints, 2 node(s) didn't match the node selector",
              held_ms: 62_000,
              observations: 4,
            },
          }),
        ]);
      }
      const fin = a.finished_at!;
      timed.push([
        fin,
        mxv(fin, { AttemptFinished: { step: s.id, attempt: a.id, ...(a.failed ? { failure: a.failure } : {}) } }),
      ]);
    }
    timed.push([
      s.attempt_list![s.attempt_list!.length - 1]!.finished_at! + 500,
      mxv(s.attempt_list![s.attempt_list!.length - 1]!.finished_at! + 500, {
        StepTransitioned: { step: s.id, from: "Running", to: wireStatus(s.status) },
      }),
    ]);
  }
  timed.sort((a, b) => a[0] - b[0]);
  const end = Math.max(...timed.map(([at]) => at));
  out.push(...timed.map(([, line]) => line));
  out.push(mxv(end + 1000, { RunTransitioned: { from: "Running", to: "Failed" } }));
  return out.join("");
})();
const matrixArtifacts: ArtifactDto[] = MX_CELLS.filter(([, , how]) => how === "ok").map(([cluster, phase], i) => ({
  name: `plan-${cluster}-${phase}.txt`,
  step: mxId(cluster, phase),
  attempt: "a1",
  of_record: true,
  succeeded: true,
  size: 22_000 + i * 13_000,
  content_type: "text/plain",
}));

// ── the RETRIES run (try tabs, Activity filter, earlier artifact copies) ────
// Times are minutes:seconds before now. `unit` failed, timed out after waiting
// for a node, then passed when a.kim retried it. `e2e` failed five times (two
// tries waited for a node first) and passed on its sixth. Both
// `deploy-preview` cells are waiting for a node right now.
const ms = (m: number, sec = 0) => (m * 60 + sec) * 1000;
type MockTry = {
  id: string;
  start: number;
  end: number | null;
  failure?: string;
  detail?: string;
  /** A Pod condition before the try started: [onset, close] (close null = still active). */
  wait?: [number, number | null];
};
const RT_TRIES: Record<string, MockTry[]> = {
  git: [{ id: "a1", start: ms(20), end: ms(19, 20) }],
  unit: [
    { id: "a1", start: ms(19, 10), end: ms(17, 10), failure: "step", detail: "cargo test exited 101: 2 tests failed" },
    { id: "a2", start: ms(17), end: ms(13, 50), failure: "timeout", detail: "no output for 2m", wait: [ms(16, 59), ms(15, 50)] },
    { id: "a3", start: ms(12, 55), end: ms(10, 40) },
  ],
  e2e: [
    { id: "a1", start: ms(19, 10), end: ms(18), failure: "step", detail: "playwright: 3 specs failed" },
    { id: "a2", start: ms(17, 50), end: ms(16), failure: "lost", detail: "the node was drained", wait: [ms(17, 49), ms(17, 10)] },
    { id: "a3", start: ms(15, 50), end: ms(14), failure: "timeout", detail: "no output for 2m", wait: [ms(15, 49), ms(15, 19)] },
    { id: "a4", start: ms(13, 50), end: ms(12), failure: "step", detail: "playwright: 1 spec failed" },
    { id: "a5", start: ms(11, 50), end: ms(10), failure: "step", detail: "playwright: 1 spec failed" },
    { id: "a6", start: ms(9, 50), end: ms(7) },
  ],
  "deploy-preview[region=eu]": [{ id: "a1", start: ms(6, 50), end: null, wait: [ms(6, 45), null] }],
  "deploy-preview[region=us]": [{ id: "a1", start: ms(6, 50), end: null, wait: [ms(6, 45), null] }],
};
/** A log stream of run #34 for a try whose Pod still waits for a node: such a
 * Pod has printed nothing, so the mock streams no lines for it. */
function unscheduledTry(url: string): boolean {
  let u: URL;
  try {
    u = new URL(url, window.location.origin);
  } catch {
    return false;
  }
  const m = u.pathname.match(/^\/v1\/runs\/([^/]+)\/steps\/([^/]+)\/logs$/);
  if (!m || m[1] !== RETRIES_RUN_ID) return false;
  const tries = RT_TRIES[mockStepId(m[2]!)] ?? [];
  const attempt = u.searchParams.get("attempt");
  const t = tries.find((x) => x.id === attempt) ?? tries[tries.length - 1];
  return !!t?.wait && t.wait[1] === null;
}
const RT_NEEDS: Record<string, string[]> = {
  git: [],
  unit: ["git"],
  e2e: ["git"],
  "deploy-preview[region=eu]": ["unit", "e2e"],
  "deploy-preview[region=us]": ["unit", "e2e"],
};
const retriesRunStatus: RunStatusDto = {
  id: RETRIES_RUN_ID,
  status: "running",
  run_number: 34,
  pipeline: "ci",
  trigger_title: "Retry the flaky checkout tests",
  origin_pr_base: "main",
  origin_pr_head: "fix/flaky-checkout",
  params: {},
  snapshot_retention: { retention_days: 14, expired: false, pinned: false },
  steps: Object.entries(RT_TRIES).map(([id, tries]) => {
    const last = tries[tries.length - 1]!;
    return {
      id,
      status: last.end === null ? "running" : last.failure ? "failed" : "succeeded",
      attempts: tries.length,
      needs: RT_NEEDS[id] ?? [],
      attempt_list: tries.map((t) => ({
        id: t.id,
        started_at: ago(t.start),
        finished_at: t.end === null ? null : ago(t.end),
        failed: !!t.failure,
        ...(t.failure ? { failure: t.failure, failure_detail: t.detail } : {}),
        outcome: t.end === null ? "running" : t.failure ? "failed" : "succeeded",
      })),
    };
  }),
};
const rtv = sse(RETRIES_RUN_ID);
const retriesEventsBody = (() => {
  const timed: [number, unknown][] = [
    [
      ms(20, 3),
      {
        Raw: {
          trigger: {
            event: {
              kind: "pull_request",
              actor: "a.kim",
              branch: "fix/flaky-checkout",
              sha: "9e1c4a7b2d3f5e6a7b8c9d0e1f2a3b4c",
              repo: { owner: "acme", name: "scarab" },
              pr: 162,
            },
          },
        },
      },
    ],
    [ms(20, 2), { RunTransitioned: { from: "Pending", to: "Running" } }],
    // a.kim retried `unit` after its second try timed out.
    [ms(13), { StepRetryRequested: { target: "unit", invalidated: [], by: "a.kim" } }],
  ];
  const message = "0/9 nodes are available: 9 Insufficient cpu";
  for (const [step, tries] of Object.entries(RT_TRIES)) {
    for (const t of tries) {
      timed.push([t.start, { AttemptStarted: { step, attempt: t.id } }]);
      if (t.wait) {
        const [onset, close] = t.wait;
        timed.push([onset, { StepInfraCondition: { step, attempt: t.id, reason: "Unschedulable", message } }]);
        if (close !== null)
          timed.push([
            close,
            {
              StepInfraCondition: {
                step,
                attempt: t.id,
                reason: "Unschedulable",
                message,
                held_ms: onset - close,
                observations: 3,
              },
            },
          ]);
      }
      if (t.end !== null)
        timed.push([t.end, { AttemptFinished: { step, attempt: t.id, ...(t.failure ? { failure: t.failure } : {}) } }]);
    }
    const last = tries[tries.length - 1]!;
    if (last.end !== null)
      timed.push([
        last.end - 500,
        { StepTransitioned: { step, from: "Running", to: last.failure ? "Failed" : "Succeeded" } },
      ]);
  }
  // Oldest first: a larger "before now" is earlier.
  timed.sort((a, b) => b[0] - a[0]);
  return timed.map(([before, kind]) => rtv(ago(before), kind)).join("");
})();
const rtArtifact = (name: string, step: string, attempt: string, of_record: boolean, size: number, content_type: string) => ({
  name,
  step,
  attempt,
  of_record,
  succeeded: !RT_TRIES[step]!.find((t) => t.id === attempt)!.failure,
  size,
  content_type,
});
const retriesArtifacts: ArtifactDto[] = [
  rtArtifact("junit.xml", "unit", "a1", false, 18_400, "application/xml"),
  rtArtifact("junit.xml", "unit", "a3", true, 18_900, "application/xml"),
  rtArtifact("coverage.txt", "unit", "a3", true, 4_100, "text/plain"),
  rtArtifact("report.html", "e2e", "a1", false, 310_000, "text/html"),
  rtArtifact("report.html", "e2e", "a4", false, 305_000, "text/html"),
  rtArtifact("report.html", "e2e", "a6", true, 298_000, "text/html"),
  // Only a failed try wrote the trace: nothing is of record for it.
  rtArtifact("trace.zip", "e2e", "a5", false, 2_400_000, "application/zip"),
];

const richArtifacts: ArtifactDto[] = [
  {
    name: "scarab-dist.tar.gz",
    step: "build",
    attempt: "a3",
    of_record: true,
    succeeded: true,
    size: 4718592,
    content_type: "application/gzip",
  },
];

const richServices: ServiceStatusDto[] = [
  { name: "postgres", status: "ready", take: 2, created_at: ago(14 * min + 58000) },
];

const serviceLogLines = [
  "PostgreSQL init process complete; ready for start up.",
  "LOG:  starting PostgreSQL 16.3 on x86_64-pc-linux-musl",
  "LOG:  listening on IPv4 address \"0.0.0.0\", port 5432",
  "LOG:  database system is ready to accept connections",
];

// `image` is what the build library writes from `buildctl --metadata-file`
// (plus `digest` and `layout`): a nested object that dwarfs the scalars.
const testResults = [
  { name: "tests_passed", type_name: "number", value: 214 },
  { name: "coverage", type_name: "string", value: "92.4%" },
  {
    name: "image",
    type_name: "object",
    value: {
      "containerimage.config.digest": "sha256:4f1c8e0d2b7a9c63e5f04d1b8a2c7e9f0d3b6a5c4e8f1a2b3c4d5e6f7a8b9c0d",
      "containerimage.descriptor": {
        mediaType: "application/vnd.oci.image.manifest.v1+json",
        digest: "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
        size: 1610,
        annotations: {
          "org.opencontainers.image.created": "2026-11-26T04:12:09Z",
          "org.opencontainers.image.revision": "7c2e91d04b8a3f5e6d1c0b9a87654321",
          "org.opencontainers.image.source": "https://github.com/acme/scarab",
        },
        platform: { architecture: "amd64", os: "linux" },
      },
      "containerimage.digest": "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
      "image.name": "registry.acme.example/scarab/server:sha-7c2e91d",
      digest: "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
      layout: "/workspace/.scarab-build/5d41402abc4b2a76b9719d911017c592/oci",
    },
  },
];
const testConsumed = { attempt: "a1", consumed: { build: "a2" } };

const testLogLines = [
  "$ cargo test --workspace --locked",
  "   Compiling scarab-core v0.4.0",
  "   Compiling scarab-engine v0.4.0",
  "   Compiling scarab-server v0.4.0",
  "    Finished test [unoptimized + debuginfo] target(s) in 48.21s",
  "     Running unittests src/lib.rs (target/debug/deps/scarab_engine-9f2c1a)",
  "running 214 tests",
  "test executor::retry::backoff_is_exponential ... ok",
  "test executor::retry::gives_up_after_max_attempts ... ok",
  "test engine::admission::env_gate_blocks_unapproved_ref ... ok",
  "test engine::dag::topological_order_is_stable ... ok",
  "test store::cas::roundtrips_content_addressed_blob ... ok",
  "test store::cas::dedups_identical_blobs ... ok",
  "test log::pipeline::appends_are_ordered_per_fence ... ok",
];

// ── log streams ─────────────────────────────────────────────────────────────
// The mock serves a log as the server does: stored chunks of raw output, each
// `data` event carrying an id `<attempt>:<next chunk>` to resume after, the
// end of the log from the chunk that holds the `?tail=`-th byte from the end
// with a `truncated` event first, and the whole log at
// `…/logs/raw` (a step try's or a shared service's). A part of a log can be
// the server's note about it instead of output: a `marker` event with an id
// like a chunk's, and a `[scarab] <text>` line in the raw log.

/** One stored part of a mock log: raw output, or the server's note. */
type MockLogPart = string | { marker: { kind: string; reason?: string; text: string } };

/** The rich run's `test` step: its log source restarted part way through, so
 * a note sits between the two parts of its log. */
function restartedTestLog(): MockLogPart[] {
  return [
    testLogLines.slice(0, 7).join("\n") + "\n",
    {
      marker: {
        kind: "source_restarted",
        text: "the log source restarted; lines before this point may be missing or repeat",
      },
    },
    testLogLines.slice(7).join("\n") + "\n",
  ];
}

/** The matrix cell whose `tofu plan` printed a large log (about 5.5 MiB): the
 * pane shows its last 2 MiB and offers Load full log and Download. */
const BIG_LOG_STEP = mxId("treebo-staging", "aws-cluster");
let bigLogChunks: string[] | null = null;
/** The large log's chunks, built on first use: 13,000 planned resources,
 * 200 to a chunk. */
function bigLog(): string[] {
  if (bigLogChunks) return bigLogChunks;
  const header = "$ tofu plan -input=false -out=plan.tfplan\nOpenTofu will perform the following actions:\n\n";
  const out: string[] = [];
  const total = 13_000;
  let chunk = header;
  for (let i = 0; i < total; i++) {
    const az = ["a", "b", "c"][i % 3];
    chunk +=
      `  # module.cluster.aws_security_group_rule.node_ingress[${i}] will be created\n` +
      `  + resource "aws_security_group_rule" "node_ingress" {\n` +
      `      + cidr_blocks              = ["10.${(i >> 8) & 255}.${i & 255}.0/24"]\n` +
      `      + from_port                = ${30000 + (i % 2768)}\n` +
      `      + id                       = (known after apply)\n` +
      `      + protocol                 = "tcp"\n` +
      `      + security_group_id        = "sg-0${(i * 2654435761 >>> 0).toString(16).padStart(8, "0")}"\n` +
      `      + description              = "node port ${i} (eu-west-1${az})"\n` +
      `    }\n\n`;
    if ((i + 1) % 200 === 0) {
      out.push(chunk);
      chunk = "";
    }
  }
  out.push(chunk + `Plan: ${total} to add, 0 to change, 0 to destroy.\n`);
  bigLogChunks = out;
  return out;
}

/** The rich run's shared `postgres` service logged about 2.5 MiB: the service
 * pane shows its last 2 MiB and offers Load full log and Download. */
let bigServiceLogChunks: string[] | null = null;
/** The large service log's chunks, built on first use: the start-up lines,
 * 30,000 statement lines, 500 to a chunk, then the ready lines again. */
function bigServiceLog(): string[] {
  if (bigServiceLogChunks) return bigServiceLogChunks;
  const out: string[] = [serviceLogLines.join("\n") + "\n"];
  const total = 30_000;
  let chunk = "";
  for (let i = 0; i < total; i++) {
    chunk +=
      `LOG:  duration: ${(i % 97) + 0.125} ms  statement: ` +
      `SELECT id, status FROM integration_runs WHERE shard = ${i % 64}\n`;
    if ((i + 1) % 500 === 0) {
      out.push(chunk);
      chunk = "";
    }
  }
  out.push(chunk + "LOG:  checkpoint complete: wrote 412 buffers\n");
  bigServiceLogChunks = out;
  return out;
}

/** The stored parts of the log a step or service stream reads, or null when
 * it has none (a Pod that never started printed nothing). */
function mockLogChunks(url: string): MockLogPart[] | null {
  let u: URL;
  try {
    u = new URL(url, window.location.origin);
  } catch {
    return null;
  }
  const path = u.pathname.replace(/\/raw$/, "");
  const step = path.match(/^\/v1\/runs\/([^/]+)\/steps\/([^/]+)\/logs$/);
  if (step) {
    if (unscheduledTry(url)) return null;
    if (step[1] === MATRIX_RUN_ID && mockStepId(step[2]!) === BIG_LOG_STEP) return bigLog();
    if (step[1] === RICH_RUN_ID && mockStepId(step[2]!) === "test" && !u.searchParams.has("sidecar")) {
      return restartedTestLog();
    }
    return [testLogLines.join("\n") + "\n"];
  }
  const service = path.match(/^\/v1\/runs\/([^/]+)\/services\/([^/]+)\/logs$/);
  if (service) {
    if (service[1] === RICH_RUN_ID && decodeURIComponent(service[2]!) === "postgres") return bigServiceLog();
    return [serviceLogLines.join("\n") + "\n"];
  }
  return null;
}

/** The events a log stream sends for `url`: where `?from=` or `?tail=` says
 * to start, a `truncated` event when the tail skipped chunks, then one `data`
 * event per chunk, or a `marker` event for a note. */
function mockLogEvents(url: string): { type: string; data: string; id?: string }[] {
  const chunks = mockLogChunks(url);
  if (!chunks) return [];
  const u = new URL(url, window.location.origin);
  const attempt = u.searchParams.get("attempt") ?? "a1";
  const from = u.searchParams.get("from");
  const tail = u.searchParams.get("tail");
  const out: { type: string; data: string; id?: string }[] = [];
  let start = 0;
  if (from) {
    start = Number(from.split(":")[1] ?? 0) || 0;
  } else if (tail) {
    const sizes = chunks.map((c) => (typeof c === "string" ? new TextEncoder().encode(c).length : 0));
    const total = sizes.reduce((a, b) => a + b, 0);
    // Start at the chunk that holds the `tail`-th byte from the end.
    let rest = total;
    while (start < chunks.length - 1 && rest - sizes[start]! >= Number(tail)) rest -= sizes[start++]!;
    if (start > 0) {
      out.push({ type: "truncated", data: JSON.stringify({ skipped_bytes: total - rest, total_bytes: total }) });
    }
  }
  for (let i = start; i < chunks.length; i++) {
    const c = chunks[i]!;
    const id = `${attempt}:${i + 1}`;
    out.push(
      typeof c === "string" ? { type: "message", data: c, id } : { type: "marker", data: JSON.stringify(c.marker), id },
    );
  }
  return out;
}

// ── environments / secrets ───────────────────────────────────────────────────
// A repository's Environments are org Environments: see `orgEnvironments`,
// `overlays` and `projectEnvironments` below.

// ── the GATES run ──────────────────────────────────────────────
//   bump        ✓ → live        external gate WAITING
//   bump-canary ✓ → live-canary external gate PASSED (with a reason)
//   bump-eu     ✓ → live-eu     external gate FAILED (released with passed:false)
//   smoke       needs live (not reached)
const externalGate = (id: string, needs: string, status: string, a1: number, outcome: string) => ({
  id,
  status,
  attempts: 1,
  needs: [needs],
  gate: "external",
  attempt_list: [
    {
      id: "a1",
      started_at: ago(a1),
      // A gate that still waits has not finished; a resolved one closed a minute in.
      finished_at: outcome === "running" ? null : ago(a1) + 60_000,
      failed: outcome === "failed",
      ...(outcome === "failed"
        ? { failure: "gate", failure_detail: "ImagePullBackOff: sha-a1b2c3d not found in registry" }
        : {}),
      outcome,
    },
  ],
});
const gatesBump = (id: string, at: number) => ({
  id,
  // The step that changes what is live — its pipeline marks it `deploy: true`.
  deploy: true,
  status: "succeeded",
  attempts: 1,
  needs: [],
  attempt_list: [{ id: "a1", started_at: ago(at), finished_at: ago(at) + 50_000, failed: false, outcome: "succeeded" }],
});
const gatesRunStatus: RunStatusDto = {
  id: GATES_RUN_ID,
  status: "suspended",
  run_number: 58,
  pipeline: "deploy",
  trigger_title: "Deploy sha-a1b2c3d to production",
  origin_pr_base: null,
  params: {},
  snapshot_retention: { retention_days: 14, expired: false, pinned: false },
  steps: [
    gatesBump("bump", 5 * min),
    externalGate("live", "bump", "pending", 4 * min, "running"),
    gatesBump("bump-canary", 5 * min),
    {
      ...externalGate("live-canary", "bump-canary", "succeeded", 4 * min, "succeeded"),
      gate_resolution: {
        passed: true,
        reason: "sha-a1b2c3d live on edge-canary: Healthy · Synced",
        at: ago(3 * min),
        attempt: "a1",
      },
    },
    gatesBump("bump-eu", 5 * min),
    {
      ...externalGate("live-eu", "bump-eu", "failed", 4 * min, "failed"),
      gate_resolution: {
        passed: false,
        reason: "ImagePullBackOff: sha-a1b2c3d not found in registry",
        at: ago(2 * min),
        attempt: "a1",
      },
    },
    { id: "smoke", status: "pending", attempts: 0, needs: ["live"], attempt_list: [] },
  ],
};
const gev = sse(GATES_RUN_ID);
const gatesEventsBody = [
  gev(ago(5 * min + 5000), {
    Raw: {
      trigger: {
        event: {
          kind: "manual",
          actor: "a.kim",
          branch: "main",
          ref: "refs/heads/main",
          sha: "a1b2c3d4e5f60718293a4b5c6d7e8f90",
          repo: { owner: "acme", name: "edge-gateway" },
        },
      },
    },
  }),
  ...["bump", "bump-canary", "bump-eu"].flatMap((s) => [
    gev(ago(5 * min), { AttemptStarted: { step: s, attempt: "a1" } }),
    gev(ago(4 * min + 10000), { AttemptFinished: { step: s, attempt: "a1" } }),
    gev(ago(4 * min + 9000), { StepTransitioned: { step: s, from: "Running", to: "Succeeded" } }),
  ]),
  ...["live", "live-canary", "live-eu"].map((s) =>
    gev(ago(4 * min), { AttemptStarted: { step: s, attempt: "a1" } }),
  ),
  gev(ago(3 * min), { AttemptFinished: { step: "live-canary", attempt: "a1" } }),
  gev(ago(3 * min), {
    GateResolved: {
      step: "live-canary",
      attempt: "a1",
      passed: true,
      reason: "sha-a1b2c3d live on edge-canary: Healthy · Synced",
    },
  }),
  gev(ago(2 * min), { AttemptFinished: { step: "live-eu", attempt: "a1", failure: "gate" } }),
  gev(ago(2 * min), {
    GateResolved: {
      step: "live-eu",
      attempt: "a1",
      passed: false,
      reason: "ImagePullBackOff: sha-a1b2c3d not found in registry",
    },
  }),
].join("");

// ── the ENVIRONMENT GATE run ────────────────────────────────────────────────
//   build ✓ → qa (plain manual gate, waiting) → smoke
//         ✓ → environment-approval (added by production, waiting) → ship (deploy)
// Approving `qa` releases it. Approving `environment-approval` records the
// approval and answers that the wait timer releases it in five minutes.
const envGate = (id: string, approval: boolean, extra: Record<string, unknown> = {}) => ({
  id,
  status: "pending",
  attempts: 1,
  needs: ["build"],
  gate: "manual",
  ...(approval ? { environment_approval_gate: true } : {}),
  attempt_list: [{ id: "a1", started_at: ago(3 * min), finished_at: null, failed: false, outcome: "running" }],
  ...extra,
});
const envGateRunStatus: RunStatusDto = {
  id: ENV_GATE_RUN_ID,
  status: "suspended",
  run_number: 152,
  pipeline: "deploy",
  trigger_title: "Deploy 7c1e2d4 to production",
  origin_pr_base: null,
  params: {},
  snapshot_retention: { retention_days: 14, expired: false, pinned: false },
  environment: { name: "production", display_name: "Production" },
  steps: [
    {
      id: "build",
      status: "succeeded",
      attempts: 1,
      needs: [],
      attempt_list: [
        { id: "a1", started_at: ago(5 * min), finished_at: ago(3 * min + 10000), failed: false, outcome: "succeeded" },
      ],
    },
    envGate("qa", false),
    { id: "smoke", status: "pending", attempts: 0, needs: ["qa"], attempt_list: [] },
    envGate("environment-approval", true, {
      origin: [{ kind: "environment", name: "production", content_hash: "" }],
    }),
    {
      id: "ship",
      status: "pending",
      attempts: 0,
      deploy: true,
      needs: ["build", "environment-approval"],
      attempt_list: [],
    },
  ],
};
const egv = sse(ENV_GATE_RUN_ID);
const envGateEventsBody = [
  egv(ago(5 * min + 5000), {
    Raw: {
      trigger: {
        event: {
          kind: "manual",
          actor: "a.kim",
          branch: "main",
          ref: "refs/heads/main",
          sha: "7c1e2d4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d",
          repo: { owner: "acme", name: "scarab" },
        },
      },
    },
  }),
  egv(ago(5 * min), { AttemptStarted: { step: "build", attempt: "a1" } }),
  egv(ago(3 * min + 10000), { AttemptFinished: { step: "build", attempt: "a1" } }),
  egv(ago(3 * min + 9000), { StepTransitioned: { step: "build", from: "Running", to: "Succeeded" } }),
  ...["qa", "environment-approval"].map((g) =>
    egv(ago(3 * min), { AttemptStarted: { step: g, attempt: "a1" } }),
  ),
].join("");

// ── Org-level Environments — Settings › Environments. production
// has a baseline and a display name; qa still has the permissive one and shows
// its name. Writes echo back and persist nothing, except a duplicate, which is
// kept for the page load.
const orgEnvironments: OrgEnvironmentDto[] = [
  {
    org: "acme",
    name: "production",
    display_name: "Production",
    description: "customer-facing, eu-west-1 + us-east-1",
    protection: {
      approvers: ["ops-lead"],
      allowed_refs: ["refs/heads/main"],
      wait_timer_s: 0,
      require_reason: false,
      // The gate it adds to a pipeline that wrote none fails after a day.
      approval_gate: {
        missing_gate_mode: "add",
        added_gate_expiry_s: 86400,
        max_approval_age_s: 86400,
      },
    },
    eligibility: { all_projects: true, projects: [] },
    created_at: ago(40 * 24 * 60 * min),
    updated_at: ago(3 * 24 * 60 * min),
    updated_by: "a.kim",
  },
  {
    org: "acme",
    name: "qa",
    display_name: null,
    description: "",
    // Still the permissive baseline the migration gave it — the report flags it.
    protection: {
      approvers: [],
      allowed_refs: [],
      wait_timer_s: 0,
      require_reason: false,
    },
    eligibility: { all_projects: false, projects: ["scarab", "web-*"] },
    created_at: ago(2 * 60 * min),
    updated_at: ago(2 * 60 * min),
    updated_by: "j.lee",
  },
  {
    // A wait timer saved before the timer needed approvers: it does nothing,
    // and its pages say so.
    org: "acme",
    name: "staging",
    display_name: "Staging",
    description: "pre-release checks, eu-west-1",
    protection: {
      approvers: [],
      allowed_refs: [],
      wait_timer_s: 600,
      require_reason: false,
    },
    eligibility: { all_projects: false, projects: ["web-*"] },
    created_at: ago(20 * 24 * 60 * min),
    updated_at: ago(20 * 24 * 60 * min),
    updated_by: "j.lee",
  },
];

// Per-project overlays and grants, keyed environment →
// project. scarab tightens production (an extra approver, a reason, a wait);
// it holds a grant for one privileged image there. It also keeps an overlay
// on staging, which is no longer eligible for it (staging admits web-* only),
// so its Environments tab shows all three cases beside qa, where no rule
// protects a deploy.
const overlays: Record<string, Record<string, OverlayDto>> = {
  production: {
    scarab: {
      approvers: ["j.lee"],
      allowed_refs: [],
      wait_timer_s: 300,
      require_reason: true,
    },
  },
  staging: {
    scarab: {
      approvers: [],
      allowed_refs: ["refs/heads/release/*"],
      wait_timer_s: 0,
      require_reason: false,
    },
  },
};
const grants: Record<string, Record<string, GrantDto>> = {
  production: {
    scarab: {
      privileged_images: [
        {
          image_digest: "sha256:4b1c0f9e2d7a8c3e5f6a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5a6",
          privileged: true,
          capabilities: [],
        },
      ],
      permit_k8s_overlay: false,
    },
  },
};

/** A copy of the server's merge, so the fixtures' `effective` blocks are
 *  what the server would answer: approvers union, ref layers ANDed, the
 *  stricter wait, reason if either sets it. `mock.test.ts` checks it against
 *  the merge cases the server's own test reads
 *  (crates/scarab-server/tests/fixtures/effective_protection_cases.json). */
export function effectiveOf(
  b: RulesDto,
  overlay: OverlayDto | undefined,
): ProjectEnvironmentDto["effective"] {
  return {
    approvers: [...new Set([...b.approvers, ...(overlay?.approvers ?? [])])],
    allowed_ref_layers: overlay ? [b.allowed_refs, overlay.allowed_refs] : [b.allowed_refs],
    wait_timer_s: Math.max(b.wait_timer_s, overlay?.wait_timer_s ?? 0),
    require_reason: b.require_reason || (overlay?.require_reason ?? false),
    approval_gate: b.approval_gate ?? { missing_gate_mode: "add" as const, max_approval_age_s: 86400 },
  };
}

const eligible = (env: OrgEnvironmentDto, project: string) =>
  env.eligibility.all_projects ||
  (env.eligibility.projects ?? []).some((p) =>
    new RegExp(`^${p.split("*").map((x) => x.replace(/[.+?^${}()|[\]\\]/g, "\\$&")).join(".*")}$`, "i").test(project),
  );

/** `GET /v1/repos/{org}/{repo}/environments`: the org Environments the
 *  project may deploy into or holds an overlay/grant in, by name. */
function projectEnvironments(project: string): ProjectEnvironmentDto[] {
  return orgEnvironments
    .map((env) => {
      const overlay = overlays[env.name]?.[project];
      return {
        name: env.name,
        display_name: env.display_name ?? null,
        eligible: eligible(env, project),
        baseline: env.protection,
        overlay: overlay ?? null,
        effective: effectiveOf(env.protection, overlay),
        privilege_grant: grants[env.name]?.[project] ?? null,
      };
    })
    .filter((e) => e.eligible || e.overlay || e.privilege_grant)
    .sort((a, b) => a.name.localeCompare(b.name));
}

/** Secrets in each Environment's per-project scopes, by project: what a
 *  delete of the Environment would take with it. Matches `secretMatrix`. */
const environmentSecrets: Record<string, Record<string, string[]>> = {
  production: { scarab: ["DATABASE_URL", "SLACK_WEBHOOK_URL"] },
  qa: { scarab: ["DATABASE_URL"] },
};

/** The 409 a plain `DELETE …/environments/{env}` answers while projects hold
 *  overlays, grants or secrets in it; `null` when nothing does. */
function deleteBlockers(env: string) {
  const overlayProjects = Object.keys(overlays[env] ?? {}).sort();
  const grantProjects = Object.keys(grants[env] ?? {}).sort();
  const secrets = Object.entries(environmentSecrets[env] ?? {})
    .filter(([, keys]) => keys.length > 0)
    .map(([project, keys]) => ({ project, keys }));
  const secretCount = secrets.reduce((n, s) => n + s.keys.length, 0);
  if (overlayProjects.length + grantProjects.length + secretCount === 0) return null;
  return {
    code: "environment_in_use",
    message: `environment \`${env}\` still has project overlays, privilege grants or secrets`,
    overlay_count: overlayProjects.length,
    privilege_grant_count: grantProjects.length,
    secret_count: secretCount,
    overlays: overlayProjects,
    privilege_grants: grantProjects,
    secrets,
  };
}

/** `GET …/environments/{env}/projects`: who holds an overlay or grant. */
function environmentProjects(env: OrgEnvironmentDto): EnvironmentProjectDto[] {
  const names = new Set([
    ...Object.keys(overlays[env.name] ?? {}),
    ...Object.keys(grants[env.name] ?? {}),
  ]);
  return [...names].sort().map((project) => ({
    project,
    eligible: eligible(env, project),
    overlay: overlays[env.name]?.[project] ?? null,
    privilege_grant: grants[env.name]?.[project] ?? null,
  }));
}

// ── Plugins — Settings › Plugins. `argocd` is running; `argocd-us` is
// killed; `argocd-staging` is held by a failed isolation probe. Each has a
// manage page at /orgs/acme/settings/plugins/<id>/manage. Writes echo a
// record and persist nothing, except an install and an upgrade, kept until
// the page reloads. `argocd-us` was installed before sources were recorded.
// An upgrade-fetch of `argocd-staging` reads a wider manifest (a read rule
// and a gate added); of any other installation, a narrower one (the version
// rule removed). Either needs a passkey with an image other than the
// installed one; only the narrower one on the installed image needs none.
const ARGOCD_REPO = "ghcr.io/thulasi-ram/scarab-plugin-argocd";
const argocdManifest: PluginManifestDto = {
  plugin: "argocd",
  image: ARGOCD_REPO,
  capabilities: ["use_connection", "resolve_gates"],
  connections: [
    {
      name: "argocd",
      auth_schemes: ["bearer"],
      read_rules: [
        { method: "GET", path: "/api/version", query: [] },
        { method: "GET", path: "/api/v1/applications", query: ["projects", "selector", "appNamespace"] },
        { method: "GET", path: "/api/v1/applications/*", query: ["appNamespace", "project"] },
        { method: "GET", path: "/api/v1/applications/*/managed-resources", query: ["appNamespace", "project"] },
        {
          method: "GET",
          path: "/api/v1/stream/applications",
          query: ["resourceVersion", "appNamespace", "projects", "name", "selector"],
        },
      ],
    },
  ],
  config_keys: ["ARGOCD_UI_URL", "OWNER_ANNOTATION"],
  gates: [
    {
      name: "wait-live",
      description:
        "Pass when Argo CD reports the Application synced, healthy and serving the given image; fail when Argo CD has given up on its sync.",
      inputs: [
        { name: "app", type: "string", required: true },
        { name: "image", type: "string", required: true },
      ],
      outputs: [{ name: "revision", type: "string", required: true }],
    },
  ],
};
const ARGOCD_MANIFEST_SHA = "b7e3".padEnd(64, "9");
const ARGOCD_SOURCE_PATH = "github.com/acme/scarab-plugin-argocd//manifest.json";
// What an upgrade-fetch of `name` reads (see above), with its diff against
// the installed manifest.
const ruleText = (r: { method: string; path: string; query: string[] }) =>
  `argocd: ${r.method} ${r.path}${r.query.length ? ` ?${r.query.join("&")}` : ""}`;
const fetchedPluginUpgrade = (
  installed: PluginInstallationDto,
  source: string,
  connection: string | null,
  image: string | null,
): FetchedPluginUpgradeDto => {
  const wider = installed.name === "argocd-staging";
  const rules = argocdManifest.connections[0]!.read_rules;
  const added = wider ? [{ method: "GET", path: "/api/v1/account", query: [] }] : [];
  const kept = wider ? rules : rules.filter((r) => r.path !== "/api/version");
  const manifest: PluginManifestDto = {
    ...argocdManifest,
    connections: [{ ...argocdManifest.connections[0]!, read_rules: [...kept, ...added] }],
    gates: wider
      ? [...argocdManifest.gates, { ...argocdManifest.gates[0]!, name: "wait-synced", description: "Pass when the Application is synced." }]
      : argocdManifest.gates,
  };
  const group = (a: string[], r: string[], u: string[]) => ({ added: a, removed: r, unchanged: u });
  return {
    source,
    connection,
    content: JSON.stringify(manifest, null, 2) + "\n",
    manifest,
    manifest_sha256: (wider ? "c4d1" : "a9f0").padEnd(64, "7"),
    current_manifest_sha256: installed.manifest_sha256,
    current_image: installed.image,
    diff: {
      capabilities: group([], [], [...argocdManifest.capabilities]),
      connections: group([], [], ["argocd"]),
      auth_schemes: group([], [], ["argocd: bearer"]),
      read_rules: group(
        added.map(ruleText),
        wider ? [] : rules.filter((r) => r.path === "/api/version").map(ruleText),
        kept.map(ruleText),
      ),
      config_keys: group([], [], [...argocdManifest.config_keys]),
      gates: group(wider ? ["wait-synced"] : [], [], ["wait-live"]),
      image: group([], [], [ARGOCD_REPO]),
    },
    ...(() => {
      const reasons = [
        ...(wider ? ["adds read rule `argocd: GET /api/v1/account`", "adds gate `wait-synced`"] : []),
        ...(image && image !== installed.image
          ? [`runs new code: image \`${image}\` instead of \`${installed.image}\``]
          : []),
      ];
      return { step_up_reasons: reasons, needs_step_up: reasons.length > 0 };
    })(),
  };
};
// What `POST …/plugin-fetch` returns for any source: the Argo CD manifest,
// and the org's two installations of it (one approved with an older
// manifest).
const fetchedArgocdManifest = (source: string): FetchedPluginManifestDto => ({
  source: source || "github.com/acme/scarab-plugin-argocd//plugin.json@6e1d0c9b8a7f6e5d4c3b2a1f0e9d8c7b6a5f4e3d",
  content: JSON.stringify(argocdManifest, null, 2) + "\n",
  manifest: argocdManifest,
  manifest_sha256: ARGOCD_MANIFEST_SHA,
  installed: [
    { name: "argocd", image: `${ARGOCD_REPO}@sha256:${"4f2a1c9e7b3d".padEnd(64, "0")}`, manifest_sha256: ARGOCD_MANIFEST_SHA },
    { name: "argocd-us", image: `${ARGOCD_REPO}@sha256:${"d81e6b02c4aa".padEnd(64, "0")}`, manifest_sha256: "91c0".padEnd(64, "2") },
  ],
});
const pluginInstallation = (
  name: string,
  digest: string,
  o: Partial<PluginInstallationDto>,
): PluginInstallationDto => ({
  id: name === "argocd" ? "4f2a1c9e7b" : "9c0d33a1e5",
  org: "acme",
  name,
  plugin: "argocd",
  principal: `plugin:${name}`,
  image: `${ARGOCD_REPO}@sha256:${digest.padEnd(64, "0")}`,
  manifest_sha256: ARGOCD_MANIFEST_SHA,
  source: `${ARGOCD_SOURCE_PATH}@6e1d0c9b8a7f6e5d4c3b2a1f0e9d8c7b6a5f4e3d`,
  source_connection: null,
  config: { ARGOCD_UI_URL: "https://argocd.acme.example" },
  state: "enabled",
  runtime_status: "running",
  runtime_message: "",
  probe_passed_at: ago(6 * 60 * min),
  approved_by: "a.kim",
  approved_at: ago(12 * 24 * 60 * min),
  killed_at: null,
  killed_by: null,
  manifest: argocdManifest,
  storage: null,
  ...o,
});
const plugins: PluginInstallationDto[] = [
  pluginInstallation("argocd", "4f2a1c9e7b3d", {
    runtime_message: "watching 6 applications",
    storage: {
      claim: "argocd-state",
      mount_path: "/data",
      owner_annotation: "scarab.io/plugin-installation",
      owner_annotation_value: "acme/argocd",
    },
  }),
  pluginInstallation("argocd-us", "d81e6b02c4aa", {
    state: "killed",
    runtime_status: "stopped",
    runtime_message: "scaled to zero by the kill switch",
    killed_by: "j.lee",
    killed_at: ago(90 * min),
    approved_at: ago(5 * 24 * 60 * min),
    source: null,
  }),
  pluginInstallation("argocd-staging", "7b1c40e9d2f6", {
    id: "2b7e90f1c4",
    runtime_status: "probe_failed",
    runtime_message: "the isolation probe reached 10.96.0.1:443; this cluster does not enforce the plugin's NetworkPolicy",
    probe_passed_at: null,
    approved_by: "j.lee",
    approved_at: ago(2 * 60 * min),
    config: {},
  }),
];
// Connections: `argocd` is bound over https; `argocd-us` was
// bound to an in-cluster Service over plain http (the flagged case). The
// credential itself is never part of any response.
const pluginConnection = (
  slot: string,
  o: Partial<PluginConnectionDto>,
): PluginConnectionDto => ({
  slot,
  auth_schemes: ["bearer"],
  read_rules: argocdManifest.connections[0]!.read_rules,
  configured: true,
  base_url: "https://argocd.acme.example",
  auth_scheme: "bearer",
  credential_set: true,
  ca_pem: null,
  insecure_http: false,
  updated_by: "a.kim",
  updated_at: ago(12 * 24 * 60 * min),
  ...o,
});
const pluginConnections: Record<string, PluginConnectionDto[]> = {
  argocd: [pluginConnection("argocd", {})],
  "argocd-us": [
    pluginConnection("argocd", {
      base_url: "http://argocd-server.argocd.svc:80",
      insecure_http: true,
      updated_by: "j.lee",
    }),
  ],
};
// Broker journal: `argocd`'s last few hours, newest first —
// application reads (a `head` and an `end` row each), a watch stream cut by a
// re-bind, and refusals: a query key no rule lists and a path no rule admits.
// Enough rows for a second page. `argocd-us` has recorded nothing.
const brokerJournal: Record<string, BrokerJournalEntryDto[]> = (() => {
  const rows: BrokerJournalEntryDto[] = [];
  let id = 4100;
  const apps = ["checkout", "payments", "search", "ledger"];
  // Oldest first; reversed at the end.
  for (let i = 0; i < 70; i++) {
    const started = ago((6 * 60 - i * 5) * min);
    const req = `5c1e${(1000 + i).toString(16)}-7a2b-4c1d-9e3f-0b4a2d6c8e1f`;
    const base = { request_id: req, slot: "argocd", method: "GET", started_at: started };
    if (i % 17 === 5) {
      rows.push({
        ...base,
        id: id++,
        phase: "refused",
        path: "/api/v1/applications",
        upstream_path: null,
        query_keys: ["projects", "refresh"],
        rule: null,
        status: 403,
        bytes: null,
        duration_ms: 0,
        denied_reason: "query",
      });
      continue;
    }
    if (i % 23 === 11) {
      rows.push({
        ...base,
        id: id++,
        phase: "refused",
        path: "/api/v1/session",
        upstream_path: null,
        query_keys: [],
        rule: null,
        status: 403,
        bytes: null,
        duration_ms: 0,
        denied_reason: "no read rule",
      });
      continue;
    }
    const stream = i === 40;
    const app = apps[i % apps.length]!;
    const path = stream ? "/api/v1/stream/applications" : `/api/v1/applications/${app}`;
    const rule = stream ? "GET /api/v1/stream/applications" : "GET /api/v1/applications/*";
    const query_keys = stream ? ["projects", "resourceVersion"] : ["appNamespace"];
    const forwarded = { ...base, path, upstream_path: path, query_keys, rule, status: 200 };
    rows.push({ ...forwarded, id: id++, phase: "head", bytes: null, duration_ms: 40 + (i % 9) * 7, denied_reason: null });
    rows.push({
      ...forwarded,
      id: id++,
      phase: "end",
      bytes: stream ? 186_412 : 3_100 + (i % 13) * 517,
      duration_ms: stream ? 47 * min : 55 + (i % 9) * 9,
      denied_reason: stream ? "cut: the connection was re-bound" : null,
    });
  }
  return { argocd: rows.reverse(), "argocd-us": [] };
})();
const secretNames = { names: ["DATABASE_URL", "SENTRY_DSN"] };
// Org scope (no `repo=`) holds the org-wide base of the inheritance chain — the
// Settings page's Org Secrets section.
const orgSecretNames = { names: ["GHCR_TOKEN", "SENTRY_DSN"] };

// One healthy GitHub connection covering the acme repos.
// Each connection serves one org (git-bug 19b0a45); `org: null` is unassigned
// and listed to global admins only.
const connections: components["schemas"]["ConnectionDto"][] = [
  {
    id: "gh-acme",
    org: "acme",
    kind: "github",
    base_url: "https://api.github.com",
    web_url: "https://github.com",
    credential_ref: "github-app",
    credential_present: true,
    last_delivery_at: ago(240000),
    // Counted from the org page's list when it is served (`withCounts`).
    enabled_repos: 0,
    supports_resync: true,
    supports_preflight: true,
    managed_by_config: false,
  },
  // A self-hosted Forgejo connection: the kind added here by an admin.
  {
    id: "forgejo-7f3a91c2",
    org: "acme",
    kind: "forgejo",
    base_url: "https://git.acme.dev",
    web_url: "https://git.acme.dev",
    credential_ref: "forgejo-7f3a91c2-credential",
    credential_present: true,
    last_delivery_at: ago(3_600_000),
    enabled_repos: 0,
    supports_resync: true,
    // Forgejo tokens are not introspectable, so the preflight line is not
    // offered for them at all (the endpoint would only answer "unknown").
    supports_preflight: false,
    managed_by_config: false,
  },
  // Two installations that serve no org yet. 90211 is the App installed on
  // the `acme` account again, under an installation whose account id no acme
  // connection records, so the delivery could not prove it is acme's account.
  // 90377 was registered before connections served orgs (a legacy
  // connection), for an account no org has the name of; its delivery a day
  // ago recorded the login. Each lists its account login and id.
  {
    id: "github-install-90211",
    org: null,
    kind: "github",
    base_url: "https://api.github.com",
    web_url: "https://github.com",
    credential_ref: "github-app",
    credential_present: true,
    last_delivery_at: null,
    enabled_repos: 0,
    supports_resync: true,
    supports_preflight: false,
    managed_by_config: false,
    installation_id: 90211,
    account_id: 1029384,
    account_login: "acme",
  },
  {
    id: "github-install-90377",
    org: null,
    kind: "github",
    base_url: "https://api.github.com",
    web_url: "https://github.com",
    credential_ref: "github-app",
    credential_present: true,
    last_delivery_at: ago(86_400_000),
    enabled_repos: 0,
    supports_resync: true,
    supports_preflight: false,
    managed_by_config: false,
    installation_id: 90377,
    account_id: 5530911,
    account_login: "umbrella",
  },
];

/**
 * Why the server refuses to assign `conn` to `org`, as it words it
 * (`admin_assign_refusal`): an org served by a connection to another forge or
 * host, or an org that exists and no connection serves. Bound repositories of
 * another owner are not modelled: no waiting connection here binds any.
 */
function assignRefusal(conn: components["schemas"]["ConnectionDto"], org: string): string | null {
  const servers = connections.filter((c) => c.id !== conn.id && c.org === org);
  const other = servers.find((c) => c.kind !== conn.kind || c.base_url !== conn.base_url);
  if (other) {
    return `org \`${org}\` has connection \`${other.id}\` to ${other.base_url}, another forge or host than ${conn.base_url}; an account of the same name there is not known to be the same account`;
  }
  if (servers.length === 0 && me.admin_orgs.includes(org)) {
    return `org \`${org}\` exists but no connection serves it, so nothing shows that the account \`${org}\` on ${conn.base_url} is the same account`;
  }
  return null;
}

// A correctly-configured App (git-bug 90644c6). The interesting fixture is the
// degraded one, but the demo stack shows a healthy install — the gap list is
// exercised by the server tests, not by screenshots.
const connectionPreflight = {
  id: "gh-acme",
  kind: "github",
  status: "ok",
  checked: true,
  unavailable_reason: null,
  required: [],
  missing: [],
  granted_permissions: [
    { name: "contents", level: "read" },
    { name: "metadata", level: "read" },
    { name: "statuses", level: "write" },
  ],
  subscribed_events: ["push", "pull_request"],
};

// ── /v1/orgs/acme/repos: the org page's repository list ─────────────────
// Every acme repository the two connections reach: the projects above are
// enabled (and infra, through Forgejo), the rest are not. 33 of them, so the
// list runs to two pages of 25. Enable and Disable flip `enabled` here and
// add or drop the project on /v1/repos, as the server would.
type OrgRepoDto = components["schemas"]["OrgRepoDto"];
const orgRepo = (
  name: string,
  connection: "gh-acme" | "forgejo-7f3a91c2",
  enabled: boolean,
  unreachable_since: number | null = null,
): OrgRepoDto => ({
  owner: "acme",
  name,
  enabled,
  project: enabled ? name : null,
  unreachable_since,
  connection_id: connection,
  kind: connection === "gh-acme" ? "github" : "forgejo",
  repo_url:
    connection === "gh-acme" ? `https://github.com/acme/${name}` : `https://git.acme.dev/acme/${name}`,
  can_change: true,
});
const orgRepos: OrgRepoDto[] = [
  ...projects.map((p) => orgRepo(p.name, "gh-acme", true, p.unreachable_since)),
  orgRepo("infra", "forgejo-7f3a91c2", true),
  orgRepo("terraform-modules", "forgejo-7f3a91c2", false),
  orgRepo("runbooks", "forgejo-7f3a91c2", false),
  ...[
    "admin-portal",
    "analytics-etl",
    "auth-service",
    "catalog",
    "checkout-web",
    "cli",
    "design-system",
    "docs-site",
    "email-templates",
    "feature-flags",
    "fraud-rules",
    "helm-charts",
    "load-tests",
    "mobile-app",
    "notifications",
    "pricing-engine",
    "recommendations",
    "search-indexer",
    "shipping-rates",
    "status-page",
    "support-bot",
    "web",
    "api",
  ].map((name) => orgRepo(name, "gh-acme", false)),
].sort((a, b) => a.name.localeCompare(b.name));

function orgReposPage(org: string, search: string): Response {
  const q = new URLSearchParams(search);
  const needle = (q.get("q") ?? "").trim().toLowerCase();
  const connection = q.get("connection");
  const page = Math.max(1, Number(q.get("page") ?? 1) || 1);
  const perPage = Math.min(100, Math.max(1, Number(q.get("per_page") ?? 25) || 25));
  const all = org === "acme" ? orgRepos : [];
  const matching = all.filter(
    (r) =>
      (!connection || r.connection_id === connection) &&
      (!needle || r.name.toLowerCase().includes(needle)),
  );
  const admin = me.admin_orgs.includes(org);
  return json({
    repos: paginate(matching, page, perPage).map((r) => ({ ...r, can_change: admin })),
    total: matching.length,
    enabled_total: all.filter((r) => r.enabled).length,
    page,
    per_page: perPage,
    can_administer: admin,
    unlisted: [],
  });
}

/** Each connection with its count of enabled repositories, from the org
 *  page's list, so Enable and Disable move the counts. */
const withCounts = (cs: typeof connections) =>
  cs.map((c) => ({
    ...c,
    enabled_repos: orgRepos.filter((r) => r.enabled && r.connection_id === c.id).length,
  }));

/** Enable or disable `owner/name`: the org page's list, /v1/repos and the
 *  connections' counts follow. */
function setRepoEnabled(owner: string, name: string, enabled: boolean): OrgRepoDto | undefined {
  const r = orgRepos.find((x) => x.owner === owner && x.name === name);
  if (!r) return undefined;
  r.enabled = enabled;
  r.project = enabled ? name : null;
  r.unreachable_since = null;
  const i = projects.findIndex((p) => p.owner === owner && p.name === name);
  if (enabled && i < 0) {
    projects.push({ ...proj(name, null), repo_url: r.repo_url, connection_id: r.connection_id });
  }
  if (!enabled && i >= 0) projects.splice(i, 1);
  return r;
}

// The secret matrix behind a repo's Secrets key list carries the
// repo-default column ("") and inheritance origins: GHCR_TOKEN comes from the
// org (the `org` tag), SENTRY_DSN is a repo key that overrides the org's and is
// inherited in both environments (so its expanded lines fold them away),
// DATABASE_URL is a repo default overridden in both environments, and
// SLACK_WEBHOOK_URL is production-only with qa unset on purpose. Its
// environment columns are the org Environments the project may deploy into:
// production and qa, as `projectEnvironments` lists them.
// ── /v1/repos/acme/scarab/schedules: three `on: cron:` schedules ───────────
// The nightly one's last slot started a Run (the scarab run fixture); the
// weekly audit's last slot was missed, so it is also a refused row in the runs
// list, as the server records it; the docs one was added since its last slot,
// so it has not fired yet.
const DAY = 24 * 60 * min;
/** The next UTC slot at `hour`:00, on `weekday` (0 = Sunday) when given. */
function nextUtc(hour: number, weekday?: number): number {
  const d = new Date(now);
  d.setUTCHours(hour, 0, 0, 0);
  while (d.getTime() <= now || (weekday != null && d.getUTCDay() !== weekday)) {
    d.setTime(d.getTime() + DAY);
  }
  return d.getTime();
}
const SCHEDULE_SHA = "c4e1a9b27d3f8e6a5b4c3d2e1f0a9b8c7d6e5f4a";
const schedules: ScheduleDto[] = [
  {
    pipeline: ".scarab/nightly.yaml",
    schedule: "0 2 * * *",
    next_fire_at: nextUtc(2),
    source_sha: SCHEDULE_SHA,
    last_fire: { schedule: "0 2 * * *", slot_at: nextUtc(2) - DAY, outcome: "run", run_id: RUN_ID },
  },
  {
    pipeline: ".scarab/audit.yaml",
    schedule: "0 6 * * 1",
    next_fire_at: nextUtc(6, 1),
    source_sha: SCHEDULE_SHA,
    last_fire: { schedule: "0 6 * * 1", slot_at: nextUtc(6, 1) - 7 * DAY, outcome: "missed" },
  },
  {
    pipeline: ".scarab/docs.yaml",
    schedule: "0 4 * * 0",
    next_fire_at: nextUtc(4, 0),
    source_sha: SCHEDULE_SHA,
    last_fire: null,
  },
];
const missedSlotMessage =
  "the slot was skipped: the forge rate-limited Scarab, so the default branch could not be read in time";
const scheduleRefused: RefusedRunDto[] = [
  {
    id: 3,
    pipeline: ".scarab/audit.yaml",
    trigger_kind: "cron",
    title: "0 6 * * 1",
    reason_kind: "schedule",
    message: missedSlotMessage,
    created_at: nextUtc(6, 1) - 7 * DAY + 2 * min,
    errors: [{ kind: "other", message: missedSlotMessage }],
  },
];

const secretMatrix = {
  columns: ["", "production", "qa"],
  environments: ["production", "qa"],
  keys: [
    {
      key: "GHCR_TOKEN",
      status: { "": "inherited", qa: "inherited", production: "inherited" },
      inherited_from: { "": "org", qa: "org", production: "org" },
      overrides: {},
    },
    {
      key: "SENTRY_DSN",
      status: { "": "set", qa: "inherited", production: "inherited" },
      inherited_from: { qa: "repo", production: "repo" },
      overrides: { "": "org" },
    },
    {
      key: "DATABASE_URL",
      status: { "": "set", qa: "set", production: "set" },
      inherited_from: {},
      overrides: { qa: "repo", production: "repo" },
    },
    {
      key: "SLACK_WEBHOOK_URL",
      status: { "": "unset", qa: "silenced", production: "set" },
      inherited_from: {},
      overrides: {},
    },
    // The org sets AWS_DEPLOY_ROLE for production and qa; scarab sets its own
    // for production.
    {
      key: "AWS_DEPLOY_ROLE",
      status: { "": "unset", qa: "inherited", production: "set" },
      inherited_from: { qa: "org_environment" },
      overrides: { production: "org_environment" },
    },
  ],
};

/**
 * The org's own values (Org settings › Secrets), by key: the scopes it sets
 * (`""` the Org default, else an Environment). Writes and clears from the page
 * change it for the life of the page. `orgSecretMatrix` builds
 * `GET /v1/orgs/{org}/secrets/matrix` from it, with the repositories that set
 * their own in front of each scope.
 */
const orgSecretScopes = new Map<string, Set<string>>([
  ["GHCR_TOKEN", new Set([""])],
  ["SENTRY_DSN", new Set([""])],
  ["AWS_DEPLOY_ROLE", new Set(["production", "qa"])],
]);
// The repositories' own values, by the scope they stand in front of.
const orgSecretReposWithOwn: Record<string, Record<string, string[]>> = {
  SENTRY_DSN: { "": ["scarab"] },
  AWS_DEPLOY_ROLE: { production: ["scarab"] },
};
function orgSecretMatrix() {
  const environments = orgEnvironments.map((e) => e.name).sort();
  const keys = [...orgSecretScopes.entries()]
    .filter(([, scopes]) => scopes.size > 0)
    .map(([key, scopes]) => {
      const own = orgSecretReposWithOwn[key] ?? {};
      const status: Record<string, string> = Object.fromEntries(
        ["", ...environments].map((c) => [
          c,
          scopes.has(c) ? "set" : c !== "" && scopes.has("") ? "inherited" : "unset",
        ]),
      );
      // An Environment that falls to the Org default has each repository
      // default in front of it too, as the server answers.
      const repos_with_own: Record<string, string[]> = {};
      for (const c of ["", ...environments]) {
        const repos = new Set([...(own[c] ?? []), ...(status[c] === "inherited" ? (own[""] ?? []) : [])]);
        if (repos.size > 0) repos_with_own[c] = [...repos].sort();
      }
      return { key, status, repos_with_own };
    });
  return { columns: ["", ...environments], environments, keys };
}

// "Check a leaked value" (`POST /v1/orgs/{org}/secrets/match`). Three stored
// values the mock knows, matched by the server's rules: a value of 20 bytes or
// more anywhere in the paste, a shorter one only as the whole paste. A paste
// holding `MOCK_RATE_LIMIT` is refused as the 21st check in an hour. With
// either leak-check limit at 0 every check is refused as turned off.
const mockMatchValues: { value: string; match: Record<string, unknown> }[] = [
  {
    value: "ghcr_mock_7Hq2Lw9xPzR4tYv8NbK3",
    match: { kind: "secret", key: "GHCR_TOKEN", scope: "org", overridden_in: [], in_use: true },
  },
  {
    // scarab sets its own AWS_DEPLOY_ROLE for production, so no Run gets this one.
    value: "arn:aws:iam::123456789012:role/mock-deploy",
    match: {
      kind: "secret",
      key: "AWS_DEPLOY_ROLE",
      scope: "org_environment",
      environment: "production",
      overridden_in: ["scarab · production"],
      in_use: false,
    },
  },
  {
    value: "forgejo_mock_token_4c1d9e3f0b4a2d6c",
    match: {
      kind: "connection",
      connection: "forge",
      id: "forgejo-7f3a91c2",
      // The server names a forge connection by its account, else its host.
      name: "git.acme.dev",
      base_url: "https://git.acme.dev",
    },
  },
];
function mockSecretMatch(org: string, body: string): Response {
  if (leakCheckLimits.per_hour === 0 || leakCheckLimits.per_day === 0) {
    return json(
      { code: "leak_checks_off", message: `Leak checks are turned off for ${org}.` },
      { status: 403 },
    );
  }
  const text = (JSON.parse(body || "{}") as { text?: string }).text ?? "";
  if (new TextEncoder().encode(text).length > 4096) {
    return json(
      { code: "too_long", message: "the text is longer than 4096 bytes", max_bytes: 4096 },
      { status: 413 },
    );
  }
  const paste = text.replace(/^[\t\n\f\r ]+|[\t\n\f\r ]+$/g, "");
  if (!paste) return json({ code: "empty", message: "There is no text to compare." }, { status: 400 });
  if (paste.includes("MOCK_RATE_LIMIT")) {
    const retry_at = Date.now() + 23 * min;
    return json(
      { code: "rate_limited", message: "the hourly limit on checks is reached", retry_at },
      { status: 429, headers: { "content-type": "application/json", "retry-after": String(23 * 60) } },
    );
  }
  const matches = mockMatchValues
    .filter(({ value }) => (value.length >= 20 ? paste.includes(value) : paste === value))
    .map(({ match }) => match);
  return json({ matches, searched: { secrets: 9, repositories: 2, connections: 3 } });
}

// ── API tokens (git-bug a4cb94f) ────────────────────────────────
// Four records covering every state the panels render differently: a live token
// in daily use, one narrowed to a single project and held by someone else, one
// inside the two-week expiry warning, and a revoked one (a killed credential
// stays visible). Last use and calls order the live ones differently (the
// project token is second by last use, the status poller second by calls), so
// the org list's sort can be seen to work. No plaintext anywhere: the mint 201 below is the only place
// one exists, and it is obviously fake.
const day = 86_400_000;
const apiTokens: components["schemas"]["ApiTokenDto"][] = [
  {
    id: "9f2c1d84-0f2b-4f3a-9a11-2b7c5d0e4a10",
    name: "release-bot",
    owner_subject: pid("a.kim"),
    owner: { id: pid("a.kim"), handle: "a.kim", display_name: null },
    org: "acme",
    project: null,
    role: "member",
    created_by: "a.kim",
    created_at: ago(40 * day),
    expires_at: now + 50 * day,
    last_used_at: ago(4 * min),
    use_count: 18_412,
    revoked_at: null,
    kind: "issued",
  },
  {
    id: "3b7a9c02-6d41-4b8e-8c22-71f0ab993c55",
    name: "orders-api deploy gate",
    owner_subject: pid("j.lee"),
    owner: { id: pid("j.lee"), handle: "j.lee", display_name: null },
    org: "acme",
    project: "orders-api",
    role: "admin",
    created_by: "a.kim",
    created_at: ago(9 * day),
    expires_at: now + 81 * day,
    last_used_at: ago(20 * min),
    use_count: 1_203,
    revoked_at: null,
    kind: "issued",
  },
  {
    id: "c41e7f60-2a88-4f19-b0d3-5e6a7c1b2d34",
    name: "status poller",
    owner_subject: pid("a.kim"),
    owner: { id: pid("a.kim"), handle: "a.kim", display_name: null },
    org: "acme",
    project: null,
    role: "viewer",
    created_by: "a.kim",
    created_at: ago(84 * day),
    expires_at: now + 6 * day,
    last_used_at: ago(52 * min),
    use_count: 9_870,
    revoked_at: null,
    kind: "issued",
  },
  {
    id: "7d5b0e13-9c44-4a27-8e61-0f3d2c8a55b7",
    name: "amy's laptop",
    owner_subject: pid("a.kim"),
    owner: { id: pid("a.kim"), handle: "a.kim", display_name: null },
    org: "acme",
    project: null,
    role: "member",
    created_by: "a.kim",
    created_at: ago(120 * day),
    expires_at: now + 20 * day,
    last_used_at: ago(30 * day),
    use_count: 312,
    revoked_at: ago(11 * day),
    kind: "issued",
  },
];

type MockToken = components["schemas"]["ApiTokenDto"];
let mintedTokens = 0;

/** A mint by a.kim: the record joins `apiTokens`, the plaintext is fake. */
function mintMockToken(body: string, pathOrg?: string): Response {
  const req = JSON.parse(body || "{}") as {
    org?: string;
    name?: string;
    project?: string;
    role?: string;
    expires_in_days?: number;
    operations?: string[] | null;
  };
  mintedTokens += 1;
  const record: MockToken = {
    id: `0000ffff-0000-4fff-8fff-${String(mintedTokens).padStart(12, "0")}`,
    name: req.name ?? "new token",
    owner_subject: me.subject,
    owner: { id: me.id, handle: me.handle, display_name: me.display_name },
    org: pathOrg ?? req.org ?? "acme",
    project: req.project ?? null,
    role: req.role ?? "viewer",
    created_by: me.handle,
    created_at: Date.now(),
    expires_at: Date.now() + (req.expires_in_days ?? 90) * day,
    last_used_at: null,
    use_count: 0,
    revoked_at: null,
    kind: "issued",
    operations: req.operations ?? null,
  };
  apiTokens.unshift(record);
  return json(
    { token: "scarab_pat_EXAMPLE0NOTAREALTOKEN0000000000000000000000", record },
    { status: 201 },
  );
}

/** The server's org-list order: descending by `sort`, then newest first. */
function sortMockTokens(tokens: MockToken[], sort: string): MockToken[] {
  const key = (t: MockToken): number =>
    sort === "use_count" ? t.use_count : sort === "created" ? 0 : (t.last_used_at ?? -1);
  return [...tokens].sort((a, b) => key(b) - key(a) || b.created_at - a.created_at);
}

// ── step-up ────────────────────────────────────────────────────
// Enforced, one admin (j.lee) without a passkey so the banner and the Settings
// panel have something to say; a.kim holds two passkeys (one synced, one
// device-bound). A pending confirmation request lives at
// /security/confirm/req-demo-1. The ceremony endpoints are not mocked: a mock
// server cannot verify a passkey, and pretending to would be the lie.
type SecurityEventDto = components["schemas"]["SecurityEventDto"];
const secEvent = (
  seq: number,
  kind: string,
  minsAgo: number,
  o: Partial<SecurityEventDto> = {},
): SecurityEventDto => ({
  seq,
  id: `ev-${seq}`,
  at: ago(minsAgo * min),
  kind,
  subject: "a.kim",
  caller: "session:cookie",
  org: "acme",
  detail: {} as Record<string, never>,
  ...o,
});
const securityEvents: SecurityEventDto[] = [
  secEvent(12, "op.completed", 6, { op: "secret.put", target: "acme/orders-api/production/DB_PASSWORD" }),
  secEvent(11, "step_up.confirmed", 7, { detail: { kind: "window" } as unknown as Record<string, never> }),
  secEvent(10, "step_up.refused", 7, {
    op: "secret.put",
    target: "acme/orders-api/production/DB_PASSWORD",
    detail: { mode: "window", reason: "not confirmed" } as unknown as Record<string, never>,
  }),
  secEvent(9, "request.raised", 40, {
    subject: "a.kim",
    caller: "api_token:9f2c1d84",
    op: "token.create",
    target: "acme",
    request_id: "req-demo-1",
  }),
  secEvent(8, "step_up.refused", 95, {
    subject: "j.lee",
    op: "binding.put",
    target: "acme",
    detail: { mode: "enroll", reason: "no passkey" } as unknown as Record<string, never>,
  }),
  secEvent(7, "passkey.enrolled", 3 * 24 * 60, { org: null, passkey_id: "pk-2" }),
];
// Older Org events, so the event log on Settings → Security has more than
// one page (50 a page) to step through. Not in `securityEvents`: the
// Account → Security list and the summary's recent refusals stay as they were.
const OLDER_KINDS = ["op.authorised", "op.completed", "step_up.confirmed", "step_up.refused"];
const OLDER_ACTORS = ["a.kim", "j.lee", "r.patel"];
const OLDER_TARGETS = [
  "acme/orders-api/production/DB_PASSWORD",
  "acme/edge-gateway/staging/TLS_KEY",
  `run ${RICH_RUN_ID}/deploy`,
  "acme",
];
const olderOrgEvents: SecurityEventDto[] = Array.from({ length: 64 }, (_, i) => {
  const kind = OLDER_KINDS[i % OLDER_KINDS.length]!;
  return secEvent(6 - i, kind, 4 * 24 * 60 + i * 90, {
    id: `ev-old-${i}`,
    subject: OLDER_ACTORS[i % OLDER_ACTORS.length]!,
    op: kind === "step_up.confirmed" ? null : "secret.put",
    target: OLDER_TARGETS[i % OLDER_TARGETS.length]!,
  });
});
const orgEventLog = [...securityEvents, ...olderOrgEvents];

/** The Org event log as the server serves it: newest first by `seq`,
 * filtered, one page, with an opaque cursor (`seq:<n>`, base64url). */
function orgEventsPage(search: string) {
  const q = new URLSearchParams(search);
  const kinds = q.get("kind")?.split(",").filter(Boolean) ?? [];
  const actor = q.get("actor")?.trim() || "";
  const target = q.get("target")?.trim().toLowerCase() || "";
  const from = q.get("from") ? Number(q.get("from")) : undefined;
  const to = q.get("to") ? Number(q.get("to")) : undefined;
  const cursor = q.get("cursor");
  let before: number | undefined;
  if (cursor) {
    let text = "";
    try {
      text = atob(cursor.replace(/-/g, "+").replace(/_/g, "/"));
    } catch {
      /* not base64 */
    }
    const m = /^seq:(-?\d+)$/.exec(text);
    if (!m) return json("`cursor` is not one this server issued", { status: 400 });
    before = Number(m[1]);
  }
  const limit = Math.min(Math.max(Number(q.get("limit") ?? 50) || 50, 1), 200);
  const rows = orgEventLog
    .filter((e) => !kinds.length || kinds.includes(e.kind))
    .filter((e) => !actor || e.subject === actor)
    .filter((e) => !target || (!!e.org && (e.target ?? "").toLowerCase().includes(target)))
    .filter((e) => from === undefined || e.at >= from)
    .filter((e) => to === undefined || e.at < to)
    .filter((e) => before === undefined || e.seq < before)
    .sort((a, b) => b.seq - a.seq);
  const events = rows.slice(0, limit);
  const more = rows.length > limit;
  const last = events[events.length - 1];
  const next = more && last ? last.seq : null;
  return json({
    events,
    next_before: next,
    next_cursor: next === null ? null : btoa(`seq:${next}`).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, ""),
  });
}

const meSecurity: components["schemas"]["MeSecurityResponse"] = {
  subject: pid("a.kim"),
  handle: "a.kim",
  step_up: "enforce",
  has_passkey: true,
  usable_passkey_count: 2,
  passkeys_hidden: false,
  can_enroll_first: false,
  fresh_login_until: null,
  fresh_login_url: "/v1/auth/login?next=/account/security%3Fenroll%3D1",
  window_expires_at: now + 3 * min,
  passkeys: [
    {
      id: "pk-1",
      label: "MacBook Touch ID",
      created_at: ago(40 * day),
      last_used_at: ago(7 * min),
      backup_eligible: true,
      backup_state: true,
      clone_suspected: false,
      transports: ["internal", "hybrid"],
    },
    {
      id: "pk-2",
      label: "YubiKey 5C",
      created_at: ago(3 * day),
      last_used_at: null,
      backup_eligible: false,
      backup_state: false,
      clone_suspected: false,
      transports: ["usb", "nfc"],
    },
  ],
};
const orgSecuritySummary: components["schemas"]["OrgSecuritySummary"] = {
  enforced: true,
  caller_role: "owner",
  admins_without_passkey: ["j.lee"],
  admins_total: 2,
  unresolved_owner_entries: 0,
  recent_refusals: securityEvents.filter((e) => e.kind === "step_up.refused"),
  pending_requests: 1,
};
// Leak-check limits (Org settings › Security), one set for every org in the
// mock, changed by a PUT for the life of the page. The mock caller is an
// Owner, so it may change them; the bounds are the server's.
const leakCheckLimits: components["schemas"]["LeakCheckLimitsDto"] = {
  per_hour: 20,
  per_day: 40,
  max_per_hour: 100,
  max_per_day: 1000,
  caller_role: "owner",
};
const forkRunApproval: components["schemas"]["ForkRunApprovalDto"] = {
  enabled: false,
  can_change: true,
};
function mockForkRunApproval(method: string, body: string): Response {
  if (method === "PUT") {
    const req = JSON.parse(body || "{}") as { enabled?: unknown };
    if (typeof req.enabled !== "boolean") {
      return new Response('the body must be JSON of the form {"enabled": true} or {"enabled": false}', {
        status: 400,
      });
    }
    forkRunApproval.enabled = req.enabled;
  }
  return json(forkRunApproval);
}
function mockLeakChecks(method: string, body: string): Response {
  if (method === "PUT") {
    const req = JSON.parse(body || "{}") as { per_hour?: unknown; per_day?: unknown };
    const ok = (v: unknown, max: number) => Number.isInteger(v) && (v as number) >= 0 && (v as number) <= max;
    if (!ok(req.per_hour, leakCheckLimits.max_per_hour) || !ok(req.per_day, leakCheckLimits.max_per_day)) {
      return json(
        {
          code: "out_of_bounds",
          message: `Checks per hour must be 0 to ${leakCheckLimits.max_per_hour} and per day 0 to ${leakCheckLimits.max_per_day}.`,
        },
        { status: 400 },
      );
    }
    leakCheckLimits.per_hour = req.per_hour as number;
    leakCheckLimits.per_day = req.per_day as number;
  }
  return json(leakCheckLimits);
}
const stepUpRequest: components["schemas"]["StepUpRequestDto"] = {
  id: "req-demo-1",
  state: "pending",
  op: "token.create",
  org: "acme",
  method: "POST",
  path: "/v1/orgs/acme/tokens",
  query: "",
  summary: {
    operation: "Create an API token",
    target: "acme",
    changes: ["expires_in_days = 90", "name = release-bot-2", "role = member"],
  },
  requested_by: "API token “release-bot” (9f2c1d84)",
  requested_via: "api_token",
  for_human: "a.kim",
  created_at: ago(40 * min),
  expires_at: now + 20 * min,
};

// ── router ────────────────────────────────────────────────────────────────
function json(body: unknown, init?: ResponseInit) {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
    ...init,
  });
}

// `method` matters where one path serves several verbs — minting a token POSTs
// to the path the list GETs; an org Environment is GET/PUT/DELETE on one path —
// so it is threaded through rather than having a route guess from the body.
function route(pathname: string, search: string, method: string, body = ""): Response | null {
  const p = pathname;
  if (p === "/v1/me") return json(me);
  // A try's or a shared service's whole log as a file.
  if (/^\/v1\/runs\/[^/]+\/(steps|services)\/[^/]+\/logs\/raw$/.test(p)) {
    const chunks = mockLogChunks(p + search);
    const raw = (chunks ?? []).map((c) => (typeof c === "string" ? c : `[scarab] ${c.marker.text}\n`)).join("");
    return new Response(raw, {
      status: 200,
      headers: {
        "content-type": "text/plain; charset=utf-8",
        "content-disposition": 'attachment; filename="log.txt"',
      },
    });
  }
  if (p === "/v1/about") {
    return json({
      version: "0.3.2",
      commit: "720f3eec4b1d9a6e2c5f8b7a0d3e6f9a1b4c7d0e",
      started_at: Date.now() - 3 * 3600_000,
      role: "converged",
      executor: "k8s",
      login: true,
      github_app: true,
      plugins: true,
    });
  }
  // Ahead of the library routes, which answer every other repo's refused
  // runs with an empty list.
  if (p === "/v1/repos/acme/scarab/refused-runs") return json({ refused: scheduleRefused });
  const lib = libraryRoute(p, method, json, body);
  if (lib) return lib;
  const pol = policyRoute(p, search, method, body, json);
  if (pol) return pol;
  if (p === "/v1/me/security") return json(meSecurity);
  if (p === "/v1/me/security/events") {
    return json({ events: securityEvents.filter((e) => e.subject === "a.kim") });
  }
  if (/^\/v1\/orgs\/[^/]+\/security\/summary$/.test(p)) return json(orgSecuritySummary);
  if (/^\/v1\/orgs\/[^/]+\/security\/leak-checks$/.test(p)) return mockLeakChecks(method, body);
  if (/^\/v1\/orgs\/[^/]+\/security\/fork-run-approval$/.test(p)) return mockForkRunApproval(method, body);
  if (/^\/v1\/orgs\/[^/]+\/security\/events$/.test(p)) return orgEventsPage(search);
  // An owner resets a member's passkeys. The mock lets it through with no
  // step-up; an admin left with none joins the summary's list.
  const reset = method === "POST" ? p.match(/^\/v1\/orgs\/[^/]+\/members\/([^/]+)\/passkeys\/reset$/) : null;
  if (reset) {
    const id = decodeURIComponent(reset[1]!);
    const handle = bindings.find((b) => b.id === id)?.handle ?? id;
    const without = orgSecuritySummary.admins_without_passkey;
    const had = !without.includes(handle);
    const admin = bindings.some((b) => b.id === id && b.project === "" && (b.role === "admin" || b.role === "owner"));
    if (had && admin) without.push(handle);
    return json({ subject: id, handle, removed: had ? 2 : 0, windows_revoked: had ? 1 : 0 });
  }
  // One member by id: who they are and their roles in the org.
  const member = method === "GET" ? p.match(/^\/v1\/orgs\/[^/]+\/members\/([^/]+)$/) : null;
  if (member) {
    const id = decodeURIComponent(member[1]!);
    const held = bindings.filter((b) => b.id === id);
    const who = principals.find((x) => x.id === id) ?? held[0];
    if (!who) return new Response("not found", { status: 404 });
    return json({
      id,
      handle: who.handle,
      display_name: who.display_name ?? null,
      claimed: who.claimed ?? true,
      bindings: held,
    });
  }
  if (p === "/v1/step-up/requests/req-demo-1") return json(stepUpRequest);
  // Removing a passkey is always-ask: answer with the refusal so the step-up
  // dialog can be eyeballed (Cancel returns the 403 to the page).
  if (/^\/v1\/me\/passkeys\/[^/]+$/.test(p) && method === "DELETE") {
    return json(
      {
        code: "step_up_required",
        message: "This needs a step-up confirmation: confirm with your passkey.",
        mode: "action",
        op: "passkey.remove",
        summary: { operation: "Remove a passkey", target: p.split("/").pop(), changes: [] },
        challenge: {
          id: "mock-challenge",
          public_key: { challenge: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", rpId: "localhost", allowCredentials: [], userVerification: "required" },
          expires_at: Date.now() + 2 * min,
        },
      },
      { status: 403 },
    );
  }
  if (p === "/v1/auth/providers") return json(authProviders);
  // Linking: no provider to bounce through, so "authorize" straight back to
  // the Account page with the outcome a real callback would carry.
  if (p === "/v1/me/identities/link") {
    return json({ authorize_url: "/settings/profile?identity_linked=forgejo" });
  }
  if (/^\/v1\/me\/identities\/[^/]+$/.test(p)) return new Response(null, { status: 204 });
  if (/^\/v1\/orgs\/[^/]+\/bindings$/.test(p)) {
    if (method === "GET") return json(bindings);
    if (method === "PUT") mockGrant(JSON.parse(body || "{}"));
    if (method === "DELETE") {
      const q = new URLSearchParams(search);
      const who = q.get("subject");
      const i = bindings.findIndex(
        (b) => (b.id === who || b.handle === who) && b.project === (q.get("project") ?? ""),
      );
      if (i >= 0) bindings.splice(i, 1);
    }
    return new Response(null, { status: 204 });
  }
  if (/^\/v1\/orgs\/[^/]+\/principals$/.test(p)) {
    const q = (new URLSearchParams(search).get("q") ?? "").toLowerCase();
    return json(
      principals.filter(
        (x) =>
          x.handle.toLowerCase().startsWith(q) ||
          (x.display_name ?? "").toLowerCase().includes(q) ||
          x.identities.some((i) => i.handle.toLowerCase().startsWith(q)),
      ),
    );
  }
  const imported = p.match(/^\/v1\/repos\/[^/]+\/([^/]+)\/bindings\/import$/);
  if (imported) return json(mockResync(decodeURIComponent(imported[1]!)));
  if (p === "/v1/repos") return json(projects);
  if (p === "/v1/runs") {
    const org = new URLSearchParams(search).get("org");
    return json({ runs: org ? runs.filter((r) => r.org === org) : runs });
  }

  let m: RegExpMatchArray | null;

  // /v1/repos/{org}/{repo}/runs
  if ((m = p.match(/^\/v1\/repos\/[^/]+\/([^/]+)\/runs$/))) {
    const { runs } = repoRuns[m[1]] ?? strip("ooooooooooooo");
    // The orders-api list leads with the library Runs.
    if (m[1] === "orders-api" && !runs.some((r) => r.id === libraryRunSummaries[0]!.id)) {
      runs.unshift(...(libraryRunSummaries as typeof runs));
    }
    // `?environment=` and `?deploy=` filter before `limit`, as the server
    // does; an empty environment is a 400.
    const q = new URLSearchParams(search);
    const env = q.get("environment");
    if (env === "") {
      return new Response("`environment` must name an Environment", { status: 400 });
    }
    const deploy = q.get("deploy");
    const matching = runs.filter(
      (r) =>
        (env == null || r.environment === env) &&
        (deploy == null || r.deploy === (deploy === "true")),
    );
    const limit = Number(q.get("limit") ?? 50);
    return json({ runs: matching.slice(0, limit) });
  }
  // /v1/repos/{org}/{repo}/environments
  if ((m = p.match(/^\/v1\/repos\/[^/]+\/([^/]+)\/environments$/))) {
    return json(projectEnvironments(decodeURIComponent(m[1]!)));
  }
  // The retired per-repo writes: 410, as the server answers.
  if (/^\/v1\/repos\/[^/]+\/[^/]+\/environments\/[^/]+$/.test(p) && method !== "GET") {
    return new Response("per-repo environments were replaced by org-level Environments", {
      status: 410,
    });
  }
  // /v1/repos/{org}/{repo}/secrets/matrix
  if (/^\/v1\/repos\/[^/]+\/[^/]+\/secrets\/matrix$/.test(p)) return json(secretMatrix);
  // /v1/repos/{org}/{repo}/schedules
  if ((m = p.match(/^\/v1\/repos\/[^/]+\/([^/]+)\/schedules$/))) {
    return json({ schedules: m[1] === "scarab" ? schedules : [] });
  }
  // /v1/repos/{org}/{repo}/refs  (ref picker — not on the shot screens).
  // `q` filters by name, as the server does. The RETRIES run's branch
  // `fix/flaky-checkout` is not here: it was deleted when its PR merged.
  if (/^\/v1\/repos\/[^/]+\/[^/]+\/refs$/.test(p)) {
    const q = (new URLSearchParams(search).get("q") ?? "").toLowerCase();
    return json({
      refs: MOCK_REFS.filter((r) => r.name.toLowerCase().includes(q)),
      default_branch: "main",
    });
  }
  // The Run pipeline launcher: the catalog at a ref, one pipeline's typed
  // inputs, and a dispatch that opens the rich run.
  if (/^\/v1\/repos\/[^/]+\/[^/]+\/pipelines$/.test(p)) {
    return json({ sha: MOCK_PIPELINE_SHA, pipelines: mockCatalog });
  }
  if ((m = p.match(/^\/v1\/repos\/[^/]+\/[^/]+\/pipelines\/([^/]+)\/interface$/))) {
    const name = decodeURIComponent(m[1]!);
    // A ref the forge does not know (a deleted branch) fails as GitHub's
    // commits API answers it, wrapped as the server wraps it.
    const ref = new URLSearchParams(search).get("ref") ?? "";
    if (ref && !/^[0-9a-f]{7,40}$/.test(ref) && ref !== "HEAD" && !MOCK_REFS.some((r) => r.name === ref)) {
      return new Response(
        `pipeline failed to compile: forge api error: 422 Unprocessable Entity: {"message":"No commit found for SHA: ${ref}","status":"422"}`,
        { status: 400 },
      );
    }
    const entry = mockCatalog.find((c) => c.name === name);
    if (!entry || entry.error) return new Response(`no pipeline "${name}"`, { status: 404 });
    return json({ sha: MOCK_PIPELINE_SHA, manual: entry.manual, api: entry.api, inputs: mockInputs[name] ?? [] });
  }
  if (/^\/v1\/repos\/[^/]+\/[^/]+\/dispatch$/.test(p) && method === "POST") {
    return json({ id: RICH_RUN_ID });
  }

  // API tokens (git-bug a4cb94f): your own under /v1/me/tokens, every token in
  // the org under /v1/orgs/{org}/tokens. A mint returns an obviously-fake
  // plaintext so the one-time reveal can be exercised (and screenshotted), and
  // joins the list for the rest of the page's life, so a token minted under
  // Account settings shows in the org's list.
  if (p === "/v1/me/tokens") {
    if (method === "POST") return mintMockToken(body);
    return json(apiTokens.filter((t) => t.owner_subject === me.subject));
  }
  if ((m = p.match(/^\/v1\/orgs\/([^/]+)\/tokens$/))) {
    const org = decodeURIComponent(m[1]!);
    if (method === "POST") return mintMockToken(body, org);
    const sort = new URLSearchParams(search).get("sort") ?? "last_used";
    return json(sortMockTokens(apiTokens.filter((t) => t.org === org), sort));
  }
  // Revoke answers 404, as the server does, for an id that is not a live token
  // of yours (personal route) or of the org (org route).
  if (method === "DELETE" && (m = p.match(/^\/v1\/(?:me|orgs\/([^/]+))\/tokens\/([^/]+)$/))) {
    const org = m[1] === undefined ? null : decodeURIComponent(m[1]);
    const id = decodeURIComponent(m[2]!);
    const t = apiTokens.find(
      (x) => x.id === id && (org === null ? x.owner_subject === me.subject : x.org === org),
    );
    if (!t || t.revoked_at != null) return new Response("not found", { status: 404 });
    t.revoked_at = Date.now();
    return new Response(null, { status: 204 });
  }

  // Org-level Environments: list, one, and echoing writes.
  if (/^\/v1\/orgs\/[^/]+\/environments$/.test(p)) return json(orgEnvironments);
  // Duplicate: the copy is kept for this page load, so "Open" lands on it.
  if ((m = p.match(/^\/v1\/orgs\/([^/]+)\/environments\/([^/]+)\/duplicate$/)) && method === "POST") {
    const old = decodeURIComponent(m[2]!);
    const source = orgEnvironments.find((e) => e.name === old);
    if (!source) return new Response("not found", { status: 404 });
    const req = JSON.parse(body || "{}") as { name?: string; display_name?: string };
    const name = req.name ?? "";
    if (orgEnvironments.some((e) => e.name === name)) {
      return new Response(`org \`acme\` already has an environment \`${name}\``, { status: 409 });
    }
    const copy: OrgEnvironmentDto = {
      ...source,
      name,
      display_name: req.display_name?.trim() || `${source.display_name || source.name} (copy)`,
      created_at: now,
      updated_at: now,
      updated_by: "a.kim",
    };
    orgEnvironments.push(copy);
    overlays[name] = { ...(overlays[old] ?? {}) };
    grants[name] = { ...(grants[old] ?? {}) };
    return json(
      {
        environment: copy,
        copied: {
          overlays: Object.keys(overlays[old] ?? {}).sort(),
          privilege_grants: Object.keys(grants[old] ?? {}).sort(),
          secrets: [{ project: "scarab", keys: ["DATABASE_URL", "DEPLOY_TOKEN"] }],
        },
        not_copied: {
          policies: [
            { id: "pol-1", name: "no-friday-deploys", project: null, environments: [old] },
          ],
          projects_with_runs: ["scarab", "web-app"],
          old_oidc_subject: `scarab:org/acme/repo/*/env/${old}/ref/*`,
          new_oidc_subject: `scarab:org/acme/repo/*/env/${name}/ref/*`,
          notes: [
            `Run history, approvals included, and the Cache stay with ${old}, which is not changed and stays until an admin deletes it.`,
          ],
        },
      },
      { status: 201 },
    );
  }
  if ((m = p.match(/^\/v1\/orgs\/([^/]+)\/environments\/([^/]+)$/))) {
    const name = decodeURIComponent(m[2]!);
    const found = orgEnvironments.find((e) => e.name === name);
    if (method === "DELETE") {
      if (!found) return new Response("not found", { status: 404 });
      // production is held by scarab's overlay, grant and secrets: a plain
      // delete is refused with the list, `?cascade=true` deletes them too.
      // Nothing is deleted here either way.
      const blockers = deleteBlockers(name);
      if (blockers && !new URLSearchParams(search).has("cascade")) return json(blockers, { status: 409 });
      return new Response(null, { status: 204 });
    }
    if (method === "PUT") {
      // Echo the stored record with the body's display name and description;
      // a change to one is not stored. A new one is kept for the life of the
      // page, so its manage page opens.
      const req = JSON.parse(body || "{}") as { display_name?: string | null; description?: string };
      const written = { display_name: req.display_name ?? null, description: req.description ?? "" };
      if (found) return json({ ...found, ...written });
      const created = {
        org: decodeURIComponent(m[1]!),
        name,
        ...written,
        // A new Environment: permissive baseline, no eligible project.
        protection: {
          approvers: [],
          allowed_refs: [],
          wait_timer_s: 0,
          require_reason: false,
        },
        eligibility: { all_projects: false, projects: [] },
        created_at: now,
        updated_at: now,
        updated_by: "a.kim",
      };
      orgEnvironments.push(created as (typeof orgEnvironments)[number]);
      return json(created);
    }
    return found ? json(found) : new Response("not found", { status: 404 });
  }
  // Governance: baseline, eligibility, projects, overlays,
  // grants. Bodies are not visible here, so writes echo what is stored.
  if ((m = p.match(/^\/v1\/orgs\/[^/]+\/environments\/([^/]+)\/(protection|eligibility|projects)$/))) {
    const env = orgEnvironments.find((e) => e.name === decodeURIComponent(m![1]!));
    if (!env) return new Response("not found", { status: 404 });
    if (m[2] === "projects") return json(environmentProjects(env));
    if (m[2] === "protection") {
      if (method !== "PUT") return json(env.protection);
      // The baseline PUT replaces it whole: keep it for the page load, so
      // the page after a Save shows what was sent.
      const req = JSON.parse(body || "{}") as Record<string, unknown>;
      const num = (k: string) => (typeof req[k] === "number" ? (req[k] as number) : null);
      const gate = (req.approval_gate ?? {}) as Record<string, unknown>;
      const gateNum = (k: string) => (typeof gate[k] === "number" ? (gate[k] as number) : null);
      env.protection = {
        approvers: (req.approvers as string[] | undefined) ?? [],
        allowed_refs: (req.allowed_refs as string[] | undefined) ?? [],
        wait_timer_s: num("wait_timer_s") ?? 0,
        require_reason: req.require_reason === true,
        approval_gate: {
          missing_gate_mode: gate.missing_gate_mode === "refuse" ? "refuse" : "add",
          added_gate_expiry_s: gateNum("added_gate_expiry_s"),
          required_approvals: gateNum("required_approvals"),
          max_approval_age_s: gateNum("max_approval_age_s") ?? 86400,
        },
      };
      env.updated_at = now;
      return json(env);
    }
    return json(env.eligibility);
  }
  if (
    (m = p.match(
      /^\/v1\/orgs\/[^/]+\/environments\/([^/]+)\/projects\/([^/]+)\/(overlay|privilege-grant)$/,
    ))
  ) {
    const [env, project] = [decodeURIComponent(m[1]!), decodeURIComponent(m[2]!)];
    const stored = (m[3] === "overlay" ? overlays : grants)[env]?.[project];
    if (method === "DELETE") return new Response(null, { status: stored ? 204 : 404 });
    if (method === "PUT") {
      return json(
        stored ??
          (m[3] === "overlay"
            ? {
                approvers: [],
                allowed_refs: [],
                wait_timer_s: 0,
                require_reason: false,
              }
            : { privileged_images: [], permit_k8s_overlay: false }),
      );
    }
    return stored ? json(stored) : new Response("not found", { status: 404 });
  }

  // Plugins: manifest review, installations, kill/enable, uninstall.
  if (/^\/v1\/orgs\/[^/]+\/plugin-fetch$/.test(p) && method === "POST") {
    // Any source reads as the Argo CD manifest.
    const req = JSON.parse(body || "{}") as { source?: string };
    return json(fetchedArgocdManifest(req.source ?? ""));
  }
  if (/^\/v1\/orgs\/[^/]+\/plugins$/.test(p)) {
    if (method === "POST") {
      // A fresh, still-pending install of what was sent, kept until the page
      // reloads, so its manage page opens.
      const req = JSON.parse(body || "{}") as {
        name?: string;
        image?: string;
        manifest_sha256?: string;
        config?: Record<string, string>;
        storage?: { claim: string; mount_path?: string | null } | null;
      };
      const name = req.name || "argocd-dev";
      const made = pluginInstallation(name, "0a1b2c3d4e5f", {
        id: "1d2e3f4a5b",
        ...(req.image ? { image: req.image } : {}),
        ...(req.manifest_sha256 ? { manifest_sha256: req.manifest_sha256 } : {}),
        config: req.config ?? {},
        storage: req.storage
          ? {
              claim: req.storage.claim,
              mount_path: req.storage.mount_path || "/data",
              owner_annotation: "scarab.io/plugin-installation",
              owner_annotation_value: `acme/${name}`,
            }
          : null,
        state: "enabled",
        runtime_status: "pending",
        runtime_message: "",
        probe_passed_at: null,
        approved_at: now,
      });
      plugins.push(made);
      return json(made, { status: 201 });
    }
    return json(plugins);
  }
  if ((m = p.match(/^\/v1\/orgs\/[^/]+\/plugins\/([^/]+)\/upgrade(-fetch)?$/)) && method === "POST") {
    const found = plugins.find((x) => x.name === decodeURIComponent(m![1]!));
    if (!found) return new Response("no such installation", { status: 404 });
    if (m[2]) {
      const req = JSON.parse(body || "{}") as {
        source?: string;
        commit?: string;
        connection?: string;
        image?: string;
      };
      if (!req.source && !found.source) {
        return new Response("this installation has no recorded source; give the full source", { status: 400 });
      }
      const source = req.source ?? `${(found.source ?? ARGOCD_SOURCE_PATH).replace(/@[0-9a-f]+$/, "")}@${req.commit}`;
      return json(
        fetchedPluginUpgrade(found, source, req.connection ?? found.source_connection ?? null, req.image ?? null),
      );
    }
    const req = JSON.parse(body || "{}") as { image: string; source: string; manifest_sha256: string };
    const read = fetchedPluginUpgrade(found, req.source, null, req.image);
    Object.assign(found, {
      image: req.image,
      source: req.source,
      manifest: read.manifest,
      manifest_sha256: req.manifest_sha256,
      approved_by: "a.kim",
      approved_at: now,
      runtime_status: "pending",
      runtime_message: "",
      probe_passed_at: null,
    });
    return json(found);
  }
  if ((m = p.match(/^\/v1\/orgs\/[^/]+\/plugins\/([^/]+)\/storage$/))) {
    const found = plugins.find((x) => x.name === decodeURIComponent(m![1]!));
    if (!found) return new Response("no such installation", { status: 404 });
    if (method === "DELETE") return json({ ...found, storage: null });
    // The body is not visible here; echo the default claim as mounted.
    return json({
      ...found,
      storage: {
        claim: `${found.name}-state`,
        mount_path: "/data",
        owner_annotation: "scarab.io/plugin-installation",
        owner_annotation_value: `${found.org}/${found.name}`,
      },
    });
  }
  if ((m = p.match(/^\/v1\/orgs\/[^/]+\/plugins\/([^/]+)\/broker-journal$/))) {
    const found = plugins.find((x) => x.name === decodeURIComponent(m![1]!));
    if (!found) return new Response("no such installation", { status: 404 });
    const q = new URLSearchParams(search);
    const limit = Math.min(Math.max(Number(q.get("limit") ?? 100) || 100, 1), 500);
    const before = q.get("before");
    const phase = q.get("phase");
    const denied = q.get("denied") === "true";
    const matching = (brokerJournal[found.name] ?? []).filter(
      (e) =>
        (before == null || e.id < Number(before)) &&
        (!phase || e.phase === phase) &&
        (!denied || e.denied_reason != null),
    );
    const entries = matching.slice(0, limit);
    return json({
      entries,
      next_before: matching.length > limit ? entries[entries.length - 1]!.id : null,
    });
  }
  if ((m = p.match(/^\/v1\/orgs\/[^/]+\/plugins\/([^/]+)\/connections(?:\/([^/]+))?$/))) {
    const found = plugins.find((x) => x.name === decodeURIComponent(m![1]!));
    if (!found) return new Response("no such installation", { status: 404 });
    const list = pluginConnections[found.name] ?? [];
    if (!m[2]) return json(list);
    const slot = decodeURIComponent(m[2]);
    if (method === "DELETE") return new Response(null, { status: 204 });
    // The body is not visible here; echo the slot as bound.
    return json(list.find((c) => c.slot === slot) ?? pluginConnection(slot, {}));
  }
  if ((m = p.match(/^\/v1\/orgs\/[^/]+\/plugin-installations\/([^/]+)$/))) {
    const found = plugins.find((x) => x.id === decodeURIComponent(m![1]!));
    return found ? json(found) : new Response("no such installation", { status: 404 });
  }
  if ((m = p.match(/^\/v1\/orgs\/[^/]+\/plugins\/([^/]+)(?:\/(kill|enable))?$/))) {
    const found = plugins.find((x) => x.name === decodeURIComponent(m![1]!));
    if (!found) return new Response("no such installation", { status: 404 });
    // Kill and enable flip `state` only, as the server does: the runtime
    // (slice d2) reports what it observes, later.
    if (m[2] === "kill") {
      return json({ ...found, state: "killed", killed_by: "a.kim", killed_at: now });
    }
    if (m[2] === "enable") {
      return json({ ...found, state: "enabled", killed_by: null, killed_at: null });
    }
    if (method === "DELETE") {
      return json({ name: found.name });
    }
    return json(found);
  }
  if (p === "/v1/connections") {
    if (method === "POST") {
      // The new connection serves the org named in the body; a new org's
      // first connection makes the caller (a global admin) its admin, as
      // `/v1/me` reports on the next ask.
      const req = JSON.parse(body || "{}") as { org?: string; base_url?: string };
      const org = req.org ?? "acme";
      const id = "forgejo-5be0d4a1";
      connections.push({
        id,
        org,
        kind: "forgejo",
        base_url: req.base_url ?? "https://git.example.dev",
        web_url: req.base_url ?? "https://git.example.dev",
        credential_ref: `${id}-credential`,
        credential_present: true,
        last_delivery_at: null,
        enabled_repos: 0,
        supports_resync: true,
        supports_preflight: false,
        managed_by_config: false,
      });
      if (!me.admin_orgs.includes(org)) me.admin_orgs = [...me.admin_orgs, org].sort();
      if (!me.token_mint_orgs.includes(org)) me.token_mint_orgs = [...me.token_mint_orgs, org].sort();
      return json({ id, credential_ref: `${id}-credential` }, { status: 201 });
    }
    // `?org=` lists one org's connections; without it, every org plus the
    // unassigned ones (the fixture caller is a global admin).
    const org = new URLSearchParams(search).get("org");
    return json(withCounts(org ? connections.filter((c) => c.org === org) : connections));
  }
  if (method === "PUT" && (m = p.match(/^\/v1\/connections\/([^/]+)\/org$/))) {
    const org = (JSON.parse(body || "{}") as { org?: string }).org ?? "";
    const found = connections.find((c) => c.id === m![1]);
    if (!found) return new Response("not found", { status: 404 });
    const refused = assignRefusal(found, org);
    if (refused) {
      return new Response(`connection \`${found.id}\` cannot serve org \`${org}\`: ${refused}`, {
        status: 409,
      });
    }
    found.org = org;
    if (!me.admin_orgs.includes(org)) me.admin_orgs = [...me.admin_orgs, org].sort();
    if (!me.token_mint_orgs.includes(org)) me.token_mint_orgs = [...me.token_mint_orgs, org].sort();
    return json({ id: found.id, org });
  }
  if (method === "POST" && /^\/v1\/connections\/[^/]+\/resync$/.test(p)) {
    return json({
      available: orgRepos.filter((r) => !r.enabled).slice(0, 3).map((r) => `${r.owner}/${r.name}`),
      moved: [],
      confirmed: 3,
      skipped: ["globex/shared-ci"],
    });
  }
  if ((m = p.match(/^\/v1\/orgs\/([^/]+)\/repos$/))) return orgReposPage(decodeURIComponent(m[1]!), search);
  if (method === "POST" && (m = p.match(/^\/v1\/connections\/[^/]+\/repos$/))) {
    const req = JSON.parse(body || "{}") as { owner?: string; name?: string };
    const r = setRepoEnabled(req.owner ?? "", req.name ?? "", true);
    if (!r) return new Response("not reachable through this connection", { status: 400 });
    return json({ org: r.owner, project: r.name, webhook_registered: true, webhook_error: null });
  }
  if (method === "POST" && (m = p.match(/^\/v1\/connections\/[^/]+\/repos\/([^/]+)\/([^/]+)\/webhook$/))) {
    const [owner, name] = [decodeURIComponent(m[1]!), decodeURIComponent(m[2]!)];
    return json({ org: owner, project: name, webhook_registered: true, webhook_error: null });
  }
  if (method === "DELETE" && (m = p.match(/^\/v1\/connections\/[^/]+\/repos\/([^/]+)\/([^/]+)$/))) {
    setRepoEnabled(decodeURIComponent(m[1]!), decodeURIComponent(m[2]!), false);
    return new Response(null, { status: 204 });
  }
  if (/^\/v1\/connections\/[^/]+\/preflight$/.test(p)) return json(connectionPreflight);
  if (p === "/v1/forges/github/app") {
    return json({
      app_mode: true,
      install_url: "https://github.com/apps/scarab-ci/installations/new",
      unavailable_reason: null,
    });
  }
  // Secret names are scope-addressed: no `repo=` is the org scope.
  // The org scope's names change with a save or a delete, for the life of
  // the page.
  // The org's list: its own values by scope.
  if ((m = p.match(/^\/v1\/orgs\/([^/]+)\/secrets\/match$/)) && method === "POST") {
    return mockSecretMatch(decodeURIComponent(m[1]!), body);
  }
  if (/^\/v1\/orgs\/[^/]+\/secrets\/matrix$/.test(p)) return json(orgSecretMatrix());
  if (p === "/v1/secrets") {
    const q = new URLSearchParams(search);
    const scoped = q.has("repo");
    if (method === "POST") {
      const req = JSON.parse(body || "{}") as { name?: string; repo?: string; environment?: string };
      if (req.name && !req.repo) {
        if (!req.environment && !orgSecretNames.names.includes(req.name)) orgSecretNames.names.push(req.name);
        const scopes = orgSecretScopes.get(req.name) ?? new Set<string>();
        scopes.add(req.environment ?? "");
        orgSecretScopes.set(req.name, scopes);
      }
      return new Response(null, { status: 204 });
    }
    if (!scoped && method === "DELETE") {
      const name = q.get("name") ?? "";
      const environment = q.get("environment") ?? "";
      if (!environment) orgSecretNames.names = orgSecretNames.names.filter((n) => n !== name);
      orgSecretScopes.get(name)?.delete(environment);
      return new Response(null, { status: 204 });
    }
    if (method === "DELETE") return new Response(null, { status: 204 });
    return json(scoped ? secretNames : orgSecretNames);
  }

  // run-scoped
  if ((m = p.match(/^\/v1\/runs\/([^/]+)\/manifest$/))) {
    return json(runManifests[m[1]!] ?? { recorded: false, invoked: [] });
  }
  if (p === `/v1/runs/${RUN_ID}`) return json(withStepKeys(runStatus));
  if (p === `/v1/runs/${RUN_ID}/events`)
    return new Response(eventsBody, {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });
  if (p === `/v1/runs/${RUN_ID}/artifacts`) return json([]);
  if (p === `/v1/runs/${RUN_ID}/services`) return json([]);

  // the GATES run (external gates: waiting, passed, failed)
  if (p === `/v1/runs/${GATES_RUN_ID}`) return json(withStepKeys(gatesRunStatus));
  if (p === `/v1/runs/${GATES_RUN_ID}/events`)
    return new Response(gatesEventsBody, {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });
  if (p === `/v1/runs/${GATES_RUN_ID}/artifacts`) return json([]);
  if (p === `/v1/runs/${GATES_RUN_ID}/services`) return json([]);
  // the ENVIRONMENT GATE run
  if (p === `/v1/runs/${ENV_GATE_RUN_ID}`) return json(withStepKeys(envGateRunStatus));
  if (p === `/v1/runs/${ENV_GATE_RUN_ID}/events`)
    return new Response(envGateEventsBody, {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });
  if (p === `/v1/runs/${ENV_GATE_RUN_ID}/artifacts`) return json([]);
  if (p === `/v1/runs/${ENV_GATE_RUN_ID}/services`) return json([]);
  if (method === "POST" && (m = p.match(/^\/v1\/runs\/([^/]+)\/gates\/([^/]+)\/approve$/))) {
    const step = envGateRunStatus.steps.find((s) => s.id === mockStepId(m![2]!));
    if (m[1] !== ENV_GATE_RUN_ID || !step) return json({ released: true }, { status: 202 });
    if (!step.environment_approval_gate) {
      // A plain gate: the first approval by anyone with write releases it.
      step.status = "succeeded";
      return json({ released: true, approvals: ["a.kim"] }, { status: 202 });
    }
    // The approvals are in; the wait timer releases the gate later.
    step.approval_release_at = Date.now() + 5 * min;
    return json(
      { released: false, approvals: ["a.kim"], releases_at: step.approval_release_at },
      { status: 202 },
    );
  }
  // Reject a gate: the fixture is static, so it only answers.
  if (method === "POST" && /^\/v1\/runs\/[^/]+\/gates\/[^/]+\/reject$/.test(p)) {
    return json({ rejected: true }, { status: 202 });
  }

  // the MATRIX run
  if (p === `/v1/runs/${MATRIX_RUN_ID}`) return json(withStepKeys(matrixRunStatus));
  if (p === `/v1/runs/${MATRIX_RUN_ID}/events`)
    return new Response(matrixEventsBody, {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });
  if (p === `/v1/runs/${MATRIX_RUN_ID}/artifacts`) return json(matrixArtifacts);
  if (p === `/v1/runs/${MATRIX_RUN_ID}/services`) return json([]);

  // the RETRIES run
  if (p === `/v1/runs/${RETRIES_RUN_ID}`) return json(withStepKeys(retriesRunStatus));
  // Cancelling it ends it, for the rest of this page load.
  if (method === "POST" && p === `/v1/runs/${RETRIES_RUN_ID}/cancel`) {
    retriesRunStatus.status = "cancelled";
    return json({}, { status: 202 });
  }
  if (p === `/v1/runs/${RETRIES_RUN_ID}/events`)
    return new Response(retriesEventsBody, {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });
  if (p === `/v1/runs/${RETRIES_RUN_ID}/artifacts`) return json(retriesArtifacts);
  if (p === `/v1/runs/${RETRIES_RUN_ID}/services`) return json([]);

  // the RICH run (Playwright walkthrough fixture)
  if (p === `/v1/runs/${RICH_RUN_ID}`) return json(withStepKeys(richRunStatus));
  if (p === `/v1/runs/${RICH_RUN_ID}/events`)
    return new Response(richEventsBody, {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });
  if (p === `/v1/runs/${RICH_RUN_ID}/artifacts`) return json(richArtifacts);
  if (p === `/v1/runs/${RICH_RUN_ID}/services`) return json(richServices);
  // The rerun PREVIEW (git-bug 4afaa3e). RunDetail fetches this
  // per selection for LIVE runs too now, and the confirm popover renders it —
  // so the mock answers with a small realistic cascade over the acme fixture's
  // chain (clone → build → test → approve[gate]) rather than understating the
  // scope as just the target. An off-chain target degrades to itself.
  if ((m = p.match(/^\/v1\/runs\/[^/]+\/steps\/([^/]+)\/rerun-plan$/))) {
    const target = mockStepId(m[1]!);
    const chain = ["clone", "build", "test", "approve"];
    const from = chain.indexOf(target);
    const members = from >= 0 ? chain.slice(from) : [target];
    return json({
      target,
      invalidated: [...members].sort(),
      widened: [],
      starts_from: [target],
      expired_inputs: [],
      steps: members.map((step, i) => ({
        step,
        reason: i === 0 ? "target" : "cascade",
        ...(i > 0 ? { because_of: members[i - 1] } : {}),
        is_gate: step === "approve",
      })),
    });
  }
  if (/\/steps\/[^/]+\/results$/.test(p)) return json(testResults);
  if (/\/steps\/[^/]+\/consumed$/.test(p)) return json(testConsumed);
  // The RETRIES run kept its workspace snapshots: one file to open.
  if (p.startsWith(`/v1/runs/${RETRIES_RUN_ID}/steps/`) && p.endsWith("/workspace/file"))
    return new Response("12 passed, 0 failed\n", { status: 200, headers: { "content-type": "text/plain" } });
  if (p.startsWith(`/v1/runs/${RETRIES_RUN_ID}/steps/`) && p.endsWith("/workspace"))
    return json({ available: true, path: "", entries: [{ name: "summary.txt", kind: "file" }] });
  if (/\/steps\/[^/]+\/workspace$/.test(p))
    return json({ available: false, path: "", entries: [] });

  // any other /v1 → benign empty 200 so nothing errors mid-capture
  if (p.startsWith("/v1/")) return json({});
  return null;
}

function urlOf(input: RequestInfo | URL): string {
  if (typeof input === "string") return input;
  if (input instanceof URL) return input.toString();
  return (input as Request).url;
}

/** The request body as text: from `init.body` when it is a string, else from
 * the built `Request` openapi-fetch passes (a clone, so a real fetch can still
 * send it). Empty when `read` is false or there is none. */
async function bodyOf(input: RequestInfo | URL, init: RequestInit | undefined, read: boolean): Promise<string> {
  if (!read) return "";
  if (typeof init?.body === "string") return init.body;
  if (typeof input !== "string" && !(input instanceof URL)) {
    try {
      return await (input as Request).clone().text();
    } catch {
      return "";
    }
  }
  return "";
}

/** The verb, from wherever the caller put it: `openapi-fetch` passes a built
 * `Request`, hand-rolled calls pass `init.method`, a bare URL means GET. */
function methodOf(input: RequestInfo | URL, init?: RequestInit): string {
  if (init?.method) return init.method.toUpperCase();
  if (typeof input !== "string" && !(input instanceof URL)) {
    return (input as Request).method.toUpperCase();
  }
  return "GET";
}

export function installMock() {
  // Optional theme pin via `?theme=dark|light` — deterministic for capture.
  // Without it, the app's normal stored/toggle theme applies.
  const pinned = new URLSearchParams(window.location.search).get("theme");
  if (pinned === "dark" || pinned === "light") {
    try {
      localStorage.setItem("scarab-theme", pinned);
    } catch {
      /* ignore */
    }
    document.documentElement.setAttribute("data-theme", pinned);
  }
  // The same for the palette: `?palette=mono|emerald`.
  const pinnedPalette = new URLSearchParams(window.location.search).get("palette");
  if (pinnedPalette === "mono" || pinnedPalette === "emerald") {
    try {
      localStorage.setItem("scarab-palette", pinnedPalette);
    } catch {
      /* ignore */
    }
    document.documentElement.setAttribute("data-palette", pinnedPalette);
  }

  // Which orgs the caller administers: `?admin_orgs=acme` (default acme and globex).
  const adminOrgs = new URLSearchParams(window.location.search).get("admin_orgs");
  if (adminOrgs) me.admin_orgs = adminOrgs.split(",").filter(Boolean);

  const realFetch = window.fetch.bind(window);
  // The last log stream the mock opened failed (see `__scarabMockLogs`
  // below): the pane's probe of a log stream gets a 503.
  let logStreamDown = false;
  window.fetch = ((input: RequestInfo | URL, init?: RequestInit) => {
    let pathname = "";
    let search = "";
    try {
      const u = new URL(urlOf(input), window.location.origin);
      pathname = u.pathname;
      search = u.search;
    } catch {
      /* fall through to real fetch */
    }
    if (!pathname) return realFetch(input as RequestInfo, init);
    if (logStreamDown && /^\/v1\/runs\/[^/]+\/(steps|services)\/[^/]+\/logs$/.test(pathname)) {
      return Promise.resolve(new Response("unavailable", { status: 503 }));
    }
    const method = methodOf(input, init);
    // A body is read only for the writes the fixture records.
    const reads = method === "POST" || method === "PUT";
    return bodyOf(input, init, reads).then((body) => {
      const res = route(pathname, search, method, body);
      return res ?? realFetch(input as RequestInfo, init);
    });
  }) as typeof window.fetch;

  // EventSource — step/service log streams. Replays the fixture's chunks
  // (`mockLogEvents`) then idles.
  //
  // A spec can slow or break the streams whose URL contains `match` (all of
  // them without it) by setting `window.__scarabMockLogs` before the app boots:
  // `delayMs` holds back each stream's open and events that long, and
  // `failStreams` fails that many streams before they open (a connection the
  // server refused), each counted down as it fails.
  type MockLogKnobs = { match?: string; delayMs?: number; failStreams?: number };
  const knobsFor = (url: string): MockLogKnobs | null => {
    const k = (window as unknown as { __scarabMockLogs?: MockLogKnobs }).__scarabMockLogs;
    return k && (!k.match || url.includes(k.match)) ? k : null;
  };
  const RealES = window.EventSource;
  class MockES extends EventTarget {
    static readonly CONNECTING = 0;
    static readonly OPEN = 1;
    static readonly CLOSED = 2;
    readyState = 1;
    url: string;
    onmessage: ((e: MessageEvent) => void) | null = null;
    onerror: ((e: Event) => void) | null = null;
    onopen: ((e: Event) => void) | null = null;
    constructor(url: string | URL) {
      super();
      this.url = url.toString();
      const knobs = knobsFor(this.url);
      logStreamDown = !!knobs?.failStreams;
      if (knobs?.failStreams) {
        knobs.failStreams -= 1;
        setTimeout(() => {
          if (this.readyState === 2) return;
          this.readyState = 2;
          this.onerror?.(new Event("error"));
        }, knobs.delayMs ?? 0);
        return;
      }
      const later = (fn: () => void) =>
        knobs?.delayMs ? setTimeout(fn, knobs.delayMs) : queueMicrotask(fn);
      const events = mockLogEvents(this.url);
      later(() => {
        if (this.readyState !== 2) this.onopen?.(new Event("open"));
      });
      if (events.length > 0) {
        later(() => {
          for (const ev of events) {
            if (this.readyState === 2) return;
            const e = new MessageEvent(ev.type, { data: ev.data, lastEventId: ev.id ?? "" });
            if (ev.type === "message") this.onmessage?.(e);
            this.dispatchEvent(e);
          }
          // leave open (running step keeps tailing) — no onerror.
        });
      }
    }
    close() {
      this.readyState = 2;
    }
  }
  // Only intercept our own log streams; anything else falls back to real ES.
  window.EventSource = new Proxy(RealES, {
    construct(target, args: [string | URL, EventSourceInit?]) {
      const url = args[0]?.toString() ?? "";
      let path = url;
      try {
        path = new URL(url, window.location.origin).pathname;
      } catch {
        /* keep raw */
      }
      if (path.startsWith("/v1/runs/") && /\/logs$/.test(path)) {
        return new MockES(args[0]) as unknown as EventSource;
      }
      return new target(...args);
    },
  });

  // eslint-disable-next-line no-console
  console.log("[scarab-mock] installed — acme fixture, dark theme");
}
