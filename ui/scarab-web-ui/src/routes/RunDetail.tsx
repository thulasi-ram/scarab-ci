// Run detail — the operator view. A provenance header answers "what is this
// run" (timing derived from the event log, launch params from the run itself),
// the DAG shows the real step graph (ADR-0006), selecting a node reveals that
// step's detail, logs stream live (ADR-0013), and the run's artifacts of record
// (ADR-0052) list with download links once they exist.
import { createSignal, createEffect, createResource, onMount, onCleanup, For, Show } from "solid-js";
import { A, useParams, useNavigate } from "@solidjs/router";
import {
  getRun,
  fetchEvents,
  streamLogs,
  restartStep,
  cancelRun,
  isTerminal,
  runParams,
  listArtifacts,
  artifactUrl,
  type RunStatus,
  type RunEvent,
} from "../api/client";
import { describeEvent } from "../events";
import { relTime, absTime, duration } from "../fmt";
import StatusBadge from "../components/StatusBadge";
import Icon from "../components/Icon";
import Doodle from "../components/Doodle";
import Dag, { type DagStep } from "../components/Dag";

const POLL_MS = 1200;

/** Human byte size for an artifact row. */
function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export default function RunDetail() {
  const params = useParams();
  const nav = useNavigate();
  const id = () => params.id!;
  const org = () => params.org!;
  const repo = () => params.repo!;

  const [run, setRun] = createSignal<RunStatus | null>(null);
  const [events, setEvents] = createSignal<RunEvent[]>([]);
  const [log, setLog] = createSignal("");
  const [live, setLive] = createSignal(true);
  const [sel, setSel] = createSignal<string | null>(null);
  const [restarting, setRestarting] = createSignal<string | null>(null);
  const [cancelling, setCancelling] = createSignal(false);
  const [follow, setFollow] = createSignal(true);
  const [wrap, setWrap] = createSignal(true);

  const runningCount = () => steps().filter((s) => s.status === "running").length;

  // Artifacts of record (ADR-0052): fetched once up front, re-fetched when the
  // run settles terminal (steps publish as they finish; the final list is only
  // complete then). Errors degrade to an empty list — the panel just hides.
  const [artifacts, { refetch: refetchArtifacts }] = createResource(id, (rid) =>
    listArtifacts(rid).catch(() => []),
  );

  let logRef: HTMLPreElement | undefined;
  let poll: ReturnType<typeof setInterval> | undefined;
  let closeLogs: (() => void) | undefined;

  const steps = (): DagStep[] =>
    (run()?.steps ?? []).map((s) => ({
      id: s.id,
      status: s.status,
      attempts: s.attempts,
      needs: s.needs ?? [],
      gate: s.gate,
    }));

  const selectedStep = () => steps().find((s) => s.id === sel()) ?? null;

  // Timing from the event log: first event = created, last = latest transition.
  const startedAt = () => (events().length ? Math.min(...events().map((e) => e.at)) : null);
  const finishedAt = () =>
    run() && isTerminal(run()!.status) && events().length
      ? Math.max(...events().map((e) => e.at))
      : null;

  async function refresh() {
    const [status, evs] = await Promise.all([
      getRun(id()),
      fetchEvents(id()).catch(() => [] as RunEvent[]),
    ]);
    setRun(status);
    setEvents(evs);
    // Default the selection to a running step, else the first.
    if (!sel() && status.steps.length) {
      const running = status.steps.find((s) => s.status === "running");
      setSel((running ?? status.steps[0]).id);
    }
    if (isTerminal(status.status)) {
      setLive(false);
      if (poll) {
        clearInterval(poll);
        poll = undefined;
      }
      void refetchArtifacts();
    }
  }

  onMount(async () => {
    await refresh();
    closeLogs = streamLogs(
      id(),
      (chunk) => setLog((prev) => prev + chunk + "\n"),
      () => void refresh(),
    );
    if (live()) poll = setInterval(() => void refresh(), POLL_MS);
  });

  onCleanup(() => {
    closeLogs?.();
    if (poll) clearInterval(poll);
  });

  createEffect(() => {
    log();
    if (logRef && follow()) logRef.scrollTop = logRef.scrollHeight;
  });

  async function onRestart(step: string) {
    setRestarting(step);
    try {
      await restartStep(id(), step);
      setLog((p) => p + `\n— restarted ${step} —\n`);
      setLive(true);
      await refresh();
      if (!poll && live()) poll = setInterval(() => void refresh(), POLL_MS);
    } finally {
      setRestarting(null);
    }
  }

  async function onCancel() {
    setCancelling(true);
    try {
      await cancelRun(id());
      setLog((p) => p + `\n— cancelled —\n`);
      await refresh();
    } finally {
      setCancelling(false);
    }
  }

  return (
    <section class="page">
      <Doodle icon="container" size={230} rotate={14} opacity={0.16} top="52px" right="48px" />

      <Show when={run()} fallback={<p class="empty">loading…</p>}>
        {(r) => (
          <>
            <div class="run-head">
              <span class={`sdot lg ${r().status}`} />
              <h1 class="crumb-head">
                <A href={`/${org()}/${repo()}`} class="crumb-head-link">{repo()}</A>
                <Icon icon="chevron-right" size={20} class="crumb-head-sep" />
                <span class="crumb-head-title mono" title={id()}>run {id().slice(0, 8)}</span>
              </h1>
              <StatusBadge status={r().status} />
              <Show when={live()}>
                <span class="live-dot" title="live">
                  <span class="dot" /> live
                </span>
              </Show>
            </div>

            <div class="run-toolbar">
              <button
                class="btn btn-ghost btn-sm"
                onClick={() => sel() && onRestart(sel()!)}
                disabled={restarting() !== null || !sel()}
                title={sel() ? `restart ${sel()} and its descendants` : "select a step"}
              >
                <Icon icon="rotate-cw" size={13} /> {restarting() ? "restarting…" : "Restart"}
              </button>
              <button
                class="btn btn-ghost btn-sm"
                onClick={() =>
                  nav(`/${org()}/${repo()}/run`, {
                    state: { prefillParams: runParams(r()) },
                  })
                }
                title="re-run this pipeline, pre-filled with these parameters"
              >
                <Icon icon="rotate-cw" size={13} /> Re-run
              </button>
              <button
                class="btn btn-ghost btn-sm"
                onClick={() => void onCancel()}
                disabled={cancelling() || (run() ? isTerminal(run()!.status) : true)}
                title="cancel this run and tear down its steps"
              >
                {cancelling() ? "cancelling…" : "Cancel"}
              </button>
            </div>

            <div class="prov">
              <div class="cell">
                <div class="k">run</div>
                <div class="v mono"><span class="sha">{id()}</span></div>
              </div>
              <Show when={Object.keys(runParams(r())).length > 0}>
                <div class="cell">
                  <div class="k">params</div>
                  <div class="v mono">
                    {Object.entries(runParams(r()))
                      .map(([k, v]) => `${k}=${String(v)}`)
                      .join(" · ")}
                  </div>
                </div>
              </Show>
              <Show when={startedAt()}>
                {(t) => (
                  <div class="cell">
                    <div class="k">started</div>
                    <div class="v" title={absTime(t())}>{relTime(t())}</div>
                  </div>
                )}
              </Show>
              <div class="cell">
                <div class="k">elapsed</div>
                <div class="v mono">
                  {startedAt()
                    ? duration(startedAt()!, finishedAt() ?? Date.now())
                    : "—"}
                </div>
              </div>
            </div>

            <div class="rd-grid">
              <div class="panel dag-panel">
                <div class="panel-h">
                  <span>DAG</span>
                  <span class="subtle">
                    {r().steps.length} steps{runningCount() ? ` · ${runningCount()} running` : ""}
                  </span>
                </div>
                <Dag steps={steps()} selected={sel()} onSelect={setSel} />
              </div>

              <div class="panel logs-panel">
                <div class="panel-h">
                  <span>Logs{sel() ? ` · ${sel()}` : ""}</span>
                  <span class="logtoolbar">
                    <button class={`logtool ${follow() ? "on" : ""}`} onClick={() => setFollow((v) => !v)}>
                      follow
                    </button>
                    <button class={`logtool ${wrap() ? "on" : ""}`} onClick={() => setWrap((v) => !v)}>
                      wrap
                    </button>
                  </span>
                </div>
                <Show when={selectedStep()}>
                  {(s) => (
                    <div class="log-substep mono">
                      <span class={`sdot ${s().status}`} /> {s().status}
                      <span class="dotsep">·</span> {s().attempts} attempt{s().attempts === 1 ? "" : "s"}
                      <Show when={s().needs.length}>
                        <span class="dotsep">·</span> needs {s().needs.join(", ")}
                      </Show>
                    </div>
                  )}
                </Show>
                <pre ref={logRef} class="logs" style={{ "white-space": wrap() ? "pre-wrap" : "pre" }}>
                  {log() || (live() ? "waiting for output…" : "no log output — a step log source lands with the executor wiring")}
                </pre>
              </div>
            </div>

            <Show when={(artifacts() ?? []).length > 0}>
              <div class="panel artifacts-panel">
                <div class="panel-h">
                  <span>Artifacts</span>
                  <span class="subtle">{artifacts()!.length}</span>
                </div>
                <ul class="secret-list" style={{ padding: "12px 16px" }}>
                  <For each={artifacts()}>
                    {(a) => (
                      <li class="secret-row">
                        <Icon icon="package" size={15} />
                        <a class="mono" href={artifactUrl(id(), a.name)} download={a.name}>
                          {a.name}
                        </a>
                        <span class="subtle mono">
                          {fmtSize(a.size)} · {a.content_type}
                        </span>
                      </li>
                    )}
                  </For>
                </ul>
              </div>
            </Show>

            <div class="panel activity-panel">
              <div class="panel-h"><span>Activity</span></div>
              <ul class="timeline">
                <For each={events()} fallback={<li class="empty">no events yet</li>}>
                  {(e) => (
                    <li class="timeline-row">
                      <span class="mono timeline-at" title={absTime(e.at)}>
                        {new Date(e.at).toLocaleTimeString()}
                      </span>
                      <span class="timeline-msg">{describeEvent(e)}</span>
                    </li>
                  )}
                </For>
              </ul>
            </div>
          </>
        )}
      </Show>
    </section>
  );
}
