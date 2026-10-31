# 0077. End-to-end, fault and agent-driven testing: one invariant checker, a few fault scenarios on a throwaway cluster, and a red-team agent that only writes tests

- **Status:** Proposed
- **Date:** 2026-10-31
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0017](0017-testing-strategy.md) (classical, lean,
  grow-from-bugs; the durability exception; the feature-acceptance addendum). This ADR
  enlarges 0017's durability exception within limits and does not change 0017.
- **Relates:** [0011](0011-durable-scheduler.md) (scheduler lease),
  [0021](0021-double-effect-fencing.md) (fences), [0059](0059-tick-fault-isolation.md)
  (tick errors), [0061](0061-workspace-data-path.md) and
  [0066](0066-the-depot-is-a-cache.md) (the Data Depot), [0063](0063-step-logs-on-the-data-depot.md)
  and [0067](0067-the-pack-is-the-record.md) (logs and packs in object storage),
  [0076](0076-libraries-and-plugins-through-invoke.md) (clone library, `forge_token`, yank,
  plugin gates)

## Context

The owner's direction (2026-10-31):

- Everything except external SaaS dependencies (the GitHub App and GitHub OAuth) is tested
  end to end on a real local stack.
- A few Jepsen-style fault tests, so that no broad class of failure goes untested.
- An `--agentic` mode on `just chaos`, `just e2e` and `just security`, where a red-team
  agent watches events and logs and drives scenarios.

An audit of that proposal (read-only, 2026-10-31, against `origin/main` @ 85309670) found
the deterministic part worth building and the agent-driven chaos part not worth building
yet. This ADR adopts the audit's conclusions with the corrections noted below. The facts
it rests on, checked again on `origin/main` @ 71af72bc:

**The stack tier is red and has no triage loop.** The one stack-grain crash test,
`crash_resume_completes_exactly_once` (`crates/scarab-e2e/tests/crash_resume.rs`), failed
every nightly `e2e` run from 1 to 19 October. It passed once on
`fix/e2e-crash-resume-workspace-env` (run 39224071150), which is merged, and failed again on
main on 26 October (run 40288915802): after the SIGKILL and restart the Run stays
`running` with `slow:running(x1)` and `second:pending(x0)` until the 240 s wait in
`crates/scarab-e2e/tests/support/mod.rs:188` gives up. `e2e.yml` was cut to weekly for this
reason (`.github/workflows/e2e.yml:7-10`). The `kind` workflow is also red on main (run
41185254634, 2026-10-31): `kind load` fails in `ctr images import --all-platforms`.
[positioning.md](../positioning.md) already says the live-Pod crash test "doesn't pass yet".
More scenarios on a tier nobody triages add more red jobs; the cost of a fault tier is the
triage, not the authoring.

**Most races in this codebase are races between transactions on one Postgres.** They are
cheaper and deterministic in-process: `crates/scarab-db-postgres/tests/durability_guard.rs`
models a crash as "drop scheduler A, build scheduler B over the same Postgres", with
`FakeClock` and `FakeExecutor`. The code red teams (A1-A7, R2-E1..E8, `202ee19c` atomic
fenced gate waits) found most of the fixed bugs in this class and pinned them there. The
stack sees what in-process tests cannot: the real Pod lifecycle, the Depot over HTTP with
ClusterIP scatter and S3 multipart, the sqlx pool under TCP faults, and a process that dies
in the middle of I/O.

**The live N>1 runs found what acceptance tests missed.** git-bug `afb13c2` (a drain's
parallel PUTs scatter across Depot replicas and the POST-receiving replica answers 422 for
good) passed the "two routers, one Postgres, one bucket" acceptance test and showed up only
on a live two-replica run. Only the dogfood and that run have ever used more than one
replica.

**Some proposed invariants do not hold on main as written:**

