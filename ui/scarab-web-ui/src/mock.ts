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
import { paginate } from "./org-page";

type RunStatusDto = components["schemas"]["RunStatusResponse"];
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

// ── /v1/me ────────────────────────────────────────────────────────────────
// An Owner of the one implicit org, so the Settings gear renders.
// The Owner role is global, so Avery is a global admin: the org dropdown in
// Organization settings ends with New organization…, and Connections lists the
// unassigned connection below (git-bug 19b0a45). Avery
// administers two orgs, so the org dropdown in Organization settings offers a
// choice; globex has no connections or repositories of its own.
const me = {
  subject: "a.kim",
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
  { subject: "a.kim", role: "owner", project: "", display_name: "Avery Kim", claimed: true, origin: "native" },
  { subject: "m.diaz", role: "admin", project: "", display_name: "Morgan Diaz", claimed: true, origin: "native" },
  { subject: "j.lee", role: "member", project: "", display_name: "Jordan Lee", claimed: true, origin: "native" },
  { subject: "j.lee", role: "admin", project: "orders-api", display_name: "Jordan Lee", claimed: true, origin: "native" },
  { subject: "j.lee", role: "admin", project: "scarab", display_name: "Jordan Lee", claimed: true, origin: "native" },
  { subject: "r.okafor", role: "member", project: "scarab", claimed: false, origin: "import" },
  { subject: "t.nguyen", role: "viewer", project: "scarab", display_name: "Taylor Nguyen", claimed: true, origin: "import" },
  { subject: "okta:s.patel@acme.example", role: "viewer", project: "", display_name: "Sam Patel", claimed: true, origin: "native" },
];
// What the forge lists as each repository's collaborators, already mapped to
// roles (admin → admin, write → member, read → viewer). A re-sync of `scarab`
// raises Taylor to member and adds Priya; a repository not listed here has an
// empty listing, which, as on the server, changes nothing.
const forgeCollaborators: Record<string, MockBinding[]> = {
  scarab: [
    { subject: "r.okafor", role: "member", project: "scarab", claimed: false },
    { subject: "t.nguyen", role: "member", project: "scarab", display_name: "Taylor Nguyen", claimed: true },
    { subject: "p.ito", role: "viewer", project: "scarab", claimed: false },
  ],
};

// The server's re-sync, in the mock: seed or refresh the forge's rows, never
// a role granted in Scarab, and drop the forge's rows it no longer lists.
function mockResync(repo: string): MockBinding[] {
  const listed = forgeCollaborators[repo] ?? [];
  if (listed.length === 0) return [];
  for (const c of listed) {
    const row = bindings.find((b) => b.subject === c.subject && b.project === repo);
    if (!row) bindings.push({ ...c, origin: "import" });
    else if (row.origin === "import") row.role = c.role;
  }
  for (let i = bindings.length - 1; i >= 0; i--) {
    const b = bindings[i]!;
    if (b.project === repo && b.origin === "import" && !listed.some((c) => c.subject === b.subject)) {
      bindings.splice(i, 1);
    }
  }
  return listed.map(({ subject, role, project }) => ({ subject, role, project }));
}

