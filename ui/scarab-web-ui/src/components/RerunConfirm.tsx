// The rerun/retry CONFIRM popover (git-bug 4afaa3e): the first click opens
// this; the POST fires only from its confirm button, so the whole cascade is
// named BEFORE anything re-arms (ADR-0027 — smart never means mysterious).
//
// Portalled to <body> with fixed positioning, the same machinery as
// SearchSelect/VersionDropdown (amendment F5: never clipped by an ancestor's
// overflow). All copy comes from the pure derivations in ../rerun-confirm —
// the no-DOM vitest tier covers those; this component only lays them out.
//
// A missing plan (the preview fetch failed) still allows confirm: unknown is
// not "expired", and disclosure must never become prevention.
import { For, Show, createEffect, onCleanup } from "solid-js";
import { Portal } from "solid-js/web";
import Icon from "./Icon";
import {
  confirmFootnote,
  confirmHeadline,
  confirmLabel,
  confirmSentence,
  confirmWarning,
  gateNote,
  planGroups,
  useExpandedList,
  type ConfirmKind,
} from "../rerun-confirm";
import { type RerunPlan } from "../snapshot-retention";

export default function RerunConfirm(props: {
  kind: ConfirmKind;
  target: string;
  /** The previewed plan FOR this target (the caller must never hand another
   * step's plan in), or null when the preview failed or is still loading. */
  plan: RerunPlan | null;
  /** True while the preview is still in flight — renders a loading line
   * instead of claiming the preview is unavailable. */
  loading?: boolean;
  /** Viewport-fixed anchor (the trigger button's rect bottom edge). */
  anchor: { top: number; left: number };
  busy: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  let popEl: HTMLDivElement | undefined;

  // Escape cancels; outside pointerdown cancels; scroll/resize would detach a
  // fixed popover from its trigger, so they cancel too (SearchSelect's rule).
  createEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && props.onCancel();
    const onDown = (e: PointerEvent) => {
      if (popEl?.contains(e.target as Node)) return;
      props.onCancel();
    };
    const onShift = () => props.onCancel();
    document.addEventListener("keydown", onKey);
    document.addEventListener("pointerdown", onDown, true);
    window.addEventListener("scroll", onShift, true);
    window.addEventListener("resize", onShift);
    onCleanup(() => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("scroll", onShift, true);
      window.removeEventListener("resize", onShift);
    });
  });

  // Keep the popover on-screen: clamp against the right edge.
  const left = () => Math.max(8, Math.min(props.anchor.left, window.innerWidth - 380));

  return (
    <Portal>
      <div
        ref={popEl}
        class="rc-pop"
        role="dialog"
        aria-label={`confirm ${props.kind}`}
        style={{ top: `${props.anchor.top}px`, left: `${left()}px` }}
      >
        <p class="rc-headline">{confirmHeadline(props.kind, props.target)}</p>
        <Show
          when={props.plan}
          fallback={
            <p class="rc-unavailable">
              {props.loading ? "Resolving the scope…" : "Scope preview unavailable."}
            </p>
          }
        >
          {(p) => (
            <>
              <Show when={confirmWarning(p())}>
                {(w) => (
                  <p class="rc-warning">
                    <Icon icon="alert-triangle" size={12} /> {w()}
                  </p>
                )}
              </Show>
              <Show
                when={useExpandedList(p())}
                fallback={<p class="rc-sentence">{confirmSentence(p(), props.kind)}</p>}
              >
                <For each={planGroups(p(), props.kind)}>
                  {(g) => (
                    <div class="rc-group">
                      <p class="rc-group-title">{g.title}</p>
                      <For each={g.rows}>
                        {(row) => (
                          <div class="rc-row">
                            <span class="mono rc-step">{row.step}</span>
                            <span class="rc-note">{row.note}</span>
                          </div>
                        )}
                      </For>
                    </div>
                  )}
                </For>
              </Show>
              <Show when={gateNote(p())}>
                {(g) => (
                  <p class="rc-gate">
                    <Icon icon="shield-check" size={12} /> {g()}
                  </p>
                )}
              </Show>
              <p class="rc-foot">{confirmFootnote(p(), props.kind)}</p>
            </>
          )}
        </Show>
        <div class="rc-actions">
          <button type="button" class="btn btn-ghost btn-sm" onClick={() => props.onCancel()}>
            Cancel
          </button>
          <button
            type="button"
            class="btn btn-primary btn-sm"
            disabled={props.busy}
            onClick={() => props.onConfirm()}
          >
            {props.busy ? "…" : confirmLabel(props.plan, props.kind)}
          </button>
        </div>
      </div>
    </Portal>
  );
}
