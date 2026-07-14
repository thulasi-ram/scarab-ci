# Handoff — building the documentation site (ADR-0040)

Implements the docs-site decision in
[`docs/adr/0040-documentation-site.md`](../adr/0040-documentation-site.md). Read that ADR and
[`docs/DESIGN.md`](../DESIGN.md) (the design system — palette, fonts, the doodle system) before
starting. This is **doc-decided but unbuilt** — no `.github/`, no site, no Helm chart exists yet.

## The one-paragraph model

The site is an **Astro Starlight** project at `ui/scarab-docs-ui/`, in-repo, that **sources its
best content in place**: it renders `docs/adr/**` and `CONTEXT.md` (the "Tech" section) and the API
reference from `../../openapi.json` (via `starlight-openapi`) at build time — **no copies**. It is
**operator-primary** in IA but published **honestly**: only pages backed by shipped reality go live;
everything else is a clearly-marked stub. It wears `docs/DESIGN.md` (carapace-green, copper
outline-only, Inter + JetBrains Mono, zero shadows, rough.js doodles throughout). It publishes to
**GitHub Pages** on **tag + manual dispatch**, not on every `main` commit.

## Do this in three commits (in order)

### Commit 1 — reorg `ui/` into a container (mechanical, no site yet)

`ui/` becomes a container like `crates/`. This shifts relative paths one level deeper.

```
git mv ui/... → ui/scarab-web-ui/     # move the whole current SolidJS app down one level
```

Then fix the paths the deeper nesting broke:

- `ui/scarab-web-ui/package.json` — the `gen` script `openapi-typescript ../openapi.json`
  → **`../../openapi.json`**.
- `Justfile` — the `ui:` recipe: `npm --prefix ui` → **`npm --prefix ui/scarab-web-ui`**.
- `.gitignore` — `/ui/node_modules` + `/ui/dist` → **`ui/**/node_modules`** + **`ui/**/dist`**.
- Prose references (cosmetic, do them so grep stays honest): `docs/DESIGN.md` line 11,
  `docs/adr/0028-ui-stack.md`, `.env.local.example` line 16, the handoffs that mention `ui/`.

Verify: `just ui` still starts the Vite dev server; `npm --prefix ui/scarab-web-ui run gen`
regenerates `src/api/schema.ts` from the root `openapi.json`.

### Commit 2 — scaffold `ui/scarab-docs-ui/` (Starlight + sourcing + IA)

- **Scaffold:** `npm create astro@latest ui/scarab-docs-ui -- --template starlight`.
  **npm** (match `scarab-web-ui`; do not introduce pnpm). Self-contained: own `package.json`, own
  `node_modules`, shares nothing with `scarab-web-ui`.
- **Astro config** (`astro.config.mjs`):
  - `site` + `base: '/scarab-ci/'` (GitHub Pages project subpath).
  - Starlight `title: 'Scarab'`, `tagline: 'Your pipeline is a workflow that survives crashes.'`
  - Pagefind search is on by default — leave it.
- **Source ADRs in place (no copy).** Point an Astro content collection glob (or a symlink
  `src/content/docs/tech/adr → ../../../../docs/adr`) at `../../docs/adr/**`. Filter out
  `_TEMPLATE.md`. Add `CONTEXT.md` (repo root) as the Tech section's lead page. Confirm ADR
  relative cross-links (`0038-...md`) resolve under the Tech route.
- **API reference:** add `starlight-openapi`, point it at **`../../openapi.json`**. This is the
  first *real* Reference page — verify it renders the current spec.
- **IA / sidebar** (publish honestly — stubs are explicit, never broken headlines):
  - **Home** — landing: durability wedge, screenshot slots (placeholders for now).
  - **Get Started** — *Run locally* (write from the real `just up`/`just demo`/`dev/` flow) ·
    *Deploy with Helm* → **stub** ("🚧 lands with the chart", ADR-0040 scope note).
  - **Guides** — pipeline authoring (YAML/CEL, invoke, matrix). Start minimal; grows with slices.
  - **Configure** — ENV / config-mechanism reference (server env vars, secrets, environments).
  - **Reference** — the auto-generated API docs.
  - **Tech** — `CONTEXT.md` then the ADRs (sourced in place).

Verify: `npm --prefix ui/scarab-docs-ui run dev` serves the site; ADRs + API render; search works.
Add a `Justfile` recipe **`docs:`** mirroring `ui:` (`npm --prefix ui/scarab-docs-ui install` +
`run dev`).

### Commit 3 — branding (DESIGN.md → Starlight) + the doodle component

This is the real work. Port [`docs/DESIGN.md`](../DESIGN.md) faithfully; §2/§3/§9 are the
authority for exact hex + fonts.

