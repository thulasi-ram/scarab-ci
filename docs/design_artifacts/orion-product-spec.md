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

## 8. v1 cut

**In:** finite Mandates; one repo per Mandate; UI + API creation; verdicts
`continue | wait(approval) | wait(question) | wait(event: ci) | done`;
done-conditions `pr_merged | checks_green_on | human_confirm`; cumulative
budgets (tokens/USD/time/turns) with EXHAUSTED + extend; steer between turns;
Docket (direction A) + Mandate pane + Run detail via `ui/kit`; one official
agent image (Claude Code runner); the public agent-image contract.

**Out (named, not forgotten):** standing Mandates (needs the minting rule +
event subscriptions); fork-from-turn (v1.1 — the data model supports it from
day one: transcripts are per-turn Artifacts); `@orion` comment-command
minting; multi-repo Mandates; panes-grid view; fleet analytics; any model
routing (refused permanently).

## 9. Open product questions

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
