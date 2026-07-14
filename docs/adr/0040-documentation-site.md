# 0040. Documentation site: Astro Starlight, in-repo, DESIGN.md-branded

- **Status:** Accepted
- **Date:** 2026-07-14
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0028](0028-ui-stack.md) (SolidJS UI, `ui/`), [0012](0012-api-surface.md) (REST/OpenAPI — `openapi.json` is the reference source), [0016](0016-code-architecture.md) (docs-as-code, one repo)
- **Consumes:** `docs/adr/**`, `CONTEXT.md`, `openapi.json`, `docs/DESIGN.md` (the design system)

## Context

Scarab is a *tool*, and a tool needs a published site. The forces:

- **Content is lopsided toward design, today.** The valuable, *real* artifacts that exist now are
  the 41 ADRs + `CONTEXT.md` (the "why"). The artifacts a normal doc site sells — install,
  tutorials, pipeline authoring, CLI/API reference — are largely **future** content, because the
  product is "design + scaffolding" (README). A site must not document vaporware.
- **The chosen editorial stance is operator-primary.** Lead with the *adopter/operator*
  (run it, evaluate it, configure it); keep the design rigor available but **subordinate** in a
  "Tech" section. So the IA is B-shaped even though today's content is C-heavy — pages get
  published only when backed by shipped reality.
- **We already own the two strongest inputs.** `openapi.json` (ADR-0012) makes an API reference
  *free and always-correct*; the local dev harness (`just up`/`just demo`, `dev/`) makes a
  "Run locally" quickstart *true today*. These are the tent-pole real pages.
- **The stack is Vite/JS already.** `ui/` is SolidJS on Vite. A React-and-webpack doc framework
  would import a second, heavier toolchain for capabilities (versioning, i18n, blog) we do not
  need yet.
- **We have a real design system.** `docs/DESIGN.md` defines the Scarab identity (carapace-green
  surface ladder, copper *outline-only*, Inter + JetBrains Mono, zero shadows, rough.js'd Lucide
  **doodles** as "the soul"). The site must wear it — screenshots of the product must not look
  out of place on the page they sit on.

## Decision

1. **Tool = Astro Starlight.** Build the site with **Astro Starlight**, not Docusaurus.
   Starlight is Vite-native (matches `ui/`), ships zero-config offline search (Pagefind), renders
   the OpenAPI reference via `starlight-openapi`, and is light to run. Docusaurus's headline wins
   (doc versioning, i18n, blog, plugin marketplace) are things we do not need yet; **versioning is
   deferred** to a later add ("a version widget", not a v1 feature). mdBook is rejected — it is a
   book, weak at the landing/screenshot and OpenAPI jobs we require.

2. **In-repo; `ui/` becomes a container.** The site lives **in this repo** so it can source ADRs
   and `openapi.json` *in place* with no cross-repo sync. `ui/` is promoted to a **container**
   (like `crates/`) holding independent front-end projects:

   ```
   crates/                 # Rust workspace members (unchanged)
   docs/                   # canonical source: ADRs, handoffs, DESIGN.md (UNCHANGED)
   ui/
     scarab-web-ui/        # the SolidJS app (moved from ui/)
     scarab-docs-ui/       # the Starlight site (new)
   ```

   `docs/` is **not renamed** — it is the source of truth (41 ADRs cross-link by relative
   `docs/adr/NNNN` paths; renaming it is expensive and pointless). `scarab-docs-ui/` is a
   self-contained Astro project sharing **nothing** with `scarab-web-ui/` (own `package.json`,
   own build).

3. **Single source of truth — read in place, never copy.** The site renders `docs/adr/**` via an
   Astro content-collection glob (or symlink) and the API reference from `../../openapi.json` via
   `starlight-openapi`, both at build time. No duplicated markdown; ADRs stay authored in `docs/`.

4. **IA is operator-primary, published honestly.** Sidebar: **Home** (landing: the durability
   wedge, screenshots) · **Get Started** (Run locally — real today; Deploy with Helm — **stub**) ·
   **Guides** (pipeline authoring; grows as slices land) · **Configure** (ENV/config reference) ·
   **Reference** (API, auto-generated) · **Tech** (`CONTEXT.md` + ADRs, sourced in place). Pages
   exist in the nav only when backed by shipped reality; aspirational pages are clearly-marked
   stubs, never a broken headline.

5. **Hosting = GitHub Pages, release-gated.** Build in a `.github/workflows/` Action and publish to
   GitHub Pages at subpath `base: '/scarab-ci/'`. Trigger on **`push: tags: 'v*'` + `workflow_dispatch`**
   — tags are the production path once releasing; the manual dispatch is the escape hatch to
   publish on demand during the pre-tag build-out. (No auto-publish on every `main` commit.)

