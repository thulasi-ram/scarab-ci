# Handoff — run provenance + run-detail polish (ADR-0056 follow-on)

**Branch:** `feat/adr-0056-rerun-retry-language` (uncommitted working tree,
~24 files). Continues the ADR-0056 run-detail work. Two threads:
(A) **shipped + verified** UI/backend polish; (B) **ADR-0057** (Proposed) —
trigger-context provenance, queued for a `/grill-with-docs` session.

## A. Shipped this session (verify, then commit)

All UI verified in-browser against the running colima-helm server on `:8080`
(Vite dev on `:5175`, `SCARAB_API_URL=http://127.0.0.1:8080`). Backend verified
by `just check`, targeted `cargo test`, the OpenAPI guard, and migration-applies
on real PG.

### UI (ui/scarab-web-ui)

1. **Deck + in-rail fan in the DAG** (`Dag.tsx`) — a step with `attempts > 1`
   renders as a deck (offset shadow-cards + copper `×N`); selecting it fans its
   tries as a vertical in-rail stack that scopes the evidence pane. Try selection
   lives in the graph; `StepPane`'s attempt dropdown is gone. (See
   [[docs/handoffs/steps-unify-graph-timeline.md]] for the deeper writeup.)
2. **Evidence header cleanup** (`StepPane.tsx`) — dropped the redundant step-name
   / "try aN" headers; a tab-bar spinner + a **log skeleton** (400/450ms) signal
   a switch, since cached tries swap instantly.
3. **STEPS / ARTIFACTS headings unified**; DAG rectangles sharpened to 3px; the
   version ("rerun") dropdown enlarged.
4. **Two-level provenance** on the run header (`RunDetail.tsx`) AND the runs list
   (`RepoView.tsx`) — labeled cells / primary+secondary lines, **status floated
   right**, and **Trigger vs Triggered-by split** (`src/trigger.ts` →
   `triggerText`/`triggerIcon`). Pipeline name leads the cluster; **commit + PR**
   are forge deep-links.
5. **Forge links** now come from the backend (`src/forge.ts` builds from
   `repo_url`; `client.repoForgeUrl(org,repo)` looks it up from `listProjects`) —
   no more hard-coded `github.com`.
6. **Beetle empty-state ball fixed** — `ui/brand/ascii/scenes.mjs` `ponderBall`
   was a dashed rim (baked to a broken ring); now a solid rim + sparse flecks,
   re-baked (`cd ui/brand/ascii && npm run bake`; only `ponder-*.json` changed).

### Backend (the "expose provenance" change)

7. **Pipeline name** — migration `0031_run_pipeline.sql` (`pipeline TEXT`), stamped
   in `persist_run_from_ir` from the explicit **`name:`** (new optional
   `PipelineIr.name`, ADR-0057 §"already landed") else the `.scarab/<file>` bare
   name. New ports `set_run_pipeline`/`run_pipeline` (db-postgres + testkit);
   threaded onto `RunSummary` → both `list_runs*` SELECTs → `RunSummaryDto` and
   `RunStatusResponse` (`get_run`).
8. **Forge web URL** — `ProjectDto.repo_url` computed in `list_projects` via
   `web_repo_url(kind, base_url, owner, name)` (GitHub `api.github.com`→`github.com`
   / GHES `/api/v3`→root; Forgejo base is already web). Multi-forge correct.
9. **Optional `name:` override** — `PipelineIr.name` (`scarab-pipeline`, parsed +
   carried through `compile_yaml_with_libs`); test `optional_name_parses_…`.

**Tests added:** `dispatch.rs` (pipeline stamped `"ship"`), `dashboard.rs`
(`repo_url == "https://github.com/acme/api"`), `scarab-pipeline` name parsing.
OpenAPI regenerated: `cargo run -p scarab-server -- --emit-openapi openapi.json`
then `npm --prefix ui/scarab-web-ui run gen`.

### NOT yet done for thread A

- **Not live-rendered.** `:8080` is the old colima-helm image and every existing
  run predates migration 0031 (NULL pipeline, no PR). To *see* pipeline + working
  forge links + PR live: `just local-helm local` (rebuild+redeploy) **and** create
  a fresh run (a **dispatch** shows pipeline + commit link; a **PR webhook** is
  needed to populate a PR cell). The UI degrades gracefully on the stale server
  (commit → plain text, pipeline/PR hidden — confirmed, no crash).
- **Not committed.** Whole arc is uncommitted on the branch.
- **Graph/Timeline sub-tabs** — still deferred (Timeline skipped per user).
- **ArgoCD-style try-stack UX** — user floated it; never scoped (which ArgoCD
  screen?).

## B. ADR-0057 — trigger context (Proposed, GRILL THIS)

`docs/adr/0057-run-provenance-trigger-context.md`. Driver: the Trigger cell shows
only the kind (push/manual/PR) with **no headline** — no commit message, PR
title, or dispatch reason. These are thrown away at `Event` normalization
(ADR-0046); every forge's webhook payload carries them.

**Proposed shape** (all in the ADR): one nullable `runs.trigger_title` headline
(commit subject / PR title / manual reason, disambiguated by `trigger_kind`);
enrich `Event::{Push.message, PullRequest.{title,base}, Manual.reason,
Api.reason}`; both adapters parse it; a manual **reason** input on the New-run
form; expose `trigger_title` on both DTOs; render as the Trigger cell's secondary
line. Analysis of deferred details (commit author, compare URL, avatars, PR
base/action) is in the ADR.

**7 open questions** are listed at the end of the ADR for the grill — the big
ones: single `trigger_title` vs discrete columns vs JSON blob; subject-only vs
full message; manual reason optional vs required; and whether enriching the
shared `Event` is acceptable or provenance should ride a side-channel.

**Do not implement B until the grill resolves those.** When it does, the
implementation mirrors thread A's origin/pipeline pattern exactly (migration →
`Event` + adapters → stamp in `persist_run_from_ir` → DTOs → UI), so the change-map
is already proven.

## Environment / traps

- Run/test via `just` recipes (`just check`, `just test`, `just local-helm`).
  Real-PG db tests: `SCARAB_TEST_DATABASE_URL=postgres://scarab:scarab@127.0.0.1:55432/scarab`
  (the local-proc dev Postgres; the harness provisions an isolated throwaway DB).
- **RTK mangles `grep`/`curl`/`docker` output** — use `rtk proxy <cmd>` for raw,
  or `node -e` to fetch/parse API JSON (the events endpoint is SSE: split on
  `data:` lines).
- `main` isn't fmt-clean — hand-format Rust edits, never repo-wide `cargo fmt`.
- Empty-state beetle: hit any repo route with no runs (e.g. `/zzz/zzz`) — the
  runs endpoint returns `{runs:[]}` for unknown repos.
