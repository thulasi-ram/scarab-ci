// The run's step graph on the always-dark canvas. Steps lay out in dependency
// layers (longest path from the roots); `needs` edges are drawn as measured
// orthogonal connectors on an SVG under the nodes, and the edge feeding a
// running step animates a dashed flow. A need that another need already
// depends on is not drawn (`drawnNeeds`), and every edge turns in the gap just
// before its target, so edges into one column share one vertical and never
// cross a node. Hovering a step lights every step it needs, drawn or not.
//
// One node per step: its status ring, its name, a meta line ("passed on try 3
// · 2m11s", "waiting for a node · 1m10s"), a gate or deploy icon, and a
// running scanline. The cells of a matrix sit under one heading naming the
// step and its keys, and each cell shows only its values. The graph is drawn
// at 100% and scrolls inside the band; only a graph wider than the band
// shrinks, to 75% at most. Clicking a node selects it.
import { createSignal, onMount, onCleanup, createEffect, createMemo, on, untrack, For, Index, Show } from "solid-js";
import { addedByForkRunApproval, isGateOpen, type GateStatus } from "../gates";
import { allNeeds, drawnNeeds, layoutGroups, type DagCell, type OriginLink } from "../dag-layout";
import { columnNodeWidth, fitBand, matrixHeading, nodeMeta, segmentByMatrix } from "../run-page";
import { shortHash } from "../libraries";
import { clearGraphZoom, MAX_ZOOM, MIN_ZOOM, readGraphZoom, writeGraphZoom } from "../graph-zoom";
import Icon from "./Icon";
import Pill from "./Pill";

/** A node's meta line and the tone it renders in. */
export type DagNote = {
  tone: GateStatus["tone"] | "warning";
  text: string;
  /** The full account for the hover title, when the text shortens it. */
  detail?: string;
  /** When the condition started (warning tone): the meta says for how long. */
  since?: number | null;
};

export type DagStep = {
  id: string;
  status: string;
  /** Tries in the version in view: more than one means it was retried. */
  attempts: number;
  /** Of those, the ones that failed ("failed 2 of 2 tries"). */
  failedTries?: number;
  /** This step was carried forward untouched from an earlier version (no
   * tries in the version in view): drawn with a dashed outline. */
  reused?: boolean;
  needs: string[];
  gate?: string | null;
  /** This step is marked `deploy: true` — the step that deploys. */
  deploy?: boolean;
  /** A line shown as the node's meta in place of the generic one: a gate's
   * verdict (`gateStatus` in gates.ts), a hold, or, in the `warning` tone, an
   * infra condition holding a running step back ("waiting for a node"). */
  note?: DagNote | null;
  /** When the current (running) attempt started, epoch-ms — drives live elapsed. */
  runningSince?: number | null;
  /** Wall-clock a finished step took (ms), derived from the event log. */
  durationMs?: number | null;
  /** This step's co-located SIDECAR services, rendered as docked
   * chips ON the node (they live inside the step's Pod — never floated peers). */
  services?: { index: number; image: string }[];
  /** Names of pipeline-level SHARED services this step opts into via `uses:`
   * — the source of the dotted service→step edges. */
  uses?: string[];
  /** The invoke chain this Step was inlined through: a Step from
   * a library is drawn inside a box per invocation. */
  origin?: OriginLink[] | null;
  /** An Environment approval gate: its approvers release it. */
  approvalGate?: boolean;
  /** For the gate the Environment added to the Run: "added by <Environment>";
   * for the gate Fork Run approval added: "added by Fork Run approval". */
  addedBy?: string | null;
};

/** A pipeline-level SHARED service — a PEER node in the services lane
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
  from: string;
  to: string;
  x1: number;
  y1: number;
  x2: number;
  y2: number;
  /** Where a `needs` edge turns: the gap just before its target. */
  bx: number;
  hot: boolean;
  dashed: boolean;
  vertical: boolean;
};

/** Zoom controls the Pipeline panel header drives. Dag owns the zoom and scroll
 * state and hands these out on mount, so the header's fit / ＋ / － buttons
 * (page chrome, outside the dark canvas) can call in. */
export type DagControls = {
  /** Back to the fitted view: 100%, or less for a graph wider than the band. */
  fit: () => void;
  zoomIn: () => void;
  zoomOut: () => void;
};

