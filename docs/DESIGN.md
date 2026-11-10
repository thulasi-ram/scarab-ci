# Scarab Design System

Scarab is a durable-execution, Kubernetes-native CI system. Its interface should
feel the way the product feels: **quietly durable, precise, and a little alive**.
The visual identity is an **engineering blueprint** — a cool paper-white worksheet
marked up with technical registration marks, monospace annotations, and a dark
pine-green console where the machine actually runs. The scarab-beetle heritage
(dark emerald/pine carapace, faint copper sheen) still owns the *dark* mode and
the terminal panels; in the light default it shows up only at the **edges** —
hairlines and the **dotted** line-icon doodles.

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
it's **restraint plus technical marks**: gold shows up only as an edge, four
text styles carry all the text (§3), and decoration speaks one **dotted** language — the page
dot-grid, dot-matrix doodles, and the dots-only ASCII beetles.

Influence: the crisp, blueprint-y, mono-annotated feel of engineering marketing
sites (dot-grid textures, spec-sheet framing) — adapted to a control plane,
kept pine/black, and made light-first. Earlier corner-rivet and corner-bracket
treatments are retired: the dotted language carries the signature now, and
corners are sharp-ish (5px cards / 3px controls) rather than soft.

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
- **Technical marks are the signature.** Pages carry a faint **dot-grid**
  texture; labels are **monospace**; decoration is **dotted** (§5). Together
  they read like a spec sheet, not a landing page.
- **Doodles as the soul.** Each page still carries **one or two** dotted
  line-icon doodles in the background (§5) — the wink; functional UI is never
  doodled.

**Key characteristics**
- Light-first cool-white ladder; opt-in carapace dark; pine-black terminals in both.
- Emerald = the single interactive accent. Gold = edges & rare tokens only, never a fill.
- One **dotted** decoration language (dot-grid, dot-matrix doodles, dots-only ASCII beetles); corners sharp-ish (5px/3px), never boxy-round.
- Four text styles (§3): Title, Label and Body in **Inter**, Mono in **JetBrains Mono** for machine values. **Space Grotesk** is only the wordmark.
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

### Edge — gold (strokes, doodles, rare tokens — never a fill)

| Token | Light | Dark |
|---|---|---|
| `--copper` | `#8a6416` | `#c0873f` |
| `--border-copper` | `rgba(138,100,22,.5)` | `rgba(192,135,63,.42)` |

Light gold is deepened so it reads on white. Used for: doodle ink, the
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

### Four text styles

A page uses at most four font variations, size and weight included. The web
UI has exactly four text styles, so every page stays within that. Each is a
`font` shorthand token in `ui/scarab-web-ui/src/styles.css`:

| Style | Token | Face | Weight · size | For |
|-------|-------|------|---------------|-----|
| Title | `--t-title` | Inter (`--font-ui`) | 600 · 20px | page titles (`h1`, the run and settings breadcrumb headings), dialog titles, stat figures |
| Label | `--t-label` | Inter | 600 · 13px | names, section and panel headings, column heads, field labels, buttons, active tabs and nav links |
| Body | `--t-body` | Inter | 400 · 13px | running text, values, form controls, nav links |
| Mono | `--t-mono` | JetBrains Mono (`--font-mono`) | 400 · 12px | machine values: ids, SHAs, refs, durations, counts, code, logs |

Every text rule sets `font:` to one of the four tokens, or `font: inherit`.
No rule sets `font-size`, `font-weight` or `font-family` on its own, and
nothing is upper-cased or tracked. Headings are in sentence case.
`src/styles.test.ts` checks the stylesheet for this.

An element with the `.mono` class takes Mono, whatever component style it
sits in: the class means "this is a machine value".

### Hierarchy without more sizes

- A heading that used to be larger is Label; the space above it and the
  Body text under it set it apart.
- Secondary text is Body in `--sage` or `--muted-sage`. Colour is not a font
  variation, so it does not count against the four.
- Status chips and badges keep their data values as written (`succeeded`,
  `push`, `unreachable`): lowercase Mono or Label, not upper-cased.

### Exemptions

These keep their own face or size, and the regression test lists the same
selectors:

- The wordmark (`.brand`) and the login lockup under the emblem
  (`.login-title`) are set in Space Grotesk (`--font-display`). They are the
  brand mark, not text.
