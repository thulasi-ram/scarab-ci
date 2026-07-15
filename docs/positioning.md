# Positioning & messaging

How we talk about Scarab in public copy (README, docs, taglines, posts). The
*architecture* is settled in [ADR-0001](adr/0001-ci-as-durable-execution.md);
this is about the *words* — and about not overclaiming what the code proves.

Grounded in a code audit (the durability tests were actually run) + a market
scan of nine competitors. Bottom line below.

## The verdict: "durable execution" is the architecture, not the headline

The original instinct — "durable CI" — is a weak banner, and the reason is
sharper than "it's a generic adjective." A headline claim has to clear three
bars. Durable execution clears none as a *headline*:

- **Not differentiated.** In Scarab's own category — k8s-native engines — durable
  execution is table stakes. Argo and Tekton reconcile run state from etcd and
  survive a controller crash without re-running completed work; Temporal is the
  canonical durable-execution engine (replay + Reset); Kestra is DB-backed with
  Pause/Replay. Leading with "durable" there reads as *reinventing Temporal,
  later*.
- **Not perceptible.** The hosted-SaaS majority (GitHub.com, GitLab.com, CircleCI)
  never watch a control plane crash. "Our control plane resumes after a restart"
  answers a question they've never asked.
- **Not fully provable (yet).** The half our code *proves* — Postgres crash/resume,
  exactly-once ([`crash_resume.rs`](../crates/scarab-db-postgres/tests/crash_resume.rs),
  runs against real Postgres) — is exactly the commodity half. The differentiated
  half (live-Pod re-attach, execution replay as a feature) is `#[ignore]`d or
  partial.

So: **keep durability, demote it.** It's the engineering north star and a real
proof point — just not the marquee.

## The real gap in the market

The CI world splits into two camps, and neither covers the whole job:

| Camp | Examples | Durable engine | Forge-native CI product |
|---|---|---|---|
| **Workflow engines** | Argo, Tekton, Temporal, Kestra | ✅ yes | ❌ you build it (webhooks, in-repo config, PR checks, identity, secrets, DSL, UI) |
| **Forge-native CIs** | GitHub Actions, GitLab, Woodpecker, Drone | ❌ job runner (restart → orphaned/lost run) | ✅ batteries included |
| *Edge cases* | Concourse (partial-durable, weak forge citizen); Tekton + Pipelines-as-Code (the real overlap) | ~ | ~ |

**Scarab aims to be both at once:** the batteries-included feel of a forge-native
CI, on a control plane whose core is a durable, crash-safe state machine. That —
*cohesion*, not durability alone — is the defensible position, because it's an
architecture claim (checkable) rather than a benchmark claim.

Honest caveat that keeps it defensible: the *durable engine* is proven and the
*forge-native domain model* is proven **against a fake forge** — the live GitHub
I/O and live-k8s execution are the remaining work (see Red lines).

## The one-liner (ranked)

1. **"CI that treats your run as state, not a process."** — recommended lead.
   Captures the durable core without saying "durable"; concrete; the one idea the
   job-runner CIs genuinely don't share.
2. **"A modern CI engine for Kubernetes — forge-native, on a durable core."** —
   recommended descriptor/H1. The "modern CI engine" framing, anchored in the two
   real substances (forge-native + durable), honest because both are architecture
   claims.
3. **"Forge-native CI, on an engine that doesn't forget."** — witty, ties the two
   halves; good for a post headline.
4. **"Your control plane can crash. Your build won't notice."** — punchiest, most
   demo-able, but **self-hosted/on-prem audiences only** — it falls flat for
   hosted-SaaS readers who never see a control plane fail.

## How to use "durable" — do / don't

**Do** (all code-backed):
- "Runs survive a control-plane restart and **resume from the last completed
  step** — no re-running work that already succeeded." (proven: `crash_resume.rs`)
- "**Exactly-once step execution**, enforced by optimistic-concurrency guards and
  fencing tokens on a transactional outbox."
- "Built on a **durable execution engine** (the DBOS/Temporal pattern, applied to
  CI)." — as a *supporting* line, never the H1.
