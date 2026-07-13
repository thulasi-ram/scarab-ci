# Scarab Design System

Scarab is a durable-execution, Kubernetes-native CI system. Its interface should
feel the way the product feels: **quietly durable, precise, and a little alive**.
The visual identity is a *scarab beetle* — a dark emerald/pine carapace with a
faint copper sheen — expressed as a clean, flat, shadow-free UI where the only
personality is a scattering of **hand-drawn (rough.js) line-icon doodles** in the
background of each page. The chrome stays out of the way; the doodles give it a
signature no other CI tool has.

> Stack note: the UI is **SolidJS** (`ui/`, ADR-0028), styled with plain CSS
> variables. There is no server-rendered templating. All decorations are inline
> SVG rendered by a Solid component (see §5).

## 1. Visual Theme & Atmosphere

- **Beetle carapace, not generic dark mode.** Surfaces are a ladder of near-black
  greens — carapace-black → pine → emerald — so depth comes from *shade*, never
  from shadows. Every dark surface is unmistakably green.
- **Flat and calm.** Zero box-shadows. Borders are hairline. The interface is
  legible and dense without being loud — a control plane you trust, not a
  landing page.
- **Copper as an edge, not a fill.** A single warm accent — copper-gold — appears
  *only* as strokes: hairline outlines on a few elements and the ink of the
  doodles. It is never a background or button fill. This restraint is what keeps
  the beetle "iridescence" tasteful.
- **Doodles as the soul.** Each page carries **one or two** line-icon doodles —
  CI/beetle motifs (a bug, a git branch, a container, a workflow) — rendered
  hand-drawn via rough.js, rotated and scaled large, sitting faintly in the
  background. They are the wink; functional UI is never doodled.

**Key characteristics**
- Emerald + pine + carapace-black surfaces; depth by shade, zero shadows.
- Copper-gold reserved strictly for **outlines and doodle strokes**.
- Two typefaces only: **Inter** (UI) and **JetBrains Mono** (code/logs/ids).
- Hand-drawn doodles = **Lucide icons run through rough.js**, 1–2 per page,
  background, rotated/scaled, low opacity.
- Tight, editorial typography; uppercase micro-labels for structure.

## 2. Color Palette & Roles

Defined as CSS custom properties (dark is the default surface set).

### Surfaces — the carapace ladder (depth via shade)

| Variable | Hex | Usage |
|---|---|---|
| `--carapace-black` | `#0e1310` | Navbar, darkest chrome, page base |
| `--pine-deep` | `#12241a` | Aside / secondary chrome |
| `--pine` | `#163a2b` | Hero strips, **primary button fill** |
| `--emerald-surface` | `#1b5240` | Dark cards |
| `--emerald-elevated` | `#236149` | Elevated: popovers, credential displays |

### Accent — green only (no gold fills)

| Variable | Hex | Usage |
|---|---|---|
| `--emerald` | `#2ea77f` | Primary accent: links, active nav, succeeded |
| `--emerald-bright` | `#3fc79a` | Hover / focus |

### Outline — copper-gold (strokes & doodles ONLY)

| Variable | Hex | Usage |
|---|---|---|
| `--copper` | `#c0873f` | Hairline outlines on featured elements; the rough.js doodle stroke. **Never a fill.** |

### Text (on dark surfaces)

| Variable | Hex | Usage |
|---|---|---|
| `--soft-white` | `#f4f8f4` | Primary text |
| `--sage` | `#a8c0b3` | Secondary text |
| `--muted-sage` | `#6f8579` | Metadata, timestamps, disabled |

### Borders

| Variable | Hex | Usage |
|---|---|---|
| `--border` | `#26443a` | Default hairline border/divider on dark |
| `--border-copper` | `#c0873f` @ ~40% | Featured/emphasis outline (copper, low alpha) |

### Status

