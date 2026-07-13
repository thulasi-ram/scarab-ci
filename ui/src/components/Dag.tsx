// The run's step DAG (ADR-0006/0028) — the differentiating surface. Steps are
// laid out in dependency layers (longest-path from the roots); `needs` in-edges
// are drawn as measured bézier connectors on an SVG under the nodes. Clicking a
// node selects it (drives the step-detail + log panes in the run view). Live
// status flows in via the caller's polling, so nodes recolor in place.
import { createSignal, onMount, onCleanup, createEffect, For, Show } from "solid-js";
import Icon from "./Icon";

export type DagStep = {
  id: string;
  status: string;
  attempts: number;
  needs: string[];
  gate?: string | null;
};

type Edge = { x1: number; y1: number; x2: number; y2: number };

export default function Dag(props: {
  steps: DagStep[];
  selected: string | null;
  onSelect: (id: string) => void;
}) {
  let container: HTMLDivElement | undefined;
  const nodes = new Map<string, HTMLElement>();
  const [edges, setEdges] = createSignal<Edge[]>([]);

  // Dependency layers: a step sits one column right of its deepest dependency.
  const layers = () => {
    const byId = new Map(props.steps.map((s) => [s.id, s]));
    const memo = new Map<string, number>();
    const depth = (id: string, seen: Set<string>): number => {
      if (memo.has(id)) return memo.get(id)!;
      if (seen.has(id)) return 0; // cycle guard (validated away server-side, but be safe)
      seen.add(id);
      const s = byId.get(id);
      const d =
        !s || s.needs.length === 0
          ? 0
          : 1 + Math.max(...s.needs.map((n) => depth(n, seen)));
      seen.delete(id);
      memo.set(id, d);
      return d;
    };
    const cols: DagStep[][] = [];
    for (const s of props.steps) {
      const d = depth(s.id, new Set());
      (cols[d] ||= []).push(s);
    }
    return cols.filter((c) => c && c.length);
  };

  function measure() {
    if (!container) return;
    const cbox = container.getBoundingClientRect();
    const es: Edge[] = [];
    for (const s of props.steps) {
      const to = nodes.get(s.id);
      if (!to) continue;
      const tb = to.getBoundingClientRect();
      for (const need of s.needs) {
        const from = nodes.get(need);
        if (!from) continue;
        const fb = from.getBoundingClientRect();
        es.push({
          x1: fb.right - cbox.left,
          y1: fb.top - cbox.top + fb.height / 2,
          x2: tb.left - cbox.left,
          y2: tb.top - cbox.top + tb.height / 2,
        });
      }
    }
    setEdges(es);
  }

  onMount(() => {
    requestAnimationFrame(measure);
    const ro = new ResizeObserver(() => measure());
    if (container) ro.observe(container);
    onCleanup(() => ro.disconnect());
  });

  // Re-measure when topology changes (statuses can change without moving nodes,
  // but a fresh run replaces the whole set).
  createEffect(() => {
    props.steps.length;
    requestAnimationFrame(measure);
  });

  return (
    <div class="dag" ref={container}>
      <svg class="dag-edges" aria-hidden="true">
        <For each={edges()}>
          {(e) => {
            const mx = (e.x1 + e.x2) / 2;
            return (
              <path
                d={`M ${e.x1} ${e.y1} C ${mx} ${e.y1}, ${mx} ${e.y2}, ${e.x2} ${e.y2}`}
                fill="none"
              />
            );
          }}
        </For>
      </svg>
      <div class="dag-cols">
        <For each={layers()}>
          {(col) => (
            <div class="dag-col">
              <For each={col}>
                {(s) => (
                  <button
                    class={`dnode ${s.status} ${props.selected === s.id ? "sel" : ""}`}
                    ref={(el) => nodes.set(s.id, el)}
                    onClick={() => props.onSelect(s.id)}
                  >
                    <span class={`sdot ${s.status}`} />
                    <span class="dnode-id">{s.id}</span>
                    <Show when={s.gate}>
                      <Icon icon="timer" size={12} class="dnode-gate" />
                    </Show>
                  </button>
                )}
              </For>
            </div>
          )}
        </For>
      </div>
    </div>
  );
}