- The ASCII scenes (`.ascii-scene pre`, `.ascii-fe`) size their dots from
  `--ascii-fs`. They are pictures drawn with characters.

The debug shell (xterm) draws on a canvas, so it takes a number instead of a
token; it uses Mono's 12px.

The docs site (§10) has its own type and is not covered here.

## 4. Signature System — dots & texture

One cheap, CSS-only language gives Scarab its spec-sheet identity. Use it as
described; don't invent new decoration.

(The corner-**rivet** era is retired — dots carry the signature now, echoed by
the dot-matrix doodles and the dots-only ASCII beetles. Corner radii
are deliberately sharp-ish: `--radius: 5px`, `--radius-sm: 3px`,
`--radius-lg: 7px` — crisp, not boxy.)

### Dot-grid texture
`.page::before` lays a faint `radial-gradient` dot grid (24px pitch, `--dot`
color) behind all content — a worksheet backdrop, never noise. It sits below
content (`z-index:0`) alongside the doodles.

## 5. Doodle & Decoration System

Still the one place Scarab lets its hair down. Take clean **Lucide** line
icons and rasterize them onto a coarse **dot matrix**: one dot per lit grid
cell, so a stroke is *several dots wide* — a true dot-matrix rendering, the
same language as the ASCII beetle scenes (dots-only ramp) and the page dot-grid. (The rough.js hand-sketched era and the earlier
single-dotted-outline pass are both retired.)

### Rendering recipe
Baked offline by `ui/brand/ascii/dot-icons.mjs` (part of `npm run bake` —
docs and web-ui consume the SAME committed SVGs from
`ui/brand/ascii/generated/doodles/`):

| Option | Value | Note |
|---|---|---|
| grid | 24×24 cells | over the icon's 24-unit viewBox (pitch 1) |
| coverage threshold | 0.28 | a cell earns a dot when ≥28% inked (10× supersampled) |
| dot radius | 0.34 | grid units |
| ink | copper `#c0873f` | a literal hex — an SVG presentation attribute |
| stroke sampled | width 2, round caps | Lucide's native stroke → ~2 dots wide |

### Placement rules
- **1–2 doodles per page. Never more.** Background layer only
  (`z-index` below text; `pointer-events: none`).
- **Rotated & scaled**: ~8–20°, 120–260px, so they read as texture.
- **Faint**: opacity ~12–16% — fine dots carry ~1/3 the ink of a solid
  stroke, so they need more opacity for the same ambient presence.
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
- **Web-UI**: state moments (all-clear, empty, loading) and the **dashboard
  footer roller** — the dung beetle walks its ball across the bottom of the
  page while runs are in flight and parks when the dashboard is quiet, so
  motion literally means "something is working". Never ambient behind live
  data; an operator surface is scanned, and a permanently moving background
  fights the "what needs me" read.
- **Static ASCII marks** (the traced emblem) follow the doodle rules: faint,
  background, ≤2 per page.
- Players honor `prefers-reduced-motion` (hold an open frame) and skip work
  while the document is hidden.

## 6. Component Stylings

### Buttons
- **Primary** (`.btn-primary`): dark-pine (`--pine`) fill, `--on-pine` light
  text, 14px/600, radius `--radius-sm` (3px), no shadow. Hover fill →
  `--emerald-bright`. A dark button on white is the strongest, calmest call
  to action.
- **Ghost** (`.btn-ghost`): transparent, `--border` hairline, `--sage` text;
  hover border → `--emerald`.
- **Featured / gold** (`.btn-copper`, rare): transparent with a `--copper`
  hairline + gold text — the only place gold touches a control, and still
  only as an edge.
- **Destructive** (`.btn-danger`): `--danger` text on transparent, danger hairline.

### Cards & panels
- `.card`/`.panel`/`.prov`/`.repo-card`: `--emerald-surface` fill, `--border`
  hairline, radius `--radius`–`--radius-lg` (5–7px), zero shadow. Panel headers
  (`.panel-h`) are a `--pine-deep` band with a Label heading. Panels
  carry no decoration.

### Terminals (logs + DAG)
- `.logs` and `.dag` fill `--terminal` (pine-black) in both themes, with
  `--terminal-ink` text and `--terminal-line` edges. DAG nodes (`.dnode`) use
  `--terminal-elev`; the selected node takes an emerald ring, a `running` node a
  gold edge. Edges are `--terminal-line` strokes.