- "Every minted forge token is revoked": on main nothing is minted that could be revoked.
  The Forgejo checkout credential is the admin's configured token
  (`crates/scarab-forge-forgejo/src/lib.rs:775-790`); workspace and results tokens are
  stateless HMAC with a TTL (`crates/scarab-executor-k8s/src/workspace_token.rs:268`).
  ADR-0076 step 3 (on `feat/plugin-gates`) adds the per-Attempt mint: the forge credential
  lives in a per-unit k8s Secret, `take_forge_credential` reads it back and deletes it when
  the Attempt ends, and only a `minted` credential (GitHub App mode) is revoked, through
  `DELETE /installation/token`. A long-lived Forgejo or PAT token is removed and never
  revoked.
- "At most one concurrency slot holder" cannot fail as a snapshot: `concurrency_slots` has
  `group_key TEXT PRIMARY KEY` (`crates/scarab-db-postgres/migrations/0008_concurrency.sql:15`),
  and the table is updated in place with no event.
- "A plugin never resolves a gate it does not own" tests a feature that is not on main
  (ADR-0076 step 4), and it is an authorization rule.

**A crash between the two halves of Run creation is not reclaimed.** `begin_run_creation`
inserts the Run with `creating = TRUE`; `finish_run_creation` clears it
(`crates/scarab-db-postgres/src/lib.rs:200-238`, migration `0069_run_creating.sql`). The
scheduler's work list skips `creating` rows (`lib.rs:1392`). Nothing else reads or clears
the flag, so a SIGKILL between the two leaves the Run Pending and invisible to the
scheduler. git-bug `ccbf7fe` (open) proposes a sweeper.

**Leadership is a Postgres lease.** `is_leader` (`crates/scarab-engine/src/scheduler.rs:2261`)
re-takes a 30 s lease (`scheduler.rs:2067`) whose expiry is computed from Postgres `now()`
(`crates/scarab-db-postgres/src/lib.rs:2464`), then acts. A leader paused past the TTL
resumes and acts once more before its next check. Whether row-level checks catch that write
is what a pause test finds out.

**The local credentials are within reach of anything that runs here.** `.envrc` (untracked)
pins `current-context: colima` through an overlay but appends `~/.kube/config`, so every
production context is one `--context` flag away, and EKS contexts authenticate through
`~/.aws`. `kubectx` is installed and changes the global current context.
`deploy/local-helm/deploy.sh:51-52` checks the context once and then runs about 20
commands. `just kube-tests` reads `gh auth token` (`Justfile:482`). A context name is set by
the user, so a check on the name proves nothing about the cluster.

## Decision

### 1. Four tiers, one oracle

ADR-0017's tiers stay: **unit**, **functional** (real router and engine in-process, fakes
at ports, real Postgres), **kind** (executor against a real apiserver) and **stack e2e**.
This ADR adds one tier and one tool:

| Tier or tool | Recipe | Stack | What belongs there |
|---|---|---|---|
| functional | `just test` | in-process, real Postgres | every race between transactions on one Postgres, every authorization rule |
| stack e2e | `just e2e` | proc mode: compose Postgres and MinIO, kind, `scarab-server` and the Depot as host processes | every feature end to end, except the GitHub App and GitHub OAuth |
| forge e2e | `just forgejo-verify` | proc mode plus a real Forgejo | webhooks, bind, clone, statuses, and each ADR-0076 forge feature as it merges |
| fault | `just chaos` | a throwaway kind cluster with the real chart at 2 servers and 2 Depot replicas | the ranked scenarios in §3 |
| security | `just security` | proc mode plus Forgejo, two orgs | probes with canary oracles (§6) |
| red-team agent | `--agentic` on the three recipes | the same stacks | finds candidate failures and writes them as replayable scenarios; gates nothing (§6, §7) |

"Everything except external SaaS" means: every feature whose path does not need GitHub.com
has a stack e2e or forge e2e case. ADR-0017's feature-acceptance rule decides when each case
is due; this ADR does not add a land-time requirement beyond it. The GitHub App and OAuth
adapters stay at the functional tier against a fake GitHub HTTP server, which is a true
external under 0017. That includes the revocation call in ADR-0076's `forge_token`.

A test runs at the cheapest tier that can see its failure. A transaction race found at the
stack tier is reproduced and pinned at the functional tier; the stack scenario stays only if
the functional test cannot see the failure.

