// The Pipeline panel's List view: every step in the order the run lists it
// (its definition order, so a step comes after what it needs), the cells of a
// matrix grouped under one heading, with an All / Failed filter. Each row
// gives the status glyph, the step's name or its cell values, its status in
// words and how long it took. Shared services come first, as rows that open
// the service. For runs too big to read as a graph. Page chrome, so it reads
// the page tokens.
import { createSignal, For, Show } from "solid-js";
import type { DagService, DagStep } from "./Dag";
import { compactDuration, matrixHeading, segmentByMatrix, statusGlyph, statusWord } from "../run-page";

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
}) {
  const [filter, setFilter] = createSignal<"all" | "failed">("all");
  const failedCount = () => props.steps.filter((s) => s.status === "failed").length;
  const shown = () => (filter() === "failed" ? props.steps.filter((s) => s.status === "failed") : props.steps);
  const segments = () => segmentByMatrix(shown(), (s) => s.id);
  const took = (s: DagStep): string =>
    s.status === "running" && s.runningSince
      ? compactDuration(props.now - s.runningSince)
      : s.durationMs != null
        ? compactDuration(s.durationMs)
        : "";

  return (
    <div class="slist">
      <div class="slist-filter" role="group" aria-label="Filter steps">
        <button
          type="button"
          class="chip"
          aria-pressed={filter() === "all"}
          onClick={() => setFilter("all")}
        >
          All <span class="mono">{props.steps.length}</span>
        </button>
        <button
          type="button"
          class="chip"
          aria-pressed={filter() === "failed"}
          onClick={() => setFilter("failed")}
        >
          Failed <span class="mono">{failedCount()}</span>
        </button>
        <span class="subtle">in dependency order</span>
      </div>
      <Show when={filter() === "all" && (props.services?.length ?? 0) > 0}>
        <div class="slist-grp">Shared services</div>
        <For each={props.services}>
          {(sv) => {
            const id = `service:${sv.name}`;
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
                <span class="slist-name">{sv.name}</span>
                <span class="slist-words">{sv.status}</span>
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
                <button
                  type="button"
                  class="slist-row"
                  classList={{ sel: props.selected === it.item.id, cell: !!seg.matrix }}
                  aria-pressed={props.selected === it.item.id}
                  onClick={() => props.onSelect(it.item.id)}
                  title={it.item.id}
                >
                  <span class={`slist-g ${tone(it.item)}`} aria-hidden="true">
                    {it.item.note?.tone === "warning" ? "◷" : statusGlyph(it.item.status)}
                  </span>
                  <span class="slist-name">{it.label}</span>
                  <span class="slist-words">{rowWords(it.item)}</span>
                  <span class="slist-dur">{took(it.item)}</span>
                </button>
              )}
            </For>
          </>
        )}
      </For>
    </div>
  );
}
