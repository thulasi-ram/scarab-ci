// The Pipeline panel's List view: every step in the order the run lists it
// (its definition order, so a step comes after what it needs), the cells of a
// matrix grouped under one heading. The All / Failed filter lives in the
// pipeline bar and comes in as `filter`. Each row gives the status glyph, the
// step's name or its cell values, its status in words (with a dashed "from
// v2" tag for a step carried over from an earlier version) and how long it
// took. A step with two or more tries in the version in
// view lists them under its row, newest first, each opening that try. Shared
// services come first, as rows that open the service. For runs too big to
// read as a graph. Drawn in the same dark panel as the graph.
import { createMemo, For, Show } from "solid-js";
import type { DagService, DagStep } from "./Dag";
import {
  compactDuration,
  matrixHeading,
  segmentByMatrix,
  statusGlyph,
  statusWord,
  type MatrixSegment,
} from "../run-page";
import { nestedTries, tryDurationMs, tryWords, type WaitResolution } from "../run-tabs";
import { causeText, type FilmstripTry } from "../attempts";

/** The words for a row: "Passed · 2 tries", "Waiting for a node". */
function rowWords(s: DagStep): string {
  if (s.note?.tone === "warning") return s.note.text.charAt(0).toUpperCase() + s.note.text.slice(1);
  const w = statusWord(s.status);
  return s.attempts > 1 ? `${w} · ${s.attempts} tries` : w;
}

/** The glyph's tone class. */
function tone(s: DagStep): string {
  if (s.note?.tone === "warning") return "warning";
  return s.status;
}

export default function StepList(props: {
  steps: DagStep[];
  services?: DagService[];
  selected: string | null;
  onSelect: (id: string) => void;
  now: number;
  /** A step's tries in the version in view. */
  triesOf?: (id: string) => FilmstripTry[];
  /** The selected step's selected try. */
  activeTry?: string | null;
  /** Open one try of a step. */
  onSelectTry?: (step: string, attempt: string) => void;
  /** How a gate step's latest wait was resolved. */
  resolutionOf?: (id: string) => WaitResolution | null;
  /** Which steps to show; the pipeline bar switches it. */
  filter?: "all" | "failed";
  /** The version a carried-over step was reused from; null when it ran here. */
  carriedFrom?: (id: string) => number | null;
}) {
  const filter = () => props.filter ?? "all";
  const shown = () => (filter() === "failed" ? props.steps.filter((s) => s.status === "failed") : props.steps);
  // Rows are keyed by id, never by object: the page rebuilds every step object
  // each second (the clock tick), and a row keyed by object would be re-created
  // and lose focus. A segment that did not change keeps its identity.
  const stepById = createMemo(() => new Map(props.steps.map((s) => [s.id, s])));
  let segCache = new Map<string, MatrixSegment<string>>();
  const segments = createMemo(() => {
    const next = new Map<string, MatrixSegment<string>>();
    const out = segmentByMatrix(
      shown().map((s) => s.id),
      (id) => id,
    ).map((seg) => {
      const key = `${seg.matrix ? matrixHeading(seg.matrix) : ""}\u0000${seg.items.map((i) => i.item).join("\u0001")}`;
      const kept = segCache.get(key) ?? seg;
      next.set(key, kept);
      return kept;
    });
    segCache = next;
    return out;
  });
  const serviceNames = () => (props.services ?? []).map((s) => s.name);
  const serviceStatus = (name: string) => props.services?.find((s) => s.name === name)?.status ?? "";
  const took = (s: DagStep): string =>
    s.status === "running" && s.runningSince
      ? compactDuration(props.now - s.runningSince)
      : s.durationMs != null
        ? compactDuration(s.durationMs)
        : "";

  return (
    <div class="slist">
      <Show when={filter() === "all" && serviceNames().length > 0}>
        <div class="slist-grp">Shared services</div>
        <For each={serviceNames()}>
          {(name) => {
            const id = `service:${name}`;
            return (
              <button
                type="button"
                class="slist-row"
                classList={{ sel: props.selected === id }}
                aria-pressed={props.selected === id}
                onClick={() => props.onSelect(id)}
              >
                <span class="slist-g service" aria-hidden="true">
                  ⬡
                </span>
                <span class="slist-name">{name}</span>
                <span class="slist-words">{serviceStatus(name)}</span>
                <span class="slist-dur" />
              </button>
            );
          }}
        </For>
      </Show>
      <For
        each={segments()}
        fallback={<p class="empty slist-empty">No step failed in this version.</p>}
      >
        {(seg) => (
          <>
            <Show when={seg.matrix}>
              {(m) => (
                <div class="slist-grp" title={matrixHeading(m())}>
                  {m().base} <span class="mono">matrix · {m().keys.join(" · ")}</span>
                </div>
              )}
            </Show>
            <For each={seg.items}>
              {(it) => (
                <Show when={stepById().get(it.item)}>
                  {(step) => {
                    const id = it.item;
                    const tries = createMemo(() => (props.triesOf ? nestedTries(props.triesOf(id)) : []));
                    const tryIds = () => tries().map((t) => t.id);
                    const tryById = (tid: string) => tries().find((t) => t.id === tid);
                    return (
                      <>
                        <button
                          type="button"
                          class="slist-row"
                          classList={{ sel: props.selected === id, cell: !!seg.matrix }}
                          aria-pressed={props.selected === id}
                          onClick={() => props.onSelect(id)}
                          title={id}
                        >
                          <span class={`slist-g ${tone(step())}`} aria-hidden="true">
                            {step().note?.tone === "warning" ? "◷" : statusGlyph(step().status)}
                          </span>
                          <span class="slist-name">{it.label}</span>
                          <span class="slist-words">
                            {rowWords(step())}
                            <Show when={props.carriedFrom?.(id)}>
                              {(v) => (
                                <span class="slist-from" title="Carried over from an earlier version">
                                  from v{v()}
                                </span>
                              )}
                            </Show>
                          </span>
                          <span class="slist-dur">{took(step())}</span>
                        </button>
                        <For each={tryIds()}>
                          {(tid) => (
                            <Show when={tryById(tid)}>
                              {(t) => {
                                const on = () => props.selected === id && props.activeTry === tid;
                                const dur = () => tryDurationMs(t(), props.now);
                                return (
                                  <button
                                    type="button"
                                    class="slist-try"
                                    classList={{ sel: on(), cell: !!seg.matrix }}
                                    aria-pressed={on()}
                                    onClick={() => props.onSelectTry?.(id, tid)}
                                  >
                                    <span class={`slist-g ${t().outcome}`} aria-hidden="true">
                                      {statusGlyph(t().outcome)}
                                    </span>
                                    <span class="slist-try-w">
                                      {tryWords(
                                        t().index + 1,
                                        t(),
                                        step().gate ? "Wait" : "Try",
                                        props.resolutionOf?.(id) ?? null,
                                      )}
                                    </span>
                                    <span class="slist-words">{causeText(t().cause)}</span>
                                    <span class="slist-dur">{dur() !== null ? compactDuration(dur()!) : ""}</span>
                                  </button>
                                );
                              }}
                            </Show>
                          )}
                        </For>
                      </>
                    );
                  }}
                </Show>
              )}
            </For>
          </>
        )}
      </For>
    </div>
  );
}
