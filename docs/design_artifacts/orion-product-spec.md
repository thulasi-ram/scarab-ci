# Orion — product specification (draft)

- **Status:** draft product spec, pre-ADR. The *architecture* underneath is
  Part 4 of [otel-and-agents-exploration.md](otel-and-agents-exploration.md)
  (the Mandate model, the public-API-only seam, `ui/kit`); this document is the
  *product*: how it works, where it earns its keep, and what the UI is.
- **Date:** 2026-08-01
- **Name:** **Orion** is the product nickname for what Part 4 called "the
  steward". The name is not arbitrary in this house: in Egyptian astronomy
  Orion is **Sah**, the celestial form of **Osiris** — the god of resurrection.
  The scarab (Khepri) rolls the sun into rebirth each morning; Orion is
  resurrection written in the sky. One mythos, two products: **Scarab
  resurrects runs; Orion resurrects work.**
- **Design system:** Orion UI shares Scarab's design system (MMD type system,
  dotted design language, emerald/gold) — decided, for now. The dots earn a
  second meaning here: dots are stars. The constellation motif is free.

---

## 1. One-liner and the sentence underneath it

> **Orion is mission control for autonomous work on your repos.**
> Delegate work to agents with the same trust you give CI — because under
> Orion, an agent's every action *is* CI.

The compass rule inherited from the exploration doc, restated as product law:
**AI may propose; only a Run disposes.** Orion never executes anything. It
holds authority (Mandates), launches Turns (Runs), collects evidence, enforces
budgets, and answers to humans. If Orion's database burned down you would lose
the loops' memory and cumulative budgets — never the evidence of what happened,
because the evidence lives in Scarab.

## 2. Customer and personas

The buyer (unchanged from the exploration doc): the platform/DevEx team at a
200–5000-engineer company told "let engineers point agents at our repos."
The users are four distinct hats:

| Persona | Job to be done | What Orion gives them |
|---|---|---|
| **Delegator** (senior eng) | hand off a goal, not babysit a chat | enforceable terms → they can actually walk away |
| **Approver** (lead / platform) | judge the agent's output without re-doing the work | diff + CI evidence + rationale in one card, at a gate |
| **Operator** (platform team) | keep the fleet inside budgets and allowlists | physical (not advisory) tool/egress bounds; $ enforced, not measured |
| **Auditor** (compliance) | reconstruct what an agent did, months later | every action a Run; every write kernel-attested; every approval human-attributed |

## 3. Object model (recap — normative text is Part 4)

- **Mandate** — durable authority: goal + terms (token/USD/time/turn budgets,
  agent image, allowed tools, approval rules) + done-condition + ledger.
- **Turn** — one whole **Run** launched under a Mandate via the existing
  `on: api` path, with the transcript-so-far as input.
- **Transcript** — append-only record re-fed each Turn; each Turn's delta is an
  **Artifact** of that Run.
- **Verdict** — the Turn's structured proposal in its Results:
  `continue | wait(reason) | done`. A proposal, never a disposition.

**New in this spec — the standing form is a minting rule, not a long loop.**
A **standing Mandate** is a *rule + template* that **mints a finite Mandate per
matching event** ("triage every red main build" → each red build mints one
finite triage Mandate). This mirrors the engine exactly — Pipeline : Run ::
standing Mandate : Mandate — and avoids the alternative (one eternal Mandate
with interleaved transcripts), which is unreviewable and unbudgetable. One
mechanism, two tenses.

## 4. How it works

### 4.1 Lifecycle

A Mandate is always in exactly one of:

```
          ┌────────────────────────────────────────────┐
 created ─▶  ACTIVE (a Turn-Run is in flight)           │
          │     │ verdict: continue ──────────▶ ACTIVE  │
          │     │ verdict: wait(you)   ──▶ WAITING·YOU  │──▶ steered/approved ─▶ ACTIVE
          │     │ verdict: wait(world) ──▶ WAITING·WORLD│──▶ webhook/CI/timer ─▶ ACTIVE
          │     │ human: pause ──────────▶ PAUSED       │──▶ resume ─▶ ACTIVE
          └────────────────────────────────────────────┘
 terminal:  DONE (done-condition true)  ·  KILLED (human)  ·  EXHAUSTED (budget)
```

Everything non-running is **waiting on exactly one of {you, world, time}** —
this triad is load-bearing for the UI (§7). "Stalled" (N turns without
transcript progress — the loop-detection heuristic) is not a state; it is a
*diagnosis* that moves the Mandate to WAITING·YOU with a reason attached.
EXHAUSTED is the dead-letter analogue: terminal-with-diagnostics, and a human
may extend the budget, which re-opens it — deliberately mirroring the engine's
"forward progress or explicit dead-letter" invariant one level up.

### 4.2 The Turn loop, mechanically

1. Orion launches a Run (`on: api`) on the agent Pipeline with Parameters:
   the goal, a reference to the cumulative Transcript, and the **steer queue**
   (any human messages queued since the last Turn).
2. The agent image is an ordinary container honouring a thin convention:
   **reads** `/scarab/agent/goal.md`, `/scarab/agent/transcript.jsonl`,
   `/scarab/agent/steer.md`; **writes** its verdict to
   `/scarab/results/verdict.json` (drained by the existing ADR-0042 sidecar),
   its transcript delta as an Artifact, and its changes as workspace outputs.
   Any runtime, zero SDK. Orion ships official images (a Claude Code runner
   first; the contract is public so anyone can bring their own).
3. Orion watches the Run over SSE; on terminal it validates the verdict
   against a schema (malformed verdict = failed Turn), appends the delta,
   updates the ledger, and acts on the verdict.
4. `wait(question: …)` is the underrated verdict: the agent **asks**, Orion
   surfaces the question in the inbox, the answer is injected into the next
   Turn. Steering formalised — the multiplexer's `attach`, at turn granularity.

