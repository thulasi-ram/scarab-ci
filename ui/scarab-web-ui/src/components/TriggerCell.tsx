// The single shared **Trigger cell** (ADR-0057 §4): trigger kind on top, the
// run **Headline** (`trigger_title` — a push's commit subject, later a PR title
// / dispatch reason) as the truncated secondary line beneath, with a full-text
// tooltip. Reused by BOTH the runs list (RepoView) and the run-detail top
// context bar (RunDetail) so the two can never drift — they used to share
// `trigger.ts` helpers but duplicate the JSX.
//
// `variant` only adapts the surrounding chrome to each host — the kind + headline
// render identically. `cell` matches the run-detail provenance grid (a caps
// label over the value, like its sibling `.pcell`s); `row` is the compact inline
// form for the runs-list secondary facts line. A null/absent `title` renders no
// headline line (graceful degrade for pre-stamping / headline-less runs).
import { Show } from "solid-js";
import { triggerText, triggerIcon } from "../trigger";
import Icon from "./Icon";

export default function TriggerCell(props: {
  kind?: string | null;
  /** The run Headline (`trigger_title`); omitted line when null/empty. */
  title?: string | null;
  variant?: "cell" | "row";
}) {
  const variant = () => props.variant ?? "cell";
  const size = () => (variant() === "cell" ? 13 : 12);
  return (
    <Show when={props.kind}>
      {(kind) => (
        <div class="tcell" classList={{ "tcell-cell": variant() === "cell", "tcell-row": variant() === "row" }}>
          <Show when={variant() === "cell"}>
            <div class="k">trigger</div>
          </Show>
          <div class="tcell-kind">
            <Icon icon={triggerIcon(kind())} size={size()} />
            <span>{triggerText(kind())}</span>
          </div>
          <Show when={props.title}>
            {(t) => (
              <div class="tcell-title" title={t()}>
                {t()}
              </div>
            )}
          </Show>
        </div>
      )}
    </Show>
  );
}
