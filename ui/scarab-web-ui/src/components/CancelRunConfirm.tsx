// The confirm popover for Cancel run: the first click opens it, and the
// cancel request is sent only from its confirm button, the same two-step
// pattern Rerun and Retry use (RerunConfirm). It shares that popover's look
// and its dismissal rules: Escape, a click outside, a scroll or a resize
// closes it without cancelling.
import { createEffect, onCleanup } from "solid-js";
import { Portal } from "solid-js/web";

export default function CancelRunConfirm(props: {
  /** Viewport-fixed anchor (the trigger button's rect bottom edge). */
  anchor: { top: number; left: number };
  busy: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  let popEl: HTMLDivElement | undefined;

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
        aria-label="confirm cancel run"
        style={{ top: `${props.anchor.top}px`, left: `${left()}px` }}
      >
        <p class="rc-headline">Cancel this run?</p>
        <p class="rc-sentence">
          Running steps are stopped and their Pods torn down. Steps that have not started will not
          run.
        </p>
        <div class="rc-actions">
          <button type="button" class="btn btn-ghost btn-sm" onClick={() => props.onCancel()}>
            Keep running
          </button>
          <button
            type="button"
            class="btn btn-danger btn-sm"
            disabled={props.busy}
            onClick={() => props.onConfirm()}
          >
            {props.busy ? "cancelling…" : "Cancel run"}
          </button>
        </div>
      </div>
    </Portal>
  );
}
