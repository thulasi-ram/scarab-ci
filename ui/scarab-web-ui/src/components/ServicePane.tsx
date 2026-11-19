// The evidence pane for a DAG-selected SHARED service — a peer node
// in the services lane, selected as `service:<name>`. Unlike a step it has no
// attempts, results, or workspace: only a lifecycle readout + a best-effort live
// log tail (the "why won't my DB come up" evidence). This lifts the fold-open
// logs the retired ServicesPanel showed into a full evidence pane beside the
// DAG. Page/theme tokens (it sits OUTSIDE the dark DAG canvas), like StepPane;
// it reuses `.steppane` chrome (coord stamp, log body) for a consistent frame.
import { createEffect, createMemo, createSignal, onCleanup, Index, Show, untrack } from "solid-js";
import { probeLogStream, serviceLogsRawUrl, serviceLogStreamUrl, streamServiceLogs } from "../api/client";
import type { DagService } from "./Dag";
import {
  appendLog,
  DEFAULT_TAIL_BYTES,
  emptyLogBuffer,
  followLog,
  FULL_LOG_BYTES,
  logNotice,
  tailRows,
  truncateLog,
  type LogFollower,
} from "../log-stream";

// Cap rendered log lines so a chatty service can't blow up the DOM (windowing).
const MAX_LINES = 1500;

// Lifecycle states in which a service may still print.
const LIVE_STATUSES = new Set(["starting", "ready", "running"]);

export default function ServicePane(props: { runId: string; service: DagService }) {
  const [buffer, setBuffer] = createSignal(emptyLogBuffer());
  // The service whose log the user asked to read in full (Load full log).
  const [fullFor, setFullFor] = createSignal<string | null>(null);
  let follower: LogFollower | undefined;

  // The service NAME as a stable memo: `props.service` is a fresh object each
  // render (derived off the polled run), so keying the stream on the string —
  // which only notifies when it actually changes — stops the 1.2s poll from
  // tearing down + reopening the SSE stream (and clearing the buffer) every tick.
  const name = createMemo(() => props.service.name);

  // (Re)open the log stream when the selected service changes, or when the
  // user asks for the full log. Same SSE contract as a step's logs: replays
  // the last DEFAULT_TAIL_BYTES (or FULL_LOG_BYTES), then live-tails; a
  // dropped stream resumes after the last event while the service runs.
  createEffect(() => {
    const n = name();
    const tail = fullFor() === n ? FULL_LOG_BYTES : DEFAULT_TAIL_BYTES;
    follower?.close();
    setBuffer(emptyLogBuffer());
    const runId = props.runId;
    follower = followLog({
      open: (q, h) => streamServiceLogs(runId, n, { ...q, ...h }),
      probe: (q) => probeLogStream(serviceLogStreamUrl(runId, n, q)),
      tail,
      outcome: () => untrack(() => (LIVE_STATUSES.has(props.service.status) ? "running" : "ended")),
      onChunk: (t) => setBuffer((prev) => appendLog(prev, t)),
      onTruncated: (t) => setBuffer((prev) => truncateLog(prev, t)),
      onEnd: () => {},
      onReset: () => setBuffer(emptyLogBuffer()),
    });
  });
  onCleanup(() => follower?.close());

  const logRows = () => tailRows(buffer(), MAX_LINES);
  // The whole log as a text file: the pane shows at most FULL_LOG_BYTES.
  const rawUrl = () => serviceLogsRawUrl(props.runId, name());
  const notice = () => logNotice(buffer());

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
        <span class="svc-readout-hint">Live logs</span>
        <a class="lgtog" href={rawUrl()} download="" title="The whole log of this service, as a text file">
          Download
        </a>
      </div>

      {/* Best-effort live log tail. */}
      <div class="tabpane logpane">
        <Show when={notice()}>
          {(n) => (
            <div class="lgnotice" role="status">
              <span>{n().text}</span>
              <Show when={n().loadFull}>
                <button type="button" class="linklike" onClick={() => setFullFor(name())}>
                  Load full log
                </button>
              </Show>
              <a class="linklike" href={rawUrl()} download="">
                Download
              </a>
            </div>
          )}
        </Show>
        <div class="lgbody nowrap">
          {/* Rows by position: a new chunk updates the rows in place instead
              of building every row again. */}
          <Index
            each={logRows()}
            fallback={
              <div class="lgrow empty">
                <span class="lgln" />
                <span class="lgtx">waiting for output…</span>
              </div>
            }
          >
            {(r) => (
              <div class="lgrow">
                <span class="lgln">{r().n}</span>
                <span class="lgtx">{r().line}</span>
              </div>
            )}
          </Index>
        </div>
      </div>
    </div>
  );
}
