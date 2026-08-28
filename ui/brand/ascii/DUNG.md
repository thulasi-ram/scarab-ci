# The dung ball

The ball is the one object that appears in more than one scene — the dungroller
pushes it, the Ponderer leans on it, and every vignette in `SCENES.md` that
involves a beetle at work will want it. It is also the object that most easily
looks *wrong*: a circle that does not rotate reads as a coin, a rim one cell too
thick reads as a doughnut, and flecks that drift out of phase with the ground
read as a ball skidding rather than rolling.

This is the spec the ball follows, and the reasoning behind each number, so a
new scene can draw one that belongs to the same world.

Rules live in `scenes.mjs` (`drawBeetle`, `ponderBall`); the numbers below are
what those functions implement.

---

## 1. Measure in cells, never in design units

Every scene draws into its own design space and is then baked to a grid, so the
same stroke width means a different number of dots in each scene:

| | design space | baked grid | `s` (cells per unit) |
|---|---|---|---|
| `dungroller` | 96 units wide | 88 cells | 0.917 |
| `ponder-*` | 74 units wide | 96 cells | 1.297 |

A rim declared as `lineWidth = 2` is therefore **1.8 cells** in the roller and
**2.6 cells** in the Ponderer — a 40% difference in weight from one line of
identical code. That is exactly the bug that made the Ponderer's ball read as a
thick doughnut next to the roller's sleek ring.

**So: any stroke that must read at a specific weight is written `w / s`.** The
beetle's limbs already did this; the ball's rim now does too. Fills that scale
*with the art* — the flecks, which are a proportion of the ball — stay in design
units. The test is: "would I want this thicker on a finer grid?" For a stroke,
never.

| feature | value | why |
|---|---|---|
| rim stroke | **2.0 cells** (`2 / s`) | one dot of ramp on each side of the true circle; 1 cell aliases into a dotted line, 3 closes the ring into a band |
| leg / arm stroke | **1.65 cells** (`LEG.cells / s`) | see §7 — thinner vanishes |
| ground dot | **1.3–1.7 cells** square | below 1 cell it flickers between frames as it crosses cell boundaries |

### Colour is the other half of weight

The bake maps **luminance to dot SIZE** on the `" .·•●"` ramp (`bake.mjs`), so a
*lighter* stroke crosses each ramp step at lower coverage and bakes to **bigger**
dots. Two strokes of identical width read at different weights if their colours
differ:

| | colour | luminance | full-coverage glyph |
|---|---|---|---|
| roller legs | `#8fa89b` | 0.634 | `•` |
| Ponderer legs (was) | `#6e7f76` | 0.481 | `•` |

Both land on `•` at full coverage, but the darker one loses its antialiased
edges, so at 1.1 cells it all but disappeared. **Darkening a limb thins it as
surely as narrowing it** — change one and re-check the other.

## 2. One `roll` drives everything

`roll` is the ball's rotation in radians, and it is the **only** animation input
the ball takes. Rim phase, fleck angle and ground travel are all derived from
it, which is what keeps them locked:

```js
const travel = /* how far the ball has moved, in design units */;
const roll   = travel / r;          // radians — arc length over radius
```

Derive, never re-time:

```js
x.lineDashOffset = -roll * r;        // rim: dash pattern walks the circumference
const a = roll + i * 2.39996;        // flecks: golden-angle scatter, rotating
const gx = i * spacing - travel;     // ground: scrolls at the surface speed
```

Get this wrong and the ball skids: flecks turning at one rate over ground
moving at another is the visual signature of a wheel losing traction, and the
eye catches it immediately even at 12 fps.

**A held pose is not an exception.** The Ponderer rolls in, *stops* for the
middle of the loop, then rolls on — its `travel` is a trapezoid, so during the
hold `roll` is constant and the rim and flecks freeze with the ground. That is
correct: the ball genuinely is not moving. Never animate the ball through a hold
to "keep it alive".

## 3. The rim rotates, and it is dashed so you can see it

A smooth circle rotating is invisible. The rim is therefore **dashed and
phase-locked to the roll** — the dashes walk around the circumference, and that
is what tells the eye the ball is turning rather than sliding.

```js
const rimPeriod = (TAU * r) / 21;              // 21 segments — see below
x.setLineDash([rimPeriod * 0.62, rimPeriod * 0.38]);
x.lineDashOffset = -roll * r;
```

