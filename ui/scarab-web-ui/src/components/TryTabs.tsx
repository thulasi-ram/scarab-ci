// The try tabs under the step header, for a step with two or more tries in
// the version in view: newest first, each with its outcome ("Try 2 · failed:
// step error"), how long it took, and an "of record" mark on the try whose
// outputs downstream steps read; a second line says why the try exists and
// when it started. Past four tries the older ones go in an "Earlier tries"
// menu, whose trigger names the selected try when the selection is in there.
// A gate's tries are its waits. Page chrome, so it reads the page tokens.
import { For, Show } from "solid-js";
import { causeText, type FilmstripTry } from "../attempts";
import { compactDuration, statusGlyph } from "../run-page";
import { foldTries, pluralNoun, tryDurationMs, tryWords } from "../run-tabs";
import { absTime, relTime } from "../fmt";
import { TryMenu } from "./AttemptsDropdown";

const tone = (t: FilmstripTry): string =>
  t.outcome === "succeeded" ? "passed" : t.outcome === "failed" ? "failed" : t.outcome === "running" ? "running" : "muted";

export default function TryTabs(props: {
  tries: FilmstripTry[];
  /** The selected try. */
  active: string | null;
  /** The try of record in this version, if any. */
  ofRecord: string | null;
  onSelect: (id: string | null) => void;
  /** "Try", or "Wait" for a gate. */
  noun: string;
  now: number;
}) {
  const fold = () => foldTries(props.tries, props.active);
  const selectedEarlier = () => props.tries.find((t) => t.id === props.active && fold().selectedEarlier);

  return (
    <div class="trytabs">
      <div class="trytabs-list" role="tablist" aria-label={`${pluralNoun(props.noun)} of this step`}>
        <For each={fold().shown}>
          {(t) => {
            const on = () => t.id === props.active;
            const dur = () => tryDurationMs(t, props.now);
            return (
              <button
                type="button"
                role="tab"
                class="tt"
                classList={{ on: on() }}
                aria-selected={on()}
                onClick={() => props.onSelect(t.id)}
              >
                <span class="tt-l1">
                  <span class={`tt-g ${tone(t)}`} aria-hidden="true">
                    {statusGlyph(t.outcome)}
                  </span>
                  <span>{tryWords(t.index + 1, t, props.noun)}</span>
                  <Show when={dur() !== null}>
                    <span class="tt-dur">{compactDuration(dur()!)}</span>
                  </Show>
                  <Show when={t.id === props.ofRecord}>
                    <span class="tt-rec">of record</span>
                  </Show>
                </span>
                <small title={t.startedAt ? absTime(t.startedAt) : undefined}>
                  {[causeText(t.cause), t.startedAt ? `started ${relTime(t.startedAt)}` : ""]
                    .filter(Boolean)
                    .join(" · ")}
                </small>
              </button>
            );
          }}
        </For>
      </div>
      <Show when={fold().earlier.length > 0}>
        <TryMenu
          rows={fold().earlier}
          active={props.active}
          onSelect={props.onSelect}
          noun={props.noun}
          class="tt tt-more"
          selected={fold().selectedEarlier}
          label={
            <Show when={selectedEarlier()} fallback={<>Earlier {pluralNoun(props.noun).toLowerCase()} ({fold().earlier.length})</>}>
              {(t) => (
                <>
                  {props.noun} {t().index + 1}
                </>
              )}
            </Show>
          }
        />
      </Show>
    </div>
  );
}
