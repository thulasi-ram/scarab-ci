# 0057. Run provenance: trigger context and origin details

- **Status:** Proposed
- **Date:** 2026-07-20
- **Deciders:** thulasi.ram (architect)
- **Complements:** [0049](0049-identity-and-access.md) (origin stamping), [0046](0046-forge-auth-and-multi-adapter.md) (normalized `Event`, multi-adapter), [0043](0043-launch-parameters-and-manual-dispatch.md) (manual dispatch), [0056](0056-run-takes-and-attempt-grain-evidence.md) (run-detail surface)

> **Grill-me target.** This ADR is intentionally Proposed and carries an **Open
> questions** section. It is meant to be stress-tested in a `/grill-with-docs`
> session before implementation. Nothing here is built yet except the two items
> called out as "already landed".

## Context

A run's **provenance** answers three operator questions: *what ran*, *on what
code*, and *who/what caused it*. Today the run read models (runs list +
`GET /v1/runs/{id}`) answer these only partially:

| Question | Fact | Status |
|----------|------|--------|
| what ran | pipeline name | **landed** (`runs.pipeline`, 0031 — explicit `name:` or `.scarab/<file>`) |
| on what code | commit sha, branch/ref | present (origin, 0028) |
| on what code | forge web URL (commit/PR deep link) | **landed** (`ProjectDto.repo_url`) |
| on what code | PR number | present (origin, 0028) |
| who caused it | trigger kind (push/pull_request/manual/…) | present (origin) |
| who caused it | Actor (pusher / dispatcher) | present (origin) |
| **why / what changed** | **commit message · PR title · dispatch reason** | **MISSING** |
| who caused it | commit **author** (vs pusher) | missing |
| on what code | PR **base** branch (`base ← head`) | missing |

The gap that motivated this ADR: the runs list and run header show a bare
commit sha and `push`/`manual`, with **no human-readable context** — you can't
tell *what* a push changed or *why* someone dispatched a run without leaving
Scarab. The normalized `Event` (ADR-0046, `scarab-forge`) deliberately discards
this: `Event::Push` keeps only `{actor, repo, ref, after}` (no message),
`Event::PullRequest` only `{actor, repo, number, head, fork}` (no title), and
`Event::Manual` has no reason. The data exists in every forge's webhook payload
(`head_commit.message`, `pull_request.title`) — it is thrown away at
normalization.

ADR-0028 set the precedent for origin facts: **discrete, independently-nullable
columns, never a bundle blob**, extracting *exactly the facts the UI shows*. This
ADR extends that model rather than inventing a new one.

## Decision (proposed)

### 1. A single `trigger_title` headline, kind-disambiguated

Add one nullable column, `runs.trigger_title` (migration `0032`), holding the
**human headline** of whatever triggered the run, its meaning fixed by the
already-stamped `origin_trigger_kind`:

- `push` → the head commit **subject** (first line of `head_commit.message`).
- `pull_request` → the **PR title**.
- `tag` / `release` → the tag/release name (or release title).
- `manual` / `api` → the operator-supplied **reason** (optional).
- `cron` → the schedule expression; `upstream` → the upstream run id.

Rationale for *one* column over three (`commit_message` / `pr_title` /
`reason`): all three are the same UI slot — "the one line that says what this
run is about" — and the kind already disambiguates. This keeps the runs-list
query flat and the UI branch-free. (The Open questions revisit this.)

Bounds: store the **subject line only** for commits (drop the body); truncate
`trigger_title` to a fixed cap server-side (proposed 200 chars) so a pathological
message can't bloat the row.

### 2. Enrich the normalized `Event` (source of the headline)

`scarab-forge::Event` variants gain the fields the headline needs, populated by
each adapter from the webhook payload:

- `Push { …, message: String }` — the head commit message (adapters read
  `head_commit.message`; GitHub **and** Forgejo both carry it).