### 2. The invariant checker is the shared oracle

One checker, in the test harness (a library plus a binary, working name `scarab-chaos`),
reads the recorded state after a test or scenario and reports every invariant that does not
hold. Every stack e2e test, fault scenario and security probe ends with a call to it. It
replaces the hand-written end-of-test asserts in `crash_resume.rs`, `happy_path.rs` and the
other stack e2e tests, so the suite gets shorter.

Its inputs:

- the event log (`events`) and the tables it projects into (`runs`, `step_runs`, `attempts`);
- the Depot's records (`depot_packs`, `depot_pack_members`, `committed`,
  `depot_fence_borrows`) and the objects in the bucket (HEAD and ranged GET);
- a Pod watcher in the harness: a kube watch on `app.kubernetes.io/managed-by=scarab`
  that writes one JSONL line per change (UID, phase, container start and finish times,
  deletion), using the `scarab.io/run`, `scarab.io/step` and `scarab.io/attempt` labels
  (`crates/scarab-executor-k8s/src/lib.rs:4236-4238`);
- Forgejo's commit statuses, when the scenario uses Forgejo.

No product change is needed for any of these.

| # | Invariant | Checked from | Past bugs it would have caught |
|---|---|---|---|
| I1 | **(a)** At most one Pod UID per Attempt fence. **(b)** Attempt N+1's step container starts only after Attempt N's container ended, or after N was journalled `Lost` or superseded. | Pod watcher, event log | git-bug `fd6e6d4` (orphaned Pod of a superseded Attempt), `4c4ab5a1` (a stopped launch not torn down), ADR-0021's double effect. The bound in (b) is an owner question. |
| I2 | A Step that was ever `Skipped` has no `AttemptStarted` and no Pod. | event log, Pod watcher | No recorded escape. Kept because it is one query over data the checker already reads, and `when:` exclusion is a promise users rely on. |
| I3 | Every Run, including rows with `creating = TRUE`, is terminal at the end of the scenario's quiesce bound. | `runs` | the red `crash_resume`; git-bug `9cdf38e` (Pod exits 0, step never settles); git-bug `e140121` (a 20-30 s Depot outage dead-letters runs); `77e6c429`, `c2570c83` and git-bug `ccbf7fe` (creation left half done) |
| I4 | Every Succeeded Step's `output_snapshot` closure is in committed packs whose objects exist and whose byte ranges read back. A NULL snapshot passes. Membership may sit in another fence's pack through `depot_fence_borrows`. Run before retention TTLs expire. | Depot tables, bucket | git-bug `afb13c2` (drain scatter, 422), git-bug `ec294b7` (unrecorded cross-fence dependency), git-bug `022aec8` (ghost pack session) |
| I5 | Folding the event log with `scarab-engine`'s fold gives the `runs` and `step_runs` rows. | event log, tables | git-bug `586187a` (step terminal CAS and its journal entry in separate writes; fixed by `edc5674c`), `89505ace` (run transition, journal entry and status post in one commit) |
| I6 | No two Runs in one concurrency group have step Pods running at the same time. | Pod watcher, `runs.concurrency_group` | git-bug `5120335` (cross-tenant slot key), `ad7a4950` (a cancelled holder holds until its unit is gone), `deed1499` (auto-cancel writes the Run's cancel intent) |
| I7 | At most one `GateResolved` per (Step, Attempt). | event log | `202ee19c` (atomic fenced gate waits, gate reject) |
| I8 | After quiesce, no Pod, Secret, Service or NetworkPolicy carries a terminal Run's label. Once ADR-0076 step 3 merges, this includes the per-unit forge-token Secret. | kube API | git-bug `fd6e6d4`, `15b0afe0` and `fbde745a` (services not torn down), `36a533da` and `4c4ab5a1` (failed launches), git-bug `e10cf7e` (sidecar Pod lingers after a terminal poll) |
| I9 | Every terminal Attempt's log bytes are in object storage (ADR-0067 §3). | `attempts`, bucket | No recorded escape at stack grain. Kept because logs moved off the control plane (ADR-0063) and only the stack exercises that path. |
| I10 | No `committed = FALSE` pack row older than the linger, and no committed pack row without its object. | Depot tables, bucket | git-bug `ad79c90` (multipart debris, packs never deleted), git-bug `022aec8` |
| I11 | The last commit status Forgejo holds for a SHA matches the Run's terminal state. | Forgejo API | `29dbd64e` (three Runs racing over one commit status), `b918b345` (a stale generation's status post) |

