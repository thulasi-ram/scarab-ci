// The run's versions as one thin line under the run header: every version,
// oldest first, coloured by how it ended, the one in view marked. Up to
// RIBBON_DOTS versions read as named dots ("v1", "v3 latest"); more read as one
// tick per version, so a run with 100 reruns still fits on one line and a
// failing streak shows at a glance (the row scrolls sideways past about 110
// and keeps the one in view visible). Hovering a version names it, why it
// exists and when; ← and → step through them. While an older version is in
// view the line is the top edge of the page's amber frame and says so on its
// right: read-only, and the way back to the latest. A run with one version
// shows no line. Page chrome, so it reads the page tokens.
import { createEffect, For, on, Show } from "solid-js";
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
  let ticks: HTMLDivElement | undefined;
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

  // Keep the version in view visible in a long row of ticks.
  createEffect(
    on(
      () => selected()?.n,
      () =>
        requestAnimationFrame(() => {
          const el = ticks?.querySelector<HTMLElement>("[aria-pressed=true]");
          if (!ticks || !el) return;
          const r = el.getBoundingClientRect();
          const c = ticks.getBoundingClientRect();
          if (r.left < c.left || r.right > c.right) ticks.scrollLeft += r.left - c.left - c.width / 2;
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
          <Show
            when={props.rows.length <= RIBBON_DOTS}
            fallback={
              <>
                <span class="vr-end">v1</span>
                <div class="vr-ticks" ref={ticks}>
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
                </div>
                <span class="vr-end">v{ordered().length}</span>
              </>
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
