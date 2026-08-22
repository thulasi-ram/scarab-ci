# Orion — validation plan (WIP)

- **Status:** WIP — a menu, not a schedule. The owner picks which tests run
  and when; nothing here is started until picked.
- **Date:** 2026-08-23 (plan drafted 2026-08-03)
- **Companion to:** [orion-product-spec.md](orion-product-spec.md). The spec
  is a stack of hypotheses; this doc names each bet, the cheapest observable
  signal for it, and the kill criterion — set *before* the test runs.

## Discipline (read before running any test)

- **Behavior is the only currency.** "Cool!" in an interview is worth
  nothing; an existing glue script, a security-veto story, a budget line, or
  "can I have it Tuesday" is signal.
- **Kill criteria are written down first** — before the test exists to be
  fond of. Especially the dogfood: define H1's metrics before the spike is
  built (§15.4 of the spec).
- **One recruit, three hypotheses:** H2/H4/H7 share interview slots.
- **Recruiting pool = the ICP:** self-hosted CI communities
  (Woodpecker / Drone / Forgejo-Gitea), CNCF Slack, platform-engineering
  forums, plus the owner's network.

## The hypothesis → test table

| # | Hypothesis (the bet) | Cheapest test | Go / kill signal | Cost |
|---|---|---|---|---|
| H1 | **It's useful at all** — governed SOP agents produce value *we'd* keep using | The §15.4 dogfood kill-test: A1 spike + 3 stock SOPs (triage-red-build, dep-bump, doc-drift) on this repo | We voluntarily keep ≥2 Mandates running after 30 days; $/outcome sane; interventions/mandate declining | ~1 wk build |
| H2 | **The buyer exists** — platform teams (200–5000 eng) own a felt "agents touching repos" problem | 10–15 mom-test interviews. Never pitch; ask about the past: *"Do agents run against your repos today? Show me the script. What did security say? Who approved it?"* | ≥6/10 have a glue script, a blocked initiative, or a security-veto story. Shrugs → kill/shelve the buyer thesis | 2 wks calendar, $0 |
| H4 | **The glue-script wall is real** — DIY agent pipelines hit loop/wait/audit pain | Same interviews, different probe: find people running claude-code-action / DIY bots ≥3 months; ask what broke | They independently name state, waiting, credentials, or cost (the spec §15.7 list). "Works fine" → the null hypothesis wins | folded into H2 |
| H5 | **Whale waste is real and measurable** (economist tier) | **Mine public data:** GitHub API over ~20 big OSS repos (kubernetes, rust-lang, …) — measure doomed-matrix-leg compute, late-cancelled runs, retry storms after infra blips | A publishable number ("~$X/yr wasted in doomed legs in repo Y"). Doubles as the demand-gen asset for H3 | 2–3 days scripting |
| H7 | **The Docket UX is right** — inbox-of-decisions beats chat / PR-only supervision | HTML prototype (Docket + Mandate pane, spec §7) in front of interviewees: *"walk me through your Monday"*; 5-second test on the approval card | They find "needs you" unprompted; they trust the approval card enough to click approve — or name exactly what evidence is missing | 2–3 days atop prototype |
| H8 | **MMP survives security review** | Write the 1-page security architecture (isolation, credential custody, egress, audit, human-token approvals); dry-run with 2–3 security engineers *as a review, not a pitch* | "This would pass, given X and Y" — and X/Y become the real MMP list | 1 day + 3 calls |
| H6 | **People will write SOPs** (the authoring behavior change) | Hand the SOP format (spec §17.1) to 5 engineers who own a real runbook; watch them convert it. No tooling — markdown + a screen share | Done in <1 hr; the two strata (enforced terms vs followed procedure) make sense unprompted. "Can't I just write a prompt?" is signal, not noise | 1 afternoon × 5 |
| H3 | **The positioning lands** — "the agent is a Run" vs "just use Cursor" | Publish the ideas: *"The agent is a Run"*, *"Cost plane vs verdict plane"* as posts + an honest early-access page | Qualitative resonance + waitlist signups from target-ICP domains. Also front-runs copycats on the spec's Tier-3 IP (speed argument) | 2–3 days writing |

## As a four-week sprint (illustrative shape — owner sequences)

- **Wk 1–2:** A1 dogfood spike running (H1 clock starts) · interview
  outreach + first 6 calls (H2/H4) · waste-mining script (H5).
- **Wk 3–4:** HTML prototype into remaining interviews (H7) · security
  one-pager + 3 dry-runs (H8) · SOP authoring sessions (H6) · draft the two
  positioning posts (H3) — publish when the H5 numbers land so the post
  carries data.

**End state:** every horizon in the spec carries an evidence tag —
*validated / killed / needs-build-to-test* — the input the MVP decision
wants.

## Artifacts on the shelf (buildable on request, no further design needed)

- [ ] **Interview guide** — mom-test discipline, probes per hypothesis
  (H2/H4/H7 in one session script).
- [ ] **Waste-mining script** — GitHub API, runs against public repos from a
  laptop; outputs per-repo doomed-compute estimates (H5).
- [ ] **Security one-pager** — the H8 dry-run document.
- [ ] **Docket + Mandate pane HTML prototype** — spec §7 wireframes made
  clickable, Scarab design system, mock data (H7; also the general
  react-to-something-that-moves artifact).
- [ ] **A1 dogfood spike** — library pipeline + one runner image on
  `.scarab/dogfood.yaml` (H1).
- [ ] **Positioning posts** — drafts of the two essays (H3); publishing is
  the owner's call and name.

The interviews and the publishing are the owner's to run — they need the
owner's name on them.

## Pick log

_(Owner marks picks here; each pick gets a date and an outcome line when
its test concludes.)_

- _none yet_
