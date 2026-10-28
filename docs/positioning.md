# Positioning & messaging

How we describe Scarab in public copy (README, docs, taglines, posts). The
architecture is decided in [ADR-0001](adr/0001-ci-as-durable-execution.md).
This doc is about the wording, and about not claiming more than the code proves.

It is based on a code audit (we ran the durability tests) and a
review of nine competitors. The conclusion comes first.

## Conclusion: "durable execution" is how it's built, not the headline

Our first idea, "durable CI", makes a weak headline, and not only because
"durable" is a generic adjective. A headline claim has to pass three tests, and
durable execution passes none of them:

- **Not differentiated.** Among k8s-native engines, Scarab's own category,
  durable execution is table stakes. Argo and Tekton reconcile run state from
  etcd and survive a controller crash without re-running completed work; Temporal
  is the standard durable-execution engine (replay + Reset); Kestra is DB-backed
  with Pause/Replay. Leading with "durable" in that market sounds like we are
  rebuilding Temporal, late.
- **Not visible to most users.** Most users are on hosted SaaS (GitHub.com,
  GitLab.com, CircleCI) and never see a control plane crash. "Our control plane
  resumes after a restart" answers a question they have never asked.
- **Only partly proven so far.** The part our code proves (Postgres crash/resume and
  exactly-once, in [`crash_resume.rs`](../crates/scarab-db-postgres/tests/crash_resume.rs),
  which runs against real Postgres) is the part the durable workflow engines already have. The
  part that would set us apart is not proven: the control-plane crash test with a real Pod in
  flight (`crates/scarab-e2e/tests/crash_resume.rs`, weekly in `e2e.yml`) doesn't pass yet, and
  execution replay is not a feature.

So we keep durability but stop leading with it. It guides the engineering and is
a real supporting claim, but it is not the headline.

## The gap in the market

CI tools fall into two groups, and neither does the whole job:

| Group | Examples | Durable engine | Forge-native CI product |
|---|---|---|---|
| **Workflow engines** | Argo, Tekton, Temporal, Kestra | ✅ yes | ❌ you build it (webhooks, in-repo config, PR checks, identity, secrets, DSL, UI) |
| **Forge-native CIs** | GitHub Actions, GitLab, Woodpecker, Drone | ❌ job runner (restart → orphaned/lost run) | ✅ batteries included |
| *Edge cases* | Concourse (partly durable, weak forge integration); Tekton + Pipelines-as-Code (the closest overlap with Scarab) | ~ | ~ |

**Scarab aims to be both:** a forge-native CI with the batteries included, on a
control plane whose core is a durable, crash-safe state machine. The
combination (*cohesion*), rather than durability alone, is the position we can
defend, because it is an architecture claim that can be checked rather than a
benchmark claim.

The caveat that keeps this defensible: the durable engine is proven, and the
forge-native domain model is proven **against a fake forge**. The GitHub and
Forgejo adapters and Pod execution on Kubernetes are implemented and tested (a
kind cluster in CI, real forges by hand), but not yet hardened, and crash/resume
with a live Pod doesn't pass yet (see Claims we don't make).

## One-liners (ranked)

1. **"CI that treats your run as state, not a process."** Recommended lead. It
   describes the durable core without saying "durable", it is concrete, and it
   is the one idea the job-runner CIs don't share.
2. **"A CI engine for Kubernetes — forge-native, on a durable core."**
   Recommended descriptor and H1. It says what Scarab is (a CI engine for
   Kubernetes) with no adjective, and ties it to the two real properties (forge-native and durable). Both are
   architecture claims, not benchmarks, so it does not overclaim.
3. **"Forge-native CI, on an engine that doesn't forget."** Witty, and links the
   two halves. Good for a post headline.
4. **"Your control plane can crash. Your build won't notice."** The punchiest
   and easiest to demo, but **for self-hosted/on-prem audiences only**. It falls
   flat for hosted-SaaS readers, who never see a control plane fail.

## How to use "durable": do and don't

**Do** (all backed by code):
- "Runs survive a control-plane restart and **resume from the last completed
  step** — no re-running work that already succeeded." (proven: `crash_resume.rs`)
- "**Exactly-once step execution**, enforced by optimistic-concurrency guards and
  fencing tokens on a transactional outbox."
- "Built on a **durable execution engine** (the DBOS/Temporal pattern, applied to
  CI)." Use it as a supporting line, never the H1.
