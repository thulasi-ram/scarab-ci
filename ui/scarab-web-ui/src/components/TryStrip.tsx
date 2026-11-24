// The step header's tries, for a step with two or more tries in the version in
// view, drawn like the run's version ribbon: one numbered dot per try, oldest
// first, coloured by its outcome and joined by a short line, the selected one
// outlined; past RIBBON_DOTS tries, one tick each. The row says at a glance
// what passed and failed and in what order, and switches tries in one click. Hovering or focusing a button
// shows its card: the outcome ("Try 3 · failed: timed out"), how long it took,
// why it exists and when it started, and "of record" on the try whose outputs
// downstream steps read. A gate's tries are its waits, and a resolved wait says
// who resolved it. A step with one try shows no strip. Page chrome, so it reads
// the page tokens.
import { createMemo, For, Show } from "solid-js";
import { causeText, type FilmstripTry } from "../attempts";
import { compactDuration } from "../run-page";
import { pluralNoun, tryDurationMs, tryWords, type WaitResolution } from "../run-tabs";
import { absTime, relTime } from "../fmt";
import { RIBBON_DOTS } from "./VersionRibbon";

export default function TryStrip(props: {
  tries: FilmstripTry[];
  /** The selected try. */
  active: string | null;
  /** The try of record in this version, if any. */
  ofRecord: string | null;
  onSelect: (id: string | null) => void;
  /** "Try", or "Wait" for a gate. */
  noun: string;
  now: number;
  /** How the gate's latest wait was resolved, for a gate. */
  resolution?: WaitResolution | null;
}) {
  // Keyed by try id: the poll rebuilds every try object, and a button keyed
  // by object would be re-created each second and lose focus.
  const ids = createMemo(() => props.tries.map((t) => t.id), undefined, {
    equals: (a, b) => a.length === b.length && a.every((x, i) => x === b[i]),
  });
  const byId = (id: string) => props.tries.find((x) => x.id === id);
  // With no try picked, the newest is the one shown.
  const activeId = () => props.active ?? props.tries[props.tries.length - 1]?.id ?? null;

  return (
    <div class="tstrip" role="tablist" aria-label={`${pluralNoun(props.noun)} of this step`}>
      <span class="tstrip-k" aria-hidden="true">
        {pluralNoun(props.noun)}
      </span>
      <For each={ids()}>
        {(id, i) => (
          <Show when={byId(id)}>
            {(t) => {
              const on = () => id === activeId();
              const dur = () => tryDurationMs(t(), props.now);
              const words = () => tryWords(t().index + 1, t(), props.noun, props.resolution);
              const sub = () =>
                [causeText(t().cause), t().startedAt ? `started ${relTime(t().startedAt!)}` : ""]
                  .filter(Boolean)
                  .join(" · ");
              const rec = () => id === props.ofRecord;
              const dots = () => props.tries.length <= RIBBON_DOTS;
              return (
                <>
                <Show when={dots() && i() > 0}>
                  <span class="vr-link" aria-hidden="true" />
                </Show>
                <button
                  type="button"
                  role="tab"
                  class={`tstrip-b ${dots() ? "vr-dot" : `vr-tick ${t().outcome}`}`}
                  classList={{ rec: rec() }}
                  aria-selected={on()}
                  aria-label={[words(), dur() !== null ? compactDuration(dur()!) : "", rec() ? "of record" : "", sub()]
                    .filter(Boolean)
                    .join(", ")}
                  onClick={() => props.onSelect(id)}
                >
                  <Show when={dots()}>
                    <span class={`vr-pip ${t().outcome}`} aria-hidden="true" />
                    <span class="tstrip-n">{t().index + 1}</span>
                  </Show>
                  <span class="tstrip-card" aria-hidden="true">
                    <span class="tstrip-l1">
                      {words()}
                      <Show when={dur() !== null}>
                        <span class="tstrip-dur"> · {compactDuration(dur()!)}</span>
                      </Show>
                      <Show when={rec()}>
                        <span class="tstrip-rec">of record</span>
                      </Show>
                    </span>
                    <Show when={sub()}>
                      <span class="tstrip-l2" title={t().startedAt ? absTime(t().startedAt!) : undefined}>
                        {sub()}
                      </span>
                    </Show>
                  </span>
                </button>
                </>
              );
            }}
          </Show>
        )}
      </For>
    </div>
  );
}
