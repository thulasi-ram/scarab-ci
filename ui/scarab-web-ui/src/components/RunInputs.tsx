// The Run page's Inputs tab: the launch inputs this run was started with, as
// the server stored them, one row per input. A bar above names the pipeline
// and starts a new run filled in with these inputs.
import { For, Show } from "solid-js";
import Icon from "./Icon";

/** An input's value as text: strings as they are, everything else as JSON. */
export function inputText(v: unknown): string {
  return typeof v === "string" ? v : JSON.stringify(v);
}

export default function RunInputs(props: {
  params: Record<string, unknown>;
  pipeline: string | null;
  onNewRun: () => void;
}) {
  const rows = () => Object.entries(props.params).sort(([a], [b]) => a.localeCompare(b));
  return (
    <div class="panel ri">
      <div class="ri-bar">
        <span class="ri-what">
          <Show when={props.pipeline} fallback="Launch inputs">
            {(p) => (
              <>
                Launch inputs of <span class="mono">{p()}</span>
              </>
            )}
          </Show>
        </span>
        <button type="button" class="btn btn-ghost btn-sm" onClick={props.onNewRun}>
          <Icon icon="play" size={13} /> New run with these inputs
        </button>
      </div>
      <Show when={rows().length > 0} fallback={<p class="empty ri-empty">This run was started with no inputs.</p>}>
        <dl class="ri-list">
          <For each={rows()}>
            {([k, v]) => (
              <div class="ri-row">
                <dt class="mono">{k}</dt>
                <dd class="mono" classList={{ str: typeof v === "string" }}>
                  {inputText(v)}
                </dd>
              </div>
            )}
          </For>
        </dl>
      </Show>
    </div>
  );
}
