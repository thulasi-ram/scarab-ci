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
import { tryTitle, tryOutcome, tryTone, type FilmstripTry } from "../attempts";

// One try of the SELECTED step (`FilmstripTry`) and its cause/outcome/tone copy
// (`causeSuffix`/`tryTitle`/`tryOutcome`/`tryTone`) are pure — extracted to
// src/attempts.ts. Re-exported so existing callers' type imports keep working.
export type { FilmstripTry };

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
          <span class="adrop-emblem" aria-hidden="true">
            ⟳
          </span>
          <span class="adrop-lbl">
            try {props.tries[0].index + 1} · {tryOutcome(props.tries[0])}
          </span>
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
            <span class="adrop-emblem" aria-hidden="true">
              ⟳
            </span>
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