- **Custom CSS** (Starlight `customCss`): map the carapace palette onto `--sl-color-*`.
  - Backgrounds from the ladder (`--carapace-black #0e1310` → `--pine-deep` → `--pine` →
    `--emerald-surface` → `--emerald-elevated`); accent = **`--emerald #2ea77f`** (links, active
    sidebar, hover `#3fc79a`); text soft-white/sage/muted-sage. **Zero shadows.**
  - **`--copper #c0873f` is outline-only** — hairline borders / featured outlines, **never a fill
    or a background** (DESIGN.md §8). Do not let Starlight's default accent-as-fill leak copper in.
  - **Fonts:** self-host **Inter** (UI) + **JetBrains Mono** (code/inline code). Two typefaces
    only.
- **Tuned for docs** (the deltas from the app):
  - **OS-respecting** light + dark (Starlight's built-in toggle). Map **both** themes to the
    palette; do not ship a generic light theme. In **light** mode, frame screenshots with a
    hairline `--border` so a dark product shot doesn't float.
  - **Looser reading density** than the dense control plane — comfortable line-height, sane measure
    for long prose.
  - **Screenshots as first-class cards** — `--emerald-surface` inset, hairline `--border` (copper
    low-alpha for a featured one), radius 8px, zero shadow (DESIGN.md §4 cards).
- **Doodles throughout (the identity).** Do **not** re-port DESIGN.md's SolidJS runtime component
  and do **not** run rough.js in the site's runtime or per-build. Instead: a **separate offline
  generator** produces committed static SVGs; the site just serves them.
  - **The generator (a standalone `gen:doodles` script, mirrors `openapi-typescript`'s `gen`):**
    reads each source Lucide icon SVG → runs its `<path d>` through rough.js **once** at the
    canonical settings (`roughness 0.1`, `curveStepCount 5`, `strokeWidth 1.6`, stroke `--copper`,
    no fill — DESIGN.md §5) → **writes the rough'd SVG to a committed assets dir** (e.g.
    `src/assets/doodles/*.svg`). Use `rough.generator()` (path data, no DOM) or `rough.svg()` under
    a headless DOM — either is fine since it runs offline, not in the browser. Run it on demand and
    commit the output; regenerate only when the motif set or rough settings change.
  - **The site side is trivial:** an Astro `<Doodle icon rotate size>` component inlines/references
    a committed SVG. **No rough.js dependency in the site's build or runtime.**
  - **Guardrails (hold these):** background layer only, `pointer-events: none`, opacity 5–10%,
    rotated ~8–20°, scaled 120–260px, **≤2 per page**, motif matched to the page (`bug` = house
    motif; `git-branch`, `container`, `key-round`, `shield-check`, `timer`, …). Never on controls.
  - **Motion (best-effort, `prefers-reduced-motion`-gated; operates on the static SVGs):**
    1. **Parallax drift** — IntersectionObserver + `transform: translateY` slower than scroll.
    2. **Draw-on** — animate SVG `stroke-dashoffset` when the doodle enters view, so the rough line
       "sketches itself in" (the most on-brand motion; DESIGN.md line 4, "a little alive").
    - **No Lottie** (hand-authored + runtime lib, violates DESIGN.md §8). If motion is fiddly, ship
      **static** doodles — nothing structural is lost.

### Commit 4 (or fold into 2) — GitHub Pages workflow

- `.github/workflows/docs.yml`: build `ui/scarab-docs-ui/` and deploy to **GitHub Pages**.
- Trigger: **`on: { push: { tags: ['v*'] }, workflow_dispatch: {} }`** — tags for production,
  manual dispatch as the pre-tag escape hatch. **Not** `push: main`.
- Use the official `withastro/action` (handles Pages build/upload) + `actions/deploy-pages`.
- Enable Pages in repo settings (source: GitHub Actions). Confirm the subpath `base` matches
  (`/scarab-ci/`) so assets resolve at `thulasi-ram.github.io/scarab-ci/`.
- First publish: trigger `workflow_dispatch` manually (no `v*` tag exists yet).

## Out of scope (explicit — do NOT balloon this)

- **The Helm chart itself** — authoring `Chart.yaml`/`values.yaml` + publishing to a chart
  repo/OCI registry is *product/ops* work. Here the "Deploy with Helm" page is a **stub** only.
- **Real screenshots** — placeholders now; capture real ones once `scarab-web-ui` + server render
  something worth showing (a running-pipeline view). Follow-up.
- **Doc versioning** — deferred (ADR-0040); a later "version widget", not a v1 feature.

## Guardrails (don't regress these)

- **Never copy `docs/adr/**` or `openapi.json` into the site** — render in place. Single source of
  truth stays in `docs/` and repo root.
- **Never rename `docs/`** — ADRs cross-link by relative `docs/adr/NNNN` path.
- **Copper is outline-only.** No copper fills/backgrounds. Zero shadows. Two typefaces.
- **Publish honestly** — a nav entry means the page is real or an explicit stub, never a broken
  "coming soon" headline in the operator-primary sections.
- **`scarab-docs-ui` shares nothing with `scarab-web-ui`** — separate Astro project, separate build.