| Variable | Hex | Meaning |
|---|---|---|
| `--ok` | `#34b37e` | Succeeded |
| `--running` | `#2f9e8f` | Running / pending (teal-green; badge gets a copper outline) |
| `--danger` | `#d1584f` | Failed / destructive |

### Gradients
**None in the interface.** The beetle "iridescence" (green→copper) is a *motif*
that lives in the doodles, not in gradient fills. Depth is shade, edges are
copper strokes.

## 3. Typography

### Families
- **UI**: `Inter`, system-ui fallback.
- **Code / logs / ids / durations**: `JetBrains Mono`.

Two typefaces, no more. Variety comes from size, weight, case, and tracking.
(The earlier `abcNormal` reference was Runway-proprietary and is dropped; Inter is
the free, self-hostable equivalent.)

### Scale

| Role | Family | Size | Weight | Line height | Tracking |
|------|--------|------|--------|-------------|----------|
| Display / Hero | Inter | 48px | 400 | 1.00 | -1.2px |
| Section heading | Inter | 32–40px | 400–500 | 1.05 | -0.8px |
| Card title | Inter | 24px | 500 | 1.05 | -0.4px |
| Feature title | Inter | 20px | 500 | 1.10 | -0.2px |
| Body / button | Inter | 16px | 400–600 | 1.30–1.50 | -0.16px |
| Caption / label | Inter | 14px | 500 | 1.30 | 0.35px (UPPERCASE) |
| Small | Inter | 13px | 400 | 1.30 | -0.16px |
| Micro / tag | Inter | 11px | 500 | 1.30 | 0.4px (UPPERCASE) |
| Code / id | JetBrains Mono | 13px | 400 | 1.40 | 0 |

### Principles
- **Tight display, comfortable body.** Headlines at line-height ~1.0 with
  negative tracking read like film titles; body stays 1.3–1.5 for readability.
- **Uppercase = structure.** 14px/11px labels use `text-transform: uppercase`
  with positive tracking (0.35–0.4px) as navigational signposts.
- **Mono for anything machine.** Run ids, step ids, durations, log lines, secret
  names — all JetBrains Mono, so identifiers are visually distinct from prose.

## 4. Component Stylings

### Buttons
- Primary: `--pine` fill, `--soft-white` text, 14px weight 600, radius 6px, **no
  shadow**. Hover lifts fill toward `--emerald-surface`.
- Secondary/ghost: transparent fill, `--border` hairline, `--sage` text; hover
  border → `--emerald`.
- Featured (rare): transparent fill with a **`--copper` hairline outline** — the
  only place copper touches a control.
- Destructive: `--danger` text on transparent, `--danger` hairline.

### Cards & containers
- Background: `--emerald-surface`; border: `1px solid --border`; radius 8px; zero
  shadow. A featured card may swap its border for a `--copper` low-alpha outline.
- Section panels sit on `--carapace-black`/`--pine-deep`.

### Navigation
- Top nav on `--carapace-black`; wordmark in `--soft-white`. Links Inter 16px
  400; active link `--emerald`; hover `--emerald-bright`. Hairline `--border`
  underline on the bar, nothing heavier.

### Tables (runs list, deploy history)
- Header row: 11px uppercase `--muted-sage` labels. Body rows: 14px, `--sage`,
  hairline `--border` row separators. Row hover: background → `--pine-deep`.
- Ids/timestamps in JetBrains Mono `--muted-sage`.

### Status badges
- Pill, radius 6px, small caps. `succeeded` → `--ok`; `failed` → `--danger`;
  `running` → `--running` **with a `--copper` hairline outline** (the running
  state is the one place the copper edge signals "in motion"). `skipped` →
  `--muted-sage`.

### Forms (secrets, settings)
- Inputs: `--pine-deep` fill, `--border` hairline, `--soft-white` text; focus
  border → `--emerald`. Secret values use `type=password` and are **write-only**
  (never rendered back — see the secrets page).

### Image / empty states
- No stock photography (this is a control plane). Empty states use a single
  faint doodle (§5) plus one line of `--muted-sage` copy.