### 4.3 Done is never self-reported

`done` from the agent only *proposes* completion. The Mandate closes when its
**done-condition** evaluates true against external evidence. v1 ships three:
`pr_merged`, `checks_green_on(branch)`, `human_confirm` — designed so the set
can later generalise to CEL over forge/Run facts (the engine's own expression
language) without changing the model. Never let the thing being governed
report its own success.

### 4.4 Budgets are cumulative and enforced

Each Turn-Run carries its own budget (existing machinery). The Mandate holds
the **cumulative** line — tokens, dollars, wall-time, max-turns — and refuses
to launch Turn N+1 past it. Metering happens at the tool/model proxy sidecar
(the thing being budgeted never self-reports); Orion only *sums*. This is the
one job no single Run can do, and it is the difference between "measured"
(LangSmith) and "enforced" (nobody, today).

### 4.5 Recovery verbs

- **Retry** a failed Turn: the engine already does this (it is a Run).
- **Steer**: queue a message for the next Turn.
- **Fork from Turn k**: truncate the Transcript to k−1, optionally change the
  steer/terms, re-drive. Prompt bisection as a first-class verb — the
  Take/rerun instinct applied to conversations.
- **Pause / kill / extend budget.**

### 4.6 Entry points

v1: the Orion UI and the API. Fast-follow, and the most forge-native thing in
this document: **mint a Mandate from an issue or PR comment** —
`@orion fix this — budget $20, needs my approval` — riding the
`comment-command` trigger vocabulary that already exists. "Assign the issue to
Orion" is the demo that explains the product in five seconds.

## 5. Where Orion adds value (the six moments)

1. **The delegation moment.** Writing a Mandate replaces babysitting a chat.
   The terms are *enforceable* — budget, tools, approvals — so walking away is
   rational, not reckless. Fire-and-trust vs fire-and-babysit.
