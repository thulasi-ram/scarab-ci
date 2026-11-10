// The one dropdown in the web UI. A trigger shows the current choice and
// opens a list under it (or above it, when there is no room below). Long
// lists get a search box at the top; short ones are a plain list you move
// through with the arrow keys and type-ahead. `Field`'s `Select` is this
// component inside a form box, and the settings org and section pickers use
// it directly.
//
// The list is PORTALLED to <body> with fixed positioning, so an ancestor's
// `overflow: hidden` never clips it, and it stacks above a Modal. It follows
// its trigger on scroll and resize, and closes on a click outside, Escape
// (which does not reach a Modal around it), Tab, or a pick.
//
// Roles: the trigger is a `combobox` controlling a `listbox` of `option`s.
// Without a search box the focus stays on the trigger and
// `aria-activedescendant` names the highlighted option; with one, the focus
// moves into the search box, which names it instead.
//
// Options can carry a group heading, a rule above them (to set off an entry
// such as "New organization…"), an icon, and `disabled`: a disabled option
// shows but cannot be picked, and the arrow keys skip it.
import { For, Show, createEffect, createMemo, createSignal, createUniqueId, onCleanup } from "solid-js";
import { Portal } from "solid-js/web";
import Icon from "./Icon";
import {
  filterRows,
  firstEnabled,
  lastEnabled,
  openIndex,
  showsSearch,
  stepIndex,
  typeAheadIndex,
} from "../dropdown";

export type SelectOption = {
  value: string;
  /** Display text; also what search and type-ahead match against. */
  label: string;
  /** Optional small leading token, e.g. "branch" / "tag". */
  tag?: string;
  /** Optional right-aligned mono hint, e.g. a short SHA. */
  hint?: string;
  /** Shown, not pickable; the arrow keys skip it. */
  disabled?: boolean;
  /** The heading of the group this option sits in. Consecutive options with
   *  the same group sit under one heading. */
  group?: string;
  /** A rule above this option, setting it off from the ones before. */
  separator?: boolean;
  /** A leading icon, e.g. `plus` on an entry that makes something. */
  icon?: string;
};

type Row = SelectOption & { clear?: boolean };

/** How far the list keeps from the window's edges. */
const EDGE = 8;
/** The list's height when there is room. */
const LIST_MAX = 256;