export default function Dag(props: {
  steps: DagStep[];
  /** Shared services — rendered as peer nodes in a lane at the top. */
  services?: DagService[];
  /** Selection id: a step is `"<stepId>"`; a shared service is `"service:<name>"`. */
  selected: string | null;
  onSelect: (id: string) => void;
  /** Click a step's docked sidecar chip — selects the step + focuses that
   * sidecar's container in the Logs tab. */
  onSelectSidecar?: (stepId: string, index: number) => void;
  /** The sidecar index whose logs are currently active on the SELECTED step —
   * drives the docked chip's selected outline. */
  sidecarFocus?: number | null;
  /** Handed the zoom controls on mount (fit / zoom in / zoom out) so the
   * panel header can drive them. */
  onControls?: (c: DagControls) => void;
}) {
  let container: HTMLDivElement | undefined; // the band viewport (overflow clipped)
  let graph: HTMLDivElement | undefined; // the unscaled graph content (fit/measure basis)
  const nodes = new Map<string, HTMLElement>();
  const [edges, setEdges] = createSignal<Edge[]>([]);
  // A 1s tick so running nodes' elapsed counters advance in place.
  const [now, setNow] = createSignal(Date.now());

  // ── Zoom and scroll ─────────────────────────────────────────────────────
  // The graph sits in a zoom layer scaled by `scale`, inside a sizer as big as
  // the scaled graph, inside the band, which scrolls. `fit()` sets the scale
  // back to 1 (a wider graph scrolls sideways, never shrunk) and the band
  // height (the graph's, capped at ~60vh). Edges are measured in UNSCALED
  // graph coordinates (offsetLeft/offsetTop, which transforms don't touch), so
  // the zoom never moves them off their nodes.
  const [scale, setScale] = createSignal(1);
  const [bandH, setBandH] = createSignal(240);
  const [contentW, setContentW] = createSignal(0);
  const [contentH, setContentH] = createSignal(0);
  const [dragging, setDragging] = createSignal(false);

  const PAD = 24; // inset (px) between the graph and the band edges
  // How far before its target an edge turns: past a library box's padding
  // (10px) and inside the 36px gap between columns.
  const BUS = 18;
  const MIN_Z = MIN_ZOOM;
  const MAX_Z = MAX_ZOOM;
  const clampZ = (z: number) => Math.min(MAX_Z, Math.max(MIN_Z, z));
  // Band ceiling ~60vh (the graph never takes more than that); floored so a
  // tiny viewport still gets a usable strip.
  const maxBandH = () => Math.max(200, Math.round(window.innerHeight * 0.6));
  const sizeW = () => Math.round(contentW() * scale() + PAD * 2);
  const sizeH = () => Math.round(contentH() * scale() + PAD * 2);

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

  // Dependency layers: a step sits one column right of its deepest dependency,
  // and a library invocation's Steps sit in one box that is a unit of its
  // parent's layering. Pure, in dag-layout.ts.
  // Laid out again only when the graph's shape changes (ids, needs, where a
  // step comes from), never on a status poll: the columns and nodes then stay
  // mounted, and each node reads its step's latest state by id.
  const shapeKey = createMemo(() =>
    JSON.stringify(props.steps.map((s) => [s.id, s.needs, s.origin ?? null])),
  );
  const layers = createMemo(on(shapeKey, () => layoutGroups(untrack(() => props.steps))));
  const stepById = createMemo(() => new Map(props.steps.map((s) => [s.id, s])));

  // A node's UNSCALED box in graph space (transform-invariant — see `offsetIn`).
  const boxOf = (el: HTMLElement) => {
    const { x, y } = offsetIn(el, graph!);
    return { left: x, top: y, right: x + el.offsetWidth, bottom: y + el.offsetHeight, w: el.offsetWidth, h: el.offsetHeight };
  };

  function measure() {
    if (!graph) return;
    const byId = new Map(props.steps.map((s) => [s.id, s]));
    const drawn = drawnNeeds(props.steps);
    const es: Edge[] = [];
    for (const s of props.steps) {
      const to = nodes.get(s.id);
      if (!to) continue;
      const tb = boxOf(to);
      const hot = s.status === "running";
      for (const need of drawn.get(s.id) ?? s.needs) {
        const from = nodes.get(need);
        if (!from) continue;
        const fb = boxOf(from);
        es.push({
          from: need,
          to: s.id,
          // Left→right flow: out the RIGHT edge of the dependency, into the LEFT
          // edge of the dependent (endpoints are vertical mid-points). Unscaled
          // graph-space coords, so the zoom transform scales them onto the nodes.
          x1: fb.right,
          y1: fb.top + fb.h / 2,
          x2: tb.left,
          y2: tb.top + tb.h / 2,
          // Turn in the gap before the target, never between two far columns:
          // edges into nodes with one left edge share one vertical.
          bx: tb.left - BUS,
          // The edge glows when it feeds a running step (or a running dep).
          hot: hot || byId.get(need)?.status === "running",
          dashed: false,
          vertical: false,
        });
      }
    }
    // Dashed `uses` edges: from each shared-service peer node down to
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
          from: `service:${name}`,
          to: s.id,
          bx: 0,
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

  // Back to the opening view and the top-left corner: the viewer's
  // remembered zoom, else 100%. Runs on mount, on a topology change and on a
  // width resize; never on a status poll.
  // `revealSelected`: then scroll the selected node into view. The header's
  // fit button passes false: it means "back to the top-left corner".
  function fit(revealSelected = true) {
    if (!container || !graph) return;
    const cw = graph.offsetWidth;
    const ch = graph.offsetHeight;
    const bandW = container.clientWidth;
    setContentW(cw);
    setContentH(ch);
    if (cw <= 0 || ch <= 0 || bandW <= 0) return;
    const f = fitBand({ bandW, contentW: cw, contentH: ch, maxH: maxBandH(), pad: PAD, zoom: readGraphZoom() });
    setScale(f.scale);
    setBandH(f.bandH);
    container.scrollLeft = 0;
    container.scrollTop = 0;
    // After the sizer takes the new scale, or the band cannot scroll that far.
    if (revealSelected) requestAnimationFrame(reveal);
  }

  // Scroll the band so the selected node is in view; a node already in view
  // stays where it is.
  function reveal() {
    if (!container || !graph || !props.selected) return;
    const el = nodes.get(props.selected);
    if (!el || !el.isConnected) return;
    const b = boxOf(el);
    const z = scale();
    const x = PAD + b.left * z;
    const y = PAD + b.top * z;
    const w = b.w * z;
    const h = b.h * z;
    const cw = container.clientWidth;
    const ch = container.clientHeight;
    if (x < container.scrollLeft || x + w > container.scrollLeft + cw)
      container.scrollLeft = Math.max(0, x - (cw - w) / 2);
    if (y < container.scrollTop || y + h > container.scrollTop + ch)
      container.scrollTop = Math.max(0, y - (ch - h) / 2);
  }

  // Zoom to `nz` around the band's centre. The band keeps its height (a jump
  // on every ＋/－ moved the whole page); the graph scrolls inside it instead.
  // The zoom is remembered, so the next graph opens at it.
  function zoomTo(nz: number) {
    if (!container) return;
    const z0 = scale();
    const z = clampZ(nz);
    writeGraphZoom(z);
    const cx = (container.scrollLeft + container.clientWidth / 2 - PAD) / z0;
    const cy = (container.scrollTop + container.clientHeight / 2 - PAD) / z0;
    setScale(z);
    container.scrollLeft = cx * z + PAD - container.clientWidth / 2;
    container.scrollTop = cy * z + PAD - container.clientHeight / 2;
  }
  const zoomIn = () => zoomTo(scale() * 1.2);
  const zoomOut = () => zoomTo(scale() / 1.2);

  // Drag to pan whenever the graph overflows the band. Starting on a node or
  // chip is a select, not a pan.
  let panStart: { x: number; y: number; left: number; top: number } | null = null;
  const pannable = () => sizeW() > (container?.clientWidth ?? Infinity) + 1 || sizeH() > bandH() + 1;
  const onPointerDown = (e: PointerEvent) => {
    if (!pannable() || !container || e.button !== 0) return;
    if ((e.target as HTMLElement).closest("button, [role='button']")) return;
    panStart = { x: e.clientX, y: e.clientY, left: container.scrollLeft, top: container.scrollTop };
    setDragging(true);
    container.setPointerCapture(e.pointerId);
    e.preventDefault();
  };
  const onPointerMove = (e: PointerEvent) => {
    if (!panStart || !container) return;
    container.scrollLeft = panStart.left - (e.clientX - panStart.x);
    container.scrollTop = panStart.top - (e.clientY - panStart.y);
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
    // Hand the zoom controls up to the panel header.
    // The reset button forgets the remembered zoom: back to 100%.
    props.onControls?.({
      fit: () => {
        clearGraphZoom();
        fit(false);
      },
      zoomIn,
      zoomOut,
    });
    fitAndMeasure();
    // Re-fit on a WIDTH resize only: a height change is fit's own doing (it
    // sets the band height). A change smaller than a scrollbar is the band's
    // own vertical scrollbar coming or going, and refitting on it could loop.
    let lastW = container?.clientWidth ?? 0;
    const ro = new ResizeObserver(() => {
      const w = container?.clientWidth ?? 0;
      if (Math.abs(w - lastW) < 24) return;
      lastW = w;
      requestAnimationFrame(() => fit());
    });
    if (container) ro.observe(container);
    // The max band height tracks the viewport (~60vh), so re-fit on window resize.
    const onWinResize = () => requestAnimationFrame(() => fit());
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
  // A new selection (a deep link, the List view, a step pane link) scrolls
  // its node into view.
  createEffect(
    on(
      () => props.selected,
      () => requestAnimationFrame(reveal),
      { defer: true },
    ),
  );

  // Hovering a node lights it and every step it needs, drawn or not.
  const [hovered, setHovered] = createSignal<string | null>(null);
  const lit = createMemo((): Set<string> | null => {
    const h = hovered();
    if (!h) return null;
    const up = allNeeds(props.steps, h);
    up.add(h);
    return up;
  });

  // Re-measure EDGES (and the graph's size) when a node's box could have
  // changed: topology or a per-step status. Zoom and scroll need no
  // re-measure: edges are stored in unscaled graph coords.
  createEffect(() => {
    props.steps
      .map((s) => `${s.id}:${s.status}:${s.attempts}:${s.gate ?? ""}:${s.note?.text ?? ""}`)
      .join(",");
    (props.services ?? []).map((s) => `${s.name}:${s.status}`).join(",");
    requestAnimationFrame(() =>
      requestAnimationFrame(() => {
        measure();
        if (graph) {
          setContentW(graph.offsetWidth);
          setContentH(graph.offsetHeight);
        }
      }),
    );
  });

  // The meta line under the node's name. Pure, in run-page.ts.
  const meta = (s: DagStep) =>
    nodeMeta(
      {
        status: s.status,
        tries: s.attempts,
        failedTries: s.failedTries,
        durationMs: s.durationMs,
        runningSince: s.runningSince,
        note: s.note,
        gate: s.gate,
        approvalGate: s.approvalGate,
      },
      now(),
    );

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

  // One Step node. A plugin gate is dashed and names its
  // plugin under its id.
  const StepNode = (p: { s: DagStep; label: string }) => {
    // The step as of the latest poll; the node itself stays mounted.
    const s = () => stepById().get(p.s.id) ?? p.s;
    const sel = () => props.selected === p.s.id;
    const plugin = () => s().origin?.find((o) => o.kind === "plugin");
    const m = () => meta(s());
    return (
      <button
        class={`dnode ${s().status} ${sel() ? "sel" : ""}`}
        classList={{
          reused: !!s().reused,
          plugin: !!plugin(),
          waiting: !!plugin() && isGateOpen(s()),
          lit: !!lit()?.has(p.s.id),
          dim: !!lit() && !lit()!.has(p.s.id),
        }}
        ref={(el) => nodes.set(p.s.id, el)}
        onClick={() => props.onSelect(p.s.id)}
        onPointerEnter={() => setHovered(p.s.id)}
        onPointerLeave={() => setHovered((h) => (h === p.s.id ? null : h))}
        onFocus={() => setHovered(p.s.id)}
        onBlur={() => setHovered((h) => (h === p.s.id ? null : h))}
        aria-pressed={sel()}
      >
        <span class={`dring ${s().status}`}>
          <span class="dring-core" />
        </span>
        <span class="dnode-main" title={p.s.id}>
          <span class="dnode-id">{p.label}</span>
          <span class={`dnode-meta ${m().tone ? `gnote ${m().tone}` : ""}`} title={s().note?.detail ?? m().text}>
            {m().text}
          </span>
          <Show when={plugin()}>
            {(o) => <span class="dnode-origin">plugin:{o().name.split("/")[0]}</span>}
          </Show>
          <Show when={s().reused}>
            <span class="sr-only">, carried over from an earlier version</span>
          </Show>
          <Show when={s().addedBy}>
            {(label) => (
              <span
                class="dnode-origin"
                title={
                  addedByForkRunApproval(s())
                    ? "Not in the pipeline file: Fork Run approval added this gate in front of a Run of fork code"
                    : "Not in the pipeline file: the environment added this approval gate"
                }
              >
                {label()}
              </span>
            )}
          </Show>
        </span>
        <Show when={s().gate}>
          <Icon icon="timer" size={12} class="dnode-gate" />
        </Show>
        <Show when={s().deploy}>
          <span class="dnode-gate" title="deploy step (`deploy: true`)">
            <Icon icon="rocket" size={12} />
          </span>
        </Show>
        <Show when={s().status === "running"}>
          <span class="dnode-prog" />
        </Show>
        {/* Docked SIDECARS: each of the step's own `services:` as a
            small copper chip INSIDE the node — they die with the step, so
            never a floated peer with an edge. A chip is its own clickable
            target (stopPropagation); a `<span role=button>` because it nests
            inside the step's own `<button>`. */}
        <Show when={(s().services?.length ?? 0) > 0}>
          <span class="dnode-sidecars">
            <Index each={s().services}>
              {(svc) => {
                const scSel = () => props.selected === p.s.id && props.sidecarFocus === svc().index;
                const pick = (e: Event) => {
                  e.stopPropagation();
                  props.onSelectSidecar?.(p.s.id, svc().index);
                };
                return (
                  <span
                    class={`dnode-sidecar ${scSel() ? "sel" : ""}`}
                    role="button"
                    tabindex="0"
                    title={`sidecar · ${svc().image} (service-${svc().index}) — logs inside ${p.s.id}`}
                    onClick={pick}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        pick(e);
                      }
                    }}
                  >
                    <span class="sc-hex">⬡</span>
                    <span class="sc-name">{shortImage(svc().image)}</span>
                  </span>
                );
              }}
            </Index>
          </span>
        </Show>
      </button>
    );
  };

  // Dependency columns; a library invocation is a box holding its own
  // columns, nested for an inner invocation.
  // The cells of one matrix in a column sit under one heading naming the step
  // and its keys; each cell's node shows only its values.
  // Each column's nodes are as wide as its longest label needs
  // (`columnNodeWidth`), so a matrix cell's values fit on one line.
  const colWidth = (col: DagCell<DagStep>[]): number =>
    columnNodeWidth(
      segmentByMatrix(col, (c) => (c.kind === "step" ? c.step.id : null)).flatMap((seg) =>
        seg.items.filter((it) => it.item.kind === "step").map((it) => it.label),
      ),
      { minChars: 21 },
    );
  const Cols = (p: { cols: DagCell<DagStep>[][] }) => (
    <div class="dag-cols">
      <For each={p.cols}>
        {(col) => (
          <div class="dag-col" style={{ "--node-w": `${colWidth(col)}px` }}>
            <For each={segmentByMatrix(col, (c) => (c.kind === "step" ? c.step.id : null))}>
              {(seg) =>
                seg.matrix ? (
                  <div class="dag-mx">
                    <div class="dag-mxh" title={matrixHeading(seg.matrix)}>
                      <b>{seg.matrix.base}</b> · matrix · {seg.matrix.keys.join(" · ")}
                    </div>
                    <For each={seg.items}>
                      {(it) => (it.item.kind === "step" ? <StepNode s={it.item.step} label={it.label} /> : null)}
                    </For>
                  </div>
                ) : (
                  <For each={seg.items}>{(it) => <Cell cell={it.item} />}</For>
                )
              }
            </For>
          </div>
        )}
      </For>
    </div>
  );

  // A step, or a library invocation's box with its own columns.
  const Cell = (p: { cell: DagCell<DagStep> }) => {
    const cell = p.cell;
    return cell.kind === "step" ? (
      <StepNode s={cell.step} label={cell.step.id} />
    ) : (
                  <div class="dgroup" title={`${cell.group.key}: ${cell.group.kind === "library" ? "lib:" : ""}${cell.group.name} ${cell.group.content_hash}`}>
                    <div class="dgroup-h">
                      <span class="gi">{cell.group.label}</span>
                      <span>
                        {cell.group.kind === "library" ? "lib:" : ""}
                        {cell.group.name}
                      </span>
                      <span class="gh">{shortHash(cell.group.content_hash)}</span>
                      <Show when={cell.group.yanked}>
                        <Pill tone="danger" look="dotted">yanked</Pill>
                      </Show>
                    </div>
                    <Cols cols={cell.cols} />
                  </div>
    );
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
      {/* The sizer is as big as the scaled graph, so the band scrolls over it.
          The zoom layer transforms edges and nodes as one, so a zoom never
          moves an edge off its node. */}
      <div class="dag-sizer" style={{ width: `${sizeW()}px`, height: `${sizeH()}px` }}>
      <div
        class="dag-zoom"
        style={{ transform: `translate(${PAD}px, ${PAD}px) scale(${scale()})` }}
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
              // Turn at the bus before the target; a target closer than the
              // corner radius (a source inside the same gap) turns mid-way.
              const mx = e.bx - r > e.x1 ? e.bx : (e.x1 + e.x2) / 2;
              const dir = e.y2 >= e.y1 ? 1 : -1;
              d =
                Math.abs(e.y2 - e.y1) < 2
                  ? `M ${e.x1} ${e.y1} H ${e.x2}`
                  : `M ${e.x1} ${e.y1} H ${mx - r} Q ${mx} ${e.y1} ${mx} ${e.y1 + r * dir} ` +
                    `V ${e.y2 - r * dir} Q ${mx} ${e.y2} ${mx + r} ${e.y2} H ${e.x2}`;
            }
            return (
              <path
                d={d}
                class={e.dashed ? "dashed" : e.hot ? "hot" : ""}
                classList={{
                  lit: !!lit()?.has(e.from) && !!lit()?.has(e.to),
                  dim: !!lit() && !(lit()!.has(e.from) && lit()!.has(e.to)),
                }}
                fill="none"
              />
            );
          }}
        </For>
      </svg>
      {/* The graph body: the services lane as a horizontal band on TOP, then the
          dependency layers flowing left→right below it. `.dag-cols` is now a ROW
          of depth layers and each `.dag-col` stacks its siblings vertically; the
          column wrapper keeps the lane spanning the width above that flow. */}
      <div class="dag-graph" ref={graph}>
        {/* Services lane: shared services are PEERS, not steps — a
            band at the top, above dependency layer 0, with dotted `uses` edges
            dropping to each consuming step. They carry no `needs` depth, so the
            lane holds them all. Registered in `nodes` (keyed `service:<name>`) so
            `measure()` can route edges to them. */}
        <Show when={(props.services?.length ?? 0) > 0}>
          <div class="dag-lane">
            <span class="dag-lane-label">Services</span>
            <div class="dag-lane-nodes">
              <For each={(props.services ?? []).map((x) => x.name)}>
                {(name) => {
                  // Keyed by name: the services list is re-read every 2s.
                  const sv = () => props.services?.find((x) => x.name === name) ?? { name, status: "pending" };
                  const selId = `service:${name}`;
                  const sel = () => props.selected === selId;
                  return (
                    <button
                      class={`dnode service ${sel() ? "sel" : ""}`}
                      ref={(el) => nodes.set(selId, el)}
                      onClick={() => props.onSelect(selId)}
                      title={`${name} — shared service · ${sv().status}`}
                    >
                      <span class={`dring service ${sv().status}`}>
                        <span class="dring-core" />
                      </span>
                      <span class="dnode-main" title={name}>
                        <span class="dnode-id">{name}</span>
                        <span class="dnode-meta">{svcMeta(sv())}</span>
                      </span>
                      <Icon icon="database" size={14} class="dnode-svc-ico" />
                    </button>
                  );
                }}
              </For>
            </div>
          </div>
        </Show>
        <Cols cols={layers()} />
      </div>
      </div>
      </div>
    </div>
  );
}
