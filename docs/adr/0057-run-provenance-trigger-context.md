# 0057. Run provenance: trigger context and origin details

- **Status:** Accepted
- **Date:** 2026-07-20
- **Grilled:** 2026-07-20 (`/grill-with-docs`; all seven open questions resolved — see "Grill outcomes")
- **Deciders:** thulasi.ram (architect)
- **Complements:** [0049](0049-identity-and-access.md) (origin stamping), [0046](0046-forge-auth-and-multi-adapter.md) (normalized `Event`, multi-adapter), [0043](0043-launch-parameters-and-manual-dispatch.md) (manual dispatch), [0037](0037-environment-governance.md) (`ProtectionRules`, admission), [0056](0056-run-takes-and-attempt-grain-evidence.md) (run-detail surface)
- **Builds on:** migration `0028_run_origin.sql` — the origin-stamping precedent (discrete, independently-nullable `origin_*` columns; no dedicated ADR)

## Context

A run's **provenance** answers three operator questions: *what ran*, *on what
code*, and *who/what caused it*. Today the run read models (runs list +
`GET /v1/runs/{id}`) answer these only partially:

| Question | Fact | Status |
|----------|------|--------|
| what ran | pipeline name | **landed** (`runs.pipeline`, 0031 — explicit `name:` or `.scarab/<file>`) |
| on what code | commit sha, branch/ref | present (origin, `0028_run_origin`) |
| on what code | forge web URL (commit/PR deep link) | **landed** (`ProjectDto.repo_url`) |
| on what code | PR number | present (origin, `0028_run_origin`) |
| who caused it | trigger kind (push/pull_request/manual/…) | present (origin) |
| who caused it | Actor (pusher / dispatcher) | present (origin) |
| **why / what changed** | **commit message · PR title · dispatch reason** | **this ADR** |
| on what code | PR **base** branch (`base ← head`) | **this ADR** |
| who caused it | commit **author** (vs pusher) | deferred (see Analysis) |

The gap that motivated this ADR: the runs list and run header show a bare
commit sha and `push`/`manual`, with **no human-readable context** — you can't
tell *what* a push changed or *why* someone dispatched a run without leaving
Scarab. The normalized `Event` (ADR-0046, `scarab-forge`) deliberately discards
this: `Event::Push` keeps only `{actor, repo, ref, after}` (no message),
`Event::PullRequest` only `{actor, repo, number, head, fork}` (no title/base), and
`Event::Manual`/`Api` have no reason. The data exists in every forge's webhook
payload (`head_commit.message`, `pull_request.title`, `pull_request.base.ref`) —
it is thrown away at normalization.

The origin-stamping work (migration `0028_run_origin.sql`; no dedicated ADR) set
the precedent for origin facts: **discrete, independently-nullable columns, never
a bundle blob**, extracting *exactly the facts the UI shows*. This ADR extends
that model rather than inventing a new one.

## Decision

### 1. A single `trigger_title` headline, kind-disambiguated

Add one nullable column, `runs.trigger_title` (migration `0032`), holding the
**human headline** of whatever triggered the run, its meaning fixed by the
already-stamped `origin_trigger_kind`:

