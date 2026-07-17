# Scarab Design System

Scarab is a durable-execution, Kubernetes-native CI system. Its interface should
feel the way the product feels: **quietly durable, precise, and a little alive**.
The visual identity is an **engineering blueprint** — a cool paper-white worksheet
marked up with technical registration marks, monospace annotations, and a dark
pine-green console where the machine actually runs. The scarab-beetle heritage
(dark emerald/pine carapace, faint copper sheen) still owns the *dark* mode and
the terminal panels; in the light default it shows up only at the **edges** —
hairlines, corner **rivets**, and the hand-drawn (rough.js) doodles.

> Stack note: the UI is **SolidJS** (`ui/scarab-web-ui/`, ADR-0028), styled with plain
> CSS custom properties in `src/styles.css`. There is no server-rendered
> templating. Decorations are inline SVG rendered by Solid components (§5).

## 0. What changed (and why)

Earlier Scarab led with a dark "beetle carapace" everywhere. It read as product
chrome you couldn't put down. The interface now leads with **light** — a calm,
high-contrast worksheet you operate all day — and keeps the carapace for two
jobs it does best: an opt-in **dark mode**, and the **terminal panels** (logs +
DAG canvas) that stay pine-black in *both* themes so the machine surface always
looks like the machine. The signature is no longer "everything is dark green";
it's **restraint plus technical marks**: gold shows up only as an edge, mono
carries every label, and each container (and CTA) wears small corner
**rivets** — like fasteners on a spec drawing.

Influence: the crisp, blueprint-y, mono-annotated feel of engineering marketing
sites (corner rivets on cards and buttons, dot-grid textures, spec-sheet framing)
— adapted to a control plane, kept pine/black, and made light-first. The rivets
replaced an earlier floating corner-bracket treatment that fought bordered/filled
elements; a rivet sits cleanly on any corner regardless of border or fill.

## 1. Visual Theme & Atmosphere

- **Light is the default. Dark is opt-in.** `:root` is the paper theme;
  `<html data-theme="dark">` restores the carapace. A toggle lives in the top
  bar; the choice persists in `localStorage` and a pre-paint guard in
  `index.html` sets it before first paint (no flash).
- **Cool, clean light — never warm.** Light surfaces are a cool near-white
  ladder (base → card → chrome) with a faint green undertone, deliberately not
  a warm/beige paper. Cards are pure white; ink is a near-black pine.
- **The console stays pine-black.** Logs and the DAG canvas are a dark
  pine/black "terminal" in *both* themes — the one place the carapace always
  lives. A dark console nested in a light worksheet reads as intentional, and
  keeps the pine/black identity present even in light mode.
- **Flat and calm.** No box-shadows on flat chrome (the one exception is the
  modal overlay). Borders are hairline. Depth comes from shade + the corner
  marks, not elevation.
- **Emerald is the one working color; gold is only an edge.** Interactive,
  active, and success states are emerald. Gold (copper) appears *only* as
  strokes — hairline outlines, the corner registration brackets/dots, doodle
  ink, and the rare accented **token** (a commit SHA, a trigger glyph). **Gold
  is never a fill.**
- **Technical marks are the signature.** Featured containers and the CTA buttons
  wear four small gold **corner rivets**; pages carry a faint **dot-grid**
  texture; labels are **monospace**. Together they read like a spec sheet, not a
  landing page.
- **Doodles as the soul.** Each page still carries **one or two** rough.js
  line-icon doodles in the background (§5) — the wink; functional UI is never
  doodled.

**Key characteristics**
- Light-first cool-white ladder; opt-in carapace dark; pine-black terminals in both.
- Emerald = the single interactive accent. Gold = edges & rare tokens only, never a fill.
- Corner **rivets** (on cards *and* CTAs) + faint dot-grid = the house signature.
- Three typefaces: **Space Grotesk** (display), **Inter** (body/UI), **JetBrains Mono** (all chrome/labels/machine).
- Zero shadows (modal overlay excepted).

## 2. Color Palette & Roles

