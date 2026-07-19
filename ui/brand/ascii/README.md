# Brand ASCII scenes

The animated cousin of the doodle system (docs/DESIGN.md §5): brand beetles
*doing things*, rendered as ASCII text. Same pipeline philosophy as
`scarab-docs-ui/scripts/gen-doodles.mjs` — **generate offline, commit the
output, ship no rendering code**. The UIs play plain text frames by swapping
`textContent` at 12 fps (ASCII reads better slow — more terminal, less video).

```
npm install && npm run bake     # deterministic; reruns don't churn
```

## Scenes (`generated/`)

| Asset | What | Used by |
|---|---|---|
| `dungroller.json` | dung beetle + ball, treadmill (ground scrolls), 88×54 | docs landing accent |
| `dungroller-bare.json` | same loop without ground, cropped at the feet, 88×44 | web-ui dashboard footer (CSS walks it across the viewport) |
| `ponder-{ponder,nap,kingofhill,faceplant}.json` | **bubble stages**: roll in → hold a pose → roll on, 96×43 | web-ui state moments (idle / queued / all-clear / failed) |
| `emblem-mark.txt` | the **traced** emblem, static, 64×57 | faint background marks |
| `doodles/*.svg` | Lucide icons rasterized to a 24×24 **dot matrix** (dot-icons.mjs) | docs + web-ui background doodles |

### Bubble stages (`ponder-*`) — text is a prop, not pixels

The Ponderer is one stage reused for four moods (the held pose differs); each
loop rolls in, **stops** for the middle, then rolls on. The speech-bubble text
is **never baked** — the JSON carries only a `bubble` block:

```
bubble: { from, to, col, row, place }   // frame window [from,to), tail anchor cell, "above" | "right"
```

The players (`AsciiScene`) take a `line` prop and composite a box around it in a
fourth `<pre>` layer, shown only on frames `[from, to)`. Change the wit, ship no
re-bake. `\n` (or real newlines) split bubble lines. Add a pose by extending
`ponderPose`/`ponderAnchor` in `scenes.mjs` and `PONDER_POSES` in `bake.mjs`.

Scenes render with a **dots-only ramp** (`" .·•●"`): luminance maps to dot
size, so the ASCII art, the dot-matrix doodles, and the page dot-grid speak
one language.

The wing-spread scene (`drawScarab`) stays in `scenes.mjs` but is currently
unbaked — the docs hero uses the square emblem SVG. Re-add its `bakeScene`
line if a state moment wants it.

**Scene backdrops** (scenery behind the beetle — sun/moon/clouds) were explored
and **parked** as overkill for v1; the design + a build recipe are preserved in
`BACKDROPS.md`. The beetle fixes from that exploration (square cells, 96 grid,
thin legs, arms off the ball) did ship.

**Scene repertoire** — the direction is to replace the four ponderer pose
variations with a set of quiet, Hollow-Knight-*in-spirit* beetle vignettes
(Stargazer, Tactician, Cleansing Drop, …), mapped to state moments. Full spec
in `SCENES.md`; to be prototyped as samples before baking.

## Format

`{ cols, rows, fps, frames: [[em, au, fe], …] }` — each frame is three
same-shape text layers split by brand role: **em**erald (wings), gold/**au**
(body, ball, nimbus), **fe**/gray (legs, films, ground). A player stacks three
`<pre>` elements and colors them via CSS custom properties, so the art follows
the theme with zero per-cell work at runtime.

## Why the animated scarab is parametric, not traced

The traced emblem's dark layer (`../logo/scarab-emblem.svg`) is one
winding-linked compound path — its background whites are *hole contours* — so
it cannot be dismembered for wing articulation. `scenes.mjs` rebuilds the
scarab parametrically at the emblem's measured proportions (ring r≈894 at
(1089,950), wing pivots (882,1030)/(1314,1030)); the traced original renders
verbatim only as the static mark. Cells render **square** — the players set
line-height to the 0.6em glyph advance — so scenes are baked unsquashed
(`CELL_ASPECT = 1.0`) and the dot matrix is evenly spaced on both axes.

## Placement rules (extends DESIGN.md §5)

- **Animated scenes**: docs **landing page only** (hero + at most one accent),
  and web-ui **state moments** (all-clear, empty, loading) — never ambient
  behind live data, never on inner docs pages.
- **Static marks**: doodle rules apply (faint, background, ≤2/page).
- Players must honor `prefers-reduced-motion` (show one open frame) and pause
  when the document is hidden.
