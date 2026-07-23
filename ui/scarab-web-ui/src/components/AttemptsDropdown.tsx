// Attempts dropdown (design feedback, supersedes the redesign-stage-3 filmstrip):
// the "which try" axis, folded INTO the evidence-pane coordinate stamp as its
// "try N" segment. The former filmstrip — a horizontal strip of outcome-shaped
// markers with a `×N` fold for retry streaks — read as noisy chrome; this is the
// compact replacement. 0 tries → nothing. 1 try → a STATIC outcome chip. >1 →
// a dropdown whose trigger names the ACTIVE try (the one scoping every tab) and
// whose menu lists every try oldest → newest, each carrying its cause + outcome,
// the active one marked. Scales to any N natively — the menu scrolls when long,
// so no fold logic is needed. This is PAGE chrome (the pane header), so it reads
// the page/theme tokens — not the always-dark DAG-canvas tokens.
import { For, Show, createSignal, onCleanup, onMount } from "solid-js";
import type { AttemptCause } from "../takes";

/** One try of the SELECTED step, resolved from the event log by the caller —
 * RunDetail's `stripTries()` produces these; the dropdown renders them. The name
 * is kept from the former filmstrip so the caller's type import is unchanged. */
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
// absent (an old server), so nothing regresses. The leading glyph is baked in
// (✓/✗/●/⊘) so `try N · {outcome}` reads as "try N · {glyph} {word}".
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

/** The re-adoption marker — a crash re-adoption is the same attempt/fence, never
 * a new execution, so it rides inside the trigger/row rather than as its own try. */
const Readopt = () => (
  <span class="adrop-readopt" title="re-adopted after control-plane restart">
    ⟲
  </span>
);

export default function AttemptsDropdown(props: {
  tries: FilmstripTry[];
  /** The try currently scoping the evidence pane. */
  active: string | null;
  /** Pick a try — scopes the evidence pane to `(selected step, attempt)`. */
  onSelect: (id: string | null) => void;
}) {
  const [open, setOpen] = createSignal(false);

  // The active try (the one scoping every tab); fall back to the frontier so the
  // trigger always names something concrete. Only read when there are ≥2 tries.
  const activeTry = (): FilmstripTry =>
    props.tries.find((t) => t.id === props.active) ?? props.tries[props.tries.length - 1];

  const pick = (id: string) => {
    props.onSelect(id);
    setOpen(false);
  };

  // Escape closes (the backdrop below handles outside-click).
  onMount(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });

  return (
    <Show when={props.tries.length > 0}>
      {/* Exactly one try: a static, non-interactive outcome chip — nothing to
          pick between, so no caret, no menu. */}
      <Show when={props.tries.length === 1}>
        <span
          class={`adrop-chip ${tryTone(props.tries[0])}`}
          classList={{ shadow: props.tries[0].shadowed }}
          aria-label="attempt"
        >
          try {props.tries[0].index + 1} · {tryOutcome(props.tries[0])}
          <Show when={props.tries[0].readopted}>
            <Readopt />
          </Show>
        </span>
      </Show>

      {/* Many tries: the compact dropdown. */}
      <Show when={props.tries.length > 1}>
        <span class="adrop">
          <button
            type="button"
            class={`adrop-btn ${tryTone(activeTry())}`}
            classList={{ shadow: activeTry().shadowed, open: open() }}
            aria-haspopup="menu"
            aria-expanded={open()}
            onClick={() => setOpen((v) => !v)}
          >
            <span class="adrop-lbl">
              try {activeTry().index + 1} · {tryOutcome(activeTry())}
            </span>
            <Show when={activeTry().readopted}>
              <Readopt />
            </Show>
            <span class="adrop-caret" aria-hidden="true">
              ▾
            </span>
          </button>

          <Show when={open()}>
            {/* Transparent full-viewport click-catcher, under the menu. */}
            <div class="adrop-backdrop" aria-hidden="true" onClick={() => setOpen(false)} />
            <div class="adrop-menu" role="menu">
              <For each={props.tries}>
                {(t) => {
                  const isActive = () => t.id === props.active;
                  return (
                    <button
                      type="button"
                      role="menuitemradio"
                      aria-checked={isActive()}
                      class={`adrop-row ${tryTone(t)}`}
                      classList={{ shadow: t.shadowed, sel: isActive() }}
                      onClick={() => pick(t.id)}
                    >
                      <span class="adrop-row-lbl">
                        {tryTitle(t)} · {tryOutcome(t)}
                      </span>
                      <Show when={t.readopted}>
                        <Readopt />
                      </Show>
                      <Show when={isActive()}>
                        <span class="adrop-dot" aria-hidden="true" />
                      </Show>
                    </button>
                  );
                }}
              </For>
            </div>
          </Show>
        </span>
      </Show>
    </Show>
  );
}