Defined as CSS custom properties. **Light is `:root`; dark overrides live under
`[data-theme="dark"]`.** Every rule reads a semantic alias, so a theme swap is
just re-pointing tokens. (The variable *names* are historical — e.g.
`--carapace-black` is now the page base in *either* theme; read them by role,
below, not by name.)

### Surfaces

| Token (role) | Light | Dark |
|---|---|---|
| `--carapace-black` — page + top-bar base | `#f1f4f3` (cool near-white) | `#0e1310` |
| `--pine-deep` — secondary chrome (table head, panel headers, inputs, steps) | `#e7ecea` | `#12211a` |
| `--pine` — primary-button fill | `#14241b` (dark pine) | `#17392a` |
| `--emerald-surface` — cards / panels | `#ffffff` | `#14261d` |
| `--emerald-elevated` — modal, popover | `#ffffff` | `#183024` |

### Terminal (logs + DAG canvas — pine-black in both themes)

| Token | Light | Dark |
|---|---|---|
| `--terminal` — console background | `#0e1a14` | `#090e0b` |
| `--terminal-elev` — DAG nodes | `#16281e` | `#14231b` |
| `--terminal-ink` — console text | `#cfe2d6` | `#a8c0b3` |
| `--terminal-line` — console borders/edges | `#263d31` | `#21372c` |

### Accent — emerald (the one working color)

| Token | Light | Dark | Usage |
|---|---|---|---|
| `--emerald` | `#137a52` | `#2ea77f` | links, active tab/nav, focus, success |
| `--emerald-bright` | `#0f6644` | `#3fc79a` | hover |

### Edge — gold (strokes, rivets, doodles, rare tokens — never a fill)

| Token | Light | Dark |
|---|---|---|
| `--copper` | `#8a6416` | `#c0873f` |
| `--border-copper` | `rgba(138,100,22,.5)` | `rgba(192,135,63,.42)` |
| `--rivet` | `var(--copper)` | `var(--copper)` |

Light gold is deepened so it reads on white. Used for: corner **rivets**,
featured-button outline, `running` badge edge, and the rare accent token (commit
SHA `.rr-sha`/`.sha`, trigger glyph `.tglyph`, gate marker, `inherited` matrix cell).

### Text

| Token | Light | Dark | Usage |
|---|---|---|---|
| `--soft-white` — primary ink | `#0f1a15` | `#f4f8f4` | headings, body |
| `--sage` — secondary | `#46504a` | `#a8c0b3` | secondary text |
| `--muted-sage` — muted | `#79837c` | `#6f8579` | metadata, timestamps, disabled |
| `--on-pine` — text on the dark primary button | `#f2f6f4` | `#f2f6f4` | (light in both, since the fill is dark pine in both) |

### Lines & texture

| Token | Light | Dark | Usage |
|---|---|---|---|
| `--border` | `#dbe1de` (cool gray) | `#244036` | default hairline |
| `--border-soft` | `#e8ecea` | `#1b3228` | softer separators |
| `--dot` | `rgba(15,26,21,.05)` | `rgba(200,224,208,.05)` | dot-grid texture |

### Status

| Token | Light | Dark | Meaning |
|---|---|---|---|
| `--ok` | `#1f8f5c` | `#34b37e` | succeeded |
| `--running` | `#1c8a7c` | `#2f9e8f` | running / pending (badge gets a gold edge) |
| `--danger` | `#c4443b` | `#d1584f` | failed / destructive |
| `--badge-ink` | `#f6f9f6` | `#0b120e` | text on solid status pills |

### Gradients
**None in the interface.** Depth is shade; edges are gold strokes; iridescence
lives only in the doodles.

## 3. Typography

### Families
- **Space Grotesk** (`--font-display`) — display only: page titles (`h1`),
  section headings (`h2`), the run title, the `Scarab` wordmark, repo/card names.
  Weights 500/700. Its geometric-grotesque letterforms (with just enough kink)
  give the headings a distinctive, engineering-adjacent character.
- **Inter** (`--font-ui`) — body & UI: paragraphs, buttons, table body, nav.
  Weights 400/500/600.
