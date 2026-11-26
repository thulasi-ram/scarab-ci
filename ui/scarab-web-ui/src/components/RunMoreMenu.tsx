// The run header's ⋯ menu: New run with these inputs (the run form for this
// run's pipeline, filled in with this run's inputs) and Keep workspace
// snapshots (pin or unpin this run's snapshots past the retention window). The
// run ID is copied from the crumb line above the title. Keyboard reachable:
// Enter or Space opens it on its first enabled item, the arrow keys move
// between enabled items, Escape closes and returns focus to the button.
import { createSignal, onCleanup, onMount, Show } from "solid-js";
import type { RetentionInfo } from "../api/client";
import { pinLabel, pinTitle } from "../snapshot-retention";
import Icon from "./Icon";

export default function RunMoreMenu(props: {
  /** Opens the run form filled in with this run's inputs. */
  onNewRun: () => void;
  /** The retention promise; null hides the pin item. */
  retention: RetentionInfo | null;
  pinning: boolean;
  onTogglePin: () => void;
}) {
  const [open, setOpen] = createSignal(false);
  let button: HTMLButtonElement | undefined;
  let menu: HTMLDivElement | undefined;

  const items = () =>
    Array.from(menu?.querySelectorAll<HTMLButtonElement>("[role=menuitem]") ?? []).filter((b) => !b.disabled);
  const openMenu = () => {
    setOpen(true);
    queueMicrotask(() => items()[0]?.focus());
  };
  const close = (refocus = true) => {
    setOpen(false);
    if (refocus) button?.focus();
  };

  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && open()) close();
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => {
      document.removeEventListener("keydown", onKey);
    });
  });

  const onMenuKey = (e: KeyboardEvent) => {
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
    e.preventDefault();
    const rows = items();
    const i = rows.indexOf(document.activeElement as HTMLButtonElement);
    const next = e.key === "ArrowDown" ? (i + 1) % rows.length : (i - 1 + rows.length) % rows.length;
    rows[next]?.focus();
  };

  return (
    <span class="rmenu">
      <button
        type="button"
        class="btn btn-ghost btn-sm rmenu-btn"
        ref={button}
        aria-haspopup="menu"
        aria-expanded={open()}
        aria-label="More run actions"
        title="More run actions"
        onClick={() => (open() ? close(false) : openMenu())}
      >
        ⋯
      </button>
      <Show when={open()}>
        <div class="vdrop-backdrop" aria-hidden="true" onClick={() => close(false)} />
        <div class="rmenu-pop" role="menu" aria-label="More run actions" ref={menu} onKeyDown={onMenuKey}>
          <button
            type="button"
            role="menuitem"
            class="rmenu-item"
            onClick={() => {
              // Focus goes back to ⋯ first, so the dialog returns it there.
              close();
              props.onNewRun();
            }}
          >
            <Icon icon="play" size={14} class="rmenu-ico" />
            <span class="rmenu-lbl">New run with these inputs</span>
            <small>Open the run form filled in with this run's inputs.</small>
          </button>
          <Show when={props.retention}>
            {(r) => (
              <button
                type="button"
                role="menuitem"
                class="rmenu-item"
                disabled={props.pinning}
                title={pinTitle(r())}
                onClick={() => {
                  props.onTogglePin();
                  close();
                }}
              >
                <Icon icon={r().pinned ? "pin-off" : "pin"} size={14} class="rmenu-ico" />
                <span class="rmenu-lbl">{props.pinning ? "…" : pinLabel(r())}</span>
                <small>
                  {r().pinned
                    ? "Snapshots are kept. Return them to the retention window."
                    : "Keep this run's workspaces past the retention window."}
                </small>
              </button>
            )}
          </Show>
        </div>
      </Show>
    </span>
  );
}