- Self-hosted comparison: "Where Drone and Woodpecker orphan in-flight builds on a
  server restart (stuck `running`, dangling containers — woodpecker-ci#3427,
  drone#2189), a Scarab run continues where it left off." Cite the issues; the
  receipts *are* the wit.

**Don't:**
- ❌ "Durable execution" as a bare H1 or one-word wedge.
- ❌ "Durable execution **on Kubernetes**" / live-Pod re-attach — no live-k8s proof
  exists; the crash test's executor is a fake.
- ❌ "**Unlike** Argo/Tekton/Temporal, we're durable" — they are; reads as
  ignorance. Never position against that set on durability.
- ❌ "Time-travel" / "replay" as a shipped feature — it's a derived property of the
  event log, no query endpoint yet. At most: "an append-only event log that makes
  execution history inspectable."
- ❌ "Indefinite approval gates" as a differentiator — GitLab/Buildkite/CircleCI/
  GitHub all suspend cheaply already.

**Scope rule:** attach every durability sentence to *the control plane / the run*,
never to *the Kubernetes execution*, and never frame it against the durable-engine
peer set.

## The AI-era angle — earned, or skipped

We claim it lightly and only where it's structurally true. Two honest hooks:
(a) long-running / autonomous pipelines that **wait cheaply** — durable gates hold
a suspended run at ~zero compute cost (real in code); (b) the **append-only event
log** as an auditable record of what an automated pipeline did. Acceptable framing:

> "As pipelines get longer and more autonomous — agents, long review gates,
> multi-day workflows — 'the job died, click re-run' stops being good enough.
> Scarab holds a suspended run at near-zero cost and resumes where it paused."

The other honest AI thread is about *how Scarab is built*, not a product feature:
ambitious systems software (a Rust durable-execution engine) from a small team at a
pace that used to need a room full of people. That's the Woodpecker "speed of
development" point — keep it to development pace, never imply an AI product feature.

**Never** write "AI-native CI", "built for the age of AI", "LLM-powered", or any AI
capability — there is none in the code. If it can't tie to durable-wait or the
auditable log, drop the AI angle rather than paint it on.

## The Woodpecker lineage (say it, respectfully)

Scarab owes its shape to [Woodpecker](https://woodpecker-ci.org/) — lean,
forge-native CI, no enterprise ceremony. It's inspired as much by Woodpecker's
*limits*: the many-backend surface it carries, and the pace a volunteer project can
sustain. Kubernetes-only sheds the backend baggage on purpose
([ADR-0005](adr/0005-tenancy-and-k8s-only.md)); the durable core is the answer to
the orphaned-build problem Woodpecker/Drone operators actually hit. Respect +
deliberate divergence — never a swipe.

## Red lines (claims the code or market won't back)

1. ❌ "Works with GitHub today." All 8 outbound `ForgePort` methods are
   `unimplemented!()` (`scarab-forge-github/src/lib.rs:177-226`). Say "designed
   forge-native; live GitHub integration in progress."
2. ❌ Any end-to-end durability-on-Kubernetes / live-Pod re-attach claim (live-k8s
   tests are `#[ignore]`d; slice-1 umbrella issue still open for this).
3. ❌ Durable execution as a wedge vs Argo/Tekton/Temporal/Kestra.
4. ❌ Time-travel / replay as a feature.
5. ❌ Indefinite gates as a differentiator.
6. ❌ Live fork-PR trust / OAuth enforcement against a real forge (`get_permissions`
   is `unimplemented!()`; the domain lockout logic is real and tested — claim that).
7. ❌ "Named-result capture works on Kubernetes" — ingest API + wiring exist, the
   egress sidecar image does not.
8. ❌ "Continuously tested" — no CI runs the Rust suite yet; wedge tests skip
   silently green without `SCARAB_TEST_DATABASE_URL`.

**Ship a visible, non-apologetic Status line.** Durable engine: proven. Forge-native
domain model: proven against a fake forge. Live GitHub I/O + live-k8s execution: in
progress. That one honest line is what lets every other claim survive scrutiny.

## Voice

Dev-tool voice: concise, technically precise, witty when it earns it. No
corporate-ese ("enterprise-grade", "seamless", "supercharge", "next-generation"),
no exclamation hype, no "revolutionary". Understatement beats superlatives. If a
sentence would embarrass you in a code review, cut it.

| Prefer | Avoid |
|---|---|
| modern CI engine; forge-native, durable core | durable CI (bare adjective) |
| treats your run as state, not a process | resilient / robust / bulletproof |
| resumes from the last completed step | "never fails" |
| designed forge-native; live GitHub in progress | works with GitHub (today) |
| built AI-first (development pace) | AI-native / AI-powered CI |
| inspired by Woodpecker's limits | "unlike Woodpecker" (swipe) |