function mockGrant(req: { subject?: string; role?: string; project?: string }) {
  const project = req.project ?? "";
  const who = principals.find((x) => x.id === req.subject);
  const row = bindings.find((b) => b.subject === req.subject && b.project === project);
  if (row) {
    row.role = req.role ?? row.role;
    row.origin = "native";
  } else {
    bindings.push({
      subject: req.subject ?? "",
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
    id: "j.lee",
    display_name: "Jordan Lee",
    claimed: true,
    identities: [{ provider: "default", handle: "j.lee" }],
  },
  {
    id: "r.okafor",
    display_name: null,
    claimed: false,
    identities: [{ provider: "default", handle: "r.okafor" }],
  },
  {
    id: "m.diaz",
    display_name: "Morgan Diaz",
    claimed: true,
    identities: [{ provider: "default", handle: "m.diaz" }],
  },
  {
    id: "t.nguyen",
    display_name: "Taylor Nguyen",
    claimed: true,
    identities: [{ provider: "default", handle: "t.nguyen" }],
  },
  {
    id: "okta:s.patel@acme.example",
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
    waiting_gates: [{ step: "approve", kind: "manual", since: ago(10 * 60000) }],
  }),
  summary({
    id: "01984f22bb334d8e9a1122cd44ef6600",
    status: "suspended",
    org: "acme",
    project: "billing",
    created_at: ago(27 * 60000),
    waiting_gates: [{ step: "release", kind: "manual", since: ago(20 * 60000) }],
  }),
  // git-bug c489346: a gate holds only its dependents, so this run is still
  // `running` (its `version` sibling runs) while its `approve` gate waits.
  summary({
    id: "01984f35cc445e9fab2233de55f07711",
    status: "running",
    org: "acme",
    project: "orders-api",
    created_at: ago(6 * 60000),
    waiting_gates: [{ step: "approve", kind: "manual", since: ago(4 * 60000) }],
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
const strip = (pattern: string) =>
  ({
    runs: pattern.split("").map((c, i) => ({
      id: `r${i}`,
      status: c === "x" ? "failed" : c === "c" ? "cancelled" : "succeeded",
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
const repoRuns: Record<string, ReturnType<typeof strip>> = {
  scarab: strip("ooxooooxooxooo"), // ~64% pass window near the older tail
  "orders-api": strip("oxooxooxooooo"),
  "inventory-svc": strip("oxoxooxxooxoo"), // ~50%
  "edge-gateway": strip("ooooooooxoooo"), // ~93%
  // matches its last_run_at: null above — the empty-state repo (ponder scene)
  "scarab-infra": { runs: [] },
};
// A gentler pass mix so the shown percentages land ~64/64/50/93 like the
// original: recompute deterministically by fixed strings above (close enough).

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
      attempt_list: [{ id: "a1", started_at: ago(325000), failed: false, outcome: "succeeded" }],
    },
    {
      id: "build",
      status: "succeeded",
      attempts: 2,
      needs: ["clone"],
      attempt_list: [
        { id: "a1", started_at: ago(297000), failed: true, failure: "step", outcome: "failed" },
        { id: "a2", started_at: ago(250000), failed: false, outcome: "succeeded" },
      ],
    },
    {
      id: "test",
      status: "running",
      attempts: 1,
      needs: ["build"],
      attempt_list: [{ id: "a1", started_at: ago(190000), failed: false, outcome: "running" }],
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
  params: {},
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
        { id: "a1", started_at: ago(25 * min), failed: false, outcome: "succeeded" },
      ],
    },
    {
      id: "build",
      status: "succeeded",
      attempts: 3,
      needs: ["clone"],
      attempt_list: [
        { id: "a1", started_at: ago(24 * min + 30000), failed: false, outcome: "succeeded" },
        { id: "a2", started_at: ago(14 * min + 55000), failed: true, failure: "step", outcome: "failed" },
        { id: "a3", started_at: ago(13 * min + 40000), failed: false, outcome: "succeeded" },
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
        { id: "a1", started_at: ago(23 * min + 15000), failed: false, outcome: "superseded" },
        { id: "a2", started_at: ago(12 * min + 25000), failed: false, outcome: "succeeded" },
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
  rev(ago(24 * min + 44000), { StepTransitioned: { step: "clone", from: "running", to: "succeeded" } }),
  rev(ago(24 * min + 30000), { AttemptStarted: { step: "build", attempt: "a1" } }),
  rev(ago(23 * min + 20000), { AttemptFinished: { step: "build", attempt: "a1" } }),
  rev(ago(23 * min + 19000), { StepTransitioned: { step: "build", from: "running", to: "succeeded" } }),
  rev(ago(23 * min + 15000), { AttemptStarted: { step: "test", attempt: "a1" } }),
  // ── the boundary: a human reran build; test (in flight) + approve cascade ──
  rev(ago(15 * min), {
    RunRerunRequested: { target: "build", invalidated: ["test", "approve"], by: "a.kim" },
  }),
  // ── version 2 (latest) ──
  rev(ago(14 * min + 55000), { AttemptStarted: { step: "build", attempt: "a2" } }),
  rev(ago(13 * min + 50000), { AttemptFinished: { step: "build", attempt: "a2", failure: "step" } }),
  rev(ago(13 * min + 49000), { StepTransitioned: { step: "build", from: "running", to: "failed" } }),
  rev(ago(13 * min + 40000), { AttemptStarted: { step: "build", attempt: "a3" } }),
  rev(ago(12 * min + 30000), { AttemptFinished: { step: "build", attempt: "a3" } }),
  rev(ago(12 * min + 29000), { StepTransitioned: { step: "build", from: "running", to: "succeeded" } }),
  rev(ago(12 * min + 25000), { AttemptStarted: { step: "test", attempt: "a2" } }),
  rev(ago(10 * min), { AttemptFinished: { step: "test", attempt: "a2" } }),
  rev(ago(10 * min - 1000), { StepTransitioned: { step: "test", from: "running", to: "succeeded" } }),
  rev(ago(9 * min + 50000), { StepTransitioned: { step: "approve", from: "pending", to: "waiting" } }),
].join("");

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

const testResults = [
  { name: "tests_passed", type_name: "number", value: 214 },
  { name: "coverage", type_name: "string", value: "92.4%" },
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
  attempt_list: [{ id: "a1", started_at: ago(at), failed: false, outcome: "succeeded" }],
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
    gev(ago(4 * min + 9000), { StepTransitioned: { step: s, from: "running", to: "succeeded" } }),
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
  attempt_list: [{ id: "a1", started_at: ago(3 * min), failed: false, outcome: "running" }],
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
      attempt_list: [{ id: "a1", started_at: ago(5 * min), failed: false, outcome: "succeeded" }],
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
  egv(ago(3 * min + 9000), { StepTransitioned: { step: "build", from: "running", to: "succeeded" } }),
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
// and a gate added: a passkey); of any other installation, a narrower one
// (the version rule removed: no passkey).
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
    widenings: wider ? ["adds read rule `argocd: GET /api/v1/account`", "adds gate `wait-synced`"] : [],
    needs_step_up: wider,
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
  // An installation whose account could not be told apart from a same-named
  // org on another forge, so it serves no org yet.
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
  },
];

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
// ── /v1/repos/acme/scarab/schedules: two `on: cron:` schedules ─────────────
// The nightly one's last slot started a Run (the scarab run fixture); the
// weekly one's last slot was missed, so it is also a refused row in the runs
// list, as the server records it.
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
  ],
};

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
    owner_subject: "a.kim",
    org: "acme",
    project: null,
    role: "member",
    created_by: "a.kim",
    created_at: ago(40 * day),
    expires_at: now + 50 * day,
    last_used_at: ago(4 * min),
    use_count: 18_412,
    revoked_at: null,
  },
  {
    id: "3b7a9c02-6d41-4b8e-8c22-71f0ab993c55",
    name: "orders-api deploy gate",
    owner_subject: "j.lee",
    org: "acme",
    project: "orders-api",
    role: "admin",
    created_by: "a.kim",
    created_at: ago(9 * day),
    expires_at: now + 81 * day,
    last_used_at: ago(20 * min),
    use_count: 1_203,
    revoked_at: null,
  },
  {
    id: "c41e7f60-2a88-4f19-b0d3-5e6a7c1b2d34",
    name: "status poller",
    owner_subject: "a.kim",
    org: "acme",
    project: null,
    role: "viewer",
    created_by: "a.kim",
    created_at: ago(84 * day),
    expires_at: now + 6 * day,
    last_used_at: ago(52 * min),
    use_count: 9_870,
    revoked_at: null,
  },
  {
    id: "7d5b0e13-9c44-4a27-8e61-0f3d2c8a55b7",
    name: "amy's laptop",
    owner_subject: "a.kim",
    org: "acme",
    project: null,
    role: "member",
    created_by: "a.kim",
    created_at: ago(120 * day),
    expires_at: now + 20 * day,
    last_used_at: ago(30 * day),
    use_count: 312,
    revoked_at: ago(11 * day),
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
  };
  mintedTokens += 1;
  const record: MockToken = {
    id: `0000ffff-0000-4fff-8fff-${String(mintedTokens).padStart(12, "0")}`,
    name: req.name ?? "new token",
    owner_subject: me.subject,
    org: pathOrg ?? req.org ?? "acme",
    project: req.project ?? null,
    role: req.role ?? "viewer",
    created_by: me.subject,
    created_at: Date.now(),
    expires_at: Date.now() + (req.expires_in_days ?? 90) * day,
    last_used_at: null,
    use_count: 0,
    revoked_at: null,
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
  subject: "a.kim",
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
  if (/^\/v1\/orgs\/[^/]+\/security\/events$/.test(p)) return orgEventsPage(search);
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
      const i = bindings.findIndex(
        (b) => b.subject === q.get("subject") && b.project === (q.get("project") ?? ""),
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
          x.id.toLowerCase().startsWith(q) ||
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
  // /v1/repos/{org}/{repo}/refs  (ref picker — not on the shot screens)
  if (/^\/v1\/repos\/[^/]+\/[^/]+\/refs$/.test(p)) return json({ refs: [] });

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
      // Echo the stored record (or a fresh one) with the body's display name
      // and description; nothing is stored.
      const req = JSON.parse(body || "{}") as { display_name?: string | null; description?: string };
      const written = { display_name: req.display_name ?? null, description: req.description ?? "" };
      return json(
        found ? { ...found, ...written } : {
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
        },
      );
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
      // the view after a Save shows what was sent.
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
      const req = JSON.parse(body || "{}") as { source?: string; commit?: string; connection?: string };
      if (!req.source && !found.source) {
        return new Response("this installation has no recorded source; give the full source", { status: 400 });
      }
      const source = req.source ?? `${(found.source ?? ARGOCD_SOURCE_PATH).replace(/@[0-9a-f]+$/, "")}@${req.commit}`;
      return json(fetchedPluginUpgrade(found, source, req.connection ?? found.source_connection ?? null));
    }
    const req = JSON.parse(body || "{}") as { image: string; source: string; manifest_sha256: string };
    const read = fetchedPluginUpgrade(found, req.source, null);
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
  // Secret names are scope-addressed: no `repo=` is the org scope.
  if (p === "/v1/secrets") {
    const scoped = new URLSearchParams(search).has("repo");
    return json(scoped ? secretNames : orgSecretNames);
  }

  // run-scoped
  if (p === `/v1/runs/${RUN_ID}`) return json(runStatus);
  if (p === `/v1/runs/${RUN_ID}/events`)
    return new Response(eventsBody, {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });
  if (p === `/v1/runs/${RUN_ID}/artifacts`) return json([]);
  if (p === `/v1/runs/${RUN_ID}/services`) return json([]);

  // the GATES run (external gates: waiting, passed, failed)
  if (p === `/v1/runs/${GATES_RUN_ID}`) return json(gatesRunStatus);
  if (p === `/v1/runs/${GATES_RUN_ID}/events`)
    return new Response(gatesEventsBody, {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });
  if (p === `/v1/runs/${GATES_RUN_ID}/artifacts`) return json([]);
  if (p === `/v1/runs/${GATES_RUN_ID}/services`) return json([]);
  // the ENVIRONMENT GATE run
  if (p === `/v1/runs/${ENV_GATE_RUN_ID}`) return json(envGateRunStatus);
  if (p === `/v1/runs/${ENV_GATE_RUN_ID}/events`)
    return new Response(envGateEventsBody, {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });
  if (p === `/v1/runs/${ENV_GATE_RUN_ID}/artifacts`) return json([]);
  if (p === `/v1/runs/${ENV_GATE_RUN_ID}/services`) return json([]);
  if (method === "POST" && (m = p.match(/^\/v1\/runs\/([^/]+)\/gates\/([^/]+)\/approve$/))) {
    const step = envGateRunStatus.steps.find((s) => s.id === decodeURIComponent(m![2]!));
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

  // the RICH run (Playwright walkthrough fixture)
  if (p === `/v1/runs/${RICH_RUN_ID}`) return json(richRunStatus);
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
    const target = m[1];
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
    const method = methodOf(input, init);
    // A body is read only for the writes the fixture records.
    const reads = method === "POST" || method === "PUT";
    return bodyOf(input, init, reads).then((body) => {
      const res = route(pathname, search, method, body);
      return res ?? realFetch(input as RequestInfo, init);
    });
  }) as typeof window.fetch;

  // EventSource — step/service log streams. Replays fixture lines then idles.
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
      const path = (() => {
        try {
          return new URL(this.url, window.location.origin).pathname;
        } catch {
          return this.url;
        }
      })();
      const replay =
        /\/steps\/[^/]+\/logs$/.test(path)
          ? testLogLines
          : /\/services\/[^/]+\/logs$/.test(path)
            ? serviceLogLines
            : null;
      if (replay) {
        queueMicrotask(() => {
          for (const line of replay) {
            const e = new MessageEvent("message", { data: line });
            this.onmessage?.(e);
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
