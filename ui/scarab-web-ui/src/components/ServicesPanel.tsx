// Services panel (ADR-0058): a run's shared services rendered BESIDE the DAG —
// never as nodes inside it. A shared service is infrastructure for the steps
// that `uses:` it, not a `needs`-able step, so it has no rerun action and no
// place in the graph; this compact section lists each instance with its
// lifecycle status and a fold to view its best-effort log tail (the "why won't
// my DB come up" evidence). Placement is minimal and on-pattern — a later human
// design pass owns the polish.
import { createSignal, createEffect, createResource, onCleanup, For, Show } from "solid-js";
import { listServices, streamServiceLogs, type Service } from "../api/client";

// Cap rendered log lines so a chatty service can't blow up the DOM.
const MAX_LINES = 800;

export default function ServicesPanel(props: { runId: string; live: boolean }) {
  // Poll alongside the run while it is live; a terminal run's set is fixed.
  const [tick, setTick] = createSignal(0);
  createEffect(() => {
    if (!props.live) return;
    const t = setInterval(() => setTick((n) => n + 1), 2000);
    onCleanup(() => clearInterval(t));
  });
  const [services] = createResource(
    () => [props.runId, tick()] as const,
    ([id]) => listServices(id).catch(() => [] as Service[]),
  );

  // The service whose logs are open (by name), or null.
  const [open, setOpen] = createSignal<string | null>(null);
  const [buffer, setBuffer] = createSignal("");
  let closeStream: (() => void) | undefined;

  // (Re)open the log stream when the selected service changes.
  createEffect(() => {
    const name = open();
    closeStream?.();
    closeStream = undefined;
    setBuffer("");
    if (!name) return;
    closeStream = streamServiceLogs(props.runId, name, {
      onChunk: (t) => setBuffer((prev) => prev + t + "\n"),
    });
  });
  onCleanup(() => closeStream?.());

  const logRows = () => {
    const all = buffer().split("\n").filter((l) => l.length > 0);
    return all.length > MAX_LINES ? all.slice(all.length - MAX_LINES) : all;
  };

  const toggle = (name: string) => setOpen((cur) => (cur === name ? null : name));

  return (
    <Show when={(services()?.length ?? 0) > 0}>
      <div class="svc-wrap">
        <div class="dag-head" title="run-scoped shared services — reachable by steps that use them">
          Services
        </div>
        <ul class="svc-list">
          <For each={services()}>
            {(s) => (
              <li class="svc-item">
                <button
                  class="svc-row"
                  classList={{ open: open() === s.name }}
                  onClick={() => toggle(s.name)}
                  title={`${s.name} — ${s.status}; click to view logs`}
                >
                  <span class={`sdot ${s.status}`} />
                  <span class="svc-name mono">{s.name}</span>
                  <span class="svc-status">{s.status}</span>
                </button>
                <Show when={open() === s.name}>
                  <div class="svc-logs lgbody nowrap">
                    <For
                      each={logRows()}
                      fallback={
                        <div class="lgrow empty">
                          <span class="lgtx">waiting for output…</span>
                        </div>
                      }
                    >
                      {(line) => (
                        <div class="lgrow">
                          <span class="lgtx">{line}</span>
                        </div>
                      )}
                    </For>
                  </div>
                </Show>
              </li>
            )}
          </For>
        </ul>
      </div>
    </Show>
  );
}
