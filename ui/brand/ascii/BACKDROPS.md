# Scene backdrops — exploration (PARKED)

Status: **parked, not built.** The beetle-rendering fixes from this exploration
(square cells, 96 grid, thin resolution-aware legs, arms off the ball) **did
ship** — see the README and `scenes.mjs`. Backdrops themselves were judged
overkill for v1 and deliberately shelved. This doc preserves the design so it
isn't lost if we ever want scenery behind the beetle scenes.

## The idea

Give the beetle scenes an *environment* — a landscape drawn behind the beetle —
instead of playing over empty space. The beetle is the subject; the backdrop is
the setting it treks through.

## What we tried, in order (and what each taught us)

1. **Solid block-shade backdrop, beetle full-frame.** Ramp `░▒▓█` behind the
   beetle. Two problems: the block shades fill whole cells, so the scenery read
   as a solid mass that the beetle's dots **meshed into** (no figure/ground
   separation); and the beetle + ball filled the frame, leaving no room for the
   world.
2. **Wide world, small beetle.** Made the diorama a big world (≈150 cells) and
   dropped the beetle in small (~30% width) on a ground line. Fixed the scale —
   the sun/clouds/trees finally had room. Sun and moon read well; the **forest
   was too dense and the beetle camouflaged into it**.
3. **Chrome-dino treatment.** Flat horizon, sparse cacti, lots of empty space,
   ground **scrolling** past a beetle rolling in place. Solved density and
   readability. But a statically-placed rolling beetle over a scrolling ground
   **looked like it floated** — the motion contract (who moves, who's still)
   wasn't convincing without more work.
4. **Rare parallax (the settled shape).** Back to the static wide world, but:
   scenery is **rare** (drifts in occasionally, not always on); it **moves**
   through with **parallax** instead of fading; and it's drawn in the
   **fine→square** ramp so it's a different material from the round beetle and
   never meshes.

## What was SETTLED (the design, if resumed)

- **Material:** scenery uses the **fine→square** ramp `" ˑ∙▪■"` — an airy
  stipple. The beetle stays **round dots** `" .·•●"`. Different shapes = clean
  figure/ground; this is what fixed the meshing.
- **Rarity:** the backdrop is a **rare event**, not ambient. Most of the time
  the scene plays plain; once in a while (minutes apart, not seconds) a world
  drifts through, then it's empty again.
- **Motion = parallax, not fade.** On a pass, depth layers move at different
  speeds and separate as they cross:
  - **far** — the sun (day) / crescent moon + stars (night) — drifts **slow**.
  - **near** — clouds — drifts **faster**.
  A layer enters from off-screen right, crosses, exits off-screen left; between
  passes it's parked off-screen (no opacity trick).
- **No ground in the backdrop.** The beetle brings its own baked ground; the
  backdrop is sky/scenery only, placed in the upper region, so nothing fights
  the beetle's feet.
- **Moods:** **sun** and **moon** are keepers. **Forest was rejected** (too
  dense, camouflaged the beetle). If a third is ever wanted, it must stay
  sparse.

## Open questions never resolved (decide before building)

- **Trigger:** time-based (every N minutes) vs meaning-based (a specific mood on
  a specific state moment — e.g. daybreak on all-clear, night on idle/queued).
- **Speed gap:** clouds currently overtake the sun (correct parallax); confirm
  it doesn't read odd, else narrow the gap.
- **Moon crescent** was a touch full/chunky — slim it for a finer sliver.

## Build recipe (if we ever un-park this)

The exploration bakes/compositors were **throwaway** (scratchpad only, deleted).
To rebuild:

- **Bake per-depth layers**, each on its own transparent canvas of its own width
  (no ground band), luminance → fine→square ramp:
  - day.far = sun: draw a dim halo disk **then** a bright core disk on top
    (order matters — the halo must not paint over the core).
  - night.far = crescent (full disk, then `destination-out` a shifted disk) +
    a few 1px stars.
  - {day,night}.mid = a handful of `cloud()` blobs (overlapping ellipses).
- **Player scheduler:** a pass every ~CYCLE; within it, each layer's X drifts
  `worldW → -layerW` over its own duration (far slower than mid); parked at
  `worldW + layerW` (hidden, poised) when idle. Honor `prefers-reduced-motion`
  (skip passes) and pause on `document.hidden`, like the existing players.
- **Placement:** world ≈150×43 cells; beetle composited at ~30% width, its baked
  ground row aligned to a world ground row; backdrop layers at the top.
- **Format:** would extend the scene JSON with a `backdrop` block (per-depth
  layer text + widths + speeds), consumed by both players — mirroring how the
  runtime `bubble` block already works.

## Placement rules (would extend DESIGN.md §5)

Backdrops are **animated scenery** — same restrictions as animated scenes:
landing/state-moments only, never ambient behind live data, honor reduced
motion. And **rare** on top of that: a backdrop pass is an occasional treat, not
a loop the eye can lock onto.