2. **The wait.** Agent work is bursty; human attention is the bottleneck.
   Orion inverts polling ("how's the agent doing?") into an inbox ("the agent
   needs you"). The product is the *absence* of checking.
3. **The judgment loop.** Agent acts → CI judges → agent reads the judgment —
   in one system. Everyone else's agent asks *itself* whether the tests pass.
4. **The audit moment.** Three weeks later: "what exactly did the agent do?"
   Every Turn a Run; every file write kernel-attested (the overlayfs upper
   layer, ADR-0062); every approval attributed to a human token, never proxied.
5. **The recovery moment.** Turn 7 went sideways → fork from 6 with a steer.
   Chat products offer "start over"; Orion offers version control on the
   collaboration itself.
6. **The fleet moment.** Ten concurrent Mandates is a team; the Docket is the
   manager's view. tmux for work: many sessions, one attention.

**Where Orion adds no value (honesty section):** interactive pairing (IDE
agents are better; Orion is for work you *leave*), sub-minute tasks (Pod-per-
Turn overhead is real), and non-repo async jobs (refused — see the exploration
doc's product compass).

## 6. Where the value lands in the UI, specifically

| UI surface | The value it carries |
|---|---|
| **The Docket** (home) | moment 2 + 6: an inbox of *decisions*, not a dashboard of statuses. "Needs you" is rank one; everything else is glanceable. |
| **The approval card** | moment 3 + 4: changeset (kernel-attested) + CI evidence + the agent's stated rationale + spend-so-far, one card, approve/deny/steer without leaving it. The approver never re-derives context. |
| **The Mandate pane** | moment 1 + 5: goal and terms always visible (the contract you wrote), turn filmstrip, transcript with **every claim linked to its evidence** (a Run, a log line, an artifact — no unsubstantiated "I fixed the tests"), steer composer, fork affordance on every past turn. |
| **The ledger rail** | moment 1: trust-at-a-glance — $14.20/$50, 41m/4h, 7/20 turns, tools used. The delegator's peripheral vision. |
| **Turn detail** | free: it *is* the existing Run detail via `ui/kit` (DAG, logs, artifacts, takes). Zero new work, full depth. |

The transcript-with-evidence-links deserves emphasis: it is the UI expression
of the compass rule. An agent's sentence is a claim; the Run chip beside it is
the proof. No other agent product can render that, because nowhere else are
the claim and the proof in the same system.

## 7. How the UI should look

Shared design system (decided): MMD type, dotted language, emerald/gold,
dark-first. Constellation motif reserved for Orion accents — progress dots,
the turn filmstrip, empty states. Below are **two layout directions for the
Docket and one for the Mandate pane** — samples for reaction, not production
(per house rule: samples before mass-production).

### 7.1 Docket — direction A: "Inbox-first" (recommended)

Decisions first, fleet second. Closest to the web-ui dashboard's action-inbox
pattern, so it reads as family.

```
┌ ORION · acme ────────────────────────────────────── ⌘K ┐
│                                                         │
│  ● NEEDS YOU (3)                                        │
│  ┌─────────────────────────────────────────────────┐    │
│  │ ◆ upgrade React 19            acme/web   T7/≤20 │    │
│  │   approve changeset · 14 files · CI ✓ · $14/$50 │    │
│  │   [diff] [approve] [deny] [steer…]              │    │
│  ├─────────────────────────────────────────────────┤    │
│  │ ◆ burn down flaky tests       acme/api   T3/≤10 │    │
│  │   question: "quarantine or fix retry_spec?"     │    │
│  │   [answer…]                                     │    │
│  ├─────────────────────────────────────────────────┤    │
│  │ ◆ dep bump: openssl           acme/gw    T9/≤10 │    │
│  │   budget exhausted at $25 · [extend] [kill]     │    │
│  └─────────────────────────────────────────────────┘    │
│                                                         │
│  ◌ working (2)      react-19 ▸T7 running 4m ·· sbom ▸T1 │
│  ◌ waiting on world (4)   ci ×2 · pr-review · timer     │
│  ◌ standing (3)     red-main triage · deps · lint-new   │
│  ─ done this week (12)                          ▸ all   │
└─────────────────────────────────────────────────────────┘
```

### 7.2 Docket — direction B: "Panes grid" (the literal multiplexer)

Every Mandate a live pane, tmux aesthetic, density over ranking. Better demo,
worse Monday morning — attention isn't ranked. Possible as a toggle later;
not the default.

```
┌ react-19    T7 ● you ┐┌ flaky-tests T3 ● you ┐┌ openssl   T9 ◑ $$ ┐
│ approve · 14 files   ││ question pending     ││ exhausted $25     │
│ ▂▄▆█ $14/$50 · 41m   ││ ▂▃ $3/$20 · 12m      ││ ████ $25/$25      │
└──────────────────────┘└──────────────────────┘└───────────────────┘
┌ sbom-audit  T1 ◌ ci  ┐┌ deps (standing) ⟳    ┐┌ + new mandate     ┐
│ waiting: checks 2m   ││ minted 4 this week   ││                   │
└──────────────────────┘└──────────────────────┘└───────────────────┘
```

### 7.3 Mandate pane (single direction — the shape is forced by §6)

```
┌ ◆ upgrade React 19 · acme/web · finite ─────────────────────────┐
│ WAITING ON YOU — approve changeset (Turn 7)      $14.20 / $50.00│
│                                                                  │
│ turns  ①──②──③──④──⑤──⑥──⑦        (each chip = a Run; ⑦ pulsing)│
│        └ fork from any chip                                      │
│ ┌ transcript ──────────────────────────────┐ ┌ ledger ─────────┐│
│ │ T6  bumped react-dom; 3 tests red        │ │ budget  $14/$50 ││
│ │     evidence: run 7f3a ✗ [logs]          │ │ time    41m/4h  ││
│ │ T7  fixed act() warnings, retried CI     │ │ turns   7/20    ││
│ │     evidence: run 9c21 ✓ [checks]        │ │ tools  fs·gh·llm││
│ │ ┌ APPROVAL ─────────────────────────┐    │ │ image  cc-run@…4││
│ │ │ 14 files · +412 −96 [full diff]   │    │ └─────────────────┘│
│ │ │ CI ✓ 9c21 · rationale: "…"        │    │  done-when:        │
│ │ │ [approve] [deny] [steer instead…] │    │  pr merged         │
│ │ └───────────────────────────────────┘    │                    │
│ └──────────────────────────────────────────┘                    │
│ steer ▸ [ …queued for next turn……………………………………… ] [queue]        │
└──────────────────────────────────────────────────────────────────┘
```

Interaction notes: the filmstrip is the TakeFilmstrip pattern reused at
Mandate grain; clicking a turn chip opens the existing Run detail (ui/kit);
approve/deny fire from the browser with the **user's own token** straight to
`scarab-server` (Part 4 rule 2 — the audit trail is the product); everything
updates over SSE, no refresh.

## 8. Product horizons (recast 2026-08-01 — owner's instruction)

> Brainstorms think in **end state / MMP / MLP**, never in v1/v2 sequencing.
> **MVP selection is the owner's decision** and is deliberately absent from
> this spec. The one property any MVP must preserve: **no chicken-and-egg** —
> no layer may require another layer's adoption to be worth running (the SOP
> agent as-a-step works with zero Orion; Orion works with zero Toolkits;
> each layer is independently valuable).

- **End state:** Orion as the multiplexer over repos; the SOP library as an
  org asset; Toolkits + Roster; **eval-as-CI gating SOP changes** the way
  tests gate code (an SOP edit triggers its eval matrix — the procedure is
  code now, so CI judges it); the A2A boundary; `ui/kit` as a public
  building block.
- **MMP — marketable:** what a platform team rolls out org-wide and defends
  to a security review: Mandates + Docket + ledger with gateway-enforced
  budgets (§15.6), approvals on human tokens, standing SOPs minting finite
  Mandates, an org-shared SOP library, audit export. Verdicts
  `continue | wait(approval|question|event|timer) | done`; done-conditions
  `pr_merged | checks_green_on | human_confirm` growing toward CEL.
- **MLP — lovable:** one engineer, one afternoon: a red build gets the stock
  `triage-red-build` SOP — it reads the Briefing (knows attempt 2 failed,
  holds the flake proof), posts an investigation where every claim carries
  its Run chip, opens the right follow-up, spends $1.40 of its $5, and the
  **SOP checklist** shows exactly which procedure steps it walked. Ships
  with three stock SOPs (triage, dep-bump, doc-drift); setup is one file and
  one gateway key. Lovable = "it knew" + "it obeyed" + zero infrastructure.

Formerly this section was a "v1 cut" (in/out lists); those lists remain
useful as a *cost inventory* and moved conceptually to §15.3. Permanent
refusals (model routing, etc.) live in the exploration doc's refuse list and
§15.5.

## 9. Multi-agent topologies and communication (added after discussion)

**Can multiple agents be part of a step? Yes — three topologies**, and choosing
between them is a latency/evidence trade the author makes, not a limitation:

| Topology | Shape | Communication | Evidence grain | When |
|---|---|---|---|---|
| **T1 Crew-in-a-step** | a multi-agent framework (CrewAI/LangGraph/AutoGen) inside one image | in-process — the framework's problem | one opaque Step | ad-hoc delegation, debate, dynamic speaker selection |
| **T2 Agents as tool sidecars** | orchestrator agent = main container; helper agents = **Sidecar services speaking MCP on localhost** | localhost HTTP/MCP, zero cross-Pod networking | **per-agent**: each sidecar has its own metering proxy, egress allowlist, and per-tool-call audit entries | a lead agent consulting specialists; per-specialist budgets |
| **T3 Agents as DAG peers** | each agent its own Step | edges only: Results + Workspace Snapshots + Artifacts | full: per-agent Attempts, retries, reruns | planner → workers → reviewer; anything worth governing separately |

The T2 insight is the load-bearing one: **an agent is a tool that thinks.** The
tools-as-sidecar-services design (deny-all NetworkPolicy, secrets held by the
sidecar, metering at the proxy) needed no modification to admit helper agents —
a helper agent is an MCP server whose implementation happens to call a model.
Multiple agents per Step with *individually enforced* budgets, fenced by
inheritance, dying with the Attempt.

**The communication doctrine.** Converse in the Pod; hand off on the edge; fan
out with matrix; and if parallel steps seem to need to chat, the design is
telling you to co-locate them.

- **Are Results enough between agents?** Between *edge-connected* agents, the
  vocabulary is already complete and typed: **Results** (small structured
  values — precisely the "structured handoff" the field converged on),
  **Workspace Snapshots** (bulk shared context, content-addressed and
  attested — a *versioned* blackboard, which beats a live one for audit),
  **Artifacts** (records), **gates** (the human channel). What this cannot do
  is mid-flight dialogue — deliberately.
- **Why no parallel-step chatter:** two live agents mid-conversation are not
  independently re-executable. A rerun of "step B, which was halfway through a
  negotiation with step A" has no coherent semantics — it is ADR-0058's
  unfenced-mutable-state warning applied to conversation. The Attempt boundary
  is the honest unit; conversations must live inside one.
- **The escape hatch already exists and is honestly labelled:** a **Shared
  service** (ADR-0058) can host a live blackboard/message bus for opt-in
  steps — explicitly unfenced, author's contract. If you reach for it for
  agent chatter, prefer T1/T2.

**What people are looking for in this space** (researched & verified
2026-08-01; sources in the research notes):

- The mid-2025 "don't build multi-agents" stance has a 2026 refinement, and
  it is stronger for us than the original: Cognition's follow-up
  (*"Multi-Agents: What's Actually Working"*, Apr 2026) lands on **"reads
  parallelize, writes stay single-threaded"** — readonly subagents work,
  parallel writers don't, and their code-review agent works *better without*
  shared prior context (fresh perspective beats context-sharing for
  verification). Anthropic's guidance (*"When to use multi-agent systems"*,
  Jan 2026): start single-agent; multi-agent costs **3–10× the tokens** and
  wins only for context protection, parallel search, and specialization; the
  **verification subagent** is the consistently successful pattern.