- `PullRequest { …, title: String, base: String }` — PR title + base branch
  (`base` also fills a `base ← head` display, see #4).
- `Manual { …, reason: Option<String> }` and `Api { …, reason: Option<String> }`
  — the dispatch reason.

Both adapter crates (`scarab-forge-github`, `scarab-forge-forgejo`) parse these;
their existing payload-parsing unit tests are extended with fixtures that carry
a message/title. The server stamps `trigger_title` in `persist_run_from_ir`
(beside `set_run_origin` / `set_run_pipeline`) via a small
`trigger_title(event) -> Option<String>` extractor.

### 3. Manual dispatch reason — a first-class input

`POST /v1/repos/{org}/{repo}/dispatch` (and the inline `POST /v1/runs`) accept an
optional `reason: String`. The web UI's **New-run** form (`RunPipeline`) grows a
free-text "reason" field (optional). The reason flows `DispatchRequest → Event::
Manual.reason → trigger_title`.

### 4. Read models + UI

- `RunSummaryDto` and `RunStatusResponse` gain `trigger_title: Option<String>`.
- UI renders it as the **secondary line of the Trigger cell** (the Trigger /
  Triggered-by split already shipped): kind on top, title beneath, truncated with
  a full-text tooltip. Runs list mirrors it.
- If #5 (base branch) is adopted, PR rows read `base ← head`.

### Analysis — other provenance details considered

Enumerated so the grill can accept/defer each explicitly:

1. **Commit author vs pusher** — GitHub distinguishes `head_commit.author` from
   `sender`. Actor today = sender. *Proposed: defer* — one "who" is enough for
   v1; revisit if reviewers ask "who wrote this" vs "who pushed it".
2. **PR base branch** (`base ← head`) — cheap (in the PR payload), high value for
   reading a PR run. *Proposed: include* (the `base` field in #2).
3. **Compare/diff URL** for a push (`repo_url/compare/<before>..<after>`) — needs
   the `before` sha (not currently in `Event::Push`). *Proposed: defer* — the
   commit deep-link already exists; add only if requested.
4. **Actor / author avatar** — pure UI polish, needs a forge user→avatar lookup
   (new port surface). *Proposed: defer.*
5. **Event timestamp** (when the push/PR happened) — nearly always == run
   `created_at`. *Proposed: skip* (redundant).
6. **PR action** (opened / synchronize / reopened) — marginal for display.
   *Proposed: skip.*

## Alternatives considered

- **A `trigger_context` JSON blob** instead of a `trigger_title` column — richer,
  but violates ADR-0028's "discrete columns, not a bundle" and pushes
  kind-branching into the UI. Rejected unless the grill decides the structured
  fields (base, author, compare) are all wanted at once.
- **Three discrete columns** (`commit_message`/`pr_title`/`dispatch_reason`) —
  more honest per-kind, but they occupy the same UI slot and only one is ever
  non-null per run. Rejected for flatness; re-open if any needs independent
  querying/filtering.
- **Fetch on read** (call the forge for the message/title at render time) —
  rejected: couples the read path to forge availability + rate limits, and the
  data is immutable, so stamp-at-creation is correct.

## Consequences

- Migration `0032_run_trigger_title.sql` (nullable, no backfill — pre-migration
  runs show no headline, exactly as origin/pipeline degrade).
- `Event` enum changes ripple to both adapters + their tests, the trigger/webhook
  path, and the dispatch API. This is the bulk of the work and the main risk
  (webhook payload shape per forge).
- No behavior change to scheduling/admission — provenance is display/audit only,
  never load-bearing (same contract as ADR-0028 origin).

## Open questions (for the grill)

1. **One `trigger_title` vs discrete columns vs JSON** — is the single-headline
   model right, or will filtering/searching by commit message or PR title be
   wanted (which favors discrete columns)?
2. **Commit subject vs full message** — store only the subject line, or the full
   body too (with UI "show more")? Truncation cap?
3. **Base branch** — include `PullRequest.base` now, or defer with the rest?
4. **Manual reason: optional or required?** Required reasons improve audit but add
   dispatch friction. Default optional?
5. **Where does the headline live in the UI** — a secondary line under Trigger
   (proposed), or its own "summary" cell / the run H1?
6. **Fork PRs** — a fork PR's title is attacker-controlled text; it's display-only
   and already escaped by the UI, but worth confirming no interpolation path
   consumes `trigger_title`.
7. **Scope of the `Event` change** — is enriching the shared `Event` acceptable,
   or should provenance ride a side-channel to keep `Event` lean for
   trigger-matching?
