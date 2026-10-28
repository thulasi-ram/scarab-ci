# Positioning & messaging

How we describe Scarab in public copy (README, docs, taglines, posts). The
architecture is decided in [ADR-0001](adr/0001-ci-as-durable-execution.md).
This doc is about the wording, and about not claiming more than the code proves.

It is based on a code audit (the durability tests were actually run) and a
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
- **Not fully proven yet.** The part our code proves (Postgres crash/resume and
  exactly-once, in [`crash_resume.rs`](../crates/scarab-db-postgres/tests/crash_resume.rs),
  which runs against real Postgres) is the part the durable workflow engines already have. The
  part that would set us apart (live-Pod re-attach, execution replay as a
  feature) is `#[ignore]`d or partial.

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
forge-native domain model is proven **against a fake forge**. Live GitHub I/O
and live-k8s execution are the remaining work (see Claims we don't make).

## One-liners (ranked)

1. **"CI that treats your run as state, not a process."** Recommended lead. It
   describes the durable core without saying "durable", it is concrete, and it
   is the one idea the job-runner CIs don't share.
2. **"A modern CI engine for Kubernetes — forge-native, on a durable core."**
   Recommended descriptor and H1. It uses the "modern CI engine" framing and
   ties it to the two real properties (forge-native and durable). Both are
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
- ❌ "Durable execution **on Kubernetes**" or live-Pod re-attach. No live-k8s
  proof exists; the crash test's executor is a fake.
- ❌ "**Unlike** Argo/Tekton/Temporal, we're durable." They are durable too, so
  it makes us look uninformed. Never compare ourselves with that group on
  durability.
- ❌ "Time-travel" or "replay" as a shipped feature. It follows from the event
  log, and there is no query endpoint yet. At most: "an append-only event log
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
the orphaned-build problem that Woodpecker and Drone operators actually hit.
Show respect and say plainly where we differ; never take a swipe.

## Claims we don't make (the code or the market won't back them)

1. ❌ "Works with GitHub today." All 8 outbound `ForgePort` methods are
   `unimplemented!()` (`scarab-forge-github/src/lib.rs:177-226`). Say "designed
   forge-native; live GitHub integration in progress."
2. ❌ Any end-to-end durability-on-Kubernetes or live-Pod re-attach claim
   (live-k8s tests are `#[ignore]`d; the slice-1 umbrella issue is still open
   for this).
3. ❌ Durable execution as a selling point against Argo/Tekton/Temporal/Kestra.
4. ❌ Time-travel / replay as a feature.
5. ❌ Indefinite gates as a differentiator.
6. ❌ Live fork-PR trust / OAuth enforcement against a real forge (`get_permissions`
   is `unimplemented!()`; the domain lockout logic is real and tested, so claim that).
7. ❌ "Named-result capture works on Kubernetes." The ingest API and wiring
   exist; the egress sidecar image does not.
8. ❌ "Continuously tested." No CI runs the Rust suite yet, and the core
   durability tests skip silently and pass without `SCARAB_TEST_DATABASE_URL`.

**Keep a visible Status line, and don't apologise for it.** Durable engine:
proven. Forge-native domain model: proven against a fake forge. Live GitHub I/O
+ live-k8s execution: in progress. Being upfront in that one line is what makes
every other claim believable.

## Voice

Write like a developer tool: concise, technically precise, witty only when it
fits. No corporate language ("enterprise-grade", "seamless", "supercharge",
"next-generation"), no exclamation marks, no "revolutionary". Prefer
understatement to superlatives. If a sentence would embarrass you in a code
review, cut it.

| Prefer | Avoid |
|---|---|
| modern CI engine; forge-native, durable core | durable CI (bare adjective) |
| treats your run as state, not a process | resilient / robust / bulletproof |
| resumes from the last completed step | "never fails" |
| designed forge-native; live GitHub in progress | works with GitHub (today) |
| built AI-first (development pace) | AI-native / AI-powered CI |
| inspired by Woodpecker's limits | "unlike Woodpecker" (swipe) |
