// The version selector beside the run number in the run header. A run with one
// version shows nothing: there is nothing to pick. With more, it is a
// dropdown, "Version 2 of 3 · latest ▾", with one row per version, newest
// first: its outcome counts, why it exists (the trigger, or "you reran from
// build"), who opened it and when. Picking an older version shows the run as
// it stood then, read-only; picking the latest returns to live. The versions
// are derived from the event log; "Take" never shows. Page chrome, so it reads
// the page tokens.
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
  /** 1-based version number: the value emitted to `onSelect` (null when latest). */
  n: number;
  /** "latest" / "original run" / "you reran b": the read-only banner's words. */
  label: string;
  /** Why the version exists: the trigger, or "you reran from b". */
  why: string;
  /** Who opened it and when ("by a.kim · 15m ago"); may be empty. */
  sub: string;
  /** Per-status tally driving the outcome chips. */
  summary: OutcomeCounts;
  /** This is the newest (open) version. */
  isLatest: boolean;
  /** This version is the one in view. */
  isSelected: boolean;
};

// Fixed render order + accent mapping. Only the five named buckets get an
// accent token; anything else falls to muted "other".
const CHIPS: { key: keyof OutcomeCounts; glyph: string; cls: string; title: string }[] = [
  { key: "succeeded", glyph: "✓", cls: "ok", title: "passed" },
  { key: "failed", glyph: "✕", cls: "danger", title: "failed" },
  { key: "running", glyph: "●", cls: "running", title: "running" },
  { key: "superseded", glyph: "⊘", cls: "copper", title: "superseded" },
  { key: "notRun", glyph: "○", cls: "sage", title: "not run" },
  { key: "other", glyph: "·", cls: "other", title: "other" },
];

/** The coloured count chips: one per non-empty status bucket. */
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
  // The menu opens toward the side of the trigger with room: right-aligned
  // when a left-aligned menu would cross the window edge.
  const [alignEnd, setAlignEnd] = createSignal(false);
  let menu: HTMLElement | undefined;
  let trigger: HTMLButtonElement | undefined;
  const MENU_MAX_W = 360;
  // Newest first, whatever order the caller assembled them in.
  const ordered = () => [...props.rows].sort((a, b) => b.n - a.n);
  const selected = () => props.rows.find((r) => r.isSelected) ?? ordered()[0];
  const viewingLatest = () => selected()?.isLatest ?? true;

  const pick = (row: VersionRow) => {
    props.onSelect(row.isLatest ? null : row.n);
    setOpen(false);
  };

  onMount(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });
  const onMenuKey = (e: KeyboardEvent) => {
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
    e.preventDefault();
    const rows = Array.from(menu?.querySelectorAll<HTMLButtonElement>("[role=menuitemradio]") ?? []);
    const i = rows.indexOf(document.activeElement as HTMLButtonElement);
    const next = e.key === "ArrowDown" ? Math.min(rows.length - 1, i + 1) : Math.max(0, i - 1);
    rows[next]?.focus();
  };

  return (
    <Show when={props.rows.length > 1}>
      <span class="vdrop">
        <button
          type="button"
          class="vpick"
          ref={trigger}
          classList={{ open: open(), traveling: !viewingLatest() }}
          aria-haspopup="menu"
          aria-expanded={open()}
          title="Pick a version to see the run as it stood then"
          onClick={() => {
            const r = trigger?.getBoundingClientRect();
            setAlignEnd(!!r && r.left + Math.min(MENU_MAX_W, window.innerWidth * 0.9) > window.innerWidth - 8);
            setOpen((v) => !v);
            if (open()) queueMicrotask(() => menu?.querySelector<HTMLButtonElement>("[aria-checked=true]")?.focus());
          }}
        >
          <span class="k">Version</span> {selected()?.n} of {props.rows.length}
          {viewingLatest() ? " · latest" : ""}
          <span class="vdrop-caret" aria-hidden="true">
            ▾
          </span>
        </button>

        <Show when={open()}>
          <div class="vdrop-backdrop" aria-hidden="true" onClick={() => setOpen(false)} />
          <nav class="vdrop-menu" classList={{ end: alignEnd() }} role="menu" aria-label="run versions" ref={menu} onKeyDown={onMenuKey}>
            <For each={ordered()}>
              {(row) => (
                <button
                  type="button"
                  role="menuitemradio"
                  aria-checked={row.isSelected}
                  class="vdrop-row"
                  classList={{ sel: row.isSelected }}
                  onClick={() => pick(row)}
                >
                  <span class="vdrop-pip" />
                  <span class="vdrop-rowbody">
                    <span class="vdrop-rowtop">
                      <span class="vdrop-rowlbl">
                        Version {row.n}
                        {row.isLatest ? " · latest" : ""}
                      </span>
                      <Show when={row.isLatest && props.live}>
                        <span class="vdrop-live">
                          <span class="dot" /> Live
                        </span>
                      </Show>
                    </span>
                    <span class="vdrop-why">{row.why}</span>
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
    </Show>
  );
}