## 5. Doodle & Decoration System

The one place Scarab lets its hair down. **Not** a hand-drawn font and **not**
bespoke SVG art — instead, take clean **Lucide** line icons and re-draw them
**hand-drawn via [rough.js](https://roughjs.com/)**, so the sketchiness is
consistent and generated, never hand-authored.

### Rendering recipe
Feed each Lucide icon's SVG path(s) to `rough.svg().path(d, opts)` with these
**canonical options** (agreed values):

| Option | Value | Note |
|---|---|---|
| `roughness` | `0.1` | Barely sketchy — a subtle wobble, not a scribble |
| `bowing` / smoothing | `5` | Curve smoothing (`curveStepCount`), gentle |
| `strokeWidth` | `1.6` | |
| `stroke` | `var(--copper)` `#c0873f` | Copper-gold ink — the only doodle color |
| `fill` | none | Outlines only |

Roughness `0.1` is deliberate: the doodles look *almost* clean, just enough
hand to feel warm — matching the restrained, durable tone.

### Placement rules
- **1–2 doodles per page. Never more.** They punctuate, they don't wallpaper.
- **Background layer only**, behind content (`z-index` below text; `pointer-events: none`).
- **Rotated and scaled**: rotate ~8–20° (varied per page), scale large (120–260px)
  so they read as texture, not iconography.
- **Faint**: opacity ~5–10% so they never fight the UI or hurt contrast.
- **Related to the page**: pick a motif that fits — e.g. `git-branch` on the runs
  list, `container`/`box` on a run detail, `key-round` on secrets, `shield` on
  environments, `bug` (the beetle!) as the catch-all house motif.
- **Never on functional controls.** Buttons, inputs, tables, and status glyphs
  stay crisp Lucide (no rough.js). Doodles are ambience only.

### Suggested motif set (Lucide names)
`bug` (house/beetle motif), `git-branch`, `git-commit-horizontal`, `container`,
`boxes`, `workflow`, `waypoints`, `network`, `package`, `terminal`, `key-round`
(secrets), `shield-check` (environments), `timer` (gates), `play`/`circle-dot`.

### Reference component (SolidJS sketch)
```tsx
// <Doodle icon="git-branch" rotate={-12} size={220} class="page-doodle" />
// - resolves the Lucide icon's raw <path d="…"> data
// - draws it via rough.svg().path(d, { roughness: 0.1, curveStepCount: 5,
//     strokeWidth: 1.6, stroke: "var(--copper)", fill: "none" })
// - wraps in an absolutely-positioned SVG: opacity .07, rotate(var), no pointer events
// Place at most two per route, in the page background.
```

## 6. Layout Principles

- **Spacing**: 8px base. Scale: 4, 8, 12, 16, 24, 32, 48, 64. Section vertical
  rhythm 48–64px; component gaps 16–24px.
- **Container**: content max-width ~1200px for app views; the marketing/hero can
  go cinema-wide (1600px). Left-aligned, generous margins.
- **Density**: the app (runs, logs, DAG) is information-dense — tighter gaps
  (12–16px) inside data views; marketing surfaces breathe more.
- **Whitespace**: calm and even. The doodle, not photography, is the only thing
  that fills "empty" space — and only faintly.

## 7. Depth & Elevation

| Level | Treatment | Use |
|---|---|---|
| Flat (0) | no shadow, no border | the dominant state |
| Bordered (1) | `1px solid --border` (or `--copper` low-alpha for featured) | cards, inputs |
| Chrome | `--carapace-black` / `--pine-deep` | navbar, asides |
| Surface | `--emerald-surface` | cards |
| Elevated | `--emerald-elevated` | popovers, credentials |

**Zero shadows, always.** Elevation is shade + (occasionally) a copper hairline.

## 8. Do's and Don'ts

### Do
- Build surfaces from the carapace → pine → emerald ladder; make darks
  unmistakably green.
- Use `--emerald` for interactive/active/success; keep it the one working color.
- Reserve `--copper` for **outlines and doodle strokes only**.
- Sprinkle **1–2** rough.js'd Lucide doodles per page — background, rotated,
  scaled, faint (5–10%), motif matched to the page.
- Keep functional UI crisp: plain Lucide icons, hairline borders, zero shadows.
- Use JetBrains Mono for every machine token (ids, durations, log lines).
- Use uppercase + positive tracking for micro-labels.

### Don't
- Don't use gold/copper as a **fill** — outlines only.
- Don't add box-shadows — depth is shade.
- Don't exceed two doodles on a page, or place any doodle on a control, or let a
  doodle's opacity harm contrast.
- Don't hand-author doodle SVGs or add a hand-drawn font — doodles come from
  Lucide + rough.js at the canonical settings, nothing else.
- Don't introduce a third typeface. Inter + JetBrains Mono only.
- Don't use pill-round radius on cards/buttons (6–8px, subtly rounded).
- Don't use gradients in the interface.

## 9. Agent Prompt Guide

### Quick color reference
- Base / navbar: **Carapace Black `#0e1310`**
- Chrome / aside: **Pine Deep `#12241a`**
- Button / hero: **Pine `#163a2b`**
- Card: **Emerald Surface `#1b5240`**
- Elevated: **Emerald Elevated `#236149`**
- Accent (links/active/success): **Emerald `#2ea77f`** (hover **`#3fc79a`**)
- Outline / doodle ink: **Copper-Gold `#c0873f`** (strokes only)
- Text: **Soft White `#f4f8f4`** / **Sage `#a8c0b3`** / **Muted Sage `#6f8579`**
- Status: ok **`#34b37e`**, running **`#2f9e8f`**, danger **`#d1584f`**

### Iconography
- **Functional icons**: [Lucide](https://lucide.dev) (`lucide-solid`), drawn
  crisp at 1.5px stroke, `currentColor`.
- **Doodles**: the *same* Lucide icons re-rendered via **rough.js** —
  `roughness 0.1`, `curveStepCount 5`, `strokeWidth 1.6`, `stroke` copper-gold,
  no fill — placed 1–2 per page in the background, rotated/scaled, ~5–10% opacity.

### Fonts
- **Inter** for all UI text; **JetBrains Mono** for code/ids/logs. Both free and
  self-hostable.

### Example component prompts
- "Runs table: header row in 11px uppercase Muted Sage (`#6f8579`), body rows
  14px Sage on Emerald Surface (`#1b5240`) with hairline `#26443a` separators, ids
  in JetBrains Mono. Row hover → Pine Deep. One faint `git-branch` doodle
  (rough.js, copper `#c0873f`, roughness 0.1, ~8% opacity, rotated -12°, 220px) in
  the page background."
- "Status badge: pill radius 6px, small-caps. succeeded → Emerald `#2ea77f`;
  failed → Danger `#d1584f`; running → Running `#2f9e8f` with a copper `#c0873f`
  hairline outline."
- "Secrets page: inputs on Pine Deep with hairline border, focus border Emerald;
  value field `type=password` (write-only). A single `key-round` doodle (rough.js,
  copper, faint) top-right in the background."
- "Primary button: Pine `#163a2b` fill, Soft White text, 14px/600, radius 6px,
  zero shadow; hover fill → Emerald Surface. Featured variant: transparent fill
  with a copper `#c0873f` hairline outline."

### Iteration checklist
1. Surfaces from the green ladder — never plain black/grey.
2. Emerald is the only working accent; copper is outline-only.
3. Zero shadows; depth = shade (+ occasional copper hairline).
4. Exactly 1–2 background doodles per page: Lucide → rough.js (0.1 / 5 / 1.6),
   copper, faint, rotated, scaled, page-relevant, never on controls.
5. Inter + JetBrains Mono; uppercase micro-labels with positive tracking.
