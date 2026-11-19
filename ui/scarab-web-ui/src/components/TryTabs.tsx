// The try tabs under the step header, for a step with two or more tries in
// the version in view: every try, newest first, each with its outcome ("Try 2
// · failed: step error"), how long it took, and an "of record" mark on the
// try whose outputs downstream steps read; a second line says why the try
// exists and when it started. The row scrolls sideways when the tries do not
// fit, at every width, and keeps the selected try in view. A gate's tries are
// its waits, and a resolved wait says who resolved it. Page chrome, so it
// reads the page tokens.
import { createEffect, createMemo, For, on, Show } from "solid-js";
import { causeText, type FilmstripTry } from "../attempts";
import { compactDuration, statusGlyph } from "../run-page";
import { pluralNoun, tabTries, tryDurationMs, tryWords, type WaitResolution } from "../run-tabs";
import { absTime, relTime } from "../fmt";

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
  /** How the gate's latest wait was resolved, for a gate. */
  resolution?: WaitResolution | null;
}) {
  let list: HTMLDivElement | undefined;
  // The tabs are keyed by try id: the poll rebuilds every try object, and a
  // tab keyed by object would be re-created each second and lose focus.
  const ids = createMemo(() => tabTries(props.tries).map((t) => t.id), undefined, {
    equals: (a, b) => a.length === b.length && a.every((x, i) => x === b[i]),
  });
  const byId = (id: string) => props.tries.find((x) => x.id === id);

  // Keep the selected tab in view by scrolling the row only, never the page.
  createEffect(
    on([() => props.active, ids], () =>
      requestAnimationFrame(() => {
        const el = list?.querySelector<HTMLElement>("[aria-selected=true]");
        if (!list || !el) return;
        const r = el.getBoundingClientRect();
        const c = list.getBoundingClientRect();
        if (r.left < c.left) list.scrollLeft -= c.left - r.left;
        else if (r.right > c.right) list.scrollLeft += r.right - c.right;
      }),
    ),
  );

  return (
    <div class="trytabs" role="tablist" aria-label={`${pluralNoun(props.noun)} of this step`} ref={list}>
      <For each={ids()}>
        {(id) => (
          <Show when={byId(id)}>
            {(t) => {
              const on = () => id === props.active;
              const dur = () => tryDurationMs(t(), props.now);
              return (
                <button
                  type="button"
                  role="tab"
                  class="tt"
                  classList={{ on: on() }}
                  aria-selected={on()}
                  onClick={() => props.onSelect(id)}
                >
                  <span class="tt-l1">
                    <span class={`tt-g ${tone(t())}`} aria-hidden="true">
                      {statusGlyph(t().outcome)}
                    </span>
                    <span>{tryWords(t().index + 1, t(), props.noun, props.resolution)}</span>
                    <Show when={dur() !== null}>
                      <span class="tt-dur">{compactDuration(dur()!)}</span>
                    </Show>
                    <Show when={id === props.ofRecord}>
                      <span class="tt-rec">of record</span>
                    </Show>
                  </span>
                  <small title={t().startedAt ? absTime(t().startedAt!) : undefined}>
                    {[causeText(t().cause), t().startedAt ? `started ${relTime(t().startedAt!)}` : ""]
                      .filter(Boolean)
                      .join(" · ")}
                  </small>
                </button>
              );
            }}
          </Show>
        )}
      </For>
    </div>
  );
}
