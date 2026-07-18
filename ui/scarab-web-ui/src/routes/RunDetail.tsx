// Run detail — the operator view. A provenance header answers "what is this
// run", the DAG (blueprint spine) shows the real step graph with live status,
// retries, and elapsed (ADR-0006/0047); logs fold per step → attempt and stream
// lazily (ADR-0013); the Inspector browses each step's results/outputs/artifacts/
// workspace (ADR-0041/0029/0052); and the Activity rail is the durable event log
// made legible — retries and recovery you can read at a glance.
import { createSignal, createEffect, createResource, onMount, onCleanup, For, Show } from "solid-js";
import { A, useParams, useNavigate } from "@solidjs/router";
import {
  getRun,
  fetchEvents,
  restartStep,
  cancelRun,
  isTerminal,
  runParams,
  listArtifacts,
  type RunStatus,
  type RunEvent,
  type StepStatus,
} from "../api/client";
import { describeEvent, eventCategory, EVENT_GLYPH } from "../events";
import { relTime, absTime, duration } from "../fmt";
import StatusBadge from "../components/StatusBadge";
import Icon from "../components/Icon";
import Doodle from "../components/Doodle";
import Dag, { type DagStep } from "../components/Dag";
import StepLogs from "../components/StepLogs";
import Inspector from "../components/Inspector";

const POLL_MS = 1200;

export default function RunDetail() {
  const params = useParams();
  const nav = useNavigate();
  const id = () => params.id!;
  const org = () => params.org!;
  const repo = () => params.repo!;

  const [run, setRun] = createSignal<RunStatus | null>(null);
  const [events, setEvents] = createSignal<RunEvent[]>([]);
  const [live, setLive] = createSignal(true);
  const [sel, setSel] = createSignal<string | null>(null);
  const [restarting, setRestarting] = createSignal<string | null>(null);
  const [cancelling, setCancelling] = createSignal(false);

  const stepList = (): StepStatus[] => run()?.steps ?? [];
  const runningCount = () => stepList().filter((s) => s.status === "running").length;

  // DAG nodes: the graph shape + live status. A running step's `runningSince`
  // (its latest attempt's start) drives the node's in-place elapsed counter.
  const dagSteps = (): DagStep[] =>
    stepList().map((s) => ({
      id: s.id,
      status: s.status,
      attempts: s.attempts,
      needs: s.needs ?? [],
      gate: s.gate,
      runningSince:
        s.status === "running" ? s.attempt_list?.[s.attempt_list.length - 1]?.started_at ?? null : null,
    }));

  const [artifacts, { refetch: refetchArtifacts }] = createResource(id, (rid) =>
    listArtifacts(rid).catch(() => []),
  );

  let poll: ReturnType<typeof setInterval> | undefined;

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
    if (live()) poll = setInterval(() => void refresh(), POLL_MS);
  });
  onCleanup(() => {
    if (poll) clearInterval(poll);
  });

  async function onRestart(step: string) {
    setRestarting(step);
    try {
      await restartStep(id(), step);
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
                  nav(`/${org()}/${repo()}/run`, { state: { prefillParams: runParams(r()) } })
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
                  {startedAt() ? duration(startedAt()!, finishedAt() ?? Date.now()) : "—"}
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
                <Dag steps={dagSteps()} selected={sel()} onSelect={setSel} />
              </div>

              <div class="panel logs-panel">
                <div class="panel-h">
                  <span>Logs</span>
                  <span class="subtle">fold by step · retries fold by attempt</span>
                </div>
                <StepLogs runId={id()} steps={stepList()} selected={sel()} onSelect={setSel} />
              </div>
            </div>

            <Inspector runId={id()} selectedStep={sel()} artifacts={artifacts() ?? []} />

            <div class="panel activity-panel">
              <div class="panel-h">
                <span>Activity</span>
                <span class="subtle">{events().length} events</span>
              </div>
              <div class="tl">
                <For each={events()} fallback={<div class="tl-empty">no events yet</div>}>
                  {(e) => {
                    const cat = eventCategory(e);
                    return (
                      <div class="tl-item">
                        <div class="tl-time" title={absTime(e.at)}>{relTime(e.at)}</div>
                        <div class="tl-rail">
                          <div class={`tl-glyph ${cat}`}>{EVENT_GLYPH[cat]}</div>
                        </div>
                        <div class="tl-body">
                          <div class="tl-msg">{describeEvent(e)}</div>
                        </div>
                      </div>
                    );
                  }}
                </For>
              </div>
            </div>
          </>
        )}
      </Show>
    </section>
  );
}
