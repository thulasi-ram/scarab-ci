// The step header's try control. One try reads as plain text, "Try 1 of 1".
// More than one is a dropdown whose trigger names the selected try, "Try 2 of
// 3 ▾", and whose menu lists every try newest first: its outcome and failure,
// why it exists (first run, automatic retry, retried by someone) and when it
// started, and how long it took. A gate's tries are its waits: "Wait 2 of 3".
// Page chrome, so it reads the page tokens, not the graph canvas ones.
import { For, Show, createSignal, onCleanup, onMount, type JSX } from "solid-js";
import { causeText, type FilmstripTry } from "../attempts";
import { failureWords, statusGlyph, statusWord } from "../run-page";
import { pluralNoun } from "../run-tabs";
import { duration, relTime } from "../fmt";

// One try of the selected step (`FilmstripTry`) is pure, in src/attempts.ts.
// Re-exported so callers' type imports keep working.
export type { FilmstripTry };

/** The re-adoption marker: a crash re-adoption is the same attempt, never a
 * new execution, so it rides inside the row rather than as its own try. */
const Readopt = () => (
  <span class="adrop-readopt" title="re-adopted after a control-plane restart">
    ⟲
  </span>
);

/** "Failed: step error", "Passed", "Running". */
const outcomeWords = (t: FilmstripTry): string =>
  t.outcome === "failed" ? `Failed: ${failureWords(t.failure)}` : statusWord(t.outcome);

/** The tone class a try renders in. */
const tone = (t: FilmstripTry): string =>
  t.outcome === "succeeded"
    ? "emerald"
    : t.outcome === "failed"
      ? "danger"
      : t.outcome === "running"
        ? "running"
        : "copper";

/** A button that opens a menu of tries, newest first. `rows` are the tries the
 * menu lists; `label` is the trigger's content. */
export function TryMenu(props: {
  rows: FilmstripTry[];
  /** The try currently scoping the step pane. */
  active: string | null;
  onSelect: (id: string | null) => void;
  noun: string;
  label: JSX.Element;
  /** The trigger's class. */
  class: string;
  /** Mark the trigger as holding the selection. */
  selected?: boolean;
}) {
  const [open, setOpen] = createSignal(false);
  let menu: HTMLDivElement | undefined;
  let trigger: HTMLButtonElement | undefined;
  // Rows are keyed by try id: the poll rebuilds every try object, and a row
  // keyed by object would be re-created and lose focus.
  const newestFirst = () => [...props.rows].sort((a, b) => b.index - a.index).map((t) => t.id);
  const byId = (id: string) => props.rows.find((t) => t.id === id);

  // Closing from the keyboard or a pick returns focus to the trigger.
  const close = () => {
    setOpen(false);
    trigger?.focus();
  };
  const pick = (id: string) => {
    props.onSelect(id);
    close();
  };

  // Escape closes (the backdrop handles an outside click); the arrow keys move
  // between rows.
  onMount(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && open() && close();
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
    <span class="adrop">
      <button
        type="button"
        class={props.class}
        ref={trigger}
        classList={{ open: open(), on: !!props.selected }}
        aria-haspopup="menu"
        aria-expanded={open()}
        onClick={() => {
          setOpen((v) => !v);
          if (open())
            queueMicrotask(() =>
              (
                menu?.querySelector<HTMLButtonElement>("[aria-checked=true]") ??
                menu?.querySelector<HTMLButtonElement>("[role=menuitemradio]")
              )?.focus(),
            );
        }}
      >
        {props.label}
        <span class="adrop-caret" aria-hidden="true">
          ▾
        </span>
      </button>

      <Show when={open()}>
        <div class="adrop-backdrop" aria-hidden="true" onClick={() => setOpen(false)} />
        <div class="adrop-menu" role="menu" aria-label={`${pluralNoun(props.noun)} of this step`} ref={menu} onKeyDown={onMenuKey}>
          <For each={newestFirst()}>
            {(id) => (
              <Show when={byId(id)}>
                {(t) => {
                  const isActive = () => id === props.active;
                  return (
                    <button
                      type="button"
                      role="menuitemradio"
                      aria-checked={isActive()}
                      class={`adrop-row ${tone(t())}`}
                      classList={{ shadow: t().shadowed, sel: isActive() }}
                      onClick={() => pick(id)}
                    >
                      <span class="adrop-n">
                        {props.noun} {t().index + 1}
                      </span>
                      <span class="adrop-body">
                        <span class="adrop-out">
                          {statusGlyph(t().outcome)} {outcomeWords(t())}
                          <Show when={t().readopted}>
                            {" "}
                            <Readopt />
                          </Show>
                        </span>
                        <small>
                          {[causeText(t().cause), t().startedAt ? relTime(t().startedAt!) : ""]
                            .filter(Boolean)
                            .join(" · ")}
                        </small>
                      </span>
                      <span class="adrop-dur">
                        {t().startedAt && t().finishedAt ? duration(t().startedAt!, t().finishedAt!) : ""}
                      </span>
                    </button>
                  );
                }}
              </Show>
            )}
          </For>
        </div>
      </Show>
    </span>
  );
}

export default function AttemptsDropdown(props: {
  tries: FilmstripTry[];
  /** The try currently scoping the step pane. */
  active: string | null;
  /** Pick a try: scopes the step pane to it. */
  onSelect: (id: string | null) => void;
  /** "Try", or "Wait" for a gate. */
  noun?: string;
  /** Plain text even with several tries ("Try 2 of 3"): the try tabs under
   * the header do the picking. */
  static?: boolean;
}) {
  const noun = () => props.noun ?? "Try";
  const activeTry = (): FilmstripTry =>
    props.tries.find((t) => t.id === props.active) ?? props.tries[props.tries.length - 1];

  return (
    <Show when={props.tries.length > 0}>
      <Show
        when={props.tries.length > 1 && !props.static}
        fallback={
          <span class="trypick static">
            <span class="k">{noun()}</span> {activeTry().index + 1} of {props.tries.length}
          </span>
        }
      >
        <TryMenu
          rows={props.tries}
          active={props.active}
          onSelect={props.onSelect}
          noun={noun()}
          class="trypick"
          label={
            <>
              <span class="k">{noun()}</span> {activeTry().index + 1} of {props.tries.length}
              <Show when={activeTry().readopted}>
                <Readopt />
              </Show>
            </>
          }
        />
      </Show>
    </Show>
  );
}
