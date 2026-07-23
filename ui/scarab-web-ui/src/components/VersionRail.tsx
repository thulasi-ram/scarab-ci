// Version rail (ADR-0056 amendment, redesign stage 2): the run-history control,
// always present as a persistent left rail beside the Pipeline component —
// replacing the old header dropdown. One row per version (Take), newest-first,
// each a whole-run version derived from the event log ("Take"/"attempt" never
// surface). A row carries its label ("latest" / "original run" / "you reran b"),
// a provenance sub-line (actor · relTime), and a compact outcome mini-summary
// (colored count chips per status). Selecting a row zooms the whole Pipeline
// component to that version; the latest row is the live frontier. This is page
// chrome (NOT the always-dark DAG canvas) → it reads the PAGE/theme tokens.
import { For, Show } from "solid-js";

/** How a version's steps landed, bucketed for the mini-summary. */
export type OutcomeCounts = {
  succeeded: number;
  failed: number;
  superseded: number;
  notRun: number;
  running: number;
  other: number;
};

/** One version row, fully resolved by the caller from the event-log takes. */
export type VersionRow = {
  /** 1-based take number — the value emitted to `onSelect` (null when latest). */
  n: number;
  /** Primary line: "latest" / "original run" / "you reran b". */
  label: string;
  /** Second line: actor · relTime (may be empty for the original run). */
  sub: string;
  /** Per-status tally driving the outcome chips. */
  summary: OutcomeCounts;
  /** This is the newest (open) version — the live frontier. */
  isLatest: boolean;
  /** This version is the one currently in view. */
  isSelected: boolean;
};

// Fixed render order + accent mapping. Only the five named buckets get an
// accent token (per the plan); anything else falls to muted-sage "other".
const CHIPS: { key: keyof OutcomeCounts; glyph: string; cls: string; title: string }[] = [
  { key: "succeeded", glyph: "✓", cls: "ok", title: "succeeded" },
  { key: "failed", glyph: "✗", cls: "danger", title: "failed" },
  { key: "running", glyph: "●", cls: "running", title: "running" },
  { key: "superseded", glyph: "⊘", cls: "copper", title: "superseded" },
  { key: "notRun", glyph: "○", cls: "sage", title: "not run" },
  { key: "other", glyph: "·", cls: "other", title: "other" },
];

function MiniSummary(props: { summary: OutcomeCounts }) {
  const shown = () => CHIPS.filter((c) => props.summary[c.key] > 0);
  return (
    <span class="vr-sum">
      <Show when={shown().length > 0} fallback={<span class="vr-chip other">—</span>}>
        <For each={shown()}>
          {(c) => (
            <span class={`vr-chip ${c.cls}`} title={`${props.summary[c.key]} ${c.title}`}>
              {props.summary[c.key]}
              <span class="vr-glyph">{c.glyph}</span>
            </span>
          )}
        </For>
      </Show>
    </span>
  );
}

export default function VersionRail(props: {
  rows: VersionRow[];
  onSelect: (n: number | null) => void;
  live: boolean;
}) {
  // Newest-first, regardless of the order the caller assembled them in.
  const ordered = () => [...props.rows].sort((a, b) => b.n - a.n);
  return (
    <nav class="version-rail" aria-label="run versions">
      <div class="vr-head">Versions</div>
      <ul class="vr-list">
        <For each={ordered()}>
          {(row) => (
            <li>
              <button
                type="button"
                class="vr-row"
                classList={{ sel: row.isSelected }}
                aria-current={row.isSelected ? "true" : undefined}
                onClick={() => props.onSelect(row.isLatest ? null : row.n)}
                title={row.sub ? `${row.label} · ${row.sub}` : row.label}
              >
                <span class="vr-pip" />
                <span class="vr-body">
                  <span class="vr-top">
                    <span class="vr-label">{row.label}</span>
                    <Show when={row.isLatest && props.live}>
                      <span class="vr-live">
                        <span class="dot" /> live
                      </span>
                    </Show>
                  </span>
                  <Show when={row.sub}>
                    <span class="vr-sub">{row.sub}</span>
                  </Show>
                  <MiniSummary summary={row.summary} />
                </span>
              </button>
            </li>
          )}
        </For>
      </ul>
    </nav>
  );
}
