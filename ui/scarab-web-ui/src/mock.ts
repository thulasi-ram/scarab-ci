// Fixture / demo mode for the web UI. Gated behind VITE_SCARAB_MOCK=1
// (`just ui-mock` / `npm run dev:mock`); patches window.fetch +
// window.EventSource to serve a fixed "acme" org so the UI renders with no
// server or DB. The fastest way to eyeball a UI change, and the source of the
// docs screenshots. Off by default — the guard in index.tsx never imports this
// module unless the flag is set, so it stays out of the production build.
//
// Neutral identity so nothing real leaks into screenshots: org=acme,
// author=a.kim, approver=j.lee; the `scarab` repo is kept (its build log
// references the crates), the rest are generic. Add `?theme=dark` (or `light`)
// to the URL to pin the theme (used for deterministic capture); otherwise the
// normal stored/toggle theme applies.

// Type-only imports — erased at compile time, so the install-before-App-loads
// ordering in index.tsx is unaffected. The fixtures typecheck against the
// generated OpenAPI types (the same contract the real server serves).
import type { components } from "./api/schema";

type RunStatusDto = components["schemas"]["RunStatusResponse"];
type ArtifactDto = components["schemas"]["ArtifactDto"];
type ServiceStatusDto = components["schemas"]["ServiceStatusDto"];

const now = Date.now();
const ago = (ms: number) => now - ms;
const min = 60000;

const RUN_ID = "0190f8a2000071fb8c0011223344aabb";
// The RICH run — the Playwright walkthrough fixture: two versions (a
// RunRerunRequested boundary with superseded/shadowed attempts), a shared
// service + a docked sidecar (ADR-0058), a pending manual gate, an artifact.
const RICH_RUN_ID = "0190f8a2000071fb8c0099887766ccdd";

// ── /v1/me ────────────────────────────────────────────────────────────────
// An Owner of the one implicit org (ADR-0060), so the Settings gear renders.
const me = {
  subject: "a.kim",
  display_name: "Avery Kim",
  roles: ["Owner"],
  can_administer: true,
  admin_orgs: ["acme"],
};

// ── /v1/repos (dashboard: first 4 → "most active" cards; count → "repos · 7")
const proj = (project: string, mins: number | null) => ({
  org: "acme",
  project,
  name: project,
  owner: "acme",
  repo_url: `https://github.com/acme/${project}`,
  last_run_at: mins == null ? null : ago(mins * 60000),
});
const projects = [
  proj("scarab", 3),
  proj("orders-api", 21),
  proj("inventory-svc", 48),
  proj("edge-gateway", 120),
  proj("billing", 9),
  proj("messaging", 300),
  proj("scarab-infra", null),
];

// ── /v1/runs (dashboard inbox = 2 suspended) ────────────────────────────────
const summary = (o: Record<string, unknown>) => ({
  duration_ms: 0,
  created_at: ago(60000),
  ...o,
});
const runs = [
  summary({
    id: "01984f10a1b24c7e8f0012ab34cd56ef",
    status: "suspended",
    org: "acme",
    project: "scarab",
    created_at: ago(12 * 60000),
  }),
  summary({
    id: "01984f22bb334d8e9a1122cd44ef6600",
    status: "suspended",
    org: "acme",
    project: "billing",
    created_at: ago(27 * 60000),
  }),
];

