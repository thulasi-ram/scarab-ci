// Attempts filmstrip (ADR-0056 amendment, redesign stage 3): the "which try"
// axis, moved OUT of the DAG graph and INTO the evidence-pane header. A
// horizontal strip of outcome-shaped markers — one per try of the selected
// step, oldest → newest. The ACTIVE try (the one scoping every tab) is enlarged
// and shows its full label; the rest are shape-only with a hover title. A
// maximal run of ≥2 CONSECUTIVE auto/human retries that landed the SAME way
// (same outcome + same failure kind) folds into one `×N` marker, so a long
// retry streak reads as one beat of the story ("✗×18 · infra → ✓") instead of
// eighteen identical dots. Never folded away: the active try, the first
// (`initial`) try, the frontier (last) try, and any rerun/cascade try. This is
// PAGE chrome (the pane header), so it reads the page/theme tokens — not the
// always-dark DAG-canvas tokens.
import { For, Show } from "solid-js";
import type { AttemptCause } from "../takes";

/** One try of the SELECTED step, resolved from the event log by the caller —
 * the strip renders these in the pane header (formerly `Dag.tsx`'s `DagTry`,
 * moved here verbatim when the try axis left the graph). */
export type FilmstripTry = {
  id: string;
  /** 0-based order; the label is `try {index + 1}`. */
  index: number;
  cause?: AttemptCause;
  /** The backend's authoritative per-attempt verdict (AttemptDto.outcome):
   * `running | succeeded | failed | superseded | cancelled`. Preferred over the
   * `failed` bool so a running/superseded/cancelled try never renders green. */
  outcome: string;
  failed: boolean;
  failure?: string;
  /** Cut short by a rerun of an ancestor (started, never finished). */
  superseded: boolean;
  /** A success that a newer success replaced as of-record. */
  shadowed: boolean;
  /** Re-adopted after a control-plane restart (visibility marker, not a re-run). */
  readopted: boolean;
};

/** Plain-english cause suffix (ADR-0056 amendment) — the machine's own retry vs
 * a human rerun of this step vs a rerun of an ancestor that dragged it along. */
const causeSuffix = (c?: AttemptCause): string =>
  c === "rerun" ? " · you reran" : c === "cascade" ? " · ⟵ rerun" : c === "retry" ? " · auto-retry" : "";
const tryTitle = (t: FilmstripTry): string => `try ${t.index + 1}${causeSuffix(t.cause)}`;
// Prefer the backend's authoritative `outcome` (AttemptDto.outcome) so a
// still-running / superseded / cancelled try is never mislabelled green. Fall
// back to the pre-fix superseded→failed→green derivation only when `outcome` is
// absent (an old server), so nothing regresses.
const tryOutcome = (t: FilmstripTry): string => {
  switch (t.outcome) {
    case "running":
      return "● running";
    case "succeeded":
      return "✓ succeeded";
    case "failed":
      return `✗ failed${t.failure ? ` · ${t.failure}` : ""}`;
    case "superseded":
      return "⊘ superseded";
    case "cancelled":
      return "⊘ cancelled";
  }
  if (t.superseded) return "⊘ superseded";
  if (t.failed) return `✗ failed${t.failure ? ` · ${t.failure}` : ""}`;
  return "✓ succeeded";
};
const tryTone = (t: FilmstripTry): string => {
  switch (t.outcome) {
    case "running":
      return "running";
    case "succeeded":
      return "emerald";
    case "failed":
      return "danger";
    case "superseded":
    case "cancelled":
      return "copper";
  }
  return t.superseded ? "copper" : t.failed ? "danger" : "emerald";
};

/** The single outcome glyph a shape-only (inactive / collapsed) marker shows —
 * mirrors `tryOutcome`'s leading symbol without the words. */
const tryGlyph = (t: FilmstripTry): string => {
  switch (t.outcome) {
    case "running":
      return "●";
    case "succeeded":
      return "✓";
    case "failed":
      return "✗";
    case "superseded":
    case "cancelled":
      return "⊘";
  }
  return t.superseded ? "⊘" : t.failed ? "✗" : "✓";
};

