# State-moment scene repertoire (direction)

Status: **spec / next up.** This replaces the four ponderer pose-variations
(`ponder` / `nap` / `kingofhill` / `faceplant`) with a richer set of quiet
beetle vignettes. Not built yet — to be prototyped as samples first (the way
every prior visual call in this system was), then baked.

## Spirit

**Hollow Knight — in spirit, not in visuals.** We keep our own look (the
dot-matrix dung beetle + gold ball, emerald / gold / gray role layers, round-dot
ramp, square cells). What we borrow is the *feeling*: small, lonely, deliberate
moments of a tiny creature resting in a vast, heavy world — a beat for the player
to breathe. Each scene is a still-ish pause with one small, telling action.

## Translating to OUR system (what maps, what we drop)

Our vocabulary already covers most of the "effects" once you strip the game HUD:

| Their device | Our equivalent |
|---|---|
| Particle icon (compass, sparkle, ?) | a **dot-matrix doodle** from `dot-icons.mjs`, or a single bright gleam dot, or the runtime **speech bubble** |
| Constellation / stars | the **star dots** from the parked backdrop (`BACKDROPS.md`), fine-square, faint |
| Soundwave ripples, water/sweat droplets, condensation, sketched map lines | **loop-perfect dot particles** in the right role layer (gold = ball/gleam, gray = dust/water/ground, emerald = beetle) |
| Screen-shake, camera pull-back, spotlight/vignette, audio | **dropped** — no audio, no camera; at most a small sprite jitter or a faint backdrop dim |
| In-game triggers (drip ceiling, resin pool) | **dropped** — each is simply a distinct state moment, no world condition |

Everything stays on the three baked role layers + the runtime bubble; scene-
specific particles are extra loop-perfect dot animations layered in. Reuse the
ponder "bubble stage" scaffolding (roll-in → hold pose → roll-on, `envelope()`,
`ponderPose`/`ponderAnchor`): each scene is a new held-pose function plus its
particles.

## The eight scenes → state moments

1. **The Stargazer** — climbs atop the locked ball, sits back, head tilted up; a
   few faint star dots drift above. *(evolves the `kingofhill` climb + backdrop
   stars.)* → **idle / empty**, occasional variant ("nothing running — looking up").
2. **The Sound of the Sphere** — kneels, presses head flat to the ball, eyes
   shut, antennae twitching; concentric gold dot-rings ripple outward from the
   ball and fade. → **loading / waiting on something**.
3. **The Fragile Repair** — pats a crack in the ball (a gap in the gold rim),
   scoops a gray dust dot, presses gold into the fracture; a brief bright gleam
   on completion. → **retrying / healing a degraded run**.
4. **The Amber Mirror** — sits facing the ball, studying a faint, warped emerald
   *reflection* of itself on the ball's surface (dim mirrored beetle dots). →
   **unknown / indeterminate status** (a quiet "am I still okay?").
5. **The Tactician** — leans back against the ball, sketches glowing dot-lines
   and a small X into the dirt, studies them, sighs, sweeps them away. The sketch
   reads as a little branching graph. → **queued / planning** (charts the run
   DAG — fits Scarab directly).
6. **The Weight of the World** — lifts the ball overhead, legs shaking (jitter),
   holds a beat, drops it, flops onto its back panting, rolls back up; a couple
   of gray sweat dots fling off. → **running heavy / under load**.
7. **The Cleansing Drop** — a gray droplet falls; the beetle scrubs mud off the
   ball, revealing a shimmering gold patch (a section brightens gray → gold) with
   a gleam. → **all-clear / success** (the grime lifts to reveal gold).
8. **The Traveler's Rest** — slumps beside the ball, head hung low, the ball's
   shadow over it, breathing slowed, faint gray condensation wisps rising. →
   **done / exhausted idle** (all runs complete).

(Eight scenes, ~seven states — Stargazer doubles as a rare idle flourish.)

## Build plan

- **Samples first:** prototype 2–3 (likely Traveler's Rest, Tactician, Cleansing
  Drop — they span rest / planning / success and exercise the particle system),
  get the Hollow-Knight-spirit translation right, then bake the rest.
- **Retire** `nap` / `kingofhill` / `faceplant`; keep one simple idle live until
  replacements land so nothing breaks.
- **Format:** each stays a bubble-stage JSON (`{cols,rows,fps,frames,bubble}`);
  particle layers ride the existing three role layers, so the players need no new
  concepts. Any doodle-as-icon beat pulls an existing `generated/doodles/*.svg`.
- **Placement:** state moments only (DESIGN.md §5) — never ambient behind live
  data; honor `prefers-reduced-motion` (hold one frame) and pause when hidden.