// ── per-repo run strips (dashboard cards) — status+duration_ms drive bars ────
// Tune succeeded/failed ratios to the "N% pass" shown per card.
const strip = (pattern: string) =>
  ({
    runs: pattern.split("").map((c, i) => ({
      id: `r${i}`,
      status: c === "x" ? "failed" : c === "c" ? "cancelled" : "succeeded",
      duration_ms: 40000 + ((i * 37) % 11) * 22000,
      created_at: ago((i + 1) * 900000),
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
  // ADR-0061 s5: a live run is not on the retention clock at all (a non-terminal
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
  // (ADR-0050) — but PINNED, to render the "keep this run's snapshots" state.
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
const environments = [
  {
    name: "production",
    protection: {
      approvers: ["ops-lead", "j.lee"],
      wait_timer: 300,
      allowed_refs: ["main"],
      concurrency: 1,
      require_reason: true,
    },
  },
  {
    name: "staging",
    protection: {
      approvers: ["ops-lead"],
      wait_timer: 0,
      allowed_refs: ["main", "release/*"],
      concurrency: 1,
      require_reason: false,
    },
  },
  {
    name: "preview",
    protection: {
      approvers: [],
      wait_timer: 0,
      allowed_refs: [],
      concurrency: 1,
      require_reason: false,
    },
  },
];
const deployments: Record<string, unknown[]> = {
  production: [
    {
      org: "acme",
      project: "scarab",
      environment: "production",
      git_ref: "main",
      run: "01984e00aa",
      approved_by: ["ops-lead"],
      at: ago(3600000),
    },
    {
      org: "acme",
      project: "scarab",
      environment: "production",
      git_ref: "main",
      run: "01984d00bb",
      approved_by: ["ops-lead"],
      at: ago(86400000),
    },
  ],
  staging: [],
  preview: [],
};

const secretNames = { names: ["GHCR_TOKEN", "DATABASE_URL", "SLACK_WEBHOOK_URL"] };
// Org scope (no `repo=`) holds the org-wide base of the inheritance chain — the
// Settings page's Org Secrets section (ADR-0060).
const orgSecretNames = { names: ["GHCR_TOKEN", "SENTRY_DSN"] };

// One healthy GitHub connection covering the acme repos (ADR-0060 part C).
const connections = [
  {
    id: "gh-acme",
    kind: "github",
    base_url: "https://api.github.com",
    web_url: "https://github.com",
    credential_ref: "github-app",
    credential_present: true,
    last_delivery_at: ago(240000),
    projects: ["scarab", "web", "api"].map((project) => ({
      org: "acme",
      project,
      owner: "acme",
      name: project,
    })),
    supports_resync: true,
    supports_preflight: true,
    managed_by_config: false,
  },
  // A self-hosted Forgejo connection — the manually-onboarded kind (ADR-0060
  // parts C/D), so the repo add/remove + webhook affordances render.
  {
    id: "forgejo-7f3a91c2",
    kind: "forgejo",
    base_url: "https://git.acme.dev",
    web_url: "https://git.acme.dev",
    credential_ref: "forgejo-7f3a91c2-credential",
    credential_present: true,
    last_delivery_at: ago(3_600_000),
    projects: [{ org: "acme", project: "infra", owner: "acme", name: "infra" }],
    supports_resync: true,
    // Forgejo tokens are not introspectable, so the preflight line is not
    // offered for them at all (the endpoint would only answer "unknown").
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

// What the Forgejo credential reaches — the bind pick-list (ADR-0060 slice 5).
const availableRepos = [
  { owner: "acme", name: "infra", bound: true },
  { owner: "acme", name: "terraform-modules", bound: false },
  { owner: "acme", name: "runbooks", bound: false },
];
// The coverage matrix is now the repo/environment secret EDITOR (ADR-0060), so
// the fixture carries the repo-default column ("") and inheritance origins:
// GHCR_TOKEN comes from the org, DATABASE_URL is a repo default overridden in
// both environments, SLACK_WEBHOOK_URL is production-only with staging silenced.
const secretMatrix = {
  columns: ["", "staging", "production"],
  environments: ["staging", "production"],
  keys: [
    {
      key: "GHCR_TOKEN",
      status: { "": "inherited", staging: "inherited", production: "inherited" },
      inherited_from: { "": "org", staging: "org", production: "org" },
    },
    {
      key: "DATABASE_URL",
      status: { "": "set", staging: "set", production: "set" },
      inherited_from: {},
    },
    {
      key: "SLACK_WEBHOOK_URL",
      status: { "": "unset", staging: "silenced", production: "set" },
      inherited_from: {},
    },
  ],
};

// ── /v1/orgs/{org}/tokens (ADR-0049) ──────────────────────────────────────
// Four records covering every state the panel renders differently: a live token
// in daily use, one narrowed to a single project, one inside the two-week
// expiry warning, and a revoked one (the graveyard row that proves a killed
// credential stays visible). No plaintext anywhere — the mint 201 below is the
// only place one exists, and it is obviously fake.
const day = 86_400_000;
const apiTokens = [
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
    last_used_at: ago(3 * 60 * min),
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
    revoked_at: ago(11 * day),
  },
];

// ── router ────────────────────────────────────────────────────────────────
function json(body: unknown, init?: ResponseInit) {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
    ...init,
  });
}

// `method` matters for exactly one surface so far — minting a token POSTs to
// the same path the list GETs — so it is threaded through rather than having
// the token route guess from the body.
function route(pathname: string, search: string, method: string): Response | null {
  const p = pathname;
  if (p === "/v1/me") return json(me);
  if (p === "/v1/repos") return json(projects);
  if (p === "/v1/runs") return json({ runs });

  let m: RegExpMatchArray | null;

  // /v1/repos/{org}/{repo}/runs
  if ((m = p.match(/^\/v1\/repos\/[^/]+\/([^/]+)\/runs$/))) {
    return json(repoRuns[m[1]] ?? strip("ooooooooooooo"));
  }
  // /v1/repos/{org}/{repo}/environments
  if (/^\/v1\/repos\/[^/]+\/[^/]+\/environments$/.test(p)) return json(environments);
  // /v1/repos/{org}/{repo}/environments/{name}/deployments
  if ((m = p.match(/^\/v1\/repos\/[^/]+\/[^/]+\/environments\/([^/]+)\/deployments$/))) {
    return json(deployments[m[1]] ?? []);
  }
  // /v1/repos/{org}/{repo}/secrets/matrix
  if (/^\/v1\/repos\/[^/]+\/[^/]+\/secrets\/matrix$/.test(p)) return json(secretMatrix);
  // /v1/repos/{org}/{repo}/refs  (ref picker — not on the shot screens)
  if (/^\/v1\/repos\/[^/]+\/[^/]+\/refs$/.test(p)) return json({ refs: [] });

  // /v1/orgs/{org}/tokens — list, and a mint that returns an obviously-fake
  // plaintext so the one-time reveal can be exercised (and screenshotted)
  // without a server. Nothing is persisted: the list is the fixture above.
  if (/^\/v1\/orgs\/[^/]+\/tokens$/.test(p)) {
    if (method === "POST") {
      return json(
        {
          token: "scarab_pat_EXAMPLE0NOTAREALTOKEN0000000000000000000000",
          record: {
            ...apiTokens[0],
            id: "0000ffff-0000-4fff-8fff-000000000000",
            name: "new token",
            created_at: now,
            expires_at: now + 90 * day,
            last_used_at: null,
          },
        },
        { status: 201 },
      );
    }
    return json(apiTokens);
  }
  if (/^\/v1\/orgs\/[^/]+\/tokens\/[^/]+$/.test(p)) return json({}, { status: 204 });

  if (p === "/v1/connections") return json(connections);
  if (/^\/v1\/connections\/[^/]+\/available-repos$/.test(p)) return json(availableRepos);
  if (/^\/v1\/connections\/[^/]+\/preflight$/.test(p)) return json(connectionPreflight);
  // Secret names are scope-addressed: no `repo=` is the org scope (ADR-0060).
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

  // the RICH run (Playwright walkthrough fixture)
  if (p === `/v1/runs/${RICH_RUN_ID}`) return json(richRunStatus);
  if (p === `/v1/runs/${RICH_RUN_ID}/events`)
    return new Response(richEventsBody, {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });
  if (p === `/v1/runs/${RICH_RUN_ID}/artifacts`) return json(richArtifacts);
  if (p === `/v1/runs/${RICH_RUN_ID}/services`) return json(richServices);
  // The rerun PREVIEW (ADR-0061 s5 + git-bug 4afaa3e). RunDetail fetches this
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
    const res = pathname ? route(pathname, search, methodOf(input, init)) : null;
    if (res) return Promise.resolve(res);
    return realFetch(input as RequestInfo, init);
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
