# State-moment scene repertoire (direction)

Status: **parked after 3 prototypes.** The intent is to replace the four
ponderer pose-variations (`ponder` / `nap` / `kingofhill` / `faceplant`) with a
richer set of quiet beetle vignettes. Three were prototyped to calibrate the
spirit (Rest, Carver, Cleansing — see "Prototyping learnings"); the full set was
**deliberately shelved** as over-investment for a pre-1.0 CI engine. Revisit when
brand delight pays off (a public launch / marketing site). Nothing here is baked
into the pipeline; the current `ponder-*` scenes stay live in the meantime.

## Spirit

**Hollow Knight — in spirit, not in visuals.** We keep our own look (the
dot-matrix dung beetle + gold ball, emerald / gold / gray role layers, round-dot
ramp, square cells). What we borrow is the *feeling*: small, lonely, deliberate
moments of a tiny creature resting in a vast, heavy world — a beat to breathe.
Each scene is a still-ish pause with one small, telling action.

## Translating to OUR system (what maps, what we drop)

Our vocabulary already covers most of the "effects" once you strip the game HUD:

| Their device | Our equivalent |
|---|---|
| Particle icon (compass, sparkle, ?) | a **dot-matrix doodle** from `dot-icons.mjs`, or a single bright gleam dot, or the runtime **speech bubble** |
| Constellation / stars | the **star dots** from the parked backdrop (`BACKDROPS.md`), fine-square, faint |
| Soundwave ripples, water/sweat droplets, condensation, shavings | **loop-perfect dot particles** in the right role layer (gold = gleam, gray = dust/water/ground, emerald = beetle) |
| A "z z z" / a spoken word | **literal glyphs** stamped over the baked art (off the dot-language on purpose, exactly like the speech bubble) |
| Screen-shake, camera pull-back, spotlight/vignette, audio | **dropped** — no audio, no camera; at most a small sprite jitter |
| In-game triggers (drip ceiling, resin pool) | **dropped** — each is simply a distinct state moment, no world condition |

Everything stays on the three baked role layers + glyph overlays; scene-specific
particles are extra loop-perfect dot animations. Reuse the ponder "bubble stage"
scaffolding (roll-in → hold pose → roll-on, `envelope()`): each scene is a new
held-pose function plus its particles.

**Two hard rules (from review):**
- The **gold ball is never modified** — no cracks, no mud, no reveals on it. The
  action lives on the beetle; effects live in the air around it.
- Use the **dung-roller carapace** (`drawBeetle`: body 10.5×6.5, prominent head),
  not the smaller ponder body — the ponder oval reads as a ladybug.

## The eight scenes → state moments

1. **The Stargazer** — climbs atop the locked ball, sits back, head tilted up; a
   few faint star dots drift above. → **idle / empty** (occasional flourish).
2. **The Sound of the Sphere** — kneels, ear pressed to the ball, eyes shut,
   antennae twitching; concentric gray dot-rings ripple out and fade. →
   **loading / waiting**.
3. **The Fragile Repair** — mimes patting/blowing dust off its own claws and
   the air around the ball (not on the ball); a brief gleam dot. →
   **retrying / healing a degraded run**.
4. **The Amber Mirror** — sits facing the ball, a faint warped emerald reflection
   of itself hovering just off the ball's surface (in the air, not painted on
   the gold). → **unknown / indeterminate status**.
5. **The Carver** *(was "Tactician")* — sits and whittles a small **souvenir**
   held out in clear space with a little knife; gray shavings flick off. (The
   earlier map/X idea was dropped.) → **idle / between runs**.
6. **The Weight of the World** — lifts the ball overhead, legs shaking (jitter),
   holds a beat, drops it, flops onto its back panting, rolls up; gray sweat
   dots fling off. → **running heavy / under load**.
7. **The Cleansing (Raincatcher)** — a gray droplet falls; the beetle **wipes its
   hands over its face** (grooming). The cleanse is *implied by the gesture* —
   nothing is scrubbed/revealed on the ball. → **all-clear / success**.
8. **The Traveler's Rest** — slumps asleep beside the ball, head hung low,
   breathing slowed; **z z z** glyphs bubble up and rise from its head. →
   **done / exhausted idle**.

(Eight scenes, ~seven states — Stargazer doubles as a rare idle flourish.)

## Prototyping learnings (Rest, Carver, Cleansing)

The medium is a coarse dot matrix; small held props (a knife, a souvenir) merge
into the beetle's dots unless you fight for legibility. What worked:

- **Finer bake grid** (~128 cols for detail scenes) so a small prop resolves into
  several distinct dots instead of one blob.
- **Portrait crop** — beetle large and centred, the (big) ball peeking in from
  the side rather than dominating the frame. At full-ball framing the beetle is
  tiny and nothing small reads. A tight viewport is the single biggest lever.
- **Props in clear space** — hold the object *out*, away from the body, so an
  empty-cell gap surrounds it.
- **Distinct role-colour** per prop (gold souvenir, bright-gray knife) so colour
  separates them from the emerald body even when close.
- Even with all four, fine gestures (a blade angle) sit near the medium's limit —
  favour scenes whose read is **silhouette + gross motion + one clear particle**,
  not fine tool detail. This is partly why the full set is parked.

## Build plan (when un-parked)

- **Samples first**, always (as every visual call here has been).
- **Retire** `nap` / `kingofhill` / `faceplant`; keep one simple idle live until
  replacements land so nothing breaks.
- **Format:** each stays a bubble-stage JSON (`{cols,rows,fps,frames,bubble}`);
  particle + glyph overlays ride the existing three role layers, so the players
  need no new concepts.
- **Placement:** state moments only (DESIGN.md §5) — never ambient behind live
  data; honor `prefers-reduced-motion` (hold one frame) and pause when hidden.