### Tables & run list
- Header row: Label in `--muted-sage` on `--pine-deep`. Body rows in Body,
  `--sage`, `--border-soft` separators, hover → faint emerald wash
  (`color-mix`). SHAs render in gold Mono; ids/timestamps in muted Mono.

### Status badges
- Pill, radius 6px, **mono** lowercase. `succeeded` → `--ok`; `failed` →
  `--danger`; `running`/`pending` → `--running` **with a gold hairline** (the
  one place a gold edge signals "in motion"); `skipped` → muted outline. Text is
  `--badge-ink`.

### Forms
- Every form field is a `Field` holding an `Input`, `Select`, `Textarea` or
  `Checkbox` (`components/Field.tsx`): a 30px box with an `--emerald-surface`
  fill and a `--border` hairline; focus → `--emerald` border + a soft emerald
  ring. The label sits above in Label, the help line below; an error
  replaces the help line and marks the box. Values are in the UI face; mono only
  for machine values (ids, refs, keys, paths, URLs). Secret values are
  `type=password` and **write-only** (never rendered back).
- Every dropdown is `SearchSelect` (`components/SearchSelect.tsx`); there is
  no native `<select>`. `Select` puts it in the form box. A list of more than
  eight options gets a search box; a shorter one is a plain list you move
  through with the arrow keys and type-ahead. Options can sit under group
  headings, behind a rule (an entry such as "New organization…"), or show
  disabled. The list is portalled to `<body>`, so it is never clipped and
  stacks above a dialog.
- A few controls stay plain on purpose: the command palette, the repo filter,
  the search inside `SearchSelect`, and the reject-reason box on a gate.

### Tabs & filter pills
- Tabs: text buttons, active → `--emerald` text + emerald underline. Filter
  pills (`.fpill`): mono, hairline; active → emerald border + emerald text.

### Modal
- Every dialog is a `Modal` (`components/Modal.tsx`): elevated
  (`--emerald-elevated`), a blurred dark scrim, and the single permitted soft
  shadow. A 20px title, one line saying what the dialog is for, numbered
  sections when it asks three or more questions, and a `--pine-deep` footer with
  the actions and, when useful, a line saying what applies after saving. Three
  widths: narrow 440px (a confirmation), standard 560px (a form), wide 760px (a
  review).
- It sits 10vh from the top. A taller dialog moves up first and scrolls its body
  only when the window is too short; a shadow above the footer then says there is
  more below.
- Removing or revoking something asks through `ConfirmDestructive`: type the
  thing's name to enable a solid danger button. An action that can be undone
  (kill a plugin, stop mounting a claim) asks through `ConfirmAction`. Never the
  browser's `confirm()` or `prompt()`.

### Empty states
- No stock photography. A single faint doodle (§5) + one line of `--muted-sage` copy.

## 7. Depth & Elevation

| Level | Treatment | Use |
|---|---|---|
| Flat (0) | no shadow | dominant state |
| Bordered (1) | `1px solid --border` | cards, panels, inputs |
| Chrome | `--pine-deep` band | panel/table headers, top bar |
| Terminal | `--terminal` pine-black | logs, DAG |
| Overlay | `--emerald-elevated` + soft shadow (the one exception) | modal |
| Well | `--well` fill + inset shadow + dot grid | empty states only |

**No shadows on flat chrome.** Depth is shade. The one other exception is an
empty state: it is drawn as a recessed well (an inset, debossed shadow over the
dot grid), so it reads as a space waiting to be filled rather than as a card.

## 8. Do's and Don'ts

### Do
- Lead with light (cool, never warm); keep dark a clean toggle. Never make the
  app *require* dark.
- Keep logs + DAG pine-black in both themes — the console is the machine.
- Use `--emerald` as the one interactive/active/success color.
- Reserve `--copper`/gold for **edges and rare tokens only** — hairlines,
  doodle ink, SHAs, trigger glyphs.
- Use one of the four text styles (§3) for every piece of text; Mono for every id and machine token.
- Keep the dot-grid faint; dots are the signature, not a texture bath.
- Set the h1/breadcrumb row in Title; body in Body.
- Sprinkle **1–2** dot-matrix doodles per page — background, faint, page-relevant.

