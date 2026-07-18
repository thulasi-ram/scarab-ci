# 0054. Product surface: embedded UI, run cancellation, API/CLI truthfulness

- **Status:** Accepted
- **Date:** 2026-07-17
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0028](0028-ui-stack.md) (UI stack), [0012](0012-api-surface.md) (API)

## Context

The user-facing surface is partly a facade (2026-07-16 audit): the web UI is
**not served in production** (no static serving, no CORS, the Dockerfile excludes
`ui/`), there is **no run-cancel API** (and step Pods leak because
`executor.cancel` is never called), `openapi.json` is hand-curated and can drift
with **no CI gate**, and 4 of 5 CLI subcommands are **stubs that exit 0** (a
`scarab validate` in CI passes while doing nothing).

## Decision

### Embed the built UI in `scarab-server`

The compiled UI bundle is **embedded in the binary** (rust-embed/include_dir) and
served via an SPA fallback route — **same-origin, no CORS**, one artifact. This
reinforces CONTEXT §7 invariant 5 ("the UI eats the same API as everyone else")
and the single-binary ethos (§5). The Dockerfile builds the UI and embeds it; the
UI versions with the server (acceptable, even desirable, for a self-hosted
product).

### Run cancellation is a first-class API

`POST /v1/runs/{id}/cancel` → transition to `Cancelled` **and tear down Pods**
(wires the existing `cancel_run` plus the never-called `executor.cancel`,
SIGTERM+grace per ADR-0020). Closes the "no cancel route / Pods leak" gap; the UI
Cancel control (currently disabled) is enabled.

### API/CLI must not lie

- **OpenAPI drift gate:** a CI test diffs the committed `openapi.json` against the
  freshly-generated document and **fails on drift**; the typed UI client is
  regenerated in the same step. (The spec must cover *all* routes, not the 14 of
  ~23 it lists today.)
- **CLI:** implement the stubbed subcommands — `validate` (offline IR compile),
  `lint` (incl. the ADR-0045 missing-`clone` rule), `logs`, `restart`. **Until a
  subcommand is real, its stub exits non-zero**, never 0 — no silent pass.
- **Dashboard:** wire the faked inbox/activity/repos/environments to real
  endpoints (`listRuns`, catalog, environments) and delete the `catalog.ts`
  mock/`enrichProvenance` fabrication; provenance comes from real run data.

## Consequences

- Docker build gains a UI stage; an embed crate; an SPA fallback route.
- A cancel endpoint + Pod teardown; a CI OpenAPI-drift job; real CLI subcommands;
  a de-mocked dashboard.

## Alternatives considered

- **Serve the UI separately (static host/CDN):** independent caching/deploy, but
  needs CORS, a second artifact, and version coordination. Rejected for embedding
  — simpler ops and same-origin honesty for a self-hosted product.
- **Leave CLI stubs exiting 0 until implemented:** silent-pass footgun in user
  CI. Rejected — non-zero until real.