**The quiesce bound** is computed per scenario, not guessed: lease TTL (30 s) + the
workload's step time + `WS_DRAIN_ESCALATION_MS` (5 minutes,
`crates/scarab-executor-k8s/src/lib.rs:3934`) for any scenario that faults the Depot. A
checker without the formula reports liveness failures that are not bugs.

**Ordering.** `events.seq` is a `BIGSERIAL` taken at insert, not at commit, and `at` comes
from each replica's clock. The checker orders events by `seq` only within one Run. Cross-Run
ordering, where a scenario needs it, uses `pg_xact_commit_timestamp(xmin)`, with the fault
cluster's Postgres started with `track_commit_timestamp = on`.

**What changed from the proposal.** "Every minted token is revoked" becomes I8's Secret
check, which holds for Forgejo too. The revocation call itself only happens in GitHub App
mode, so it is a functional test against the fake GitHub server. "At most one slot holder"
becomes I6's Pod-overlap form, which is the property users want ("serialize prod"). Plugin
gate ownership moves to a functional test of the `PluginGateResolver` role when ADR-0076
step 4 merges.

**Known slow paths are not failures.** At Depot `replicaCount > 1`, cache restores almost
always miss (git-bug `12e2f6b`). The checker treats a cache miss as correct; whether that
issue blocks the 2+2 topology is an owner question.

### 3. Fault scenarios, ranked

A scenario is kept only while it names real bugs it would have caught, or a finding
predicted from the code. Each is a seeded, replayable file: workload, faults and timing,
quiesce bound.

| Rank | Scenario | Where | Precedent | Notes |
|---|---|---|---|---|
| 0 | `crash_resume` green, then kept green | `just e2e` | red on main, 2026-10-26 | Prerequisite for the rest. Moves onto the checker. |
| 1 | Two Depot replicas; delete one Pod (grace 0) mid-drain, and in a second run mid-feed | `just chaos` | git-bugs `afb13c2`, `e140121`, `022aec8`, `ad79c90`, `ec294b7` | Highest yield. A toxiproxy reset in front of MinIO mid-multipart folds in here. |
| 2 | Two server replicas at steady state, then SIGKILL one mid-Run | `just chaos` | git-bugs `9cdf38e`, `98ea804` (artifacts uploaded, never indexed); `51d53707` (results ingest for a non-frontier Attempt); readoption | Part of the yield comes from running two replicas at all, with no fault. |
| 3 | Postgres partition and ambiguous commit through toxiproxy (`timeout`, `reset_peer`, `limit_data` to cut a COMMIT before its ack) | `just chaos` | none: the fault has never been injected | Separate proxies for the control plane and the Depot, which also connects to Postgres (ADR-0067 §2). Asks: is a retried `commit_step_transition` after an unknown commit a no-op; does the sqlx pool recover; do tick errors stay contained (ADR-0059). |
| 4 | Leader paused (SIGSTOP) past the lease TTL, then resumed | proc mode, in the harness | `e85f927e`, `33a67053` (plugin-runtime lease outlived by a pass) | In helm mode the server is PID 1 with no init (`docker/server/Dockerfile:92`), so it ignores SIGSTOP sent inside its PID namespace. Proc mode needs nothing new. Checked with I1 and I5. |
| 5 | A concurrency group under parallel dispatch, then kill the holder's server | `just chaos` | git-bug `5120335`, `ad7a4950`, `deed1499` | Only the k8s-visible part: the old holder's Pods still terminating while the new holder's start (I6). |
| 6 | Kill mid-Run-creation through a failpoint | `just chaos` | `77e6c429`, `c2570c83`; git-bug `ccbf7fe` | Predicted finding: the Run stays `creating = TRUE` (Context). Fix and pin it at the functional tier first; the stack scenario stays as a failpoint regression. |