**21 segments, 62/38 duty.** The segment count must divide the circumference
exactly or the loop seams — 21 is coprime with nothing in particular, it is
simply the count that leaves each dash long enough to survive the bake at both
scene scales:

| | radius | circumference | dash | gap |
|---|---|---|---|---|
| `dungroller` | 18.3 cells | 115.2 cells | 3.40 cells | 2.08 cells |
| `ponder-*` | 15.6 cells | 97.8 cells | 2.89 cells | 1.77 cells |

**A dash must survive as at least ~2.5 cells.** Below that the bake's dot ramp
swallows it and the rim reads as a *broken* ball rather than a textured one. If
a new scene's ball is much smaller than the Ponderer's, lower the segment count
rather than accepting shorter dashes — `segments ≈ circumference / 4.7`.

> **Reversal, recorded:** `ponderBall` originally used a **solid** rim, with a
> comment stating that a dashed rim "baked to these small dot grids reads as a
> *broken* ball". That was true when the Ponderer baked to a **64**-cell grid.
> At the 96 grid it is not: the dashes land at 2.89 cells and read as texture on
> a continuous ring. The three-way comparison (solid ponder / dashed ponder /
> dashed roller) settled it by eye. The lesson is in the threshold above, not in
> the verdict — re-check it if a scene's ball gets smaller.

## 4. Flecks: golden-angle scatter, rotating with the roll

The dung texture is a handful of filled dots that turn with the ball:

```js
const a  = roll + i * 2.39996;                 // golden angle ≈ 137.5°
const rr = r * (band0 + bandW * ((i * 0.61) % 1));
```

The golden angle is doing real work: successive flecks land maximally far apart
in rotation, so a small number of them reads as *scattered texture* rather than
as a pattern or a spoke. `(i * 0.61) % 1` does the same job for the radius, so
flecks do not settle into a ring.

Flecks stay **inside** the rim with a margin — a fleck touching the rim merges
with it in the bake and reads as a dent.

## 5. Ground: the wrap identity

The ground is dots, and its spacing is not free. One loop must scroll the
pattern by exactly one wrap or the ground jumps at the loop seam:

```js
const spacing = (TAU * r) / DOTS;   // DOTS dots per revolution
```

Draw `DOTS + 6` of them and modulo the x into `[-spacing*3, …]` so dots enter
and leave off-canvas instead of popping at the edges.

## 6. Colour and layer roles

The bake splits every frame into three text layers by brand role, and the ball
spans two of them. Getting this wrong silently recolours the ball.

| part | colour | layer | note |
|---|---|---|---|
| rim | `#d9b45e` | **au** (gold) | the ball is a gold object; gold is an edge in this system, never a fill |
| flecks | `#b3924a` | **au** (gold) | a step darker so texture sits *under* the rim |
| ground dots | `#57685e` | **fe** (gray) | ground is scenery, not subject |
| beetle body | `#27b584` / `#1c8a64` | **em** (emerald) | for contrast against the ball |
| legs, arms, antenna | `LEG.color` (`#8fa89b`) | **fe** (gray) | reads in front of the ball without competing; one constant, see §7 |

## 7. Limbs: one style, two geometries

The two beetles' legs had drifted apart on **both** weight axes — 1.65 cells /
`#8fa89b` on the roller against 1.10 cells / `#6e7f76` on the Ponderer. Baked
and compared side by side at a 9px cell, the Ponderer's legs all but vanished:
thinner *and* darker is a compounding loss, not an additive one.

The style is now one exported constant, `LEG = { color, cells }`, and both
scenes read it. The roller's historical `lineWidth = 1.8` design units is
exactly `1.65 / s` in its own space, so adopting the constant left its bake
**byte-identical** — the reference did not move.

**The geometry cannot be shared, and should not be.** The roller's five limbs
are absolute constants for a beetle pinned at one spot with one tilt. The
Ponderer's are functions of a moving origin, a `grip` amount (it lets go of the
ball to pose), a per-pose `tilt`, and an `onBall` flag (kingofhill climbs up and
tucks its legs onto the crown). Copying the roller's coordinates in would freeze
the Ponderer mid-push and break three of its four poses. Share the style, keep
the choreography.

## 8. Nothing may be drawn *over* the rim

