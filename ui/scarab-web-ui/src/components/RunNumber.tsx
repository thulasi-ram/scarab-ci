// The run's identity handle (ADR-0057 amendment). Shows the per-repo **run
// number** `#N` — the human handle — and, when absent (untenanted inline runs /
// pre-allocation runs), degrades to the short internal id. Click copies the
// handle. Shared by the runs-list gutter and the run-detail bar so the two can
// never drift. The opaque internal UUIDv7 id stays the key/URL; this is display.
import { Show } from "solid-js";

export default function RunNumber(props: {
  n?: number | null;
  /** The internal run id — copied (and shown short) when no number exists. */
  id: string;
  /** Show a small "run" caption above the number (the run-detail gutter). */
  cap?: boolean;
}) {
  const copy = (e: MouseEvent) => {
    e.stopPropagation(); // never trigger the row's navigate
    const val = props.n != null ? `#${props.n}` : props.id;
    navigator.clipboard?.writeText(val).catch(() => {});
  };
  return (
    <span
      class="runnum"
      classList={{ fallback: props.n == null }}
      title={props.n != null ? `run #${props.n} · ${props.id} — click to copy` : `${props.id} — click to copy`}
      onClick={copy}
    >
      <Show when={props.cap}>
        <span class="cap">run</span>
      </Show>
      <Show when={props.n != null} fallback={<span class="n mono">{props.id.slice(0, 7)}</span>}>
        <span class="n">
          <span class="hsh">#</span>
          {props.n}
        </span>
      </Show>
    </span>
  );
}
