// The run header's ⋯ menu: Keep workspace snapshots (pin or unpin this run's
// snapshots past the retention window) and Copy run ID. Keyboard reachable:
// Enter or Space opens it on its first enabled item, the arrow keys move
// between enabled items, Escape closes and returns focus to the button.
import { createSignal, onCleanup, onMount, Show } from "solid-js";
import type { RetentionInfo } from "../api/client";
import { pinLabel, pinTitle } from "../snapshot-retention";

export default function RunMoreMenu(props: {
  runId: string;
  /** The retention promise; null hides the pin item. */
  retention: RetentionInfo | null;
  pinning: boolean;
  onTogglePin: () => void;
}) {
  const [open, setOpen] = createSignal(false);
  const [copied, setCopied] = createSignal(false);
  let button: HTMLButtonElement | undefined;
  let menu: HTMLDivElement | undefined;
  let copiedTimer: ReturnType<typeof setTimeout> | undefined;

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
      clearTimeout(copiedTimer);
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

  async function copyId() {
    try {
      await navigator.clipboard?.writeText(props.runId);
      setCopied(true);
      clearTimeout(copiedTimer);
      copiedTimer = setTimeout(() => setCopied(false), 2000);
    } catch {
      /* the clipboard refused; the id is still shown in the item */
    }
  }

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
                <span class="rmenu-lbl">{props.pinning ? "…" : pinLabel(r())}</span>
                <small>
                  {r().pinned
                    ? "Snapshots are kept. Return them to the retention window."
                    : "Keep this run's workspaces past the retention window."}
                </small>
              </button>
            )}
          </Show>
          <button type="button" role="menuitem" class="rmenu-item" onClick={() => void copyId()}>
            <span class="rmenu-lbl">{copied() ? "Copied" : "Copy run ID"}</span>
            <small class="mono">{props.runId}</small>
          </button>
          <span class="sr-only" role="status">
            {copied() ? "Run ID copied" : ""}
          </span>
        </div>
      </Show>
    </span>
  );
}