- **JetBrains Mono** (`--font-mono`) — **all chrome and machine tokens**:
  eyebrows, section labels, table/column headers, filter pills, badges,
  breadcrumbs, provenance keys, ids, SHAs, durations, log lines. Weights 400/500.

Three typefaces, each with one clear job (display / body / machine). The
**mono-for-chrome** rule is the technical voice: anything that labels or
identifies is mono; prose is Inter; only titles are Space Grotesk.

### Scale

| Role | Family | Size | Weight | Tracking |
|------|--------|------|--------|----------|
| Display / Hero (`h1`) | Space Grotesk | 40px | **700** | -1.4px |
| Run title | Space Grotesk | 22px | 700 | -0.6px |
| Section heading (`h2`) | Space Grotesk | 20px | 600 | -0.4px |
| Card / repo name | Space Grotesk | 15px | 600 | -0.3px |
| Body / button | Inter | 14–16px | 400–600 | -0.16px |
| Eyebrow / label / column head | **Mono** | 10–11px | 500 | +0.8–1.2px, UPPERCASE |
| Filter pill / chip | Mono | 11.5px | 400 | 0 |
| Code / id / SHA / log | Mono | 12–13px | 400 | 0 |

### Principles
- **Punchy display, comfortable body.** Headlines are Space Grotesk 700 with
  tight negative tracking (film-title weight); body is Inter at 1.5 for readability.
- **Uppercase mono = structure.** Every micro-label — section headers, table
  columns, provenance keys, form labels — is mono, uppercase, positive tracking.
  This is the single biggest carrier of the "engineering worksheet" feel.
- **Mono for anything machine.** Ids, SHAs, durations, log lines, secret names.
- **Space Grotesk only for titles.** Don't spread the display face into body,
  labels, or buttons — its character reads as personality precisely because it's
  reserved for the few things that name a page or object.

## 4. Signature System — rivets & texture

Two cheap, CSS-only devices give Scarab its spec-sheet identity. Use them as
described; don't invent new decoration.

### Corner rivets (the signature)
Featured containers — and the CTA buttons — wear **four small square rivets** at
the corners, like fasteners on a panel. Each is painted as a single `::after`
overlay (`position:absolute; inset:0`) whose `background-image` stacks four
3px `linear-gradient` squares, one per corner. Because it's a full-box overlay,
it lands cleanly on **any** element regardless of its own border or fill — this
replaced an earlier inset L-bracket that fought bordered/filled cards.

- **The `.riveted` class** is the reusable form; `--rivet` sets the color and
  `--rivet-inset` the corner offset.
- **Containers — used sparingly.** Only the landing **repo cards** (`.repo-card`)
  and the run **summary block** (`.prov`) get container rivets (gold, 7px inset,
  ~60% opacity). Panels, the run list, environments, and secrets stay clean —
  their CTA carries the motif instead. Over-riveting reads as noise.
- **CTA buttons** (`.btn-primary`, `.btn-copper`): 2.5px rivets, 4px inset. The
  dark-pine primary uses **light** rivets (`--on-pine`); the gold-outline
  featured button uses **gold** rivets. Gold stays an edge — never a button fill.

Rule of thumb: rivet the landing cards, the run summary, and the CTAs — nothing else.

### Dot-grid texture
`.page::before` lays a faint `radial-gradient` dot grid (24px pitch, `--dot`
color) behind all content — a worksheet backdrop, never noise. It sits below
content (`z-index:0`) alongside the doodles.

## 5. Doodle & Decoration System

Unchanged from the beetle era, and still the one place Scarab lets its hair
down. Take clean **Lucide** line icons and re-draw them **hand-sketched via
[rough.js](https://roughjs.com/)** so the sketchiness is generated, not authored.

### Rendering recipe
Feed each Lucide icon's SVG path(s) to `rough.svg().path(d, opts)` (see
`components/Doodle.tsx`):

| Option | Value | Note |
|---|---|---|
| `roughness` | `0.12` | Barely sketchy — a subtle wobble |
| `curveStepCount` | `5` | gentle curve smoothing (`bowing` kept low at `0.5`) |
| `strokeWidth` | `1.6` | |
| `stroke` | copper `#c0873f` | a literal hex — rough.js writes it as an SVG presentation attribute, where CSS custom properties don't resolve |
| `fill` | none | outlines only |

