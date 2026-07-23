// The run's step DAG (ADR-0006/0028) — the differentiating surface, "blueprint
// spine" treatment. Steps lay out in dependency layers (longest-path from the
// roots); `needs` in-edges are drawn as measured orthogonal connectors on an SVG
// under the nodes, and the edge feeding a running step animates a dashed flow.
//
// This is a PURE STATUS MAP (redesign stage 3): one node per step showing its
// status ring, id, a short meta line, a `×N` badge when the step took more than
// one try, a gate icon, and a running scanline. The "which try" axis lives
// elsewhere now — the attempts filmstrip in the evidence-pane header — so the
// graph answers "what shape, and where does each step stand", not "which try".
// Clicking a node selects it.
import { createSignal, onMount, onCleanup, createEffect, For, Show } from "solid-js";
import Icon from "./Icon";

export type DagStep = {
  id: string;
  status: string;
  /** Attempt count — >1 means it was retried/restarted (the rerun signal). */
  attempts: number;
  /** Time-travel only: this step was carried forward untouched by the viewed
   * Take (0 attempts in it) — render muted, "not part of this rerun". */
  reused?: boolean;
  needs: string[];
  gate?: string | null;
  /** When the current (running) attempt started, epoch-ms — drives live elapsed. */
  runningSince?: number | null;
  /** Wall-clock a finished step took (ms), derived from the event log. */
  durationMs?: number | null;
  /** This step's co-located SIDECAR services (ADR-0058), rendered as docked
   * chips ON the node (they live inside the step's Pod — never floated peers). */
  services?: { index: number; image: string }[];
  /** Names of pipeline-level SHARED services this step opts into via `uses:`
   * (ADR-0058) — the source of the dotted service→step edges. */
  uses?: string[];
};

/** A pipeline-level SHARED service (ADR-0058) — a PEER node in the services lane
 * at the top of the DAG, with its own lifecycle. Distinct from a step's docked
 * sidecars: a shared service is one instance many steps reach via `uses:`. */
export type DagService = {
  name: string;
  /** Lifecycle: `starting` | `ready` | `running` | `torn-down` | `failed`. */
  status: string;
  /** Ports it listens on, when known (the /services projection omits them). */
  ports?: number[];
};

