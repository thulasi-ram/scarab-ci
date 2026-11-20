// The log body's rows for a log that has no lines to show yet: still loading,
// or could not be read. Shared by the step pane (main container and sidecars)
// and the service pane.
import { For } from "solid-js";

// Varied widths read as text.
const SKELETON_WIDTHS = [46, 72, 58, 83, 35, 64, 77, 50, 69, 40, 61, 74, 44, 67];

/** Shimmer stand-in for log lines. Reuses the `.lgrow` gutter grid so it
 * lands where the real lines will. */
export function LogSkeleton(props: { rows?: number }) {
  return (
    <div class="lgbody lgskel" aria-hidden="true">
      <For each={SKELETON_WIDTHS.slice(0, props.rows ?? SKELETON_WIDTHS.length)}>
        {(w, i) => (
          <div class="lgrow">
            <span class="lgln">{i() + 1}</span>
            <span class="lgskel-bar" style={{ width: `${w}%`, "animation-delay": `${i() * 40}ms` }} />
          </div>
        )}
      </For>
    </div>
  );
}

/** The log has not arrived yet. */
export function LogLoading() {
  return (
    <>
      <div class="lgrow empty" role="status">
        <span class="lgln" />
        <span class="lgtx">Loading the log…</span>
      </div>
      <LogSkeleton rows={4} />
    </>
  );
}

/** `reason` as the end of a sentence: one final period. */
function sentenceEnd(reason: string): string {
  const r = reason.trim();
  return /[.!?]$/.test(r) ? r : `${r}.`;
}

/** The log could not be read: why, and a way to try again. `rest`: part of
 * the log is on screen already. */
export function LogError(props: { reason: string; onRetry: () => void; rest?: boolean }) {
  return (
    <div class="lgrow empty lgwhy" role="alert">
      <span class="lgln" />
      <span class="lgtx">
        <span class="lgwhy-reason">{props.rest ? "Could not load the rest of the log:" : "Could not load the log:"}</span>{" "}
        {sentenceEnd(props.reason)}{" "}
        <button type="button" class="linklike" onClick={() => props.onRetry()}>
          Retry
        </button>
      </span>
    </div>
  );
}