### Don't
- Don't use gold/copper as a **fill**, or as body text.
- Don't add box-shadows to flat chrome (modal overlay is the only exception).
- Don't paint cards or active states in saturated emerald blocks — emerald is a
  thin working accent, not a slab.
- Don't add a fifth text style: no new size, weight or face, no upper-casing, no tracking.
- Don't warm the light background toward beige/paper — it's a cool near-white.
- Don't exceed two doodles per page.
- Don't use Space Grotesk outside the wordmark and the login lockup, or use gradients in the interface.

## 9. Agent Prompt Guide

### Quick token reference (read by role, light / dark)
- Page base: **`--carapace-black`** `#f1f4f3` / `#0e1310`
- Card / panel: **`--emerald-surface`** `#ffffff` / `#14261d`
- Chrome band: **`--pine-deep`** `#e7ecea` / `#12211a`
- Primary button: **`--pine`** `#14241b` / `#17392a` fill, **`--on-pine`** `#f2f6f4` text
- Console (logs/DAG): **`--terminal`** `#0e1a14` / `#090e0b`, ink **`--terminal-ink`**
- Accent (links/active/success): **`--emerald`** `#137a52` / `#2ea77f` (hover `--emerald-bright`)
- Edge / doodle / SHA: **`--copper`** `#8a6416` / `#c0873f` — **strokes & rare tokens only**
- Ink: **`--soft-white`** `#0f1a15` / `#f4f8f4`; secondary **`--sage`**; muted **`--muted-sage`**
- Status: ok / running / danger (running badge gets a gold edge)

### Theming
- Light is `:root`; dark is `[data-theme="dark"]`. Toggle via `src/theme.ts`
  (`toggleTheme`), stored in `localStorage["scarab-theme"]`; pre-paint guard in
  `index.html`. Style **both** themes for anything new.

### Iconography
- **Functional**: Lucide (`lucide`), crisp, 1.5px stroke, `currentColor` (`components/Icon`).
- **Doodles**: same Lucide icons, dot-matrixed at the §5 settings, 1–2/page, faint.

### Fonts
- **Inter** 400/600 for Title, Label and Body; **JetBrains Mono** 400 for Mono;
  **Space Grotesk** for the wordmark only. All three self-hosted via `@fontsource`.

### Example component prompts
- "Runs table: Label `--muted-sage` column heads on `--pine-deep`;
  Body rows `--sage` on `--emerald-surface`, `--border-soft` separators,
  SHA in gold Mono, hover → faint emerald wash. One faint `git-branch` doodle
  in the background."
- "Status badge: pill radius 6px, Mono lowercase. succeeded → `--ok`; failed →
  `--danger`; running → `--running` with a `--copper` hairline. Text `--badge-ink`."
- "Run detail: run title in Title; provenance block = card with
  Label keys + gold Mono SHA; DAG and logs are `--terminal` pine-black consoles
  in both themes; primary actions dark-pine."
- "Primary button: `--pine` fill, `--on-pine` text, Label, radius 3px, zero
  shadow; hover → `--emerald-bright`. Featured variant: transparent +
  `--copper` hairline, gold text."

### Iteration checklist
1. Light default (cool, not warm); dark is a clean toggle; both styled. Logs/DAG stay pine-black.
2. Emerald is the only working accent; gold is edges + rare tokens, never a fill.
3. Zero shadows on flat chrome; depth = shade.
4. Four text styles only (Title, Label, Body, Mono); Mono for ids and machine values; sentence case, no upper-casing.
5. One dotted decoration language — faint dot-grid + dot-matrix doodles (1–2/page) + dots-only ASCII beetles.

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
- Doodles (§5) still apply — faint copper dotted motifs, unchanged.

### Docs iteration checklist
1. Light default; dark stays neutral. Never lead with green.
2. Emerald only on links / TOC-current / active-rail / focus. Copper only on the
   `tip` aside + doodles. No fills; use Lucode's default buttons.
3. Geist + JetBrains Mono. Don't reintroduce Inter or a display face here.
4. Override brand tokens **unlayered**, or `@layer lucode` will beat you.
5. Keep it a reading surface: quiet, high-contrast, no gimmicks.