**Cut:**

- **The gate release-versus-reject race** (`202ee19c`, `4d8f671f`): a CAS race on one
  Postgres. A functional test with two concurrent API calls is deterministic and runs in
  milliseconds.
- **Plugin gate ownership:** an authorization rule; functional tier (§2).
- **Clock skew:** `CLOCK_REALTIME` has no namespace, so every Pod on one kind node sees the
  same clock; per-Pod skew needs Chaos Mesh TimeChaos or `libfaketime` in the image. Leases
  use Postgres `now()`, and the past clock bugs (`9f28b666`, `9d9daf1b`) were fixed and
  pinned with `FakeClock`, which is the right tier.
- **Argo CD in the fault cluster:** 1 to 1.5 GiB and nothing for the durability invariants.
  A separate, rarely run `just argocd-verify` (core install, no UI, no Dex) covers
  `wait-live` once ADR-0076 step 4 merges.
- **Chaos Mesh and Litmus:** privileged daemons and 0.5 to 1 GiB on a laptop VM. A small
  kube-rs nemesis (Pod delete with grace 0), host signals and toxiproxy cover the faults
  above.

**The nemesis** is code in the harness: kube-rs Pod deletes, SIGKILL and SIGSTOP/SIGCONT on
host processes, and a toxiproxy client. A scenario run is driven from a seed and writes its
actions to a scenario file, so a failing run replays without the original timing luck.
Random-timing scenarios mean something only over repeats; a local nightly loop over the
kept set is more useful than one pass.

### 4. Where `just chaos` runs

`just chaos` creates a throwaway kind cluster, `scarab-chaos`, with its own kubeconfig file
(the pattern of `deploy/local-proc/up.sh` and `deploy/local-proc/plugin-verify.sh:45-55`).
It installs the real chart with `replicaCount: 2` and `workspace.replicaCount: 2`, plus
Postgres (with `track_commit_timestamp = on`), MinIO and toxiproxy, and Forgejo when a
scenario checks I11. It deletes the cluster afterwards unless asked to keep it, as
`just forgejo-verify keep` does.

- It never runs in colima's `scarab` namespace, which holds the dogfood stack and its data.
- It never uses the ambient kubeconfig. Every command gets the cluster's own file, and the
  recipe checks that file's server URL and CA against the values recorded at creation.
- Binaries are built on the host and copied into a thin image. Building the server image
  inside a 2-CPU colima VM takes minutes per change.
- The real chart gives what proc mode cannot: Services, the Depot StatefulSet and its PVCs,
  and ClusterIP scatter across replicas.

Scenario 0 and scenario 4 run in proc mode, where the harness owns the server process, as
`crash_resume.rs` does today.

### 5. Failpoints

The windows for scenario 6 and for a kill "after the Pod exists, before the Attempt is
recorded" are milliseconds wide. An external kill rarely lands in them; a `thread::spawn`
race in this repo missed its window 64 times out of 64. So Scarab gains a few named
failpoints through the `fail` crate, behind a cargo feature `failpoints` that is off by
default and absent from release images:

- after Pod create, before the Attempt record;
- between `begin_run_creation` and `finish_run_creation`;
- after a pack is sealed, before its `committed` row;
- after the S3 multipart complete.

The release image build checks that the feature is off. New points are added only with a
scenario that uses them. These are the "deterministic simulation later" that ADR-0017's
ports allow for, applied to named points instead of a framework. This puts test-only code
paths in product crates, so it is an owner question.

### 6. The red-team agent

The agent is not a test tier. It is a red team at stack grain, run by hand, and its only
output is a candidate scenario file. It gates nothing, and it never decides whether an
invariant holds: the checker does.

**A finding counts** only when at least 3 of 5 agent-free replays of its scenario file
fail the checker. Findings are deduplicated by invariant and code site. A finding that
counts becomes a deterministic test at the cheapest tier that shows it (§1), under 0017's
grow-from-bugs rule.