// A `needs` edge is solid; a `uses` edge (shared service → consumer) is dashed
// and never "hot" (a service is not part of the running-flow highlight).
// `vertical` picks the routing: `needs` flow left→right (horizontal-first),
// while a `uses` edge drops from the top services lane (vertical-first).
type Edge = {
  x1: number;
  y1: number;
  x2: number;
  y2: number;
  hot: boolean;
  dashed: boolean;
  vertical: boolean;
};

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
  /** Shared services (ADR-0058) — rendered as peer nodes in a lane at the top. */
  services?: DagService[];
  /** Selection id: a step is `"<stepId>"`; a shared service is `"service:<name>"`. */
  selected: string | null;
  onSelect: (id: string) => void;
  /** Click a step's docked sidecar chip — selects the step + focuses that
   * sidecar's container in the Logs tab (ADR-0058). */
  onSelectSidecar?: (stepId: string, index: number) => void;
  /** The sidecar index whose logs are currently active on the SELECTED step —
   * drives the docked chip's selected outline. */
  sidecarFocus?: number | null;
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
          // Left→right flow: out the RIGHT edge of the dependency, into the LEFT
          // edge of the dependent (endpoints are vertical mid-points). Coordinates
          // are relative to the scroll container's content, so they stay correct
          // as the canvas scrolls.
          x1: fb.right - cbox.left + container.scrollLeft,
          y1: fb.top - cbox.top + container.scrollTop + fb.height / 2,
          x2: tb.left - cbox.left + container.scrollLeft,
          y2: tb.top - cbox.top + container.scrollTop + tb.height / 2,
          // The edge glows when it feeds a running step (or a running dep).
          hot: hot || byId.get(need)?.status === "running",
          dashed: false,
          vertical: false,
        });
      }
    }
    // Dashed `uses` edges (ADR-0058): from each shared-service peer node down to
    // every step that opts into it. A service edge is never "hot" — a service is
    // infra, not part of the running-step flow highlight. The services lane stays
    // a band at the TOP, so this edge drops from the service node's BOTTOM into
    // the consuming step's TOP (vertical), distinct from the left→right `needs`
    // connectors.
    for (const s of props.steps) {
      const to = nodes.get(s.id);
      if (!to) continue;
      const tb = to.getBoundingClientRect();
      for (const name of s.uses ?? []) {
        const from = nodes.get(`service:${name}`);
        if (!from) continue;
        const fb = from.getBoundingClientRect();
        es.push({
          x1: fb.left - cbox.left + container.scrollLeft + fb.width / 2,
          y1: fb.bottom - cbox.top + container.scrollTop,
          x2: tb.left - cbox.left + container.scrollLeft + tb.width / 2,
          y2: tb.top - cbox.top + container.scrollTop,
          hot: false,
          dashed: true,
          vertical: true,
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

  // Re-measure when topology changes (a fresh run replaces the whole set) —
  // `.dag` has a fixed box + inner scroll, so a ResizeObserver on it never fires
  // for content growth. Two rAFs: one past DOM insertion, one past layout.
  createEffect(() => {
    props.steps.length;
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
    // Time-travel display statuses (ADR-0056): prettify the underscore form.
    if (s.status === "superseded") return "superseded";
    if (s.status === "not_run") return "not run";
    return s.status;
  };

  // A shared-service node's meta line: ports (when the projection carries them)
  // then its lifecycle status — e.g. `port 5432 · ready`, or just `ready`.
  const svcMeta = (sv: DagService): string =>
    sv.ports?.length ? `port ${sv.ports.join(", ")} · ${sv.status}` : sv.status;

  // A sidecar chip's short label: drop the registry/repo path and the `:tag`.
  // `redis:7.4` → `redis`; `ghcr.io/org/cache:1` → `cache`.
  const shortImage = (image: string): string => {
    const lastSlash = image.lastIndexOf("/");
    const seg = lastSlash >= 0 ? image.slice(lastSlash + 1) : image;
    const colon = seg.indexOf(":");
    return colon >= 0 ? seg.slice(0, colon) : seg;
  };

  return (
    <div class="dag" ref={container}>
      <svg class="dag-edges" aria-hidden="true">
        <For each={edges()}>
          {(e) => {
            const r = 8;
            let d: string;
            if (e.vertical) {
              // Vertical-first "circuit" (the `uses` drop from the top services
              // lane): out the bottom, a mid HORIZONTAL jog, into the top.
              const my = (e.y1 + e.y2) / 2;
              const dir = e.x2 >= e.x1 ? 1 : -1;
              d =
                Math.abs(e.x2 - e.x1) < 2
                  ? `M ${e.x1} ${e.y1} V ${e.y2}`
                  : `M ${e.x1} ${e.y1} V ${my - r} Q ${e.x1} ${my} ${e.x1 + r * dir} ${my} ` +
                    `H ${e.x2 - r * dir} Q ${e.x2} ${my} ${e.x2} ${my + r} V ${e.y2}`;
            } else {
              // Horizontal-first "circuit" (the left→right `needs` flow): out the
              // right, a mid VERTICAL jog, into the left — the engineering-
              // blueprint identity, re-oriented so depth runs along the wide axis.
              const mx = (e.x1 + e.x2) / 2;
              const dir = e.y2 >= e.y1 ? 1 : -1;
              d =
                Math.abs(e.y2 - e.y1) < 2
                  ? `M ${e.x1} ${e.y1} H ${e.x2}`
                  : `M ${e.x1} ${e.y1} H ${mx - r} Q ${mx} ${e.y1} ${mx} ${e.y1 + r * dir} ` +
                    `V ${e.y2 - r * dir} Q ${mx} ${e.y2} ${mx + r} ${e.y2} H ${e.x2}`;
            }
            return (
              <path d={d} class={e.dashed ? "dashed" : e.hot ? "hot" : ""} fill="none" />
            );
          }}
        </For>
      </svg>
      {/* The graph body: the services lane as a horizontal band on TOP, then the
          dependency layers flowing left→right below it. `.dag-cols` is now a ROW
          of depth layers and each `.dag-col` stacks its siblings vertically; the
          column wrapper keeps the lane spanning the width above that flow. */}
      <div class="dag-graph">
        {/* Services lane (ADR-0058): shared services are PEERS, not steps — a
            band at the top, above dependency layer 0, with dotted `uses` edges
            dropping to each consuming step. They carry no `needs` depth, so the
            lane holds them all. Registered in `nodes` (keyed `service:<name>`) so
            `measure()` can route edges to them. */}
        <Show when={(props.services?.length ?? 0) > 0}>
          <div class="dag-lane">
            <span class="dag-lane-label">services</span>
            <div class="dag-lane-nodes">
              <For each={props.services}>
                {(sv) => {
                  const selId = `service:${sv.name}`;
                  const sel = () => props.selected === selId;
                  return (
                    <button
                      class={`dnode service ${sel() ? "sel" : ""}`}
                      ref={(el) => nodes.set(selId, el)}
                      onClick={() => props.onSelect(selId)}
                      title={`${sv.name} — shared service · ${sv.status}`}
                    >
                      <span class={`dring service ${sv.status}`}>
                        <span class="dring-core" />
                      </span>
                      <span class="dnode-main" title={sv.name}>
                        <span class="dnode-id">{sv.name}</span>
                        <span class="dnode-meta">{svcMeta(sv)}</span>
                      </span>
                      <Icon icon="database" size={14} class="dnode-svc-ico" />
                    </button>
                  );
                }}
              </For>
            </div>
          </div>
        </Show>
        <div class="dag-cols">
        <For each={layers()}>
          {(col) => (
            <div class="dag-col">
              <For each={col}>
                {(s) => {
                  const sel = () => props.selected === s.id;
                  return (
                    <button
                      class={`dnode ${s.status} ${sel() ? "sel" : ""}`}
                      classList={{ reused: !!s.reused }}
                      ref={(el) => nodes.set(s.id, el)}
                      onClick={() => props.onSelect(s.id)}
                    >
                      <span class={`dring ${s.status}`}>
                        <span class="dring-core" />
                      </span>
                      <span class="dnode-main" title={s.id}>
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
                      {/* Docked SIDECARS (ADR-0058): each of the step's own
                          `services:` as a small copper chip INSIDE the node —
                          honest to Pod containment (they die with the step), so
                          never a floated peer with an edge. A chip is its own
                          clickable target (stopPropagation, so it doesn't just
                          select the step); a `<span role=button>` because it
                          nests inside the step's own `<button>`. */}
                      <Show when={s.services && s.services.length > 0}>
                        <span class="dnode-sidecars">
                          <For each={s.services}>
                            {(svc) => {
                              const scSel = () =>
                                props.selected === s.id && props.sidecarFocus === svc.index;
                              const pick = (e: Event) => {
                                e.stopPropagation();
                                props.onSelectSidecar?.(s.id, svc.index);
                              };
                              return (
                                <span
                                  class={`dnode-sidecar ${scSel() ? "sel" : ""}`}
                                  role="button"
                                  tabindex="0"
                                  title={`sidecar · ${svc.image} (service-${svc.index}) — logs inside ${s.id}`}
                                  onClick={pick}
                                  onKeyDown={(e) => {
                                    if (e.key === "Enter" || e.key === " ") {
                                      e.preventDefault();
                                      pick(e);
                                    }
                                  }}
                                >
                                  <span class="sc-hex">⬡</span>
                                  <span class="sc-name">{shortImage(svc.image)}</span>
                                </span>
                              );
                            }}
                          </For>
                        </span>
                      </Show>
                    </button>
                  );
                }}
              </For>
            </div>
          )}
        </For>
        </div>
      </div>
    </div>
  );
}
