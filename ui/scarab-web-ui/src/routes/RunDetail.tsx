// Run detail — the operator view. A provenance header answers "what is this
// run" (timing derived from the event log), the DAG shows the real step graph
// (ADR-0006), selecting a node reveals that step's detail, and logs stream live
// (ADR-0013). Repo/commit/trigger provenance and per-step log attribution land
// with the forge + log-source backend slices; this view is built to show them
// the moment they exist.
import { createSignal, createEffect, onMount, onCleanup, For, Show } from "solid-js";
import { A, useParams, useNavigate } from "@solidjs/router";
import {
  getRun,
  fetchEvents,
  streamLogs,
  restartStep,
  isTerminal,
  runParams,
  type RunStatus,
  type RunEvent,
} from "../api/client";
import { describeEvent } from "../events";
import { relTime, absTime, duration } from "../fmt";
import { enrichProvenance, TRIGGER_GLYPH } from "../data/catalog";
import StatusBadge from "../components/StatusBadge";
import Icon from "../components/Icon";
import Doodle from "../components/Doodle";
import Dag, { type DagStep } from "../components/Dag";

const POLL_MS = 1200;

export default function RunDetail() {
  const params = useParams();
  const nav = useNavigate();
  const id = () => params.id!;
  const org = () => params.org!;
  const repo = () => params.repo!;
  // Representative commit/trigger provenance until the forge slice lands; the
  // rest of this view (status, DAG, events, logs, restart) is fully live.
  const prov = () => enrichProvenance(id(), repo());

  const [run, setRun] = createSignal<RunStatus | null>(null);
  const [events, setEvents] = createSignal<RunEvent[]>([]);
  const [log, setLog] = createSignal("");
  const [live, setLive] = createSignal(true);
  const [sel, setSel] = createSignal<string | null>(null);
  const [restarting, setRestarting] = createSignal<string | null>(null);
  const [follow, setFollow] = createSignal(true);
  const [wrap, setWrap] = createSignal(true);

  const runningCount = () => steps().filter((s) => s.status === "running").length;

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
                <span class="crumb-head-title" title={id()}>{prov().message}</span>
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
              <button class="btn btn-ghost btn-sm" disabled title="cancel lands with scheduler support">
                Cancel
              </button>
            </div>

            <div class="prov">
              <div class="cell">
                <div class="k">commit</div>
                <div class="v mono"><span class="sha">{prov().sha}</span> on {prov().branch}</div>
              </div>
              <div class="cell">
                <div class="k">trigger</div>
                <div class="v">
                  <span class="tglyph">{TRIGGER_GLYPH[prov().trigger]}</span>{" "}
                  {prov().trigger === "pull_request" ? `PR #${prov().prNumber}` : prov().trigger} · {prov().author}
                </div>
              </div>
              <div class="cell">
                <div class="k">pipeline</div>
                <div class="v mono">.scarab/ci.yaml</div>
              </div>
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