**It starts with `just security --agentic`**, because security probes have oracles that
give a clear yes or no:

- **Canary secrets** seeded in the secret store; any appearance in step logs, the event
  log or object storage is a finding.
- **A canary command** that writes a marker file if injected text ever executes; the target
  is templating (`${{ }}`, `event.*` once ADR-0076 merges) and crafted launch parameters.
- **Forged and replayed webhooks:** a Run created, or a 2xx, is a finding.
- **Cross-org reads and writes** between two seeded orgs: any 2xx on the other org's
  resources is a finding.
- **Fork pull requests:** a secret or a write token reaching a fork Run is a finding.

The deterministic `just security` runs a fixed set of these probes; the agent extends them.

**The second use is confirming a code red-team hypothesis live.** The agent gets one
hypothesis ("a window between X and Y at file:line") and tries to show it on the stack.
That is a bounded search with a named target.

**`just chaos --agentic` and `just e2e --agentic` come later, and only if the security
mode's yield justifies them** (findings that pass the replay rule, per run and per dollar).
For distributed faults the act-observe cycle takes minutes (30 s lease, 5-minute drain
escalation), so 100 turns explore a few dozen interleavings, where a seeded random nemesis
explores hundreds at no token cost. The pass-or-fail signal depends on a quiesce bound, and
an agent will chase timeouts.

**Tools.** Each mode has a small typed menu. The menu calls the same nemesis and probe code
as the deterministic recipes, so the agent's action log is a scenario file. Tools return
summaries and diffs, never full logs: raw log dumps can triple the token cost, and logs are
untrusted input.

### 7. Agent safety

These are requirements of the agentic mode, not guidance. A run that cannot meet one does
not start.

1. **The agent process holds no credentials.** It runs with `HOME` set to its run
   directory, and cannot read `~/.kube`, `~/.aws`, `~/.ssh`, `~/.config/gh` or
   `~/.gitconfig`. The preferred form is a container with only the repository (read-only)
   and the run directory mounted; the lighter form is the Claude Code sandbox with
   deny-read rules for those paths.
2. **Only the tool server holds a credential**, and it is minted per run for the throwaway
   cluster: a namespace-scoped ServiceAccount token from TokenRequest, one-hour expiry, a
   Role that allows `pods` get, list, watch and delete and `pods/log` get. The tool server
   pins the API server URL (127.0.0.1 and the kind port) and the CA fingerprint recorded at
   cluster creation, and refuses to start on any mismatch. It never reads the admin
   kubeconfig after setup. For `security`, it also holds Scarab API tokens for the two
   seeded orgs and a token for the throwaway Forgejo.
3. **No shell.** The tools are a local stdio MCP server, a subcommand of the harness
   (`scarab-chaos mcp`). Claude Code runs headless (`--print`) with `--mcp-config` and
   `--strict-mcp-config`, its built-in tools turned off with `--tools`, and only the
   `mcp__chaos__*` tools allowed. These flags exist in the installed CLI (checked
   2026-10-31); the recipe checks them again before each run. There is no "run command"
   tool.
4. **Git goes only to the throwaway Forgejo.** The tool server runs git with
   `GIT_CONFIG_GLOBAL=/dev/null` and `GIT_CONFIG_NOSYSTEM=1`, and fixes the remote URL
   itself; no tool takes a remote as an argument.
5. **Network:** the agent process reaches only the model API. The tool server reaches only
   127.0.0.1 and the kind network.
6. **Caps:** wall clock (60 minutes by default), spend (`--max-budget-usd`) and a maximum
   action count, all set by the recipe.
7. **Record:** every tool call and result goes to `actions.jsonl` in a gitignored run
   directory. That file is the replayable scenario.
8. **Logs, Forgejo content and API responses are untrusted input.** The fixed menu bounds
   what a prompt injection in them can do.

### 8. CI

Nothing in this ADR runs per pull request. Actions minutes are limited, and stacked pull
requests already spend them (git-bug `ba1cab5` proposes label-based skipping). The checker
runs inside the existing weekly `e2e.yml`, because it replaces that suite's asserts. After
`crash_resume` has been green there for several weeks, at most one fault scenario (rank 1,
the Depot replica kill) may join the weekly workflow. The agentic mode never runs in CI.

