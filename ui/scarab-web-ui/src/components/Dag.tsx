// The run's step DAG (ADR-0006/0028) — the differentiating surface, "blueprint
// spine" treatment. Steps lay out in dependency layers (longest-path from the
// roots); `needs` in-edges are drawn as measured orthogonal connectors on an SVG
// under the nodes, and the edge feeding a running step animates a dashed flow.
//
// A step that took more than one try renders as a DECK (ADR-0056): offset
// shadow-cards behind the node + a `×N` badge — the at-a-glance "this step was
// retried/rerun N times" signal, right in the graph. Selecting such a node FANS
// its tries out beneath it as a compact in-rail stack (`try 1 ✓`, `try 2 ·
// auto-retry`, …); picking a try scopes the evidence pane. Attempt selection
// lives HERE, in the graph — there is no separate attempt dropdown. Clicking a
// node selects it.
import { createSignal, onMount, onCleanup, createEffect, For, Show } from "solid-js";
import type { AttemptCause } from "../takes";
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

/** One try of the SELECTED step, resolved from the event log by the caller —
 * the fan renders these beneath the node (ADR-0056 amendment). */
export type DagTry = {
  id: string;
  /** 0-based order; the label is `try {index + 1}`. */
  index: number;
  cause?: AttemptCause;
  failed: boolean;
  failure?: string;
  /** Cut short by a rerun of an ancestor (started, never finished). */
  superseded: boolean;
  /** A success that a newer success replaced as of-record. */
  shadowed: boolean;
  /** Re-adopted after a control-plane restart (visibility marker, not a re-run). */
  readopted: boolean;
};

type Edge = { x1: number; y1: number; x2: number; y2: number; hot: boolean };

/** Deck depth: shadow-cards drawn behind a node, capped so a very hot step
 * doesn't grow an unbounded stack. */
const MAX_DECK = 3;

/** Plain-english cause suffix (ADR-0056 amendment) — the machine's own retry vs
 * a human rerun of this step vs a rerun of an ancestor that dragged it along. */
const causeSuffix = (c?: AttemptCause): string =>
  c === "rerun" ? " · you reran" : c === "cascade" ? " · ⟵ rerun" : c === "retry" ? " · auto-retry" : "";
const tryTitle = (t: DagTry): string => `try ${t.index + 1}${causeSuffix(t.cause)}`;
const tryOutcome = (t: DagTry): string => {
  if (t.superseded) return "⊘ superseded";
  if (t.failed) return `✗ failed${t.failure ? ` · ${t.failure}` : ""}`;
  return "✓ succeeded";
};
const tryTone = (t: DagTry): string =>
  t.superseded ? "copper" : t.failed ? "danger" : "emerald";

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
  /** Tries of the SELECTED step (the fan). Empty unless a decked step is picked. */
  tries?: DagTry[];
  /** The try currently scoping the evidence pane (highlighted in the fan). */
  activeAttempt?: string | null;
  /** Pick a try — scopes the evidence pane to `(selected step, attempt)`. */
  onAttemptSelect?: (id: string | null) => void;
}) {
  let container: HTMLDivElement | undefined;
  const nodes = new Map<string, HTMLElement>();
  const [edges, setEdges] = createSignal<Edge[]>([]);
  // A 1s tick so running nodes' elapsed counters advance in place.
  const [now, setNow] = createSignal(Date.now());

  const tries = () => props.tries ?? [];

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
          // of the dependent (endpoints are horizontal mid-points). Coordinates
          // are relative to the scroll container's content, so they stay correct
          // as the fan grows the column and the rail scrolls.
          x1: fb.left - cbox.left + container.scrollLeft + fb.width / 2,
          y1: fb.bottom - cbox.top + container.scrollTop,
          x2: tb.left - cbox.left + container.scrollLeft + tb.width / 2,
          y2: tb.top - cbox.top + container.scrollTop,
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
    if (container) {
      ro.observe(container);
      container.addEventListener("scroll", measure, { passive: true });
    }
    const tick = setInterval(() => setNow(Date.now()), 1000);
    onCleanup(() => {
      ro.disconnect();
      container?.removeEventListener("scroll", measure);
      clearInterval(tick);
    });
  });

  // Re-measure when topology changes (a fresh run replaces the whole set) OR the
  // selection/fan changes the column's height (`.dag` has a fixed box + inner
  // scroll, so a ResizeObserver on it never fires for content growth). Two rAFs:
  // one past the fan's DOM insertion, one past layout settling.
  createEffect(() => {
    props.steps.length;
    props.selected;
    tries().length;
    requestAnimationFrame(() => requestAnimationFrame(measure));
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

  const activeTry = () => {
    const ts = tries();
    if (!ts.length) return null;
    return ts.find((t) => t.id === props.activeAttempt)?.id ?? ts[ts.length - 1].id;
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
                {(s) => {
                  const sel = () => props.selected === s.id;
                  const deck = () => Math.min(Math.max(0, s.attempts - 1), MAX_DECK);
                  const fanned = () => sel() && tries().length > 1;
                  return (
                    <div class="dcell" classList={{ fanned: fanned() }}>
                      {/* Deck: shadow-cards behind the node, one per extra try. */}
                      <div class="ddeck">
                        <For each={Array.from({ length: deck() })}>
                          {(_, i) => (
                            <span
                              class="ddeck-layer"
                              style={{ transform: `translate(${(i() + 1) * 3}px, ${(i() + 1) * 4}px)` }}
                            />
                          )}
                        </For>
                        <button
                          class={`dnode ${s.status} ${sel() ? "sel" : ""}`}
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
                            <span class="dnode-count" title={`${s.attempts} tries`}>
                              ×{s.attempts}
                            </span>
                          </Show>
                          <Show when={s.gate}>
                            <Icon icon="timer" size={12} class="dnode-gate" />
                          </Show>
                          <Show when={s.status === "running"}>
                            <span class="dnode-prog" />
                          </Show>
                        </button>
                      </div>

                      {/* Fan: the selected step's tries, a compact in-rail stack.
                          Picking one scopes the evidence pane. */}
                      <Show when={fanned()}>
                        <div class="dfan">
                          <For each={tries()}>
                            {(t, i) => (
                              <button
                                class={`dfan-card ${tryTone(t)}`}
                                classList={{
                                  on: activeTry() === t.id,
                                  shadow: t.shadowed,
                                }}
                                style={{ "animation-delay": `${i() * 30}ms` }}
                                onClick={() => props.onAttemptSelect?.(t.id)}
                                title={`${tryTitle(t)} — ${tryOutcome(t)}`}
                              >
                                <span class="dfan-t">
                                  {tryTitle(t)}
                                  <Show when={t.readopted}>
                                    <span class="dfan-readopt" title="re-adopted after control-plane restart">
                                      {" "}⟲
                                    </span>
                                  </Show>
                                </span>
                                <span class="dfan-o">
                                  {tryOutcome(t)}
                                  {t.shadowed ? " · shadowed" : ""}
                                </span>
                              </button>
                            )}
                          </For>
                        </div>
                      </Show>
                    </div>
                  );
                }}
              </For>
            </div>
          )}
        </For>
      </div>
    </div>
  );
}
