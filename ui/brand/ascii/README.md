# Brand ASCII scenes

The animated cousin of the doodle system (docs/DESIGN.md §5): brand beetles
*doing things*, rendered as ASCII text. Same pipeline philosophy as
`scarab-docs-ui/scripts/gen-doodles.mjs` — **generate offline, commit the
output, ship no rendering code**. The UIs play plain text frames by swapping
`textContent` at 12 fps (ASCII reads better slow — more terminal, less video).

```
npm install && npm run bake     # deterministic; reruns don't churn
```

The two beetles that carry the brand — the dung roller and the Ponderer — are
**traced pixel art**, not parametric drawings. The sheet and the tracer live in
`sprites/` (read its README first); `sprites.mjs` composes the poses and holds
the cell-exact primitives every scene draws with.

## Scenes (`generated/`)

| Asset | What | Used by |
|---|---|---|
| `dungroller.json` | dung beetle + ball, treadmill (ground scrolls), 88×54 | docs landing accent |
| `dungroller-bare.json` | same loop without ground, cropped at the feet, 88×44 | web-ui dashboard footer (CSS walks it across the viewport) |
| `ponder-{ponder,nap,kingofhill,faceplant}.json` | **bubble stages**: roll in → hold a pose → roll on, 96×43 | web-ui state moments (idle / queued / all-clear / failed) |
| `emblem-mark.txt` | the **traced** emblem, static, 64×57 | faint background marks |
| `doodles/*.svg` | Lucide icons rasterized to a 24×24 **dot matrix** (dot-icons.mjs) | docs + web-ui background doodles |

### Bubble stages (`ponder-*`) — text is a prop, not pixels

`ponder` is the **idle** state moment (RepoView's empty repo), where an arrival
reads wrong, so unlike the other three it is settled for the whole loop: the
beetle is already lying there, back against its ball, and the ball only rocks a
short distance to and fro under an idle foot. Its ball is smaller than the
stage's (r 8 vs 12) — at the stage radius no foreleg can reach the rim without
becoming a horizontal plank. A Zzz rises off the horn.

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

**The dung ball** is the one object that recurs across scenes, so its rules —
rim weight in cells, the single `roll` that drives rim/flecks/ground, the dash
threshold, layer roles — live in `DUNG.md`. Read it before drawing a ball in a
new scene.

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

## Cell spacing: the dots stand apart

A cell is **0.9em**, against JetBrains Mono's 0.6em advance — so a cell is
bigger than the glyph box and the dots do not touch. A solid fill then reads as
a dot matrix rather than a slab. Both axes take the same factor (letter-spacing
on x, line-height on y) or the grid stops being square.

The factor lives in three coupled places and they **must** move together:
`--ascii-cell` in `scarab-web-ui/src/styles.css`, `ASCII_CELL` in that UI's
`AsciiScene.tsx`, and `ASCII_CELL` in `scarab-docs-ui`'s `AsciiScene.astro`.
The players size the scene's box from the constant and the box clips, so a
mismatch crops the art. Call sites carry a smaller `fontSize` to keep the same
physical footprint as the old flush spacing.

Spacing is a RENDERING choice — it never touches the bake — but it changes what
the art has to do, because the eye stops closing small gaps for us. That is why
the ball's rim is drawn the way `DUNG.md` now describes.

## Limb weight: an ink threshold, then a glyph scale

Legs kept disappearing at the size the site actually renders, and the obvious
fix — make them wider — is the wrong one. There are two better levers.

**The ink is a threshold, not a shade.** Luminance picks a step on `" .·•●"`,
and a colour must reach **0.6665** to land on the biggest dot. The old limb
grey `#8fa89b` measures **0.6343** — just under — so the legs baked one step
below the body and the ball. `#9bb4a7` (0.6814) clears it: same cell count,
every one a size bigger, still grey to `classify()`. Cost nothing.

**Then the ramp runs out.** Its top two steps are far apart — in the font the
site renders in, `•` is 12px of ink and `●` is 25px, a 2.1× jump with nothing
between. Every single-advance glyph in that gap is a square or a triangle, so
there is no rounder middle step to reach for, and coverage cannot help either:
the glyph set *is* the quantisation. A leg heavy enough to see at ship size is
therefore necessarily as heavy as the body.

So the last bit of control lives in the PLAYER, not the bake: the grey layer
draws at `ASCII_LEG` (0.85) of the font size on the same grid — the shrink
comes back out of letter-spacing, so the cell is unchanged and the layers stay
registered, and `translateX` re-centres the glyph in its cell. The matching
vertical error is **0.04px** at ship size and identical across JetBrains Mono,
SF Mono and Menlo, so it is ignored. Like `--ascii-cell`, the constant lives in
three coupled places — `styles.css`, `AsciiScene.tsx`, `AsciiScene.astro`.

Note this scales the whole grey layer, ground dots included. That is safe here
because the legs are the only cells in it that reach the top of the ramp: a
census of the shipped bakes puts the ground at `.`/`·` and nothing else at `●`.

## Moving traced art: sub-cell, and RESOLVED not blended

A parametric body can be nudged by a fraction of a cell and the whole silhouette
reacts, because the shape is antialiased and the bake turns coverage into dot
size. The dung roller's original oval rode on `sin(leg) * 0.8` design units —
**0.73 of a cell** — and that alone made it read as soft and alive.

Traced sprite art has no such give. It is hard cells, so a sub-cell offset does
nothing and a whole-cell offset is a jump: at gait frequency that is vibration,
not a gait. A traced body therefore animates only if `blit()` renders the
fraction itself, which it does by giving each output cell the coverage of the
two source rows it straddles.

**Take the ink from the dominant row; never blend the two.** Brand layers are
assigned by hue, and a mix of two layers is not between them — it is somewhere
else entirely. Laying the shell's gold highlight over the emerald at 0.73 of a
cell mixes to `rgb(170,189,117)`, which `classify()` sorts into the gray LIMB
layer, so a naive fractional blit paints a gray stripe across the shell at every
peak of the bob. Resolving per cell keeps interior seams crisp and lets only the
silhouette breathe, which is the part that should.

The check that catches a flat body: count how many cells of the beetle's own
layer change between consecutive frames. A translated bitmap scores **zero**.

## Why the animated scarab is parametric, not traced

(The *scarab* here is the wing-spread emblem, not the beetles — those are
traced, see `sprites/`.) The traced emblem's dark layer (`../logo/scarab-emblem.svg`) is one
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
