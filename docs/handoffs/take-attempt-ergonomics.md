# Handoff — Take vs attempt: end-user ergonomics & language

> **RESOLVED (2026-07-20).** The design below is settled and captured in the
> **ADR-0056 amendment** ("end-user language, retry vs rerun, superseded/
> shadowed") and the `CONTEXT.md` glossary (Rerun, Auto-retry, Cascade,
> Superseded, Shadowed; Take realigned to internal-only). Implementation is on
> branch `feat/adr-0056-rerun-retry-language` and tracked in git-bug:
> `ca6b530` (read model), `10d346e` (merge into one Pipeline component),
> `b1d9d13` (tries vocabulary + read-only version view + "Rerun" verb),
> `fd6e6d4` (engine follow-up: tear down the orphaned Pod on a superseded
> in-flight attempt — the one deferred piece). The resolved model in one line:
> **the run is a timeline; one human button (Rerun) forks a new version row;
> the machine's auto-retry stays within a row; every try is kept.** The
> brainstorm below is retained for the reasoning trail.

---

A design brainstorm (now **resolved**, see above). This session shipped a batch of
Run-detail / Run-pipeline UI polish (see the PR this doc lands in) and then
opened a bigger question we continued in a fresh session: how do we
explain the run-history model to an end user *without* leaking our internal
constructs ("Take", "attempt")?

**Read first:** ADR-0056 (Run Takes — the derived version lens) and `CONTEXT.md`
§ glossary (Take, attempt). Then skim `ui/scarab-web-ui/src/routes/RunDetail.tsx`
(the Take dropdown + `take-banner` + `timeTraveling()`), `src/takes.ts`
(`deriveTakes` / `replayTake`), and `src/components/StepPane.tsx` (the attempt
strip). The memory note `scarab-adr-0056-takes` has more.

## The problem we're solving

Today two history controls are presented as **peers**, and both move on the same
action (a manual restart), which is the core confusion:

- **attempt** — step-scoped, the *evidence* grain (Logs/Results/Outputs/Workspace
  are scoped to a `(step, attempt)`). Chips in the strip inside the step pane.
  Every auto-retry **and** every manual restart adds one. No mode switch.
- **Take** — run-scoped, the *time-travel* grain. A `<select>` in the run header.
  One per **human restart**; selecting a closed one flips the whole screen
  read-only, truncates the event log at the boundary, and replays statuses.

Restart step `check` → the strip gains `check@a2` **and** the header gains
"Take 2 (latest)". If you later want the old state, it's ambiguous which control
to use, and they don't even show the same thing (Take 1 = the *whole run* as of
the restart; `check@a1` = *just that step's* first execution).

## Decisions / leanings reached this session (tentative — confirm next session)

1. **attempt is the workhorse; Take is the exception view.** ~95% of inspection
   is "evidence for execution N of this step" — attempts nail that with no mode
   switch. Take earns its keep only for "reconstruct the whole run before I hit
   restart."
2. **Read-only should be tied to the restart (Take) boundary, NOT to changing
   attempts.** Changing an attempt is a *lens*, not a rewind — the rest of the
   run stays in the present, so forcing read-only there would falsely imply a
   rewind and kill fast a1-vs-a2 comparison. The clean rule:
   > Read-only ⟺ you're looking across a restart boundary into a closed Take.
   > Attempts within the current Take are live; attempts inside a closed Take
   > bring the read-only run with them.
3. This makes a **"strip segmented by Take"** idea pay off — the segment boundary
   *is* the read-only boundary; entering a closed segment = time-travel, one
   click, unambiguous. And time-travel would be reached from the Activity rail's
   restart events, demoting the header dropdown to a breadcrumb.

## The actual open question (where the new session picks up)

> **Find an elegant, jargon-free way to explain this to the end user** — the user
> explicitly does NOT want "Take"/"attempt" to be the explanation. Brainstorm
> more framings. The best answer likely uses ONE familiar mental model + plain
> verbs, leaving the two-grain distinction *implicit in what you click*.

Framings floated so far (unranked except the lean):

- **DVR / rewind a recording — current lean.** Run = live broadcast + recording.
  "Now" is Live; scrub back to a restart point → "Playback" (read-only, because
  you can't edit a recording). Auto-retries = tiny timeline blips; restarts =
  labeled scrub points. Collapses both grains into the timeline (which *is* the
  Activity rail), makes read-only self-evident, lets "Take" retreat to an
  internal word. User-facing sentence: *"Scrub back to a restart to watch how the
  run looked then — recordings are read-only; live is where you act."*
- **Version history (Docs/Figma).** Restart = save a version; old versions open
  read-only; auto-retries = autosave noise. Risk: "version" collides with the
  *code* version / commit the run built.
- **Snapshots.** Each restart leaves a frozen photo of the whole run. Clean but
  passive; weaker on conveying the causal "you did this."
- **No coined noun at all.** Plain verbs + timestamps on the rail
  ("14:32 you restarted check → see how the run looked then"; viewing: "As of
  14:32, before your restart · read-only"). Most jargon-free; leans entirely on
  the timeline, no compact switcher handle.

Cross-cutting regardless of metaphor — the **two "agains" need distinct plain
language**: system re-ran → **"auto-retried"**; you re-ran → **"you restarted"**;
and keep **"New run"** (a whole different run) clearly separate.

### Questions left on the table for the user

1. Does **Live ↔ Playback/rewind** land, or too media-y for an ops tool?
2. OK to **retire the visible noun entirely** (no "Take"/"Version" chip — just a
   scrubable timeline + a Live/Playback state)?
3. Any framing they actively dislike (fastest way to converge)?

## What shipped this session (context for the PR this doc rides in)

Run-detail / Run-pipeline polish, all client-side, `tsc --noEmit` clean,
spot-checked in the browser against the local proc stack:

- Activity panel: dropped the card surface — lays flat on the page background.
- Artifacts panel: same flat treatment (consistent run-level footer with
  Activity); empty-state + rows aligned to the page margin.
- Run-pipeline ref picker: resolved commit SHA moved below the input.
- Secrets: inline inputs replaced with a **New secret modal** carrying an
  **overwrite guard** (Save disabled until "overwrite" is ticked for an existing
  name — the API is an unconditional upsert).
- Logs empty state (`no output`): now has the line-number gutter.
- Workspace tab: no longer tears down the pane on a failed browse — `listWorkspace`
  and `fileBody` both swallow errors; the no-snapshot state offers **"Explore in a
  Debug pod →"** (with a note that a Debug pod is a *reproduction*, not the
  attempt's immutable bytes).
- `.rd-grid` gained `margin-bottom` so Artifacts no longer butts against the grid.

## Related, tracked separately

git-bug **`4c27eac`** — searchable branch/tag ref picker (needs a new
`GET .../refs` endpoint) **and** restoring Runs-list filters by PR / user /
branch (client-side; `RunSummaryDto` already carries `actor`/`git_ref`/
`pr_number`). Independent of the Take/attempt work above.
