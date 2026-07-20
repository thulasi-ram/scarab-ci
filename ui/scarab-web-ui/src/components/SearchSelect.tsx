// A Select2-style searchable single-select: a trigger showing the current
// selection, opening a popover with a search box + live-filtered option list.
// The popover is PORTALLED to <body> with fixed positioning, so it is never
// clipped by an ancestor's `overflow: hidden` (the trap plain absolute dropdowns
// keep hitting). Keyboard-navigable (↑/↓/Enter/Esc); closes on outside click or
// on scroll/resize (a fixed popover would otherwise detach from its trigger).
import { For, Show, createSignal, createEffect, onCleanup } from "solid-js";
import { Portal } from "solid-js/web";
import Icon from "./Icon";

export type SelectOption = {
  value: string;
  /** Display text; also what the search query matches against. */
  label: string;
  /** Optional small leading token, e.g. "branch" / "tag". */
  tag?: string;
  /** Optional right-aligned mono hint, e.g. a short SHA. */
  hint?: string;
};

type Row = SelectOption & { clear?: boolean };

export default function SearchSelect(props: {
  value: string;
  options: SelectOption[];
  onChange: (value: string) => void;
  /** Trigger label when nothing is selected; also the label of the clear row. */
  placeholder?: string;
  searchPlaceholder?: string;
  /** Leading icon on the trigger. */
  icon?: string;
  /** Offer a top row that resets the value to "" (e.g. "any author"). */
  clearable?: boolean;
  disabled?: boolean;
  /** Extra class on the trigger — e.g. `ss-block` for a full-width form field. */
  class?: string;
}) {
  const [open, setOpen] = createSignal(false);
  const [query, setQuery] = createSignal("");
  const [hi, setHi] = createSignal(0);
  const [rect, setRect] = createSignal<{ top: number; left: number; width: number } | null>(null);

  let trigger!: HTMLButtonElement;
  let searchInput: HTMLInputElement | undefined;
  let popEl: HTMLDivElement | undefined;

  const selected = () => props.options.find((o) => o.value === props.value);
  const triggerLabel = () => selected()?.label ?? props.placeholder ?? "";

  const filtered = () => {
    const q = query().trim().toLowerCase();
    return q ? props.options.filter((o) => o.label.toLowerCase().includes(q)) : props.options;
  };

  // Navigable rows: an optional clear row, then the filtered options.
  const rows = (): Row[] => {
    const opts = filtered();
    return props.clearable
      ? [{ value: "", label: props.placeholder ?? "any", clear: true }, ...opts]
      : opts;
  };

  const openMenu = () => {
    if (props.disabled) return;
    const r = trigger.getBoundingClientRect();
    setRect({ top: r.bottom + 4, left: r.left, width: r.width });
    setQuery("");
    setHi(0);
    setOpen(true);
  };
  const close = () => setOpen(false);

  const pick = (value: string) => {
    props.onChange(value);
    close();
  };

  const onKey = (e: KeyboardEvent) => {
    const rs = rows();
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setHi((h) => Math.min(h + 1, rs.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setHi((h) => Math.max(h - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (rs.length) pick(rs[Math.min(hi(), rs.length - 1)].value);
    } else if (e.key === "Escape") {
      e.preventDefault();
      close();
    }
  };

  // Focus the search box as the menu opens.
  createEffect(() => {
    if (open()) searchInput?.focus();
  });

  // Dismiss on outside pointerdown / scroll / resize while open.
  createEffect(() => {
    if (!open()) return;
    const onDown = (e: PointerEvent) => {
      const t = e.target as Node;
      if (trigger.contains(t) || popEl?.contains(t)) return;
      close();
    };
    const onShift = () => close();
    document.addEventListener("pointerdown", onDown, true);
    window.addEventListener("scroll", onShift, true);
    window.addEventListener("resize", onShift);
    onCleanup(() => {
      document.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("scroll", onShift, true);
      window.removeEventListener("resize", onShift);
    });
  });

  return (
    <>
      <button
        ref={trigger}
        type="button"
        class={`ss-trigger ${props.class ?? ""}`}
        disabled={props.disabled}
        onClick={() => (open() ? close() : openMenu())}
      >
        <Show when={props.icon}>{(ic) => <Icon icon={ic()} size={12} />}</Show>
        <span class={`ss-label ${selected() ? "" : "ss-placeholder"}`}>{triggerLabel()}</span>
        <Icon icon="chevron-down" size={13} class="ss-caret" />
      </button>

      <Show when={open() && rect()}>
        {(r) => (
          <Portal>
            <div
              ref={popEl}
              class="ss-pop"
              style={{
                top: `${r().top}px`,
                left: `${r().left}px`,
                "min-width": `${r().width}px`,
              }}
            >
              <div class="ss-search">
                <Icon icon="search" size={13} />
                <input
                  ref={searchInput}
                  value={query()}
                  placeholder={props.searchPlaceholder ?? "Search…"}
                  autocomplete="off"
                  onInput={(e) => {
                    setQuery(e.currentTarget.value);
                    setHi(0);
                  }}
                  onKeyDown={onKey}
                />
              </div>
              <ul class="ss-list">
                <For each={rows()} fallback={<li class="ss-empty">no matches</li>}>
                  {(o, i) => (
                    <li
                      class={`ss-opt ${i() === hi() ? "on" : ""} ${
                        !o.clear && o.value === props.value ? "sel" : ""
                      } ${o.clear ? "ss-clear" : ""}`}
                      onMouseEnter={() => setHi(i())}
                      onMouseDown={(e) => {
                        e.preventDefault();
                        pick(o.value);
                      }}
                    >
                      <Show when={o.tag}>{(t) => <span class={`ss-tag ${o.tag}`}>{t()}</span>}</Show>
                      <span class="ss-opt-label">{o.label}</span>
                      <Show when={o.hint}>{(h) => <span class="mono ss-hint">{h()}</span>}</Show>
                      <Show when={!o.clear && o.value === props.value}>
                        <span class="ss-dot" />
                      </Show>
                    </li>
                  )}
                </For>
              </ul>
            </div>
          </Portal>
        )}
      </Show>
    </>
  );
}
