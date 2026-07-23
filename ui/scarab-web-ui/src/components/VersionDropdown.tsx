// Version dropdown (Proposal B, redesign): the run-history control, folded from
// the former persistent left rail (VersionRail) into a compact dropdown that
// lives in the Pipeline component's band toolbar — the `◈ latest ▾` control.
// One row per version (Take), newest-first, each a whole-run version derived
// from the event log ("Take"/"attempt" never surface). A row carries its label
// ("latest" / "original run" / "you reran b"), a provenance sub-line
// (actor · relTime), and a compact outcome mini-summary (colored count chips per
// status). The trigger names the version currently in view; picking a row zooms
// the whole Pipeline component to that version (an older version turns it read-
// only, flagged by the banner below the toolbar). Selecting the latest row
// returns to live. This is page chrome (NOT the always-dark DAG canvas) → it
// reads the PAGE/theme tokens.
import { For, Show, createSignal, onMount, onCleanup } from "solid-js";

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
// accent token; anything else falls to muted-sage "other".
const CHIPS: { key: keyof OutcomeCounts; glyph: string; cls: string; title: string }[] = [
  { key: "succeeded", glyph: "✓", cls: "ok", title: "succeeded" },
  { key: "failed", glyph: "✗", cls: "danger", title: "failed" },
  { key: "running", glyph: "●", cls: "running", title: "running" },
  { key: "superseded", glyph: "⊘", cls: "copper", title: "superseded" },
  { key: "notRun", glyph: "○", cls: "sage", title: "not run" },
  { key: "other", glyph: "·", cls: "other", title: "other" },
];

/** The colored count chips (shared with the former rail) — one per non-empty
 * status bucket, using the shared status accent tokens. */
export function MiniSummary(props: { summary: OutcomeCounts }) {
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

export default function VersionDropdown(props: {
  rows: VersionRow[];
  onSelect: (n: number | null) => void;
  live: boolean;
}) {
  const [open, setOpen] = createSignal(false);
  // Newest-first, regardless of the order the caller assembled them in.
  const ordered = () => [...props.rows].sort((a, b) => b.n - a.n);
  const selected = () => props.rows.find((r) => r.isSelected) ?? ordered()[0];
  // The trigger names the version in view; when it isn't the latest we're time-
  // travelling — tint the trigger copper (echoes the read-only banner's caution).
  const viewingLatest = () => selected()?.isLatest ?? true;

  const pick = (row: VersionRow) => {
    props.onSelect(row.isLatest ? null : row.n);
    setOpen(false);
  };

  // Escape closes (the backdrop below handles outside-click).
  onMount(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });

  return (
    <span class="vdrop">
      <button
        type="button"
        class="vdrop-btn"
        classList={{ open: open(), traveling: !viewingLatest() }}
        aria-haspopup="menu"
        aria-expanded={open()}
        title="run versions — pick one to view it"
        onClick={() => setOpen((v) => !v)}
      >
        <span class="vdrop-emblem" aria-hidden="true">◈</span>
        <span class="vdrop-lbl">{selected()?.label ?? "latest"}</span>
        <Show when={viewingLatest() && props.live}>
          <span class="vdrop-live" title="live">
            <span class="dot" />
          </span>
        </Show>
        <span class="vdrop-caret" aria-hidden="true">▾</span>
      </button>

      <Show when={open()}>
        {/* Transparent full-viewport click-catcher, under the menu. */}
        <div class="vdrop-backdrop" aria-hidden="true" onClick={() => setOpen(false)} />
        <nav class="vdrop-menu" role="menu" aria-label="run versions">
          <For each={ordered()}>
            {(row) => (
              <button
                type="button"
                role="menuitemradio"
                aria-checked={row.isSelected}
                class="vdrop-row"
                classList={{ sel: row.isSelected }}
                onClick={() => pick(row)}
                title={row.sub ? `${row.label} · ${row.sub}` : row.label}
              >
                <span class="vdrop-pip" />
                <span class="vdrop-rowbody">
                  <span class="vdrop-rowtop">
                    <span class="vdrop-rowlbl">{row.label}</span>
                    <Show when={row.isLatest && props.live}>
                      <span class="vdrop-live">
                        <span class="dot" /> live
                      </span>
                    </Show>
                  </span>
                  <Show when={row.sub}>
                    <span class="vdrop-sub">{row.sub}</span>
                  </Show>
                  <MiniSummary summary={row.summary} />
                </span>
              </button>
            )}
          </For>
        </nav>
      </Show>
    </span>
  );
}