export default function SearchSelect(props: {
  value: string;
  options: SelectOption[];
  onChange: (value: string) => void;
  /** Trigger label when nothing is selected; also the label of the clear row. */
  placeholder?: string;
  searchPlaceholder?: string;
  /** Show the search box. By default it shows above eight options. */
  search?: boolean;
  /** Leading icon on the trigger. */
  icon?: string;
  /** Offer a top row that resets the value to "" (e.g. "any author"). */
  clearable?: boolean;
  disabled?: boolean;
  /** Show the label and the options in Mono (machine values). */
  mono?: boolean;
  /** Extra class on the trigger: `ss-block` for a full-width control,
   *  `ss-compact` for a sidebar. */
  class?: string;
  id?: string;
  "aria-label"?: string;
  "aria-describedby"?: string;
  "aria-invalid"?: boolean;
}) {
  const uid = createUniqueId();
  const listId = `ss-list-${uid}`;
  const optId = (i: number) => `ss-opt-${uid}-${i}`;

  const [open, setOpen] = createSignal(false);
  const [query, setQuery] = createSignal("");
  const [hi, setHi] = createSignal(-1);
  const [pos, setPos] = createSignal<{
    left: number;
    width: number;
    top?: number;
    bottom?: number;
    maxH: number;
  } | null>(null);

  let trigger!: HTMLButtonElement;
  let searchInput: HTMLInputElement | undefined;
  let popEl: HTMLDivElement | undefined;

  const selected = () => props.options.find((o) => o.value === props.value);
  const triggerLabel = () => selected()?.label ?? props.placeholder ?? "";
  const searching = () => showsSearch(props.options.length, props.search);

  // Navigable rows: an optional clear row, then the filtered options.
  const rows = createMemo((): Row[] => {
    const opts = searching() ? filterRows(props.options, query()) : props.options;
    return props.clearable ? [{ value: "", label: props.placeholder ?? "any", clear: true }, ...opts] : opts;
  });
  const isSelected = (o: Row) => (o.clear ? props.value === "" : o.value === props.value);
  const active = () => (open() && hi() >= 0 && hi() < rows().length ? optId(hi()) : undefined);

  const place = () => {
    const r = trigger.getBoundingClientRect();
    const below = window.innerHeight - r.bottom - EDGE;
    const above = r.top - EDGE;
    const width = Math.max(r.width, 200);
    const left = Math.max(EDGE, Math.min(r.left, window.innerWidth - EDGE - width));
    // Open upward only when the list does not fit below and there is more room above.
    if (below < Math.min(LIST_MAX, 160) && above > below) {
      setPos({ left, width: r.width, bottom: window.innerHeight - r.top + 4, maxH: above - 4 });
    } else {
      setPos({ left, width: r.width, top: r.bottom + 4, maxH: below - 4 });
    }
  };

  const openMenu = (seed?: string) => {
    if (props.disabled || open()) return;
    place();
    setQuery(seed && searching() ? seed : "");
    const rs = rows();
    let start = openIndex(rs, rs.findIndex(isSelected));
    if (seed && !searching()) {
      const m = typeAheadIndex(rs, seed, start);
      if (m >= 0) start = m;
    } else if (seed) {
      start = firstEnabled(rows());
    }
    setHi(start);
    setOpen(true);
  };
  /** Close; `refocus` puts the focus back on the trigger. */
  const close = (refocus = false) => {
    if (!open()) return;
    setOpen(false);
    typed = "";
    if (refocus) trigger.focus();
  };

  const pick = (o: Row | undefined) => {
    if (!o || o.disabled) return;
    close(true);
    if (o.value !== props.value || o.clear) props.onChange(o.value);
  };

  // Type-ahead buffer, reset after a pause as a native select does.
  let typed = "";
  let typedAt = 0;
  const typeAhead = (ch: string) => {
    const now = Date.now();
    typed = now - typedAt > 700 ? ch : typed + ch;
    typedAt = now;
    const i = typeAheadIndex(rows(), typed, hi());
    if (i >= 0) setHi(i);
  };
  const printable = (e: KeyboardEvent) => e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey;

  /** Keys while the list is open, from the trigger or the search box. */
  const onOpenKey = (e: KeyboardEvent, inSearch: boolean) => {
    const rs = rows();
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        setHi((h) => stepIndex(rs, h, 1));
        return;
      case "ArrowUp":
        e.preventDefault();
        setHi((h) => stepIndex(rs, h, -1));
        return;
      case "Home":
      case "End":
        if (inSearch) return; // the search box keeps its caret keys
        e.preventDefault();
        setHi(e.key === "Home" ? firstEnabled(rs) : lastEnabled(rs));
        return;
      case "PageDown":
      case "PageUp":
        e.preventDefault();
        setHi(e.key === "PageDown" ? lastEnabled(rs) : firstEnabled(rs));
        return;
      case "Enter":
        e.preventDefault();
        pick(rs[hi()]);
        return;
      case "Escape":
        // A Modal around the dropdown closes on Escape too; this one is ours.
        e.preventDefault();
        e.stopPropagation();
        close(true);
        return;
      case "Tab":
        // Back on the trigger, the browser's Tab moves on from there.
        close(true);
        return;
      case " ":
        if (inSearch) return;
        e.preventDefault();
        if (typed && Date.now() - typedAt <= 700) typeAhead(" ");
        else pick(rs[hi()]);
        return;
    }
    if (!inSearch && printable(e)) {
      e.preventDefault();
      typeAhead(e.key);
    }
  };

  const onTriggerKey = (e: KeyboardEvent) => {
    if (open()) {
      if (!searching()) onOpenKey(e, false);
      return;
    }
    if (["ArrowDown", "ArrowUp", "Enter", " "].includes(e.key)) {
      e.preventDefault();
      openMenu();
    } else if (printable(e)) {
      e.preventDefault();
      typed = e.key;
      typedAt = Date.now();
      openMenu(e.key);
    }
  };

  // Focus the search box as the list opens, except on a touch screen, where
  // that would raise the on-screen keyboard over the list: there a tap on the
  // box starts a search.
  createEffect(() => {
    if (open() && searching() && !window.matchMedia?.("(pointer: coarse)").matches) searchInput?.focus();
  });

  // Keep the highlighted option in view.
  createEffect(() => {
    const i = hi();
    if (!open() || i < 0) return;
    document.getElementById(optId(i))?.scrollIntoView({ block: "nearest" });
  });

  // While open: close on a press outside, follow the trigger on scroll/resize.
  createEffect(() => {
    if (!open()) return;
    const onDown = (e: PointerEvent) => {
      const t = e.target as Node;
      if (trigger.contains(t) || popEl?.contains(t)) return;
      close();
    };
    const onShift = (e: Event) => {
      // The list scrolling itself moves nothing.
      if (popEl && e.target instanceof Node && popEl.contains(e.target)) return;
      const r = trigger.getBoundingClientRect();
      if (r.bottom < 0 || r.top > window.innerHeight) close();
      else place();
    };
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
        id={props.id}
        type="button"
        role="combobox"
        aria-haspopup="listbox"
        aria-expanded={open()}
        aria-controls={open() ? listId : undefined}
        aria-activedescendant={searching() ? undefined : active()}
        aria-label={props["aria-label"]}
        aria-describedby={props["aria-describedby"]}
        aria-invalid={props["aria-invalid"] || undefined}
        data-value={props.value}
        class={`ss-trigger${props.mono ? " mono" : ""} ${props.class ?? ""}`}
        disabled={props.disabled}
        onClick={() => (open() ? close() : openMenu())}
        // Native listeners, not Solid's delegated ones (which run at the
        // document): Escape must stop here, before a Modal's document
        // listener closes the dialog.
        on:keydown={onTriggerKey}
        // Space opens or picks on keydown; its keyup must not click the
        // button again.
        on:keyup={(e) => e.key === " " && e.preventDefault()}
      >
        <Show when={props.icon}>{(ic) => <Icon icon={ic()} size={12} />}</Show>
        <span class={`ss-label ${selected() ? "" : "ss-placeholder"}`}>{triggerLabel()}</span>
        <Icon icon="chevron-down" size={13} class="ss-caret" />
      </button>

      <Show when={open() && pos()}>
        {(p) => (
          <Portal>
            <div
              ref={popEl}
              class={`ss-pop${props.mono ? " mono" : ""}`}
              style={{
                top: p().top !== undefined ? `${p().top}px` : undefined,
                bottom: p().bottom !== undefined ? `${p().bottom}px` : undefined,
                left: `${p().left}px`,
                "min-width": `${p().width}px`,
              }}
            >
              <Show when={searching()}>
                <div class="ss-search">
                  <Icon icon="search" size={13} />
                  <input
                    ref={searchInput}
                    value={query()}
                    placeholder={props.searchPlaceholder ?? "Search…"}
                    aria-label={props.searchPlaceholder ?? "Search"}
                    aria-controls={listId}
                    aria-activedescendant={active()}
                    autocomplete="off"
                    spellcheck={false}
                    onInput={(e) => {
                      setQuery(e.currentTarget.value);
                      setHi(firstEnabled(rows()));
                    }}
                    on:keydown={(e) => onOpenKey(e, true)}
                  />
                </div>
              </Show>
              <ul
                id={listId}
                class="ss-list"
                role="listbox"
                aria-label={props["aria-label"]}
                style={{ "max-height": `${Math.max(Math.min(LIST_MAX, p().maxH - (searching() ? 48 : 12)), 96)}px` }}
              >
                <For each={rows()} fallback={<li class="ss-empty">no matches</li>}>
                  {(o, i) => (
                    <>
                      <Show when={o.separator && i() > 0}>
                        <li class="ss-sep" role="separator" />
                      </Show>
                      <Show when={o.group && o.group !== rows()[i() - 1]?.group}>
                        <li class="ss-group" role="presentation">
                          {o.group}
                        </li>
                      </Show>
                      <li
                        id={optId(i())}
                        role="option"
                        aria-selected={isSelected(o)}
                        aria-disabled={o.disabled || undefined}
                        class={`ss-opt ${i() === hi() ? "on" : ""} ${!o.clear && isSelected(o) ? "sel" : ""} ${
                          o.clear ? "ss-clear" : ""
                        } ${o.disabled ? "ss-dis" : ""}`}
                        onMouseEnter={() => !o.disabled && setHi(i())}
                        onMouseDown={(e) => {
                          // Keep the focus where it is until the pick moves it.
                          e.preventDefault();
                          pick(o);
                        }}
                      >
                        <Show when={o.icon}>{(ic) => <Icon icon={ic()} size={13} class="ss-opt-ic" />}</Show>
                        <Show when={o.tag}>{(t) => <span class={`ss-tag ${o.tag}`}>{t()}</span>}</Show>
                        <span class="ss-opt-label">{o.label}</span>
                        <Show when={o.hint}>{(h) => <span class="mono ss-hint">{h()}</span>}</Show>
                        <Show when={!o.clear && isSelected(o)}>
                          <span class="ss-dot" />
                        </Show>
                      </li>
                    </>
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