- Self-hosted comparison: "Where Drone and Woodpecker orphan in-flight builds on a
  server restart (stuck `running`, dangling containers — woodpecker-ci#3427,
  drone#2189), a Scarab run continues where it left off." Cite the issues; the
  links make the point better than any joke.

**Don't:**
- ❌ "Durable execution" as a bare H1 or a one-word pitch.
- ❌ "Durable execution **on Kubernetes**" or live-Pod re-attach. The Postgres
  crash test's executor is a fake, and the stack-level crash test with a real Pod
  (`scarab-e2e` `crash_resume`) doesn't pass yet.
- ❌ "**Unlike** Argo/Tekton/Temporal, we're durable." They are durable too, so
  it makes us look uninformed. Never compare ourselves with that group on
  durability.
- ❌ "Time-travel" or "replay" as a shipped feature. It follows from the event
  log, but the only endpoint streams the log as it is (`GET /v1/runs/{id}/events`);
  nothing reconstructs past state or replays a run. At most: "an append-only event log
  that makes execution history inspectable."
- ❌ "Indefinite approval gates" as a differentiator. GitLab, Buildkite, CircleCI
  and GitHub already suspend cheaply.

**Scope rule:** tie every durability sentence to the control plane or the run,
never to the Kubernetes execution, and never compare it with the other durable
engines.

## The AI angle: only where it's true

We mention AI lightly, and only where the architecture backs it up. Two claims
hold: (a) long-running or autonomous pipelines that **wait cheaply**, because
durable gates hold a suspended run at ~zero compute cost (real in code); (b) the
**append-only event log** as an auditable record of what an automated pipeline
did. Acceptable wording:

> "As pipelines get longer and more autonomous (agents, multi-day workflows,
> long review gates), 'the job died, click re-run' is no longer good enough.
> Scarab holds a suspended run at near-zero cost and resumes where it paused."

The other true AI point is about how Scarab is built, not what it does: a small
team writing ambitious systems software (a Rust durable-execution engine) at a
pace that used to take a much larger team. This is the Woodpecker "speed of
development" point. Keep it about development pace and never imply an AI
product feature.

**Never** write "AI-native CI", "built for the age of AI", "LLM-powered", or any AI
capability; there is none in the code. If a sentence can't be tied to cheap
waiting or the auditable log, drop the AI angle instead of forcing it in.

## The Woodpecker lineage (mention it respectfully)

Scarab's design owes a lot to [Woodpecker](https://woodpecker-ci.org/), a lean,
forge-native CI without enterprise overhead. Woodpecker's limits shaped it just
as much: the many backends it has to support, and the pace a volunteer project
can keep up. Supporting only Kubernetes drops the extra backends on purpose
([ADR-0005](adr/0005-tenancy-and-k8s-only.md)), and the durable core addresses
the orphaned-build problem that Woodpecker and Drone operators hit.
Show respect and say where we differ; never take a swipe.

## Claims we don't make (the code or the market won't back them)

1. ❌ "Works with GitHub" with no qualifier, or "production-ready on GitHub". The
   adapter is implemented: every outbound `ForgePort` method GitHub needs makes real GitHub API calls (webhook
   registration is a no-op, because the App's subscription is the hook)
   (`crates/scarab-forge-github/src/lib.rs`), and the live demo takes its webhooks
   from a GitHub App. But its live contract test (`tests/contract_live.rs`) runs only
   by hand, and no CI job talks to real GitHub. Say "GitHub and Forgejo adapters
   implemented; not yet hardened against live forges."
2. ❌ Any end-to-end durability-on-Kubernetes or live-Pod re-attach claim. The
   executor's live-cluster tests, re-attach after relaunch included, pass on kind
   (`kind.yml`), but the stack-level crash/resume test with a real Pod in flight
   doesn't pass yet.
3. ❌ Durable execution as a selling point against Argo/Tekton/Temporal/Kestra.
4. ❌ Time-travel / replay as a feature.
5. ❌ Indefinite gates as a differentiator.
6. ❌ Live fork-PR trust / OAuth enforcement against a real forge. `get_permissions`
   is implemented for GitHub, but no test exercises it against real GitHub; the
   domain lockout logic is real and tested, so claim that.
7. ❌ "Named-result capture is hardened on Kubernetes." The egress sidecar image
   (`docker/sidecar/`) exists and a kind test captures a named result end to end
   (`results_sidecar_captures_a_named_result_end_to_end`), so claim it scoped to
   that: tested on a kind cluster.
8. ❌ "Every change runs the full test suite." `ci.yml` runs the Rust suite against
   real Postgres on every push to main and on PRs that touch Rust (in CI a skipped
   Postgres test fails the job), but `kind.yml` runs the live-cluster tests only for
   changes to the execution path, and the full-stack e2e tier runs weekly.

**Keep a visible Status line, and don't apologise for it.** Durable engine:
proven. Forge-native domain model: proven against a fake forge. Forge adapters
and Kubernetes execution: implemented and tested, not yet hardened. Being upfront
in that one line is what makes every other claim believable.

## Voice

Write like a developer tool: concise, technically precise, witty only when it
fits. No corporate language ("enterprise-grade", "seamless", "supercharge",
"next-generation"), no exclamation marks, no "revolutionary". Prefer
understatement to superlatives. If a sentence would embarrass you in a code
review, cut it.

| Prefer | Avoid |
|---|---|
| CI engine for Kubernetes; forge-native, durable core | durable CI (bare adjective) |
| treats your run as state, not a process | resilient / robust / bulletproof |
| resumes from the last completed step | "never fails" |
| GitHub and Forgejo adapters implemented, not yet hardened | works with GitHub (no qualifier) |
| built AI-first (development pace) | AI-native / AI-powered CI |
| inspired by Woodpecker's limits | "unlike Woodpecker" (swipe) |