### 9. How this fits ADR-0017

- **It is the durability exception, enlarged by evidence.** Every kept scenario names the
  bugs it would have caught, or a predicted finding. A scenario with neither stays out.
- **The checker is the shared oracle, not a framework.** It replaces hand-written asserts,
  and the suite gets shorter.
- **It stays classical:** no mocks at the stack tiers; real Postgres, real Kubernetes, real
  Depot, real Forgejo. The GitHub App and OAuth are true externals and stay faked.
- **Races go down a tier.** A race between transactions on one Postgres belongs to the
  functional tier.
- **The agent feeds grow-from-bugs.** Its only output is a test.
- **Failpoints are the deferred simulation**, limited to named points behind a feature.

## Consequences

- `crash_resume` has to be green before any new fault scenario is written. Until then this
  ADR produces the checker and nothing else at the stack tier.
- The checker reads the event payloads and the Depot tables directly, so a schema change
  there also changes the checker. That is the price of an oracle that does not trust the
  API it checks.
- Scarab gains new dependencies: the `fail` crate (behind a feature), toxiproxy in the fault
  cluster, and a kube watch in the harness. Product crates gain failpoint macros, if the
  owner accepts them.
- **Laptop resources.** colima is 2 vCPU and 4 GiB today. The fault cluster's working set
  without Argo CD is about 5 to 7 GiB (kind control plane 0.7-1 GiB; two servers 0.5-1 GiB;
  two Depot replicas 1-2 GiB; Postgres, MinIO and toxiproxy 0.6-0.8 GiB; step Pods 0.5-1.5
  GiB; Forgejo 0.3-0.5 GiB). colima needs about 6 vCPU and 10 GiB for fault runs, and
  colima's own k3s, which runs the dogfood stack, is stopped during them. That leaves about
  6 GiB on a 16 GiB host, not enough for a workspace cargo build at the same time: build
  first, then run.
- **Wall time.** Cluster and image setup 3 to 6 minutes with host-built binaries (about 15
  with an image built inside colima); 3 to 8 minutes per scenario including its quiesce
  bound; 45 to 90 minutes for the kept set.
- **Agent cost.** About 100 turns with context growing to about 150k tokens: 10 to 15 MTok
  of input (mostly cache reads), 0.5 to 1.5 MTok uncached or cache writes, 0.1 to 0.2 MTok
  of output. At Claude Opus 5.5 rates ($4 in, $20 out, $0.20 cache read per MTok, checked
  2026-10-31) that is about $8 to $20 per run; Claude Sonnet 5.5 ($2, $10, $0.20) about $5
  to $12. On a Claude Code subscription it is usage quota instead of dollars.
- **Triage.** A red fault scenario is a timing bug until shown otherwise, and costs a human
  hour or more to read. The replay rule and the checker keep agent findings from adding to
  that.
- Scenario 6's predicted finding means git-bug `ccbf7fe` gets fixed and pinned at the
  functional tier before the scenario is written.

## Alternatives considered

- **Run the fault tier in colima's `scarab` namespace.** Rejected: it holds the dogfood
  stack and its data, and the recipes there trust the current context by name.
- **Use the ambient kubeconfig with a current-context check.** Rejected: a context name is
  set by the user, the check is a TOCTOU window that `kubectx` can cross, and the merged
  config keeps production contexts one flag away.
- **Chaos Mesh or Litmus.** Rejected: privileged daemons and memory the laptop VM does not
  have, for faults a small nemesis and toxiproxy already cover.
- **A NetworkPolicy to partition Postgres.** Rejected: it does not cut established
  connections tracked by conntrack. Toxiproxy does, and can also cut a COMMIT before its
  ack.
- **Elle or Knossos as the checker.** Rejected: they check client-observed transactional
  histories (list-append, registers). Scarab's history is a journal the server writes. We
  take Jepsen's structure (generator, nemesis, checker over a recorded history) and its
  rule that the checker, not the test author, decides.