- `push` → the head commit **subject** (first line of `head_commit.message`).
- `pull_request` → the **PR title**.
- `tag` / `release` → the tag/release name (or release title).
- `manual` / `api` → the operator-supplied **reason** (optional; see #3).
- `cron` → the schedule expression; `upstream` → the upstream run id.

Rationale for *one* column over three (`commit_message` / `pr_title` /
`reason`): all three are the same UI slot — "the one line that says what this
run is about" — and the kind already disambiguates. Only one is ever non-null
per run, so three columns would be three-way-sparse with a UI `switch` on kind
to pick the live one. This keeps the runs-list query flat and the UI branch-free.
`trigger_title` is **display/audit only** — no run-list filtering or search by it
is foreseen; should a per-facet search (commit-message vs PR-title) ever be
wanted, that is the trigger to revisit discrete columns.

Bounds: store the **subject line only** for commits (drop the body). Truncate
`trigger_title` to a fixed cap server-side (**200 chars**), on a **char/grapheme
boundary** (never mid-UTF-8-sequence), and store the value **clean — no ellipsis
marker**. The DB holds honest "first 200 chars"; the UI owns the overflow signal
(tooltip / clamp). 200 covers effectively every real subject/PR title (GitHub
hard-limits PR titles at 256, soft-wraps subjects at ~72); the cap is an
anti-bloat backstop, not the display truncation.

### 2. Enrich the normalized `Event` (source of the headline)

`scarab-forge::Event` variants gain the fields the headline needs, populated by
each adapter from the webhook payload:

- `Push { …, message: String }` — the head commit message (adapters read
  `head_commit.message`; GitHub **and** Forgejo both carry it).
- `PullRequest { …, title: String, base: String }` — PR title + base branch
  (`base` from `pull_request.base.ref`; also fills the `base ← head` display, #4).
- `Manual { …, reason: Option<String> }` and `Api { …, reason: Option<String> }`
  — the dispatch reason.

The fields ride the **shared `Event`** (no side-channel provenance struct):
`Event` is already where each adapter produces the normalized facts, in the same
payload-parse pass; a side-channel would parse twice and thread a second value
everywhere `Event` flows. Both adapter crates (`scarab-forge-github`,
`scarab-forge-forgejo`) parse these; their payload-parsing unit tests are
extended with fixtures carrying a message/title/base. The server stamps
`trigger_title` in `persist_run_from_ir` (beside `set_run_origin` /
`set_run_pipeline`) via a small `trigger_title(event) -> Option<String>`
extractor.

**Security boundary (load-bearing — do not undo):** these provenance fields are
**deliberately excluded from `Event::context()`** — the flat JSON that feeds CEL
trigger-matching and `${{ }}` interpolation. They flow *only* adapter → `Event`
field → `trigger_title` / origin column → DTO → UI, never into the matching /
interpolation map. Rationale in "Grill outcomes" Q6/Q7: `${{ event.message }}`
spliced into a `run:` script is the GitHub-Actions script-injection class, and
shell has no context-free escape (the sink — quoted / unquoted / env / arg — is
unknowable at template time). `Comment.body` *is* in `context()` because
comment-command triggers structurally need to match it; `message`/`title`/`base`
have no matching or interpolation use, so exposing them would be pure attack
surface with zero benefit. Enriching `Event` therefore leaves the *matching
context* byte-for-byte as lean as today. A code comment at `context()` states
this invariant.

### 3. Manual dispatch reason — a first-class input, environment-gated

`POST /v1/repos/{org}/{repo}/dispatch` (and the inline `POST /v1/runs`) accept an
optional `reason: String`. The web UI's **New-run** form (`RunPipeline`) grows a
free-text "reason" field (optional). The reason flows `DispatchRequest → Event::
Manual.reason → trigger_title`. The dispatch endpoints stay dumb — they accept
and stamp the reason and perform **no requiredness check**.

**Requiredness is an Environment `ProtectionRule`, enforced at the admission
gate** — a third guardrail beside `approvers` and `allowed_refs` (ADR-0037), not
a global switch and not a dispatch-endpoint check:

- `ProtectionRules` gains `require_reason: bool` (`#[serde(default)]`, off =
  fail-open for existing environments; **Administer-scoped** to set, like the
  privilege whitelist — separation of duties).
- Why the admission gate and not the dispatch endpoint: a dispatch targets a
  repo + ref + *pipeline*; the Environments it will touch are only known once the
  IR compiles and deploy steps resolve. Environments already gate at admission
  when a step targets them (that is where `approvers` lives). Checking the reason
  there reuses the exact ADR-0037 machinery; checking it at dispatch would force
  the endpoint to compile the IR and duplicate admission.
- The check fires iff **`env.require_reason && trigger_kind ∈ {manual, api} &&
  reason is empty`**. Scoped to `manual`/`api` deliberately: those are the
  human-initiated deploys where a person could type a justification. `push` /
  `pull_request` / `tag` / `release` carry their own intrinsic headline (commit
  subject / PR title / tag) and are exempt — the audit target is "someone
  hand-clicked deploy-to-prod without saying why," not "block automated
  triggers." `cron` / `upstream` (machine-initiated, no human to prompt) are
  likewise exempt. A blocked run surfaces the violation exactly like a missing
  approval.

### 4. Read models + UI

- `RunSummaryDto` and `RunStatusResponse` gain `trigger_title: Option<String>`
  and the PR `base` (`origin_pr_base: Option<String>`).
- The provenance / **Trigger cell is a single shared component**, reused by both
  the runs-list row (`RepoView`) *and* the run-detail top context bar
  (`RunDetail`) — not two render sites sharing helpers. Thread A currently shares
  `trigger.ts` helpers but duplicates the JSX; this ADR unifies them into one
  component so the list and header can never drift.
- The component renders the headline as the **secondary line of the Trigger
  cell** (kind on top, title beneath), truncated with a full-text tooltip. PR
  rows read `base ← head` in the ref/commit cluster (a discrete origin fact, not
  folded into `trigger_title`).
- **Placement is Trigger-cell only** on both surfaces. Promoting the headline to
  a run-detail **H1 subtitle** is deferred — pure layout, no data-model impact;
  revisit if the headline reads buried once live.
- Display remains defense-in-depth: SolidJS auto-escapes (a fork PR's title is
  attacker-controlled), and the value is already subject-only + capped. Any
  *future* need to surface the headline into a typed sink (Slack body, PR-comment
  template) uses a purpose-built template that encodes for *that* format — never
  a blanket `context()` exposure.

### Analysis — other provenance details considered

Enumerated so each is accepted/deferred explicitly:

1. **Commit author vs pusher** — GitHub distinguishes `head_commit.author` from
   `sender`. Actor today = sender. **Deferred** — one "who" is enough for v1;
   revisit if reviewers ask "who wrote this" vs "who pushed it".
2. **PR base branch** (`base ← head`) — cheap (in the PR payload), high value for
   reading a PR run. **Included** (the `base` field in #2, `origin_pr_base`).
3. **Compare/diff URL** for a push (`repo_url/compare/<before>..<after>`) — needs
   the `before` sha (not currently in `Event::Push`). **Deferred** — the commit
   deep-link already exists; add only if requested.
4. **Actor / author avatar** — pure UI polish, needs a forge user→avatar lookup
   (new port surface). **Deferred.**
5. **Event timestamp** (when the push/PR happened) — nearly always == run
   `created_at`. **Skipped** (redundant).
6. **PR action** (opened / synchronize / reopened) — marginal for display.
   **Skipped.**
7. **Conditionally-required reason via `ProtectionRule`** — **Included** (#3
   `require_reason`), the environment-configurable form of the manual reason.

## Alternatives considered

- **A `trigger_context` JSON blob** instead of a `trigger_title` column — richer,
  but violates origin-stamping's "discrete columns, not a bundle" and pushes
  kind-branching into the UI. Rejected.
- **Three discrete columns** (`commit_message`/`pr_title`/`dispatch_reason`) —
  more honest per-kind, but they occupy the same UI slot and only one is ever
  non-null per run. Rejected for flatness; re-open only if a per-facet
  search/filter (commit-message vs PR-title) is wanted.
- **Fetch on read** (call the forge for the message/title at render time) —
  rejected: couples the read path to forge availability + rate limits, and the
  data is immutable, so stamp-at-creation is correct.
- **Provenance fields in `Event::context()` with sanitize-and-escape** — debated
  and rejected (Grill Q6). Shell has no context-free escape at template time; it
  is the GitHub-Actions `pull_request` script-injection class, which the incumbent
  mitigates by *routing to a data sink*, not by escaping. Escape belongs at
  *known* sinks (display, typed templates); the un-encodable shell-interpolation
  map gets **exclusion**, not escaping.
- **Requiredness as a global switch or a dispatch-endpoint check** — rejected.
  Global punishes the common frictionless dispatch; the dispatch endpoint can't
  know the target Environments without compiling the IR. Per-Environment at the
  admission gate is the correct seam (mirrors `approvers`).
- **Provenance on a side-channel to keep `Event` lean** — rejected (Grill Q7).
  The leanness that mattered is the *matching context*, preserved by the Q6
  exclusion; a side-channel would double-parse and thread a parallel value
  everywhere `Event` flows.

## Consequences

- Migration `0032_run_trigger_title.sql` — nullable `runs.trigger_title TEXT` and
  `runs.origin_pr_base TEXT` (no backfill — pre-migration runs show no headline /
  base, exactly as origin/pipeline degrade). A separate migration adds
  `require_reason` to the environment protection-rules storage.
- `Event` enum changes ripple to both adapters + their tests, the trigger/webhook
  path, and the dispatch API. This is the bulk of the work and the main risk
  (webhook payload shape per forge). Mitigated by extending each adapter's
  existing payload fixtures.
- `require_reason` extends the admission check (`ProtectionRules::admits` / the
  admission flow) with the run's `trigger_kind` + reason presence. Administer-only
  to set; off by default so existing environments are unaffected.
- **Security invariant:** provenance fields never enter `Event::context()`
  (comment at the call site; enforced by review). Display paths escape + truncate.
- No behavior change to scheduling — provenance is display/audit only, never
  load-bearing (same contract as origin stamping). The one behavioral addition is
  `require_reason`, which can *block* a manual/api deploy at a gated environment.

## Grill outcomes (resolved 2026-07-20)

The seven open questions, as resolved in the `/grill-with-docs` session:

1. **One `trigger_title` vs discrete vs JSON** → **one column**, display-only. No
   per-facet filtering foreseen; discrete columns are the fallback if it is.
2. **Commit subject vs full message; cap** → **subject-line only**, 200-char
   server cap, char-boundary-safe, stored clean (no ellipsis; UI owns overflow).
3. **PR base branch** → **included now** as discrete `origin_pr_base`, rendered
   `base ← head`.
4. **Manual reason optional or required** → **optional at dispatch**, with
   requiredness **environment-configurable** via `ProtectionRules.require_reason`,
   enforced at the **admission gate**, scoped to `manual`/`api` with an empty
   reason.
5. **Where the headline lives** → **Trigger-cell secondary line**, both surfaces;
   the Trigger cell is **one shared component** across the runs list and the
   run-detail top context bar; H1 promotion deferred.
6. **Fork-PR title / interpolation** → provenance fields on `Event` but
   **excluded from `Event::context()`**; escape at *known* sinks only. Shell
   interpolation is un-encodable and unneeded here → exclusion, not escaping.
7. **Scope of the `Event` change** → **enrich the shared `Event`** (no
   side-channel); matching-context leanness is preserved by Q6's exclusion.
