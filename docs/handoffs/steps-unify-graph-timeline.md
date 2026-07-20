# Handoff — unify the STEPS component: deck-in-DAG attempts + Graph/Timeline

**Status: deck + in-rail fan SHIPPED (Graph); prototype deleted. Timeline
deferred (user: secondary — skipped for now).**

## What shipped (2026-07-20, on `feat/adr-0056-rerun-retry-language`)

Variant A folded into the real components, verified in-browser against run
`8f2b256c…` (`check ×3`, `checkout ×2`) in both light and dark themes:

- **`Dag.tsx`** — a step with `attempts > 1` renders as a **deck** (offset
  shadow-cards + a copper `×N` badge). Selecting it **fans its tries** into a
  compact, full-rail-width **vertical stack directly beneath the node** (not the
  prototype's horizontal fan — that cramps the ~190px rail). Only the *selected*
  node fans; others stay compact decks. Each try card carries cause + outcome +
  shadowed/superseded/readopted state; picking one drives `onAttemptSelect`.
  Edges re-measure when the fan changes the column height (the `.dag` box is
  fixed with inner scroll, so a `createEffect` on `selected`/`tries` + a scroll
  listener re-run `measure()`; coords now include `scrollLeft/Top`).
- **`RunDetail.tsx`** — derives the selected step's tries (`dagTries()`) and the
  active try (`dagActiveAttempt()`, mirroring StepPane's `scoped()`) from the
  event log, honoring the Take frontier; passes them to `<Dag>`.
- **`StepPane.tsx`** — the `.trydrop` attempt dropdown is **gone**; the graph
  owns try selection. The step header now shows a **read-only** "which try
  you're viewing" chip (`try N · cause · outcome`, tone-barred, + shadowed/⟲).
- **CSS** — `.dcell/.ddeck/.ddeck-layer/.dnode-count/.dfan/.dfan-card` +
  `.step-try`. **Gotcha baked in:** the DAG canvas uses the always-dark
  *terminal* palette (`--terminal-ink/-elev/-line`), NOT the theme-flipping page
  tokens (`--soft-white`, `--emerald-surface` go WHITE in light theme). The fan
  must use terminal tokens or it renders white-on-white in light mode.
- **Prototype deleted** — `StepsProto.tsx` + its `/proto` route in `App.tsx`.

## Still open

- **Graph / Timeline sub-tabs** under the "STEPS" header — NOT built (Timeline
  deferred). When picking it up: the toggle + the waterfall (one bar per step,
  tries as colored segments, shadowed dimmed). Real timing is already in
  `RunDetail` (`stepTiming()` → first `AttemptStarted` … last `AttemptFinished`;
  `dagSteps()` exposes `durationMs`). Don't add a lone toggle until Timeline
  exists.

---

## (original design notes — kept for the Timeline pickup)

**Status: design decided (prototype done), implementation NOT started.**

Continues the ADR-0056 run-detail work (see [[docs/adr/0056-run-takes-and-attempt-grain-evidence.md]]
and its 2026-07-20 amendment). The rerun/retry language + one-Pipeline-component
refactor already shipped on branch `feat/adr-0056-rerun-retry-language`
(**PR #44**). This handoff is the *next* slice: how the STEPS column presents
step attempts and sub-views.

## The decision

A `/prototype` session explored three ways to place a step's **attempt
card-stack**. **Variant A — "Deck in the DAG" — won.** Also agreed: the STEPS
column gets **Graph / Timeline** sub-views (space vs time — the same duality the
whole feature is built on).

### What "Variant A" is (the target)

- Each DAG node for a step that has **>1 try** renders as a **deck**: offset
  shadow-layer cards behind the node + a `×N` badge — an at-a-glance "this step
  was retried/rerun N times" signal, right in the graph.
- **Selecting a step fans its tries** out as inline mini-cards (`try 1 ✓`,
  `try 2 · auto-retry`, `try 3 · you reran`, superseded/shadowed styled), each
  clickable. Picking a try scopes the evidence panel. **Attempt selection lives
  in the graph — the attempt dropdown goes away.**
- Evidence (Logs/Results/Outputs/Workspace) stays in the right pane, scoped to
  the selected `(step, try)`.
- **Graph / Timeline toggle** sits under the "STEPS" header. Timeline = a
  waterfall: one bar per step positioned by time, each try a colored segment
  (shadowed dimmed).

### Known weaknesses of pure-A to resolve while productionizing

The user picked A knowing these; decide how far to mitigate:

1. **Layout shift** — fanning tries pushes sibling nodes down. Options: reserve
   space, animate, or fan into an overlay/popover instead of inline.
2. **Busyness at scale** — many decked+fanned nodes crowd the graph. The DAG is
   already vertical/narrow (left rail); consider only fanning the *selected*
   node (prototype does this) and keeping others as compact decks.
3. **Hybrid worth considering** (floated, not chosen): keep A's deck as the
   *signal* in the graph but do try *selection* from a card-stack in the
   evidence header (variant C's placement). Only build if inline-fan feels
   cramped once it's in the real, dense layout.

## The prototype (throwaway — read it, then delete it)

- `ui/scarab-web-ui/src/routes/StepsProto.tsx` — all three variants + a floating
  switcher + a stubbed evidence panel + the Timeline waterfall. Real data from
  the run in the URL; logs are stubbed (layout was the question).
- Route registered in `ui/scarab-web-ui/src/App.tsx`:
  `/:org/:repo/runs/:id/proto?variant=A|B|C` (marked PROTOTYPE).
- Run it: `just ui` (or `SCARAB_API_URL=http://127.0.0.1:8080 npm --prefix
  ui/scarab-web-ui run dev`), then open
  `/{org}/{repo}/runs/{id}/proto?variant=A`. Variant A's logic (deck depth, fan,
  try labels/tones, timing) is the reference implementation — **lift its shape,
  rewrite properly** (no stubs, real SSE logs) when folding in.

**When done: delete `StepsProto.tsx` and its route in `App.tsx`.**

## Build plan (fresh session)

1. **Graph / Timeline sub-tabs** under the "STEPS" header in `RunDetail.tsx`
   (mirror the evidence-side tabs). Default Graph.
2. **Deck + fan in the DAG** — this is the real work. `Dag.tsx` currently renders
   one node per step (vertical, top-to-bottom). Add: deck shadow layers + `×N`
   when `step.attempts > 1`; on select, fan the tries (from `attempt_list` +
   `attemptCauses`). Drive `onAttemptSelect` from the fan.
3. **Remove the attempt ("try") dropdown** from `StepPane.tsx` (`.trydrop`) — the
   graph now owns try selection. `StepPane` becomes pure evidence for the
   passed-in `(step, attempt)`.
4. **Timeline view** — real timing off the event log. The prototype approximates
   step end via the last terminal `StepTransitioned` event and start via first
   `AttemptStarted`; do it properly (RunDetail already derives per-step
   durations for the DAG — see `dagSteps()` `durationMs`). Tries = segments.
5. **Delete the prototype** (file + route).

## Key code & data (all already in the tree)

- **`ui/scarab-web-ui/src/takes.ts` → `attemptCauses(events, stepId)`** returns
  `{ causes, readopted, superseded, shadowed }`. Cause ∈ `initial | retry |
  rerun | cascade`. This is the source of every try's label/outcome — reuse it.
- **Step/attempt shape**: `StepStatusDto { id, status, attempts, attempt_list?,
  needs?, gate? }`; `AttemptDto { id, failed, failure?, started_at }`.
- **Run detail DTO is thin** — `GET /v1/runs/{id}` returns only `id/status/steps`.
  Provenance (trigger/actor/branch/commit) and timing come from the **event log**
  (`fetchEvents(id)`): a `Raw` trigger event carries `trigger.event.{actor,
  branch,kind,ref,sha}`; `StepTransitioned` / `AttemptStarted` carry timing.
  RunDetail already parses the trigger event (`triggerInfo()`).
- **Vocabulary/labels** (keep consistent): `causeSuffix` → " · you reran" /
  " · ⟵ rerun" / " · auto-retry"; outcome → `✓ succeeded` / `✗ failed` /
  `⊘ superseded`; `shadowed` = finished-but-not-of-record.
- Everything is client-derived from the event log (ADR-0056's no-store rule) —
  no backend change needed for any of this.

## Environment / traps (from the last session)

- `just up` (proc stack) needs `kind` (`brew install kind`) and docker compose
  v2 (`brew install docker-compose` + symlink into `~/.docker/cli-plugins`). It
  was also blocked by prior-session leftovers on `:55432`. The browser check
  reused a **running colima Helm server** via the existing `:8080` port-forward
  + Vite (`SCARAB_API_URL=http://127.0.0.1:8080`, landed on `:5174`). Run
  `8f2b256c-a878-476d-95e2-7290580c296c` has a rerun (2 versions, `check ×3`) —
  a good specimen.
- **RTK mangles `docker`/`curl`/`grep` output** — use `rtk proxy <cmd>` for raw.
- `main` isn't fmt-clean; hand-format Rust edits, never repo-wide `cargo fmt`.

## PR #44 state

Branch `feat/adr-0056-rerun-retry-language`, ~9 commits: rerun/retry language +
one Pipeline component, engine orphan-Pod teardown (git-bug `fd6e6d4`), and UI
polish (flat headers, transparent body + border, green dropdowns, attempt
dropdown, caps section headers, restored top-bar metadata, tab alignment). This
STEPS/Timeline work can extend PR #44 or land as its own PR — your call.