### Placement rules
- **1–2 doodles per page. Never more.** Background layer only
  (`z-index` below text; `pointer-events: none`).
- **Rotated & scaled**: ~8–20°, 120–260px, so they read as texture.
- **Faint**: opacity ~5–8% so they never fight contrast (on paper they're
  ambient; that's intended).
- **Related to the page**: `boxes`/`workflow` on lists, `container` on run
  detail, `key-round` on secrets, `shield-check` on environments, `bug` as the
  house motif.
- **Never on functional controls.**

### Suggested motif set (Lucide names)
`bug`, `git-branch`, `git-commit-horizontal`, `container`, `boxes`, `workflow`,
`waypoints`, `network`, `package`, `terminal`, `key-round`, `shield-check`,
`timer`, `play`/`circle-dot`.

### ASCII beetle scenes (the animated tier)

Above the doodles sits one louder decoration: brand beetles **doing things**,
rendered as ASCII text (`ui/brand/ascii` — same pipeline philosophy as the
doodle generator: bake offline, commit the output, ship no rendering code).
Scenes play at 12 fps by swapping baked text frames; three `<pre>` layers carry
the brand roles (emerald wings / gold body / gray detail) and are colored via
theme tokens, so both themes come free.

Placement is stricter than doodles, because motion competes with content:

- **Docs**: the landing page ONLY — the hero (wing-spread in its nimbus) plus
  at most one accent (the dung-roller). Inner pages keep static doodles.
- **Web-UI**: state moments only — all-clear, empty, loading. Never ambient
  behind live data; an operator surface is scanned, and a permanently moving
  background fights the "what needs me" read.
- **Static ASCII marks** (the traced emblem) follow the doodle rules: faint,
  background, ≤2 per page.
- Players honor `prefers-reduced-motion` (hold an open frame) and skip work
  while the document is hidden.

## 6. Component Stylings

### Buttons
- **Primary** (`.btn-primary`): dark-pine (`--pine`) fill, `--on-pine` light
  text, 14px/600, radius 6px, no shadow, **light corner rivets** (§4). Hover
  fill → `--emerald-bright`. A dark riveted button on white is the strongest,
  calmest call to action.
- **Ghost** (`.btn-ghost`): transparent, `--border` hairline, `--sage` text;
  hover border → `--emerald`. No rivets.
- **Featured / gold** (`.btn-copper`, rare): transparent with a `--copper`
  hairline + gold text + **gold corner rivets** — the only place gold touches a
  control, and still only as an edge.
- **Destructive** (`.btn-danger`): `--danger` text on transparent, danger hairline.

### Cards & panels
- `.card`/`.panel`/`.prov`/`.repo-card`: `--emerald-surface` fill, `--border`
  hairline, radius 8–12px, zero shadow. Panel headers (`.panel-h`) are a
  `--pine-deep` band with a mono uppercase label. Only `.repo-card` and `.prov`
  wear rivets (§4); plain panels stay clean.

### Terminals (logs + DAG)
- `.logs` and `.dag` fill `--terminal` (pine-black) in both themes, with
  `--terminal-ink` text and `--terminal-line` edges. DAG nodes (`.dnode`) use
  `--terminal-elev`; the selected node takes an emerald ring, a `running` node a
  gold edge. Edges are `--terminal-line` strokes.

### Tables & run list
- Header row: **mono** 10–11px uppercase `--muted-sage` on `--pine-deep`. Body
  rows 14px `--sage`, `--border-soft` separators, hover → faint emerald wash
  (`color-mix`). SHAs render in gold mono; ids/timestamps in muted mono.

### Status badges
- Pill, radius 6px, **mono** lowercase. `succeeded` → `--ok`; `failed` →
  `--danger`; `running`/`pending` → `--running` **with a gold hairline** (the
  one place a gold edge signals "in motion"); `skipped` → muted outline. Text is
  `--badge-ink`.

### Forms
- Inputs (`.input`): `--emerald-surface` fill, `--border` hairline, mono value
  text; focus → `--emerald` border + a soft emerald focus ring. Labels are mono
  uppercase. Secret values are `type=password` and **write-only** (never rendered back).

### Tabs & filter pills
- Tabs: text buttons, active → `--emerald` text + emerald underline. Filter
  pills (`.fpill`): mono, hairline; active → emerald border + emerald text.

### Modal
- Elevated (`--emerald-elevated`), a blurred dark scrim, and the single
  permitted soft shadow. Mono uppercase field labels; its primary button is
  riveted, but the modal body isn't.

### Empty states
- No stock photography. A single faint doodle (§5) + one line of `--muted-sage` copy.

## 7. Depth & Elevation

| Level | Treatment | Use |
|---|---|---|
| Flat (0) | no shadow | dominant state |
| Bordered (1) | `1px solid --border` (rivets on repo cards + run summary only) | cards, panels, inputs |
| Chrome | `--pine-deep` band | panel/table headers, top bar |
| Terminal | `--terminal` pine-black | logs, DAG |
| Overlay | `--emerald-elevated` + soft shadow (the one exception) | modal |

**No shadows on flat chrome.** Depth is shade + the corner rivets.

## 8. Do's and Don'ts

### Do
- Lead with light (cool, never warm); keep dark a clean toggle. Never make the
  app *require* dark.
- Keep logs + DAG pine-black in both themes — the console is the machine.
- Use `--emerald` as the one interactive/active/success color.
- Reserve `--copper`/gold for **edges and rare tokens only** — corner rivets,
  hairlines, doodle ink, SHAs, trigger glyphs.
- Put mono on every label, header, id, and machine token — it *is* the voice.
- Rivet only the landing repo cards, the run summary block, and the CTA buttons; keep the dot-grid faint.
- Set only titles in Space Grotesk (the display face); body in Inter.
- Sprinkle **1–2** rough.js doodles per page — background, faint, page-relevant.

### Don't
- Don't use gold/copper as a **fill**, or as body text.
- Don't add box-shadows to flat chrome (modal overlay is the only exception).
- Don't paint cards or active states in saturated emerald blocks — emerald is a
  thin working accent, not a slab.
- Don't set labels/headers/ids in Inter — those are mono.
- Don't warm the light background toward beige/paper — it's a cool near-white.
- Don't exceed two doodles per page, or rivet every control (CTAs + cards only).
- Don't spread Space Grotesk beyond titles, or use gradients in the interface.

## 9. Agent Prompt Guide

### Quick token reference (read by role, light / dark)
- Page base: **`--carapace-black`** `#f1f4f3` / `#0e1310`
- Card / panel: **`--emerald-surface`** `#ffffff` / `#14261d`
- Chrome band: **`--pine-deep`** `#e7ecea` / `#12211a`
- Primary button: **`--pine`** `#14241b` / `#17392a` fill, **`--on-pine`** `#f2f6f4` text
- Console (logs/DAG): **`--terminal`** `#0e1a14` / `#090e0b`, ink **`--terminal-ink`**
- Accent (links/active/success): **`--emerald`** `#137a52` / `#2ea77f` (hover `--emerald-bright`)
- Edge / rivet / doodle / SHA: **`--copper`** `#8a6416` / `#c0873f` — **strokes & rare tokens only**
- Ink: **`--soft-white`** `#0f1a15` / `#f4f8f4`; secondary **`--sage`**; muted **`--muted-sage`**
- Status: ok / running / danger (running badge gets a gold edge)

### Theming
- Light is `:root`; dark is `[data-theme="dark"]`. Toggle via `src/theme.ts`
  (`toggleTheme`), stored in `localStorage["scarab-theme"]`; pre-paint guard in
  `index.html`. Style **both** themes for anything new.

### Iconography
- **Functional**: Lucide (`lucide`), crisp, 1.5px stroke, `currentColor` (`components/Icon`).
- **Doodles**: same Lucide icons via rough.js at the §5 settings, 1–2/page, faint.

### Fonts
- **Space Grotesk** for titles only; **Inter** for body/UI; **JetBrains Mono**
  for all chrome/labels/machine. All three self-hosted via `@fontsource`.

### Example component prompts
- "Runs table: mono 10.5px uppercase `--muted-sage` column heads on `--pine-deep`;
  body rows 14px Inter `--sage` on `--emerald-surface`, `--border-soft` separators,
  SHA in gold mono, hover → faint emerald wash. Gold corner rivets on the list. One
  faint `git-branch` doodle in the background."
- "Status badge: pill radius 6px, mono lowercase. succeeded → `--ok`; failed →
  `--danger`; running → `--running` with a `--copper` hairline. Text `--badge-ink`."
- "Run detail: run title in Space Grotesk; provenance block = riveted card with
  mono uppercase keys + gold SHA; DAG and logs are `--terminal` pine-black consoles
  in both themes; primary actions dark-pine with light rivets."
- "Primary button: `--pine` fill, `--on-pine` text, 14px/600, radius 6px, zero
  shadow, light corner rivets; hover → `--emerald-bright`. Featured variant:
  transparent + `--copper` hairline + gold rivets."

### Iteration checklist
1. Light default (cool, not warm); dark is a clean toggle; both styled. Logs/DAG stay pine-black.
2. Emerald is the only working accent; gold is edges + rare tokens, never a fill.
3. Zero shadows on flat chrome; depth = shade + corner rivets.
4. Mono on every label/header/id; Space Grotesk 700 for titles; Inter for body.
5. Corner rivets only on repo cards + run summary + CTAs; faint dot-grid; 1–2 doodles/page.

## 10. Documentation surface (a distinct skin)

Sections §1–9 describe the **control plane** (the SolidJS app). The **docs site**
is a different job — long-form *reading*, not operating — so it keeps its own
skin. The rule that carries across both is not a palette; it's the *restraint*:
**Scarab shows up at the edges, never as a fill.** Now that the app is
light-first too, the two surfaces finally agree on temperament (light, calm,
high-contrast) while keeping distinct type and chrome.

**Base: Lucode Starlight, not a hand-rolled theme.** The docs are Astro
Starlight themed with [`lucode-starlight`](https://github.com/lucas-labs/lucode-starlight-theme)
(a shadcn/ui-inspired theme). We do **not** re-theme it — we layer a thin Scarab
identity over it (`ui/scarab-docs-ui/src/styles/scarab.css`).

**Light default; neutral dark.** Docs default to paper-white ink-dark; dark mode
is Lucode's neutral shadcn dark (not green). Green is used **sparingly**.

**Where the two accents are allowed (and nowhere else):**
- **Emerald** — prose links (underlined), the TOC current item, the active-page
  nav rail, and the focus ring. No emerald fills.
- **Copper** — the `tip` aside's edge and the faint background doodles (§5).
  Kept rare; the primary CTA uses Lucode's default solid button.
- Asides: `note` → emerald edge, `tip` → copper edge; `caution`/`danger` keep
  semantic **orange/red**.

**Typography: Geist + JetBrains Mono** (not the app's Inter). Geist is shadcn/ui's
face, so it's a faithful match for Lucode. JetBrains Mono stays for machine tokens.

**Implementation notes (so future edits don't fight the cascade):**
- Brand token overrides (`--sl-color-text-accent`, `--ring`, `--scarab-emerald`,
  `--scarab-copper`) are set **unlayered**, loaded after Starlight, or
  `@layer lucode` will beat them.
- Lucode fixes each sidebar entry to `height: 30px`; long ADR titles wrap, so
  `.entry-link` is forced to `height: auto` (`!important`, over an Astro-scoped style).
- Doodles (§5) still apply — faint copper rough.js motifs, unchanged.

### Docs iteration checklist
1. Light default; dark stays neutral. Never lead with green.
2. Emerald only on links / TOC-current / active-rail / focus. Copper only on the
   `tip` aside + doodles. No fills; use Lucode's default buttons.
3. Geist + JetBrains Mono. Don't reintroduce Inter or a display face here.
4. Override brand tokens **unlayered**, or `@layer lucode` will beat you.
5. Keep it a reading surface: quiet, high-contrast, no gimmicks.