- **Scarab enforces read-parallel/write-serial *by construction*:** each
  Attempt owns its Workspace, parallel steps read shared Snapshots, merges
  happen only at explicit DAG joins. The discipline the field converged on by
  painful convention is this architecture's default. And the fresh-context
  reviewer is literally a DAG peer step that reads outputs but not the
  transcript — plus CI itself as the blackbox verifier.
- The asks that survived, mapped: typed handoffs (= Results), shared context
  (= Snapshots), fan-out/join (= matrix + join policies), human interrupts
  (= gates), durable resume (= Turns/Mandates), per-agent cost & tracing
  (= sidecar metering + the span projection). The 3–10× token figure also
  argues for our default: one strong agent per step, parallelism via the DAG.
- **A2A** hit v1.0 (Mar 2026, Linux Foundation; 150+ orgs, in Azure Foundry /
  Copilot Studio / Bedrock AgentCore) — real at the platform layer, still
  thin on named production users. Relevant someday at the *Orion boundary*
  (a Mandate exposed/consumed as an A2A task), never inside the DAG. **MCP**'s
  2026-07-28 spec (stateless core, extensions) is the tool boundary; the
  official registry is still preview.
- **Production pain, ranked** (LangChain survey, n=1,340, late 2025):
  **quality 32% > security ~25% > latency 20%**, with cost *declining* as a
  blocker as model prices fall — though runaway-cost incidents stay vivid
  (a documented 11-day loop billing ~$47k; Orion's `max_turns` + cumulative
  budget kills that class outright). Observability is table stakes (89%
  adoption) while **evals lag (52% offline / 37% online)** — which validates
  eval-as-CI as the adjacency and orders the pitch: quality/evidence first,
  budget enforcement as insurance, latency addressed by topology choice (§9).
- **The budget gap is verified open:** enforcement today lives only at the
  LLM-gateway layer (LiteLLM per-session budgets are the closest; Portkey
  virtual keys; OpenRouter's 2026 guardrails — all keyed on keys/sessions by
  convention). *A budget enforced by the orchestrator itself against a
  first-class run identity does not ship anywhere* — that is exactly Orion's
  metering-proxy + Mandate-ledger design, with identity by construction (the
  fence), not by key-discipline.

## 10. Cold starts and the latency budget

Pod-per-Step (and per-Turn) decomposes as:

| Leg | Cost | Mitigation |
|---|---|---|
| Pod schedule | ~1–2 s | fine at agent pace |
| Image pull | 0 if node-cached; 10–60 s cold | slim official runner images; pre-pull via PlacementProfile-targeted nodes; registry mirror; lazy-pull snapshotters (operator-level) |
| Workspace materialise | sub-second | **already solved** — this is exactly what ADR-0061/0062 built (warm CAS, Snapshot Farm, lazy overlayfs Exports) |
| Agent runtime boot | author's container | official images ship compiled/slim |
| **Context re-feed** | tokens + $ + latency to replay the transcript | **the real agent cold start** — see below |

The dominant cost is not the Pod — it is re-feeding the Transcript each Turn.
Three mitigations, in order:

1. **Turn coarseness is the author's dial, and the verdict vocabulary already
   enforces the right shape:** a Turn ends when the agent *needs the world or a
   human* (`wait(...)`), not on a timer and not per think-act cycle. A Turn may
   contain hundreds of model calls. Coarse turns amortise everything above.