- **A full deterministic simulation (FoundationDB, TigerBeetle's VOPR, Antithesis).** Not
  now: the cheap version exists (`FakeClock`, `InMemoryDb`, `FakeExecutor`,
  `durability_guard.rs`), and Antithesis is commercial and sized for larger systems. A
  seeded interleaving explorer over two `Scheduler` instances on one Postgres may find more
  races than the stack fault tier at lower cost; it would be its own decision.
- **Random external kills for the creation and launch windows.** Rejected: the windows are
  milliseconds wide; failpoints land in them every time.
- **Clock skew per Pod with `libfaketime`.** Rejected: it changes the image under test, and
  the clock bugs found so far are covered with `FakeClock`.
- **Agent-driven chaos first.** Rejected: slow act-observe cycles, a quiesce-dependent
  oracle and a seeded random nemesis that explores more interleavings for free. Security
  probing has clear oracles and goes first.
- **An agent with a shell, or scripts behind an allowlisted Bash prefix.** Rejected:
  argument and metacharacter injection make an allowlisted prefix a weak boundary, and a
  shell reaches every credential on the machine. A stdio MCP server is less code and a
  narrower interface.
- **Letting the agent judge findings.** Rejected: an agent will report timeouts as bugs. The
  checker decides, through agent-free replays.
- **Running fault scenarios per pull request.** Rejected: Actions minutes are limited, and a
  red timing test on every push buys no signal.
- **Keeping "every minted token is revoked" and "at most one slot holder" as stack
  invariants.** Rejected (§2): the first has nothing to check on a Forgejo-only stack, the
  second is enforced by a primary key.

## Implementation horizon

1. **`crash_resume` green, and the checker.** Diagnose and fix the 26 October failure; build
   the checker with the invariants that main can check (I1-I7, I9-I11, I8 without the
   forge-token Secret) and the Pod watcher; move every stack e2e test onto it and delete the
   asserts it replaces. Restore the weekly `e2e.yml` to green.
2. **Depot kill (rank 1).** `just chaos` with the throwaway cluster, the real chart at 2+2,
   the kube-rs nemesis, seeded scenario files and replay; the MinIO reset mid-multipart.
3. **Server kill (rank 2), leader pause (rank 4) and the concurrency holder kill (rank 5).**
4. **Postgres faults (rank 3)** through toxiproxy, with separate proxies for the control
   plane and the Depot.
5. **Failpoints and the creation kill (rank 6),** after git-bug `ccbf7fe` is fixed and
   pinned at the functional tier.
6. **`just security` and `just security --agentic`** with §7's safety design: the MCP tool
   server, the per-run credential, canary secrets, the canary command, the replay rule.
   Hypothesis confirmation uses the same tool server.
7. **Later, if step 6's yield justifies it:** `just chaos --agentic` and `just e2e
   --agentic`.

Alongside these steps: each ADR-0076 step extends `just forgejo-verify` when it merges (the
clone library, `forge_token` and I8's forge-token Secret, yank), and step 4 adds
`just argocd-verify` for `wait-live`.

## Open questions for the owner

1. **I1(b):** may Attempt N+1's container start while Attempt N's container is still
   terminating after a `Lost` verdict? If yes, within what bound?
2. **Runs left with `creating = TRUE`:** should a sweeper fail Runs whose creation is older
   than a bound (git-bug `ccbf7fe`), and is a webhook redelivery for such a Run
   deduplicated away or does it create a second Run?
3. **Failpoints in product code**, behind a cargo feature and absent from release images:
   acceptable?
4. **git-bug `12e2f6b`** (cache restores at Depot `replicaCount > 1` almost always miss): an
   accepted slow path that the checker allows, or a blocker for the 2+2 topology?
5. **Where fault runs happen:** a throwaway kind cluster (proposed) or colima's k3s, and may
   colima be resized to 6 vCPU and 10 GiB for them?
6. **Agent budget:** a monthly cap, and does the agentic layer start with `security`
   (proposed) rather than `chaos`?
7. **The SIGSTOP pause scenario:** in scope? It was not in the original proposal.