The bake classifies each cell to **exactly one** layer (`bake.mjs: classify`).
A limb drawn across the rim therefore does not overlay it — it **replaces** those
cells with gray and punches a notch out of the gold ring. This is the same
failure as a fleck touching the rim from the inside (§4), and it is why the
Ponderer's arms "wrapped around onto the border": they ended *inside* the rim
band.

Two things have to be right, and the second is the one that surprises:

**Reach.** The hand stops where its own stroke edge meets the rim's outer edge.
Derive it — do not tune it by eye:

```js
const clear = (2 / 2 + LEG.cells / 2) / s;   // half the rim + half the limb, in cells
const reach = ballR + clear;                 // → design units
```

A hand-tuned `ballR * 1.04` put the tip at 12.48 units with the rim spanning
11.23–12.77 — inside the band.

**Approach angle.** Reach alone is not enough: a limb aimed at a far point on
the arc crosses the band **obliquely**, running *along* the ring rather than
into it, and an oblique crossing steals far more cells than a radial one. The
Ponderer's shoulder sits at ~154° from the ball centre, so hands placed high on
the arc (~200°) raked across it. Measured over the rolling frames:

| hands at | limb cells in the rim band | gold rim cells |
|---|---|---|
| 1.11π / 1.19π | 74 | 891 |
| 1.11π / 1.16π | 52 | 913 |
| 1.02π / 1.09π | 18 | 947 |
| **0.95π / 1.03π** | **9** | **956** |
| 0.88π / 0.96π | 13 | 954 |

Place hands near the radial direction from the shoulder, and put **both** hands
on the clearance circle — never one placed and the other nudged off it by a
hand-tuned offset, which is exactly how the Ponderer's lower arm ended up back
inside the band.

**How to check it.** Count cells in the gray layer whose distance from the ball
centre falls in the rim band, over the frames where the beetle is gripping. It
should be ~2 — the floor is inherent, because the ball rests *on* the ground and
ground dots necessarily cross the band at the bottom of the circle. Anything
above single digits is a notched rim.

> `ponder-kingofhill` is the deliberate exception: it climbs onto the ball and
> tucks its legs onto the crown, so contact is the pose. It is not wired into
> any UI today.

## 9. Where the two balls still differ

The rim is now identical in weight and behaviour. The texture is not, and this
is deliberate for now — the Ponderer's ball is 16% smaller, so the same fleck
count reads as busier:

| | `dungroller` | `ponder-*` |
|---|---|---|
| flecks | 16 | 9 |
| radius band | `r * (0.15 … 0.90)` | `r * (0.22 … 0.70)` |
| fleck size | 1.28 / 2.02 / 2.75 cells (varied) | 1.32 cells (uniform) |
| ground dots per revolution | 29 | 22 |

If a third scene needs a ball, take the Ponderer's values as the starting point
and scale the fleck **count** with the ball's area, not its radius:
`flecks ≈ round(9 * (r_cells / 15.6)²)`.

> **Open:** the *texture* could also share one `dungBall({cx, cy, r, roll, s})`
> helper. Unlike the rim and the limbs — where the shared values happened to be
> the roller's own, so unifying cost nothing — unifying the fleck count and band
> would change the roller's baked output. The roller is the reference everyone
> is happy with, so that wants a deliberate diff you review, not a side effect.

## 10. Checklist for a new scene with a ball

1. Compute `s = cols / VB_W` and pass it to anything that strokes.
2. Rim at `2 / s`; legs and arms via `LEG` (`LEG.cells / s`, `LEG.color`) — never a
   raw design-unit width, and never a darker limb colour without re-checking §1.
3. Derive `roll = travel / r`, and derive the rim offset, fleck angles and
   ground travel from it — never from the frame index directly.
4. `segments ≈ circumference_in_cells / 4.7`, then check the dash lands at
   ≥ 2.5 cells.
5. Ground spacing `= (TAU * r) / DOTS`; draw `DOTS + 6`.
5b. Limbs stop at `ballR + (2/2 + LEG.cells/2) / s`, approached radially — then
   count gray cells in the rim band (§8) and expect ~2.
6. Rim and flecks gold (`au`), ground and legs gray (`fe`), beetle emerald (`em`).
7. Bake, and **look at the ball beside `dungroller-bare`** at the same cell
   size. The roller is the reference; if the new ball reads heavier or busier,
   it is wrong.
