// The run's step DAG (ADR-0006/0028) — the differentiating surface, "blueprint
// spine" treatment. Steps lay out in dependency layers (longest-path from the
// roots); `needs` in-edges are drawn as measured orthogonal connectors on an SVG
// under the nodes, and the edge feeding a running step animates a dashed flow.
// Each node carries a status ring, live elapsed (while running), and a retry
// badge when it took more than one attempt (ADR-0047). Clicking selects it.
import { createSignal, onMount, onCleanup, createEffect, For, Show } from "solid-js";
import Icon from "./Icon";

export type DagStep = {
  id: string;
  status: string;
  /** Attempt count — >1 means it was retried/restarted (the rerun signal). */
  attempts: number;
  needs: string[];
  gate?: string | null;
  /** When the current (running) attempt started, epoch-ms — drives live elapsed. */
  runningSince?: number | null;
  /** Wall-clock a finished step took (ms), derived from the event log. */
  durationMs?: number | null;
};

type Edge = { x1: number; y1: number; x2: number; y2: number; hot: boolean };

/** m:ss (or h:mm:ss) elapsed since `since`, ticking off the caller's `now`. */
function elapsed(since: number, now: number): string {
  return fmtDur(now - since);
}

/** A compact duration label (e.g. `4.2s`, `1m12s`, `1h03m`) from a ms delta. */
function fmtDur(ms: number): string {
  const s = Math.max(0, ms) / 1000;
  if (s < 10) return `${s.toFixed(1)}s`;
  const total = Math.round(s);
  if (total < 60) return `${total}s`;
  const mm = Math.floor(total / 60);
  const ss = total % 60;
  if (mm < 60) return `${mm}m${String(ss).padStart(2, "0")}s`;
  const hh = Math.floor(mm / 60);
  return `${hh}h${String(mm % 60).padStart(2, "0")}m`;
}

export default function Dag(props: {
  steps: DagStep[];
  selected: string | null;
  onSelect: (id: string) => void;
}) {
  let container: HTMLDivElement | undefined;
  const nodes = new Map<string, HTMLElement>();
  const [edges, setEdges] = createSignal<Edge[]>([]);
  // A 1s tick so running nodes' elapsed counters advance in place.
  const [now, setNow] = createSignal(Date.now());

  // Dependency layers: a step sits one column right of its deepest dependency.
  const layers = () => {
    const byId = new Map(props.steps.map((s) => [s.id, s]));
    const memo = new Map<string, number>();
    const depth = (id: string, seen: Set<string>): number => {
      if (memo.has(id)) return memo.get(id)!;
      if (seen.has(id)) return 0; // cycle guard (validated away server-side)
      seen.add(id);
      const s = byId.get(id);
      const d =
        !s || s.needs.length === 0 ? 0 : 1 + Math.max(...s.needs.map((n) => depth(n, seen)));
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
    const byId = new Map(props.steps.map((s) => [s.id, s]));
    const cbox = container.getBoundingClientRect();
    const es: Edge[] = [];
    for (const s of props.steps) {
      const to = nodes.get(s.id);
      if (!to) continue;
      const tb = to.getBoundingClientRect();
      const hot = s.status === "running";
      for (const need of s.needs) {
        const from = nodes.get(need);
        if (!from) continue;
        const fb = from.getBoundingClientRect();
        es.push({
          // Top-to-bottom flow: out the BOTTOM of the dependency, into the TOP
          // of the dependent (endpoints are horizontal mid-points).
          x1: fb.left - cbox.left + fb.width / 2,
          y1: fb.bottom - cbox.top,
          x2: tb.left - cbox.left + tb.width / 2,
          y2: tb.top - cbox.top,
          // The edge glows when it feeds a running step (or a running dep).
          hot: hot || byId.get(need)?.status === "running",
        });
      }
    }
    setEdges(es);
  }

  onMount(() => {
    requestAnimationFrame(measure);
    const ro = new ResizeObserver(() => measure());
    if (container) ro.observe(container);
    const tick = setInterval(() => setNow(Date.now()), 1000);
    onCleanup(() => {
      ro.disconnect();
      clearInterval(tick);
    });
  });

  // Re-measure when topology changes (a fresh run replaces the whole set).
  createEffect(() => {
    props.steps.length;
    requestAnimationFrame(measure);
  });

  // A short status meta line under the node id.
  const meta = (s: DagStep): string => {
    if (s.status === "running") {
      return s.runningSince ? `running · ${elapsed(s.runningSince, now())}` : "running";
    }
    if (s.gate && (s.status === "pending" || s.status === "waiting" || s.status === "ready")) {
      return `gate · ${s.gate}`;
    }
    // Finished steps show how long they took (from the event log).
    if (s.durationMs != null && (s.status === "succeeded" || s.status === "failed")) {
      const mark = s.status === "succeeded" ? "✓" : "✕";
      return `${mark} ${fmtDur(s.durationMs)}`;
    }
    return s.status;
  };

  return (
    <div class="dag" ref={container}>
      <svg class="dag-edges" aria-hidden="true">
        <For each={edges()}>
          {(e) => {
            // Orthogonal "circuit" routing, top-to-bottom: out the bottom, a mid
            // horizontal, into the top — the engineering-blueprint identity.
            const my = (e.y1 + e.y2) / 2;
            const r = 8;
            const dir = e.x2 >= e.x1 ? 1 : -1;
            const d =
              Math.abs(e.x2 - e.x1) < 2
                ? `M ${e.x1} ${e.y1} V ${e.y2}`
                : `M ${e.x1} ${e.y1} V ${my - r} Q ${e.x1} ${my} ${e.x1 + r * dir} ${my} ` +
                  `H ${e.x2 - r * dir} Q ${e.x2} ${my} ${e.x2} ${my + r} V ${e.y2}`;
            return <path d={d} class={e.hot ? "hot" : ""} fill="none" />;
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
                    <span class={`dring ${s.status}`}>
                      <span class="dring-core" />
                    </span>
                    <span class="dnode-main">
                      <span class="dnode-id">{s.id}</span>
                      <span class="dnode-meta">{meta(s)}</span>
                    </span>
                    <Show when={s.attempts > 1}>
                      <span class="dnode-retry" title={`${s.attempts} attempts (retried)`}>
                        <Icon icon="rotate-cw" size={10} />×{s.attempts}
                      </span>
                    </Show>
                    <Show when={s.gate}>
                      <Icon icon="timer" size={12} class="dnode-gate" />
                    </Show>
                    <Show when={s.status === "running"}>
                      <span class="dnode-prog" />
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