6. **Branding = `docs/DESIGN.md`, tuned for documentation.** Port the palette onto Starlight's
   `--sl-color-*` tokens (carapace ladder backgrounds, `--emerald` accent, `--copper`
   outline-only), Inter + JetBrains Mono, zero shadows. Tuned for docs: **OS-respecting** light +
   dark (both mapped to the palette; screenshots hairline-framed in light mode so a dark shot never
   floats), looser reading density than the dense control plane, screenshots styled as
   hairline-bordered cards. **Doodles run throughout** (the identity), honoring DESIGN.md's
   guardrails (background layer, 5–10% opacity, `pointer-events: none`, ≤2 per page). The rough.js
   pass is a **separate offline generator, not a runtime or per-build step**: a standalone script
   reads source (Lucide) SVGs → runs them through rough.js **once** → **writes the rough'd SVGs to
   a committed assets directory**. The site consumes those static SVGs and never depends on
   rough.js (mirrors the existing `openapi-typescript` `gen` pattern — regenerate on demand, commit
   the output). **Motion is best-effort** and operates on the static SVGs: parallax drift + SVG
   `stroke-dashoffset` draw-on, gated behind `prefers-reduced-motion`; **no Lottie** (it means
   hand-authored animation + a runtime lib, violating DESIGN.md §8 "Lucide + rough.js, nothing
   else"). If motion gets fiddly, ship static doodles — nothing structural is lost.

## Consequences

- **+ The site is real from day one** despite a pre-product state: ADRs + `CONTEXT.md` (Tech), the
  OpenAPI reference, and a working local quickstart are all true content, not placeholders.
- **+ Zero content drift.** ADRs and the API reference render from their canonical files in place;
  a PR that adds an ADR or changes the API updates the site in the same commit.
- **+ Toolchain coherence.** A second front-end project, but same Vite family as `ui/`; no
  React/webpack toolchain added.
- **− `ui/` reorg has a blast radius** (mechanical): the deeper nesting shifts relative paths —
  `scarab-web-ui`'s gen script `../openapi.json` → `../../openapi.json`; `Justfile`
  `npm --prefix ui` → `ui/scarab-web-ui`; `.gitignore` → `ui/**/node_modules`, `ui/**/dist`; prose
  refs in `DESIGN.md`/handoffs/ADR-0028. Done as its own commit before scaffolding the site.
- **± The doodle system is a decoupled generator, not site-build coupling.** DESIGN.md's SolidJS
  runtime component is *not* reused; instead a standalone script rough.js-processes source SVGs
  **once** and commits the output. This removes rough.js from the site's runtime and build
  entirely — the site just serves static SVGs — so the only real work is the one-time generator +
  a small client motion script; the framework-port risk goes away.
- **− Release-gated publishing means the site stays dark until the first `v*` tag** (mitigated by
  `workflow_dispatch`). Between tags the published Tech section lags `main` — acceptable, and
  arguably desirable (published = a coherent snapshot).
- **Follow-ups (out of scope here):** authoring + distributing the Helm chart (the "Deploy with
  Helm" page is a stub until it exists); real screenshots (once `scarab-web-ui` + server render
  something worth showing); doc versioning (the deferred "widget").

## Alternatives considered

- **Docusaurus** — the user's initial lean. Rejected: React + its own webpack build is heavier and
  off-stack; its differentiators (versioning, i18n, blog, plugin marketplace) are all deferred
  needs. Starlight covers everything we need now at a fraction of the weight, and stays Vite-native.
- **mdBook (Rust-idiomatic)** — rejected: strong for a book, but weak at the landing/screenshot job
  we explicitly want and has no first-class OpenAPI story.
- **Separate docs repo** — rejected: would force syncing `docs/adr/**` and `openapi.json` across
  repos (submodule/copy/publish-artifact), pure overhead for a solo pre-1.0 project, and it rots.
  In-repo keeps a single source of truth.
- **Rename `docs/` → free up "docs" for the site** — rejected: `docs/` is the canonical source with
  41 ADRs cross-linked by relative path; the rename is expensive and buys nothing. The `-ui`
  suffix on `scarab-docs-ui` disambiguates "the site app" from "`docs/` the content" instead.
- **Lottie / hand-authored animated doodles** — rejected: violates DESIGN.md §8 (doodles are
  Lucide→rough.js, never hand-authored) and adds a runtime animation lib. The on-brand motion
  (parallax + rough.js draw-on) is cheaper and truer to "a little alive".