2. **Provider prompt caching** covers bursts (turns minutes apart share the
   transcript prefix); it does not survive a three-day gate, and the design
   should not pretend it does.
3. **Compaction in the contract:** the verdict may carry a compacted state
   summary alongside the delta, so Turn N+1 re-feeds a digest + recent tail
   rather than the full history. The full Transcript remains the evidence; the
   digest is an optimisation, never the record.

And the boundary statement: when someone needs sub-second agent loops, that is
not a Turn — it is T1/T2 *inside* a Step, where the loop is process-local and
free. Pod-per-Turn buys durability, governance and evidence at the price of
seconds; in-step loops buy speed at the price of opacity. Both are offered;
neither is disguised as the other. **Warm pod pools are refused for now** —
they fight namespace-per-run isolation and the trust model; revisit only if
real Mandates show turn latency as the binding pain.

## 11. The Briefing — pipeline meta-context as an agent capability

Agents everywhere else start blind; context engineering is the acknowledged
hard part (Cognition's principle 1: *share full context*). Scarab holds an
evidence corpus no agent platform has — so inject it.

**Every agent step receives `/scarab/briefing.json`, read-only:**

| Section | Contents | Why the agent is better for it |
|---|---|---|
| identity & cause | Actor, Headline, ref/SHA, PR title, environment | knows *whose* work and *why* |
| position | DAG placement; what ran before; **prior Attempts with failure diagnoses** | attempt 3 starts knowing what killed attempts 1–2 |
| terms | budget remaining (tokens/$/time/turns), allowed tools, **downstream gates** ("your changeset will need 2 approvals") | writes its rationale *for the approver it knows is coming* |
| repo intel | flake verdicts **with content-identity proof**, recent failure clusters, ownership hints | doesn't chase a known flake; routes to the right owner |
| links | run URL, mandate URL, investigation card if one exists | the explain→act ladder: an altitude-2 investigation is the fixing Mandate's first briefing |

Plus **`scarab-briefing`** — an MCP sidecar in the default toolkit exposing
read-only, run-scoped queries ("what changed since last green", "history of
this test", "who owns this path"). Ambient doc for cheap context; MCP for
drill-down; both governed.

**Budget-aware agents** fall out of the terms row and nobody ships them: the
runner *discloses* remaining budget so the agent can economize (cheap model
for lint fixes when $6 remain) — while enforcement stays at the proxy.
Disclosure and enforcement are different organs; we have both.

## 12. Toolkits and the Roster — setup moved one layer up

The direction requested ("org picks from a catalog, not fresh setup") is the
**house pattern applied twice more**. PlacementProfile and RetentionProfile
already established the species: *operator-owned named bundle, referenced by
name from authored YAML, values never in the repo*. Two new members:

**Toolkit** — the full capability unit, admin-curated, granted by inheritance:

```yaml
# authored yaml names bundles; admins own contents
steps:
  - id: fix
    agent:
      image: roster://claude-code-runner        # from the Roster
      tools: [github-standard, jira-readonly]   # Toolkits, by name
```

A Toolkit bundles: **MCP server images** (digest-pinned) + **credentials**
(held by the sidecar; the agent process never sees them) + **egress
allowlist** (a NetworkPolicy — kernel-enforced, not proxy configuration) +
**budget defaults** + usage instructions. Granted org → project →
Environment on the ADR-0037 secret-scope inheritance chain; which Toolkits an
Environment admits is a protection rule beside the ADR-0039 privilege
whitelist. Every tool call lands in the per-tool-call audit, tied to Run
evidence.

**Roster** — the curated agent catalog: digest-pinned approved agent images
with default terms (budgets, turn timeout) and allowed Toolkits. Creating a
Mandate = pick from Roster + pick Toolkits + write the goal. Fresh-from-
scratch setup disappears; so does "which model key do I paste where".

**Market position (researched 2026-08-01):** *no shipped product offers the
full unit* — tools + credentials + egress + budget defaults as one named
object, inherited across an org tree, audited. The fragments: **Claude Tag's
Access bundles** are the closest prior art (named bundles carrying
credentials + repo grants + domains + plugins + instructions, credentials
injected at a proxy so the model never holds the key — the same custody
principle as our tool sidecars) but they are Slack-scoped (workspace/channel,
not an org tree), have no per-bundle budgets, and govern one surface.
**Microsoft** went identity-first (Entra Agent ID GA, Agent 365 as registry;
policy is subtractive allow/block per environment — no nameable bundle).
**Google** curates at the *agent* grain (Gemini Enterprise Agent Gallery
with request-and-approve). **OpenAI** has a cross-product Connector Registry
+ Codex `requirements.toml` — registry and policy as separate systems, no
bundling of credential/egress/budget. Arcade.dev is the closest independent
(OAuth custody + MCP gateway) with no org-tree inheritance or budget
primitive. **Toolkits + Roster on a real org tree, with kernel-enforced
egress and orchestrator-enforced budgets, is unoccupied ground** — and our
inheritance semantics, custody pattern, and admission machinery all exist.

## 13. Herding — CI scheduling wisdom applied to agent fleets

"Easily herd agents" decomposes into eyes (the Docket, §7) and **hands** —
and the hands are a transplant of what a CI scheduler already knows:

- **Mandate admission:** concurrency groups for agents ("≤1 active Mandate
  per repo per goal-class"), priority lanes, fairness across teams — the
  ADR-0011/0032 machinery at Mandate grain.
- **Supersede-on-new-commit:** a push to the branch a Turn is fixing
  supersedes that Turn — ADR-0056's vocabulary applied to agent work; no
  other platform can even express this.
- **Hierarchical budget pools:** org → team → Mandate ceilings ("agents org-
  wide: $500/day"), enforced at admission, visible in the Docket.
- **Fleet policies:** declarative rules — auto-pause any Mandate touching
  protected paths; require human confirm past N turns; quiet hours.
- **Identity-first alignment:** Microsoft's Entra Agent ID validates the
  instinct — agents need first-class identities. Ours exist by construction:
  the Mandate's service principal + the per-Run fence, RBAC-scoped, audited.

## 14. If a step is just a container running a LangGraph crew — what do we add?

The one-pager answer. The crew brings the brain; everything that makes it
*employable rather than merely runnable* is ours, and each row below is a
verified market gap or an architectural default nobody else has:

| The crew cannot give itself… | Scarab/Orion provides | Status elsewhere |
|---|---|---|
| write-serialized parallelism | Attempt-owned Workspaces, merges only at joins | the field's hard-won *convention*; our *construction* |
| an unbypassable budget | metering proxy + Mandate ledger on the fence identity | **verified open gap** — gateways enforce per-key; no orchestrator enforces per-run |
| capabilities it didn't configure | Toolkits + Roster, inherited, audited | **verified open gap** — closest (Claude Tag bundles) is Slack-scoped, budget-less |
| knowledge of where it is | the Briefing (§11) | agents start blind everywhere |
| a judge it can't sweet-talk | CI as blackbox verifier; done-conditions over external evidence | verification = *the* validated pattern (Cognition/Anthropic '26) |
| proof of what it did | kernel-attested changesets, per-tool-call audit, evidence-linked transcript | `git diff` inside the sandbox being audited |
| survival | durable Runs/Turns/Mandates, fork-from-turn | checkpoint-replay requires determinism (Hatchet); we don't |
| a manager | the Docket + Mandate admission + fleet policies (§13) | dashboards without hands |

One line: **LangGraph decides what to do next; Scarab decides what it is
allowed to do, pays for it, proves what happened, and survives everything in
between.**

## 15. Why this doesn't already exist — and the surface-area audit

This section exists because the owner asked the falsification question directly:
*"Isn't this too much surface area for us? What is currently stopping people
from taking Jenkins/GHA/Temporal and building agent teams? People don't do
that — why? Or even use n8n as a runner — why?"* The question is the right
one, and the answers are the positioning.

### 15.1 The falsification test, taken seriously

**GHA/Jenkins.** The premise needs one correction: the biggest player did
exactly this — GitHub's Copilot coding agent runs on ephemeral Actions
runners (verified, §12 research). The substrate instinct is *correct*. But
the agent plane had to be built privately on top, and the governance came out
holed (firewall covers only the Bash tool; Copilot is a rulesets *bypass
actor*). The lesson: **an agent plane bolted onto a job runner inherits the
job runner's assumptions, and they are the wrong ones.** Which assumptions —
why teams don't do it themselves:

1. *The job model bills you for waiting.* No durable suspend; GHA caps at 6 h
   and charges while blocked; Jenkins `input` parks an executor. HITL means
   holding a machine or exiting and losing state. Our gate costs a row.
2. *Statelessness by design.* No memory between jobs → you hand-build
   transcript storage, resume, and locking in bash + S3 — i.e. a bad Orion.
   Everyone gets exactly far enough to discover they are building a durable
   orchestrator, then stops.
3. *Secrets are all-or-nothing per job.* An agent with CI secrets has
   everything — no custody, no egress control, no per-tool grants. This is
   where security teams veto.
4. *No cost primitive* beyond machine-minutes; token spend is invisible.
5. *Logs-as-text observability* — no tool-call grain, no evidence model.

**Temporal.** People **do** build agent orchestration on it (it markets
durable agents; OpenAI SDK integration). But Temporal is a *library for
building your own agent platform*: it executes nothing (bring your own
workers — no isolation, no images, no checkout), governs nothing (no
approvals/secrets/egress product), and demands determinism discipline in
workflow code. Adopting it means building identity, sandboxing, budgets,
audit, the forge loop, and a UI yourself. **That DIY layer is this product's
hypothesis.** Temporal's existence proves demand for the loop; the layer
everyone rebuilds on top proves what's missing.

**n8n.** It **is** a huge agent runner — for the Zapier job (business glue),
where it is winning. Not for repo work, structurally: one shared trust
domain (pooled credentials, no per-run isolation), no repo semantics
(checkout/PR/checks), no evidence model, node-graph authoring rather than
containers. As a coding-agent runner it means handing one process all your
credentials and getting no audit out.

### 15.2 Synthesis and the threat

Nobody has done it not because the idea is wrong but because it takes four
things in one system — **execution substrate + durable orchestration +
forge-native semantics + governance/evidence** — and each incumbent owns one
or two. GitHub owns substrate+forge but monetises Copilot, not a platform,
on a legacy job model. Temporal owns the loop. n8n owns orchestration UX for
a different job. Scarab is odd in owning all four in one codebase already —
which is the entire reason this is not delusional.

The threat model has one name: **GitHub** owns both sides and Agent HQ shows
them moving. The counter-position is the shape Cursor-self-hosted and
Codex's two-phase runtime validate: **self-hosted, framework-neutral, on
your cluster, with real governance.**

### 15.3 The surface-area audit

The owner's concern is correct: this spec as written is multi-year. It is a
**map, not a commitment**. Sorted:

- **Rides existing rails (cheap):** agent step kind (one IR field + one
  persist arm), tools-as-sidecars (ADR-0058), Turn-as-Run (`on: api`),
  transcript-as-Artifact, CI-as-judge, supersede semantics, Briefing v0 (a
  JSON assembled from data already stored).
- **The real bet (genuinely new):** the Orion service (a table + a driver
  loop + the Docket) and the budget ledger. ~~The metering-proxy sidecar
  binary~~ — superseded by §15.6: metering is offloaded to an LLM gateway;
  Orion mints identities and keeps the ledger.
- **Deferred until a named user pulls:** Toolkit *UI* (gitops files first —
  the PlacementProfile pattern needs no UI to exist), the Roster (a config
  list first), fleet policies, standing Mandates, A2A, eBPF, crew-in-a-box
  scaffolding, the panes-grid.
- **Dual-use, not booked to this bet:** OTel/metrics (the engine is
  operationally blind today regardless), `ui/kit`, the sidecar rewrite (an
  unobservable shell script today).

### 15.4 The kill test

Dogfood on this repo: dep bumps, flaky-test triage, doc-drift PRs, under
real budgets and real gates. **If we don't leave Mandates running on
scrarab-ci after a month, the market's answer was in the falsification
question, and we find out in weeks, not quarters.**

### 15.5 The owner's compass (recorded stances, 2026-07-31 → 2026-08-01)

Positions the owner has taken during this design arc, recorded so future
work doesn't relitigate them silently:

- **An agent should be nothing more than a container + command + some YAML.**
  Simplicity is the bar; every layer above that must justify itself. (The
  spec's answer: the layers are what make it *employable* — §14 — but the
  authoring surface must stay at container+YAML.)
- **eBPF: never a DaemonSet; acceptable as a sidecar** under governed grants.
- **The Superlogical frame is the chosen direction:** the engine stays the
  public building block; the AI layer (Orion) is the multiplexer equivalent.
  If the agent foundry isn't a natural fit for a CI runner, *bend the problem
  until it fits* — the fit found: agents-as-Runs, governance as the value.
- **Orion may be given away free later.** Every seam must therefore stand on
  coupling discipline, never on a monetisation boundary.
- **"Move setup one layer up" is the direction** (the Claude Tag
  observation): org-curated catalogs — connections, MCPs, credentials,
  bundles with inheritance — over fresh per-user setup. Toolkits + Roster
  (§12) are this instinct in house pattern form.
- **Novelty is required, not optional:** "if a step is just a container
  running a crew of LangGraph agents, what does Scarab/Orion add" must have
  a sharp answer at all times (currently §14).
- **Surface-area skepticism is standing policy:** the falsification question
  (§15.1) should be re-asked at every scope expansion.

### 15.6 The offload doctrine (owner, 2026-08-01)

The owner pushed the audit one level further: *"agents can run inside and
have observability hooked, costs manage via LiteLLM — why do we even need to
support those? The only thing we have to make easy is: run an orchestrator
at the top and have steps bubble up the right results/context."* Correct,
and adopted. **Offload the muscle; keep the identity and the ledger.**

| Concern | Offloaded to | What Orion keeps (irreducible) |
|---|---|---|
| Budget *enforcement* | an LLM gateway (LiteLLM-class: hard per-key caps, verified shipping) | **mint a gateway key per Turn from the fence** (`m42-t7`), cap it at `min(turn cap, ledger remaining)`, read spend back. The cross-turn **ledger** and the refusal to launch Turn N+1 are Orion's — no gateway can do them. Unbypassability comes from deny-all NetworkPolicy *except the gateway*, which is ours and cheap — not from owning a proxy. |
| Interior observability | OTel — agents emit their own traces; we inject `TRACEPARENT` + endpoint env | **evidence is not telemetry**: verdict, Results, transcript, changeset stay on Scarab rails (audit must not depend on a span being ingested — Part 1's doctrine). |
| Tools | v1: the agent image brings its own MCP config; Toolkit *sidecar mechanics* defer with the rest of §12 | scoped secrets + egress NetworkPolicy (both already exist). Per-tool-call audit arrives with Toolkits, when pulled. |

This supersedes §4.4's "metering happens at the tool/model proxy sidecar" —
the *model* survives (enforcement at a boundary the agent cannot bypass,
identity by construction), the *implementation* is now someone else's
software holding an identity we minted. The verified market gap (§9) is
unchanged and clarified: the gap was never the gateway — it is the **binding
of gateway enforcement to a first-class run identity with a durable
cross-turn ledger**. That binding is the product.

Restated core after this cut: **a durable orchestrator at the top (launch,
verdict, park/wake, ledger, refuse) + the bubble-up contract (Briefing down;
verdict/Results/transcript/changeset up) + identity minting + the Docket.**
Everything else is either already-built Scarab or an integration.

## 16. Open product questions

1. **Docket direction** — A (inbox-first) vs B (panes grid) vs A-with-B-toggle.
2. **Notifications** — is the Docket enough, or does WAITING·YOU page you in
   Slack/email? (The inbox inverts polling only if people actually see it.)
3. **Who may create Mandates** — any Write-role engineer, or is minting itself
   a governed grant per Environment? (The conservative default is governed.)
4. **The question verdict's answer path** — free text only, or may the agent
   offer structured options? (Structured tempts the UI toward wizard-ware;
   free text keeps the human in charge. Leaning free text + optional choices.)
5. **Naming residue** — the crate/binary: `scarab-orion` (workspace
   convention) vs bare `orion` (product-forward). Cosmetic, decide at scaffold
   time.
6. **Briefing scope** — does repo intel in the Briefing ever cross repos
   ("this failure cluster hit 14 repos")? Cross-repo context is valuable and
   is also an information-disclosure surface; needs an explicit scoping rule
   (default: same-project only, org-wide behind a grant).
7. **Toolkit authorship** — operator-config only (gitops, like
   PlacementProfile) or also an org-settings UI (like ADR-0060 connections)?
   The Claude Tag comparison argues for the UI; the house pattern argues for
   gitops. Probably both, UI writing through to the same store.
8. **Roster curation flow** — who approves an image onto the Roster, and is
   that approval itself an Environment protection rule or a new org-settings
   surface?

## 17. The SOP agent — interface first, LangGraph as the first adapter
(decided direction, 2026-08-01)

Scarab ships a **first-party SOP-based agent**. Not only a contract for
arbitrary agent images — an opinionated official agent whose behaviour is a
**Standard Operating Procedure**: a human-authored, in-repo, code-reviewed
procedure document. SOPs are how organizations already encode trust in human
operators; a pipeline is a procedure for machines, an SOP is a procedure for
judgment, and both live in `.scarab/`, read at the triggering ref. The
differentiated sentence: **agents that follow your runbooks, provably.**

### 17.1 Two strata, and the honesty line between them

```markdown
---                                # TERMS — enforced by Scarab/Orion
name: triage-red-build
tools: [repo, ci-evidence]
budget: { usd: 5, turns: 6 }
escalate_to: "@platform-oncall"
---
## Objective                       # PROCEDURE — followed by the model,
Classify the failure: flake / infra / regression; open the right follow-up.
## Procedure                       #   audited by the transcript
1. Read the failing run's evidence. If content-identity shows prior green
   on identical inputs → flake.
2. If flake: quarantine per the quarantine SOP; open PR; link evidence.
3. If regression: bisect, notify the author. Do NOT attempt a fix.
## Escalation
When classification confidence is low, ask (wait) rather than guess.
```

Frontmatter is **enforced** (budgets, tools, gates — machinery that exists).
The body is **followed** — and the UI must never let advisory look enforced.

### 17.2 The architecture is the house pattern

**SOP → parsed procedure IR (ours, the stable seam) → `AgentRuntime` port →
adapters.** First adapter: **LangGraph** — the procedure IR *constructs the
graph*, so the SOP's control flow is structural, not a prompt suggestion.

| Seam | Ours / theirs | Swappable |
|---|---|---|
| SOP format + procedure IR | ours — the interface, versioned like the pipeline IR | no (it is the contract) |
| `AgentRuntime` | port (house sense: domain-owned interface) | — |
| LangGraph | first adapter | yes — Claude Agent SDK, raw-loop, CrewAI later |
| models | via the LLM gateway (§15.6) | yes — gateway concern |
| tools | MCP | yes |

Key property bought by IR-drives-the-graph: **procedure position is not
self-reported.** The runner knows the agent is on step 2 because step 2 is a
graph node. Hence the **SOP checklist** in the Mandate pane / step view —
procedure steps ticking off live, evidence per step, deviations flagged
structurally. Possibly the most lovable surface in the product: watching an
agent execute *your* procedure with proof at each line.

### 17.3 One artifact, both altitudes

- **As a step:** `agent: { sop: .scarab/sops/triage.md }` — inside a
  pipeline, zero Orion required.
- **On top of pipeline:** the same SOP is a Mandate's brain; its procedure
  may launch pipelines (a governed `scarab` MCP tool) and gate for humans.
  SOP-on-top makes "the agent is a Run" recursive: the orchestrator at the
  top is itself following a reviewed procedure.

This realizes §8's anti-chicken-egg property: one SOP file + one gateway key
= a running agent on one repo, valuable alone; Orion makes many manageable;
Toolkits make them org-curated. Stock library ships with the product:
`triage-red-build`, `dep-bump`, `doc-drift`.

### 17.4 Consequences

- Eval-as-CI gains its object: **an SOP edit triggers that SOP's eval
  matrix** — procedures get judged by CI the way code does (§8 end state).
- The Roster's rows become mostly SOPs, not images — the image is ours; what
  orgs curate is procedures + terms. Setup moves another layer up.
- Honest limits, stated: the agent *follows* the procedure — adherence at
  the graph grain is structural, but judgment inside a step is still a
  model; the body's prose (e.g. "do NOT attempt a fix") is instruction, not
  enforcement, unless it maps to a term. Deviation detection flags, it does
  not prevent.

### 17.5 Composition: the SOP library and rasterization (owner-decided, 2026-08-01)

SOPs **compose**, and the composition model is a linked document vault, not a
package system:

- **SOPs live in their own repo(s)** — an org runbook vault of plain markdown
  + frontmatter + wikilinks, **Obsidian-compatible as-is**. `.scarab/` stays
  workflows + repo-scoped stuff; pipeline YAML carries only a reference
  (`sop: runbooks//triage-red-build`, or `./sops/local.md` for repo-scoped).
  The vault is read via existing ForgeConnection machinery; which projects
  may consume which libraries is an org-settings mapping (ADR-0060 surface).
- **Two link semantics, two resolution times.** Transclusion
  (`![[quarantine-flaky-test]]`) *becomes procedure* — compiled into the
  graph eagerly. Reference (`[[deploy-rollback-notes]]`) *stays knowledge* —
  fetched on demand mid-run through the briefing MCP tool, at the same
  pinned SHA (the agent never sees a mixed-version vault).
- **Rasterization happens at mint/creation, never lazily mid-run:** resolve
  the root against ONE vault SHA → walk transclusions transitively (cycle
  detection + depth cap — the `inline_invokes` algorithm applied to docs) →
  the **rasterized procedure** → procedure IR → the runtime graph. Recorded
  as evidence on the Run ({root, repo_sha, raster_hash} + bytes) and
  delivered to the agent *as bytes* — doc fetching is control-plane-side, so
  the agent never holds forge credentials. Fail-closed: a broken link is a
  creation error, not a turn-7 surprise.
- **The update boundary is minting.** Standing Mandates re-rasterize per
  minted Mandate (always the latest vetted SOP); a running Mandate keeps its
  snapshot, with an explicit re-pin steer verb for exceptions. The vault has
  its own CI, so **eval-as-CI runs where the SOPs live**; consumers mint
  from a vetted tip.
- **Terms never widen.** A transcluded SOP's frontmatter terms are
  *requirements checked against the root's envelope*, never additions: child
  needs tool `gh`, envelope lacks it → rasterization error at mint.
  Composition narrows or fails; it cannot escalate (ADR-0039's
  grants-are-ceilings applied to documents).
