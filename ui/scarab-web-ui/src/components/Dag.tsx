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
import { createSignal, onMount, onCleanup, createEffect, createMemo, For, Show } from "solid-js";
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

/** Imperative zoom controls the Pipeline band toolbar drives (Proposal B, stage
 * 3). Dag owns the zoom/pan state and hands these out on mount so the toolbar's
 * fit / ＋ / － cluster (page chrome, outside the dark canvas) can call in. */
export type DagControls = {
  /** Fit the whole graph within the band and re-center (the auto-fit baseline). */
  fit: () => void;
  zoomIn: () => void;
  zoomOut: () => void;
};

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
  /** Handed the zoom controls on mount (fit / zoom in / zoom out) so the band
   * toolbar can drive them (Proposal B, stage 3). */
  onControls?: (c: DagControls) => void;
}) {
  let container: HTMLDivElement | undefined; // the band viewport (overflow clipped)
  let graph: HTMLDivElement | undefined; // the unscaled graph content (fit/measure basis)
  const nodes = new Map<string, HTMLElement>();
  const [edges, setEdges] = createSignal<Edge[]>([]);
  // A 1s tick so running nodes' elapsed counters advance in place.
  const [now, setNow] = createSignal(Date.now());

  // ── Fit-to-view zoom + pan (Proposal B, stage 3) ──────────────────────────
  // The graph content is wrapped in a zoom layer transformed by `scale` +
  // (`tx`,`ty`). `fit()` scales the whole graph to sit within the band (never
  // upscaling past 1×) and centers it; the band's rendered height tracks the
  // fitted content height, capped at a max. Edges are measured in UNSCALED
  // content coordinates (offsetLeft/offsetTop, which CSS transforms don't
  // touch), so the zoom transform never breaks where they land.
  const [scale, setScale] = createSignal(1);
  const [tx, setTx] = createSignal(0);
  const [ty, setTy] = createSignal(0);
  const [bandH, setBandH] = createSignal(320);
  const [fitZ, setFitZ] = createSignal(1); // the last fit scale — pan unlocks above it
  const [contentH, setContentH] = createSignal(0);
  const [dragging, setDragging] = createSignal(false);

  const PAD = 24; // inset (px) between the graph and the band edges, at scale 1
  const MIN_Z = 0.2;
  const MAX_Z = 1.5;
  const clampZ = (z: number) => Math.min(MAX_Z, Math.max(MIN_Z, z));
  // Band ceiling ~60vh (the DAG never eats more than that); floored so a tiny
  // graph still gets a usable strip.
  const maxBandH = () => Math.max(200, Math.round(window.innerHeight * 0.6));

  // Cumulative unscaled offset of `el` up to `ancestor` (the graph). offsetLeft/
  // offsetTop are LAYOUT coordinates — unaffected by the zoom layer's transform —
  // so this yields the graph-space position regardless of the current scale/pan.
  const offsetIn = (el: HTMLElement, ancestor: HTMLElement) => {
    let x = 0;
    let y = 0;
    let n: HTMLElement | null = el;
    while (n && n !== ancestor) {
      x += n.offsetLeft;
      y += n.offsetTop;
      n = n.offsetParent as HTMLElement | null;
    }
    return { x, y };
  };

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

  // A node's UNSCALED box in graph space (transform-invariant — see `offsetIn`).
  const boxOf = (el: HTMLElement) => {
    const { x, y } = offsetIn(el, graph!);
    return { left: x, top: y, right: x + el.offsetWidth, bottom: y + el.offsetHeight, w: el.offsetWidth, h: el.offsetHeight };
  };

  function measure() {
    if (!graph) return;
    const byId = new Map(props.steps.map((s) => [s.id, s]));
    const es: Edge[] = [];
    for (const s of props.steps) {
      const to = nodes.get(s.id);
      if (!to) continue;
      const tb = boxOf(to);
      const hot = s.status === "running";
      for (const need of s.needs) {
        const from = nodes.get(need);
        if (!from) continue;
        const fb = boxOf(from);
        es.push({
          // Left→right flow: out the RIGHT edge of the dependency, into the LEFT
          // edge of the dependent (endpoints are vertical mid-points). Unscaled
          // graph-space coords, so the zoom transform scales them onto the nodes.
          x1: fb.right,
          y1: fb.top + fb.h / 2,
          x2: tb.left,
          y2: tb.top + tb.h / 2,
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
      const tb = boxOf(to);
      for (const name of s.uses ?? []) {
        const from = nodes.get(`service:${name}`);
        if (!from) continue;
        const fb = boxOf(from);
        es.push({
          x1: fb.left + fb.w / 2,
          y1: fb.bottom,
          x2: tb.left + tb.w / 2,
          y2: tb.top,
          hot: false,
          dashed: true,
          vertical: true,
        });
      }
    }
    setEdges(es);
  }

  // Fit the whole graph within the band: scale so it fits both axes (never past
  // 1×), center it, and shrink the band to the fitted content height (capped at
  // the max — no wasted vertical space when the graph is small). Auto-fit runs on
  // mount, on topology change, and on a width resize.
  function fit() {
    if (!container || !graph) return;
    const bandW = container.clientWidth;
    const cw = graph.offsetWidth;
    const ch = graph.offsetHeight;
    setContentH(ch);
    if (cw <= 0 || ch <= 0 || bandW <= 0) return;
    const availW = Math.max(1, bandW - PAD * 2);
    const availH = Math.max(1, maxBandH() - PAD * 2);
    const z = Math.min(availW / cw, availH / ch, 1);
    const scaledW = cw * z;
    const scaledH = ch * z;
    const h = Math.round(scaledH + PAD * 2);
    setScale(z);
    setFitZ(z);
    setTx((bandW - scaledW) / 2);
    setTy((h - scaledH) / 2);
    setBandH(h);
  }

  // Zoom to `nz`, keeping the focal point (band center by default) fixed, and
  // grow the band up to the max so there's room to read/pan when zoomed in.
  function zoomTo(nz: number, fx?: number, fy?: number) {
    if (!container) return;
    const z0 = scale();
    const z = clampZ(nz);
    const cx = fx ?? container.clientWidth / 2;
    const cy = fy ?? bandH() / 2;
    const contentX = (cx - tx()) / z0;
    const contentY = (cy - ty()) / z0;
    setScale(z);
    setTx(cx - contentX * z);
    setTy(cy - contentY * z);
    // NB: manual zoom scales the INNER canvas only — it must NOT resize the band
    // container. Growing `bandH` here made the whole component jump on every
    // ＋/－; the band is a fixed viewport (set by `fit()` on topology change),
    // and zooming past the fit just overflows it (clipped) and unlocks panning.
  }
  const zoomIn = () => zoomTo(scale() * 1.2);
  const zoomOut = () => zoomTo(scale() / 1.2);

  // Drag-to-pan, unlocked only once zoomed in past the fit scale (when the graph
  // overflows the band). Starting on a node/chip is a select, not a pan.
  let panStart: { x: number; y: number; tx: number; ty: number } | null = null;
  const pannable = () => scale() > fitZ() + 0.001;
  const onPointerDown = (e: PointerEvent) => {
    if (!pannable() || !container) return;
    if ((e.target as HTMLElement).closest("button, [role='button']")) return;
    panStart = { x: e.clientX, y: e.clientY, tx: tx(), ty: ty() };
    setDragging(true);
    container.setPointerCapture(e.pointerId);
    e.preventDefault();
  };
  const onPointerMove = (e: PointerEvent) => {
    if (!panStart) return;
    setTx(panStart.tx + (e.clientX - panStart.x));
    setTy(panStart.ty + (e.clientY - panStart.y));
  };
  const onPointerUp = (e: PointerEvent) => {
    if (!panStart) return;
    panStart = null;
    setDragging(false);
    try {
      container?.releasePointerCapture(e.pointerId);
    } catch {
      /* pointer already released */
    }
  };

  // Fit + measure once the DOM is laid out (two rAFs: past insertion, past
  // layout).
  const fitAndMeasure = () =>
    requestAnimationFrame(() =>
      requestAnimationFrame(() => {
        fit();
        measure();
      }),
    );

  onMount(() => {
    // Hand the zoom controls up to the band toolbar (Proposal B, stage 3).
    props.onControls?.({ fit, zoomIn, zoomOut });
    fitAndMeasure();
    // Re-fit on a WIDTH resize only — a height change is self-inflicted by fit
    // (it sets the band height), so guarding on width avoids a feedback loop.
    let lastW = container?.clientWidth ?? 0;
    const ro = new ResizeObserver(() => {
      const w = container?.clientWidth ?? 0;
      if (Math.abs(w - lastW) < 1) return;
      lastW = w;
      requestAnimationFrame(fit);
    });
    if (container) ro.observe(container);
    // The max band height tracks the viewport (~60vh), so re-fit on window resize.
    const onWinResize = () => requestAnimationFrame(fit);
    window.addEventListener("resize", onWinResize);
    const tick = setInterval(() => setNow(Date.now()), 1000);
    onCleanup(() => {
      ro.disconnect();
      window.removeEventListener("resize", onWinResize);
      clearInterval(tick);
    });
  });

  // Auto-fit when the STEP SET / topology changes (a rerun re-arms steps, a
  // fresh run replaces the whole set) — the fitted zoom + band height re-derive
  // for the new shape. Keyed on the id set + service names, NOT status, so a
  // live run's 1.2s status poll never yanks the view back to fit.
  //
  // The key is a createMemo so the fit effect only re-runs when the id/name set
  // actually CHANGES value — a poll that hands us a fresh `steps` array of the
  // same shape recomputes the string but the memo's === guard swallows it, so
  // the user's manual zoom/pan survives a running pipeline's polls.
  const topologyKey = createMemo(
    () =>
      props.steps.map((s) => s.id).join(",") +
      "|" +
      (props.services ?? []).map((s) => s.name).join(","),
  );
  createEffect(() => {
    topologyKey();
    fitAndMeasure();
  });

  // Re-measure EDGES (only) when a node's box could have changed — topology or
  // per-step status (a running scanline, a ×N badge, the finished duration each
  // resize the node). Zoom/pan need no re-measure: edges are stored in unscaled
  // graph coords, so the transform scales them onto the nodes for free.
  createEffect(() => {
    props.steps.map((s) => `${s.id}:${s.status}:${s.attempts}:${s.gate ?? ""}`).join(",");
    (props.services ?? []).map((s) => `${s.name}:${s.status}`).join(",");
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
    <div
      class="dag"
      classList={{ pannable: pannable(), dragging: dragging() }}
      ref={container}
      style={{ height: `${bandH()}px` }}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
    >
      {/* Zoom/pan layer: the whole graph (edges + nodes) is transformed as one,
          so a fit-to-view scale (or manual zoom + pan) never desyncs the edges
          from the nodes. Edge coords are measured UNSCALED, so `scale()` here
          scales them onto the nodes for free. */}
      <div
        class="dag-zoom"
        style={{ transform: `translate(${tx()}px, ${ty()}px) scale(${scale()})` }}
      >
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
      <div class="dag-graph" ref={graph}>
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
    </div>
  );
}
