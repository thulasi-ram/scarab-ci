// The single shared **Trigger cell** (ADR-0057): the trigger kind as a compact
// chip, then the run **Headline** (`trigger_title` — a push's commit subject, a
// PR title, a dispatch reason) beside it on one line, truncated with a full-text
// tooltip. The A·2 primary line, reused by BOTH the runs list (RepoView) and the
// run-detail context bar (RunDetail) so the two can never drift. A null/absent
// `title` renders no headline (graceful degrade for pre-stamping / headline-less
// runs). Per-host sizing is applied by the host (e.g. the run-detail bar scales
// it up via `.prov` overrides) — the markup is identical.
import { Show } from "solid-js";
import { triggerText, triggerIcon } from "../trigger";
import Icon from "./Icon";

export default function TriggerCell(props: {
  kind?: string | null;
  /** The run Headline (`trigger_title`); omitted when null/empty. */
  title?: string | null;
}) {
  return (
    <Show when={props.kind}>
      {(kind) => (
        <div class="tcell tcell-row">
          <div class="tcell-kind">
            <Icon icon={triggerIcon(kind())} size={12} />
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
