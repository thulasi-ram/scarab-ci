# Docs-site + screenshots refresh — progress & handoff

**Started:** 2026-07-21 · **Driver:** autonomous orchestrator (Claude) · **Owner:** thulasi

## Goal
Refresh the docs site, README(s), helm/local-setup docs, and regenerate app
screenshots to reflect the current product. Scope decisions from owner:
- **Verify scope:** docs-only. Read helm/local configs to document accurately;
  do NOT spin up colima/kind to verify end-to-end.
- **Screenshot theme:** dark.
- **Tracking:** this progress doc + git-bug only for real defects found.

## Known facts (recon 2026-07-21)
- Docs site: `ui/scarab-docs-ui` (Astro Starlight). Serve: `just docs`.
  Content: `ui/scarab-docs-ui/src/content/docs/` (index.mdx,
  get-started/{run-locally,deploy-helm}.mdx, configure/reference.mdx,
  guides/authoring.mdx, tech/context.md, tech/adr/*.md).
- Docs screenshots: `ui/scarab-docs-ui/src/assets/{dashboard,run-detail}.jpg`
  (imported by index.mdx), mirrored in `public/`.
- README screenshots: `docs/assets/screenshots/{dashboard,run-detail}-{light,dark}.jpg`
  via `<picture>` in root README.md.
- Web UI: `ui/scarab-web-ui` (SolidJS). Dev: `just ui` (Vite :5173, proxies /v1,
  needs deploy/local-proc/.env). **No mock/demo mode exists** — must add a temp one.
- Screens: `/` Repos · `/:org/:repo` RepoView (Environments/Secrets/Settings tabs
  hold env+secret create) · `/:org/:repo/run` RunPipeline · `/:org/:repo/runs/:id`
  RunDetail.
- Helm chart: `deploy/helm/scarab/`. Deploy modes: deploy/{helm,local-proc,local-helm}.
- "Deploy with Helm" docs page is a stub (ADR-0040).

## Decisions (to red-team & surface to owner at end)
- D1: Screenshots via a **temporary frontend mock mode** in scarab-web-ui
  (fixture API responses behind a VITE flag), reverted before commit — rather than
  spinning up the full proc stack + seeding. Precedent: prior dashboard-redesign
  temp DEMO flag. Rationale: reliable/autonomous, no infra flakiness.

## Progress log
- [x] Recon + scope questions answered.
- [ ] Docs content audit → per-file change list.
- [ ] README + deploy docs audit → change list.
- [ ] Screenshot pipeline (mock mode → capture dark → save → revert).
- [ ] Apply doc edits.
- [ ] Red-team decisions; summarize for owner.

## Resolved during work
- Environments API IS real: `/v1/repos/{org}/{repo}/environments` (+ `/{name}`,
  `/{name}/deployments`), `EnvironmentStore` + Postgres impl, UI calls it.
  Feature-gated: store is `Option`, returns 404 if not wired at startup.
  (Docs-content audit's "no environments route" claim was wrong; ADR-0037 correct.)
- Both audits agree: "GitHub adapter `unimplemented!()`" is stale across README +
  docs; adapter is implemented (webhook ingest + commit-status posting); Forgejo
  adapter also exists (multi-adapter, ADR-0046).

## Defect candidates (file to git-bug at end if confirmed)
- DEF1: Helm chart has no first-class value for `SCARAB_FORGEJO_WEBHOOK_SECRET`
  (template only renders the GitHub one) despite Forgejo being a shipped adapter.
  CONFIRMED at templates/secret.yaml:24-26. **FILED git-bug `f3da2aa`** (enhancement,
  area:infra/area:forge). Fix = add secrets.forgejoWebhookSecret mirroring GitHub.

## Red-team verdicts (acted on)
- Screenshots verified against real components: dashboard/environments/secrets REAL;
  "Debug shell" + "Concurrency" REAL. ONE fabrication: run-detail provenance strip
  showed `toolchain=… features=…` (no such field in real UI) → re-capturing.
- Q1 README light/dark → collapse <picture> to single dark <img> (in flight).
- Q2 secrets shot zoomed/truncated → re-capture at consistent scale (in flight).
- Q3 Forgejo chart gap → filed git-bug f3da2aa (done).

## Progress (writers)
- [x] README + helm README + values.yaml accuracy fixes applied (no commit).
- [x] Docs-site pages fixed: index.mdx (adapters/k8s/sidecar/CI moved to implemented,
  honest "not hardened at scale" caveat), run-locally.mdx (dev/→deploy/local-proc/,
  +just docs/local-helm), deploy-helm.mdx (full rewrite to real chart story),
  reference.mdx (env table expanded from config.rs, 3 ADR link slugs fixed). No commit.
- [x] guides/authoring.mdx expanded into a full guide; all YAML verified against
  .scarab/ci.yaml + serde structs + catalog/dispatch tests. No commit.
- [x] ADR sync: NOT a real gap — `scripts/sync-content.mjs` (predev/prebuild) auto-
  mirrors ADRs; `tech/` is gitignored build output. Ran it so 0056/0057 present now;
  they'd auto-appear on next build regardless. Sidebar autogenerates. No commit.
- [x] screenshots captured (dark, mock mode, reverted clean): dashboard, run-detail,
  environments, secrets → scratchpad/shots/.
- [x] dashboard + run-detail wired into docs-site src/assets/*.jpg + README
  docs/assets/screenshots/*-dark.jpg (light variants left; see red-team Q1).
- [x] run-detail re-captured (truthful fact strip) + secrets re-captured (full chrome,
  consistent scale); both re-wired. env + secret figures embedded in authoring.mdx
  "Environments, secrets & gates" section (plain-markdown images, mirrors index.mdx).
- [x] README <picture> collapsed to single dark <img>; -light.jpg variants git-rm'd.
- [x] docs site build passes (66 pages, images optimized).

## STATUS: SHIPPED as PR #48 (branch docs/site-refresh-and-screenshots).
Owner-approved route. Later owner asks, all applied:
- Removed stale WIP/stub sidebar badges + reference WIP note.
- deploy-helm reframed: deploys on ANY k8s (local kind/colima OR live cluster);
  removed "local-oriented today" hedging; added values.yaml example.
- ADR refs pulled from inline prose into `## References` citations at page bottom
  (all 5 docs content pages + root README + helm README).
PR: https://github.com/thulasi-ram/scarab-ci/pull/48

### Uncommitted working-tree changes
- Docs: index/run-locally/deploy-helm/reference/authoring .mdx (accuracy + capability
  expansion + 2 new figures).
- README.md (accuracy fixes + single-dark screenshots); helm README + values.yaml
  (config.rs ref + Forgejo env var doc).
- Assets: docs-ui dashboard/run-detail .jpg replaced; environments.jpg + secrets.jpg
  new; README dashboard/run-detail -dark.jpg replaced; -light.jpg deleted.
- ui/scarab-web-ui: CLEAN (screenshot mock reverted).
- ADR mirror (tech/adr 0056/0057): gitignored build output, auto-synced — not in status.

### git-bug filed
- `f3da2aa` — Helm chart: add first-class secrets.forgejoWebhookSecret (enhancement).

## Decisions taken autonomously (red-teamed) — for owner sign-off
- D1: screenshots via temporary frontend mock mode (reverted). Red-team confirmed shots
  depict only real UI (1 fabricated field caught + fixed).
- D2: README standardized on DARK single-image (dropped stale light variants).
- D3: env + secret figures placed in the authoring guide (not a new page).
- D4: filed Forgejo chart gap as git-bug enhancement rather than fixing inline
  (out of docs scope).
- Nothing committed — left for owner to review the diff and commit/branch as desired.

## Open questions for owner (surface at end)
- (pending)