/** A render cell: a single try, or a folded run of ≥2 identical consecutive
 * retries drawn as one `×N` marker. */
type Cell =
  | { kind: "one"; t: FilmstripTry }
  | { kind: "run"; tries: FilmstripTry[]; count: number };

export default function AttemptsFilmstrip(props: {
  tries: FilmstripTry[];
  /** The try currently scoping the evidence pane (the enlarged marker). */
  active: string | null;
  /** Pick a try — scopes the evidence pane to `(selected step, attempt)`. */
  onSelect: (id: string | null) => void;
}) {
  // Partition the tries (oldest → newest) into render cells. A try may fold into
  // a `×N` run ONLY when it is an auto/human retry, is not the active try, and is
  // neither the first (`initial`) nor the last (frontier) try — so `initial`,
  // `rerun`, `cascade`, the active, and the frontier always stand alone. A run
  // extends while its neighbours share that foldability AND the same outcome and
  // failure kind (ORCHESTRATOR DECISION #1: identical failures DO collapse).
  const cells = (): Cell[] => {
    const ts = props.tries;
    const n = ts.length;
    const foldable = (t: FilmstripTry, i: number): boolean =>
      t.cause === "retry" && t.id !== props.active && i > 0 && i < n - 1;
    const out: Cell[] = [];
    let i = 0;
    while (i < n) {
      const t = ts[i];
      if (foldable(t, i)) {
        let j = i + 1;
        while (
          j < n &&
          foldable(ts[j], j) &&
          ts[j].outcome === t.outcome &&
          (ts[j].failure ?? "") === (t.failure ?? "")
        ) {
          j++;
        }
        if (j - i >= 2) {
          out.push({ kind: "run", tries: ts.slice(i, j), count: j - i });
          i = j;
          continue;
        }
      }
      out.push({ kind: "one", t });
      i++;
    }
    return out;
  };

  // The hover title on a folded marker: the try range, shared cause, shared
  // outcome, and the fold count — the whole streak in one line.
  const rangeTitle = (c: Extract<Cell, { kind: "run" }>): string => {
    const a = c.tries[0];
    const b = c.tries[c.count - 1];
    return `tries ${a.index + 1}–${b.index + 1}${causeSuffix(a.cause)} — ${tryOutcome(a)} (${c.count}×)`;
  };

  return (
    <Show when={props.tries.length > 0}>
      <div class="fstrip" aria-label="attempts">
        <For each={cells()}>
          {(c) => {
            if (c.kind === "run") {
              const rep = c.tries[0];
              return (
                <button
                  type="button"
                  class={`fmark ${tryTone(rep)}`}
                  classList={{ shadow: rep.shadowed }}
                  title={rangeTitle(c)}
                  onClick={() => props.onSelect(c.tries[c.count - 1].id)}
                >
                  <span class="fglyph">{tryGlyph(rep)}</span>
                  <span class="fcount">×{c.count}</span>
                </button>
              );
            }
            const t = c.t;
            const isActive = t.id === props.active;
            return (
              <button
                type="button"
                class={`fmark ${tryTone(t)}`}
                classList={{ shadow: t.shadowed, "fmark-active": isActive }}
                aria-current={isActive ? "true" : undefined}
                title={isActive ? undefined : `${tryTitle(t)} — ${tryOutcome(t)}`}
                onClick={() => props.onSelect(t.id)}
              >
                <Show when={isActive} fallback={<span class="fglyph">{tryGlyph(t)}</span>}>
                  <span class="flabel">{`${tryTitle(t)} · ${tryOutcome(t)}`}</span>
                </Show>
                <Show when={t.readopted}>
                  <span class="freadopt" title="re-adopted after control-plane restart">
                    ⟲
                  </span>
                </Show>
              </button>
            );
          }}
        </For>
      </div>
    </Show>
  );
}
