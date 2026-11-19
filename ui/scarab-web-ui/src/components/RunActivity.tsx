// The run's Activity tab: the event log, newest first, with three filters.
// "Run and steps" (the default) hides the Pod conditions that have cleared, a
// "waiting for a node" that came and went; a condition still holding a step
// back always shows, highlighted. "Infrastructure" shows only the Pod
// conditions and control-plane re-adoptions; "Everything" shows all of it. The
// filter only changes what this tab shows. The run's trigger reads
// "Triggered by …". Rows name a step by its display name and count tries
// within their version; a step's move to passed or failed shows on the row
// of the try that finished that way.
import { createMemo, createSignal, For, Show } from "solid-js";
import type { RunEvent } from "../api/client";
import { eventCategory, EVENT_GLYPH } from "../events";
import { activityHiddenNote, activityView, activityWords, isTriggerEvent, type ActivityFilter } from "../run-tabs";
import { absTime, relTime } from "../fmt";

export default function RunActivity(props: {
  /** The events as of the version in view, oldest first. */
  events: RunEvent[];
  /** Events after the version in view (an older version only). */
  later: number;
  /** The conditions holding a step back now, by step. */
  active: Record<string, { attempt: string; since: number }>;
  /** The trigger event in words. */
  trigger: string | null;
}) {
  const [filter, setFilter] = createSignal<ActivityFilter>("steps");
  const view = () => activityView(props.events, props.active, filter());
  const note = () => activityHiddenNote(view().hiddenCleared, props.later);
  const words = createMemo(() => activityWords(props.events));

  const Chip = (p: { f: ActivityFilter; label: string }) => (
    <button type="button" class="chip" aria-pressed={filter() === p.f} onClick={() => setFilter(p.f)}>
      {p.label} <span class="mono">{view().counts[p.f]}</span>
    </button>
  );

  return (
    <div class="panel run-tab-panel">
      <div class="rt-filters" role="group" aria-label="Activity filter">
        <Chip f="steps" label="Run and steps" />
        <Chip f="infra" label="Infrastructure" />
        <Chip f="all" label="Everything" />
        <span class="grow1" />
        <span class="subtle">Newest first</span>
      </div>
      <div class="tl rt-tl">
        <For each={view().rows} fallback={<div class="tl-empty">No events to show.</div>}>
          {(r) => {
            const cat = eventCategory(r.event);
            const parts = words()(r.event, r.merged);
            const trigger = isTriggerEvent(r.event) && props.trigger;
            return (
              <div class="tl-item" classList={{ "tl-active": r.cls === "active", "tl-cleared": r.cls === "cleared" }}>
                <div class="tl-time" title={absTime(r.event.at)}>
                  {relTime(r.event.at)}
                </div>
                <div class="tl-rail">
                  <div class={`tl-glyph ${cat}`}>{EVENT_GLYPH[cat]}</div>
                </div>
                <div class="tl-body">
                  <div class="tl-msg">
                    <Show when={!trigger} fallback={<>{trigger}</>}>
                      <Show when={parts.step}>
                        <b class="tl-step mono" title={parts.stepId ?? undefined}>
                          {parts.step}
                        </b>
                      </Show>
                      {parts.text}
                      <Show when={r.cls === "active"}>
                        <span class="tl-still"> · still active</span>
                      </Show>
                    </Show>
                  </div>
                </div>
              </div>
            );
          }}
        </For>
      </div>
      <Show when={note().cleared || note().later}>
        <div class="rt-hidden">
          <Show when={note().cleared}>
            {(c) => (
              <span>
                {c()} ·{" "}
                <button type="button" class="linklike" onClick={() => setFilter("infra")}>
                  Show
                </button>
              </span>
            )}
          </Show>
          <Show when={note().cleared && note().later}>
            <span> · </span>
          </Show>
          <Show when={note().later}>{(l) => <span>{l()}</span>}</Show>
        </div>
      </Show>
    </div>
  );
}
