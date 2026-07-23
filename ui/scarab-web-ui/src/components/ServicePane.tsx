// The evidence pane for a DAG-selected SHARED service (ADR-0058) — a peer node
// in the services lane, selected as `service:<name>`. Unlike a step it has no
// attempts, results, or workspace: only a lifecycle readout + a best-effort live
// log tail (the "why won't my DB come up" evidence). This lifts the fold-open
// logs the retired ServicesPanel showed into a full evidence pane beside the
// DAG. Page/theme tokens (it sits OUTSIDE the dark DAG canvas), like StepPane;
// it reuses `.steppane` chrome (coord stamp, log body) for a consistent frame.
import { createEffect, createMemo, createSignal, onCleanup, For, Show } from "solid-js";
import { streamServiceLogs } from "../api/client";
import type { DagService } from "./Dag";

// Cap rendered log lines so a chatty service can't blow up the DOM (windowing).
const MAX_LINES = 1500;

export default function ServicePane(props: { runId: string; service: DagService }) {
  const [buffer, setBuffer] = createSignal("");
  let closeStream: (() => void) | undefined;

  // The service NAME as a stable memo: `props.service` is a fresh object each
  // render (derived off the polled run), so keying the stream on the string —
  // which only notifies when it actually changes — stops the 1.2s poll from
  // tearing down + reopening the SSE stream (and clearing the buffer) every tick.
  const name = createMemo(() => props.service.name);

  // (Re)open the log stream when the selected service changes. Same SSE contract
  // as a step's logs: replays committed chunks, then live-tails.
  createEffect(() => {
    const n = name();
    closeStream?.();
    closeStream = undefined;
    setBuffer("");
    closeStream = streamServiceLogs(props.runId, n, {
      onChunk: (t) => setBuffer((prev) => prev + t + "\n"),
    });
  });
  onCleanup(() => closeStream?.());

  const logRows = () => {
    const all = buffer().split("\n").filter((l) => l.length > 0);
    return all.length > MAX_LINES ? all.slice(all.length - MAX_LINES) : all;
  };

  return (
    <div class="panel inspector steppane servicepane">
      {/* Coordinate stamp: name · shared service — parallels StepPane's stamp so
          the evidence never floats context-free. */}
      <div class="coord-stamp mono">
        <span class="cs-step">{props.service.name}</span>
        <span class="cs-dot">·</span>
        <span class="cs-ver">shared service</span>
      </div>

      {/* Readiness readout: the lifecycle dot + status, plus ports when known. */}
      <div class="svc-readout">
        <span class={`sdot ${props.service.status}`} />
        <span class="svc-readout-status">{props.service.status}</span>
        <Show when={props.service.ports?.length}>
          <span class="svc-readout-ports mono">port {props.service.ports!.join(", ")}</span>
        </Show>
        <span class="grow1" />
        <span class="svc-readout-hint">live logs</span>
      </div>

      {/* Best-effort live log tail. */}
      <div class="tabpane logpane">
        <div class="lgbody nowrap">
          <For
            each={logRows()}
            fallback={
              <div class="lgrow empty">
                <span class="lgln" />
                <span class="lgtx">waiting for output…</span>
              </div>
            }
          >
            {(line, i) => (
              <div class="lgrow">
                <span class="lgln">{i() + 1}</span>
                <span class="lgtx">{line}</span>
              </div>
            )}
          </For>
        </div>
      </div>
    </div>
  );
}
