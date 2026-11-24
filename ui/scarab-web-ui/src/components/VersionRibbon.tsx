// The run's versions as one thin line under the run header: every version,
// oldest first, coloured by how it ended, the one in view marked. Up to
// RIBBON_DOTS versions read as named dots ("v1", "v3 latest"); more read as one
// tick per version, so a run with 100 reruns still fits on one line and a
// failing streak shows at a glance. Either row scrolls sideways when it does
// not fit (dots on a phone, ticks past about 110): the edge with more fades
// and ‹ › buttons page through, and the version in view is kept visible.
// Hovering a version names it, why it
// exists and when; ← and → step through them. While an older version is in
// view the line is the top edge of the page's grey frame and says so on its
// right: read-only, and the way back to the latest. A run with one version
// shows no line. Page chrome, so it reads the page tokens.
import { createEffect, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import { versionOutcome } from "../takes";

/** How a version's steps landed, bucketed for its outcome. */
export type OutcomeCounts = {
  succeeded: number;
  failed: number;
  superseded: number;
  notRun: number;
  running: number;
  other: number;
};

/** One version, fully resolved by the caller from the event-log takes. */
export type VersionRow = {
  /** 1-based version number: the value emitted to `onSelect` (null when latest). */
  n: number;
  /** "latest" / "original run" / "you reran b". */
  label: string;
  /** Why the version exists: the trigger, or "you reran from b". */
  why: string;
  /** Who opened it and when ("by a.kim · 15m ago"); may be empty. */
  sub: string;
  /** Per-status tally: how the version's steps landed. */
  summary: OutcomeCounts;
  /** This is the newest (open) version. */
  isLatest: boolean;
  /** This version is the one in view. */
  isSelected: boolean;
};

/** Up to this many versions read as named dots; more as ticks. */
export const RIBBON_DOTS = 8;

export default function VersionRibbon(props: { rows: VersionRow[]; onSelect: (n: number | null) => void }) {
  let scroller: HTMLDivElement | undefined;
  // Whether the row has more to either side, for the fades and ‹ › buttons.
  const [more, setMore] = createSignal({ left: false, right: false });
  const measure = () => {
    if (!scroller) return;
    const { scrollLeft, scrollWidth, clientWidth } = scroller;
    setMore({ left: scrollLeft > 1, right: scrollLeft + clientWidth < scrollWidth - 1 });
  };
  const overflows = () => more().left || more().right;
  const page = (dir: -1 | 1) =>
    scroller?.scrollBy({ left: dir * Math.max(80, scroller.clientWidth * 0.8), behavior: "smooth" });
  onMount(() => {
    const ro = new ResizeObserver(measure);
    if (scroller) ro.observe(scroller);
    onCleanup(() => ro.disconnect());
  });
  const ordered = () => [...props.rows].sort((a, b) => a.n - b.n);
  const selected = () => props.rows.find((r) => r.isSelected) ?? ordered()[ordered().length - 1];
  const older = () => !(selected()?.isLatest ?? true);
  const pick = (row: VersionRow) => props.onSelect(row.isLatest ? null : row.n);
  const title = (row: VersionRow) =>
    [`Version ${row.n}${row.isLatest ? " · latest" : ""}`, versionOutcome(row.summary), row.why, row.sub]
      .filter(Boolean)
      .join(" · ");

  // ← and → step to the previous and next version.
  const onKey = (e: KeyboardEvent) => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    const rows = ordered();
    const i = rows.findIndex((r) => r.isSelected);
    const next = rows[Math.max(0, Math.min(rows.length - 1, (i < 0 ? rows.length - 1 : i) + (e.key === "ArrowLeft" ? -1 : 1)))];
    if (!next || next.isSelected) return;
    e.preventDefault();
    pick(next);
    queueMicrotask(() =>
      (e.currentTarget as HTMLElement | null)?.querySelector<HTMLElement>(`[data-n="${next.n}"]`)?.focus(),
    );
  };

  // Keep the version in view visible when the row scrolls, then re-measure.
  createEffect(
    on(
      () => [selected()?.n, props.rows.length],
      () =>
        requestAnimationFrame(() => {
          const el = scroller?.querySelector<HTMLElement>("[aria-pressed=true]");
          if (scroller && el) {
            const r = el.getBoundingClientRect();
            const c = scroller.getBoundingClientRect();
            if (r.left < c.left || r.right > c.right) scroller.scrollLeft += r.left - c.left - (c.width - r.width) / 2;
          }
          measure();
        }),
    ),
  );

  return (
    <Show when={props.rows.length > 1}>
      <div class="vribbon" classList={{ older: older() }}>
        <span class="vr-k" id="vr-k">
          Versions
        </span>
        <div class="vr-row" role="group" aria-labelledby="vr-k" onKeyDown={onKey}>
          <Show when={overflows()}>
            <button
              type="button"
              class="vr-pg"
              aria-label="Earlier versions"
              disabled={!more().left}
              onClick={() => page(-1)}
            >
              ‹
            </button>
          </Show>
          <Show when={props.rows.length > RIBBON_DOTS}>
            <span class="vr-end">v1</span>
          </Show>
          <div
            class="vr-scroll"
            classList={{ ticks: props.rows.length > RIBBON_DOTS, "more-l": more().left, "more-r": more().right }}
            ref={scroller}
            onScroll={measure}
          >
            <Show
              when={props.rows.length <= RIBBON_DOTS}
              fallback={
                <For each={ordered()}>
                  {(row) => (
                    <button
                      type="button"
                      class={`vr-tick ${versionOutcome(row.summary)}`}
                      data-n={row.n}
                      aria-pressed={row.isSelected}
                      aria-label={title(row)}
                      title={title(row)}
                      onClick={() => pick(row)}
                    />
                  )}
                </For>
              }
            >
              <For each={ordered()}>
                {(row, i) => (
                  <>
                    <Show when={i() > 0}>
                      <span class="vr-link" aria-hidden="true" />
                    </Show>
                    <button
                      type="button"
                      class="vr-dot"
                      data-n={row.n}
                      aria-pressed={row.isSelected}
                      title={title(row)}
                      onClick={() => pick(row)}
                    >
                      <span class={`vr-pip ${versionOutcome(row.summary)}`} aria-hidden="true" />
                      v{row.n}
                      {row.isLatest ? " latest" : ""}
                    </button>
                  </>
                )}
              </For>
            </Show>
          </div>
          <Show when={props.rows.length > RIBBON_DOTS}>
            <span class="vr-end">v{ordered().length}</span>
          </Show>
          <Show when={overflows()}>
            <button
              type="button"
              class="vr-pg"
              aria-label="Later versions"
              disabled={!more().right}
              onClick={() => page(1)}
            >
              ›
            </button>
          </Show>
        </div>
        <Show when={older()}>
          <span class="vr-note" role="status">
            <b>
              Version {selected()?.n} of {props.rows.length}
            </b>{" "}
            · read-only · rerun, approve and cancel act on the latest
          </span>
          <button type="button" class="vr-latest" onClick={() => props.onSelect(null)}>
            Show latest
          </button>
        </Show>
      </div>
    </Show>
  );
}
