// Run detail — the operator view. A provenance header answers "what is this
// run"; the Take dropdown (ADR-0056) is the run's version history — one Take
// per human restart, derived purely from the event log, each closed Take a
// read-only snapshot-at-boundary replay. The DAG (blueprint spine) shows the
// step graph with live status; the merged step pane scopes Logs/Results/
// Outputs/Workspace to the selected (step, attempt); artifacts are run-level,
// immutable per attempt; and the Activity rail is the durable event log made
// legible — restarts, retries, and crash re-adoptions you can read at a glance.
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
  artifactUrl,
  type RunStatus,
  type RunEvent,
  type StepStatus,
  type Artifact,
} from "../api/client";
import { eventParts, eventCategory, EVENT_GLYPH } from "../events";
import { deriveTakes, replayTake, type Take, type TakeView } from "../takes";
import { relTime, absTime, duration } from "../fmt";
import StatusBadge from "../components/StatusBadge";
import Icon from "../components/Icon";
import Doodle from "../components/Doodle";
import Dag, { type DagStep } from "../components/Dag";
import StepPane from "../components/StepPane";
import DebugShell from "../components/DebugShell";

const POLL_MS = 1200;

/** Human byte size. */
function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** Numeric part of an attempt id (`a3` → 3) for as-of-boundary comparisons. */
const attemptN = (id: string) => parseInt(id.replace(/^a/, ""), 10) || 0;

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
  const [selAttempt, setSelAttempt] = createSignal<string | null>(null);
  const [restarting, setRestarting] = createSignal<string | null>(null);
  const [cancelling, setCancelling] = createSignal(false);
  const [shellOpen, setShellOpen] = createSignal(false);
  // The viewed Take (1-based), or null = latest. Only a CLOSED take is a
  // time-travel view; selecting the latest take clears back to live.
  const [viewTake, setViewTake] = createSignal<number | null>(null);

  const stepList = (): StepStatus[] => run()?.steps ?? [];

  // --- Takes (ADR-0056): derived from the event log, stored nowhere. ---
  const takes = (): Take[] => deriveTakes(events());
  const latestTakeN = () => takes().length;
  const viewing = (): Take | null => {
    const n = viewTake();
    if (n === null || n >= latestTakeN()) return null;
    return takes().find((t) => t.n === n) ?? null;
  };
  const timeTraveling = () => viewing() !== null;
  // Snapshot-at-boundary: a closed Take's view is a pure replay of the log up
  // to the restart press that closed it.
  const takeView = (): TakeView | null => {
    const t = viewing();
    return t ? replayTake(events(), takes(), t) : null;
  };
  // Events visible in the Activity rail: truncated at the boundary while
  // time-traveling — the rail shows what had happened AS OF that instant.
  const visibleEvents = () => {
    const t = viewing();
    return t ? events().slice(0, t.endIdx) : events();
  };

  const selectedStep = () => stepList().find((s) => s.id === sel()) ?? null;
  const selectedStatus = () => {
    const tv = takeView();
    const s = sel();
    if (tv && s) return tv.status[s];
    return selectedStep()?.status;
  };
  const selectedRunning = () => selectedStatus() === "running" && !timeTraveling();
  // A running step attaches to its live Pod; a finished one is reproduced in a
  // fresh debug Pod. Debug-pod is the ONE action allowed while viewing a
  // closed Take (it reproduces immutable evidence and mutates nothing durable).
  const shellMode = (): "attach" | "debug-pod" => (selectedRunning() ? "attach" : "debug-pod");
  const canShell = () =>
    !!sel() && ["running", "succeeded", "failed"].includes(String(selectedStatus() ?? ""));
  const runningCount = () => stepList().filter((s) => s.status === "running").length;

  // Per-step wall-clock from the event log: first AttemptStarted → last
  // AttemptFinished. While time-traveling, computed over the truncated log so
  // a closed Take never shows timing from its future.
  const stepTiming = (): Record<string, { start?: number; end?: number }> => {
    const m: Record<string, { start?: number; end?: number }> = {};
    for (const e of visibleEvents()) {
      const k = e.kind;
      if (typeof k === "string") continue;
      const tag = Object.keys(k)[0];
      const step = (k[tag]?.step as string | undefined) ?? undefined;
      if (!step) continue;
      const t = (m[step] ??= {});
      if (tag === "AttemptStarted" && (t.start === undefined || e.at < t.start)) t.start = e.at;
      if (tag === "AttemptFinished" && (t.end === undefined || e.at > t.end)) t.end = e.at;
    }
    return m;
  };

  // DAG nodes: the graph shape + status. Live: the run object. Time-traveling:
  // the replayed statuses/attempt-counts as of the boundary.
  const dagSteps = (): DagStep[] => {
    const timing = stepTiming();
    const tv = takeView();
    return stepList().map((s) => {
      const t = timing[s.id];
      return {
        id: s.id,
        status: tv ? (tv.status[s.id] ?? "pending") : s.status,
        attempts: tv ? (tv.attempts[s.id] ?? 0) : s.attempts,
        needs: s.needs ?? [],
        gate: s.gate,
        runningSince:
          !tv && s.status === "running"
            ? s.attempt_list?.[s.attempt_list.length - 1]?.started_at ?? null
            : null,
        durationMs: t?.start != null && t?.end != null ? t.end - t.start : null,
      };
    });
  };

  const [artifacts, { refetch: refetchArtifacts }] = createResource(id, (rid) =>
    listArtifacts(rid).catch(() => [] as Artifact[]),
  );

  // Artifact versions visible in the current view: while time-traveling, only
  // versions from attempts that existed as of the boundary — and of-record is
  // recomputed within that horizon (the server's flag is latest-global).
  const visibleArtifacts = (): Artifact[] => {
    const all = artifacts() ?? [];
    const tv = takeView();
    if (!tv) return all;
    const rows = all.filter((a) => {
      if (!a.step) return true; // pre-ADR-0056 row: no provenance to judge
      const frontier = tv.frontier[a.step];
      return frontier !== undefined && attemptN(a.attempt) <= attemptN(frontier);
    });
    const ofRecord = new Map<string, number>();
    rows.forEach((a, i) => {
      if (a.succeeded) ofRecord.set(a.name, i);
    });
    return rows.map((a, i) => ({ ...a, of_record: ofRecord.get(a.name) === i }));
  };

  let poll: ReturnType<typeof setInterval> | undefined;

  // Timing from the event log: first event = created; the view's end is the
  // boundary instant while time-traveling, else the latest transition.
  const startedAt = () =>
    visibleEvents().length ? Math.min(...visibleEvents().map((e) => e.at)) : null;
  const finishedAt = () => {
    const t = viewing();
    if (t) return t.closedAt;
    return run() && isTerminal(run()!.status) && events().length
      ? Math.max(...events().map((e) => e.at))
      : null;
  };

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

  // Selecting a step or switching Takes resets the attempt scope to the new
  // default (latest attempt, or the Take frontier while time-traveling).
  createEffect(() => {
    void sel();
    void viewTake();
    setSelAttempt(null);
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

  const takeLabel = (t: Take) =>
    t.n === latestTakeN()
      ? `Take ${t.n} of ${latestTakeN()} (latest)`
      : `Take ${t.n} of ${latestTakeN()} — closed by restart of ${t.closedByTarget ?? "?"}`;

  // Straddling steps in the viewed Take: mid-flight at the boundary, finished
  // in a later Take (or never) — the "finished in Take N →" affordance.
  const straddlers = () => {
    const tv = takeView();
    if (!tv) return [] as { step: string; take: number }[];
    return Object.entries(tv.finishedInTake).map(([step, take]) => ({ step, take }));
  };

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
              <Show when={live() && !timeTraveling()}>
                <span class="live-dot" title="live">
                  <span class="dot" /> live
                </span>
              </Show>
              {/* The Take dropdown (ADR-0056): appears once history exists. */}
              <Show when={latestTakeN() > 1}>
                <label class="take-select" title="run history — one take per restart">
                  <Icon icon="history" size={13} />
                  <select
                    value={String(viewTake() ?? latestTakeN())}
                    onChange={(e) => {
                      const n = Number(e.currentTarget.value);
                      setViewTake(n >= latestTakeN() ? null : n);
                    }}
                  >
                    <For each={takes()}>
                      {(t) => <option value={String(t.n)}>{takeLabel(t)}</option>}
                    </For>
                  </select>
                </label>
              </Show>
            </div>

            <Show when={viewing()}>
              {(t) => (
                <div class="take-banner">
                  <Icon icon="history" size={14} />
                  <span>
                    viewing <b>Take {t().n}</b> — a read-only snapshot of the run the instant{" "}
                    <b class="mono">{t().closedByTarget}</b> was restarted
                    {t().closedBy ? ` by ${t().closedBy}` : ""}
                    {t().closedAt ? ` ${relTime(t().closedAt!)}` : ""}
                  </span>
                  <For each={straddlers()}>
                    {(x) => (
                      <span class="take-straddle">
                        <b class="mono">{x.step}</b> was still running —{" "}
                        <Show when={x.take > 0} fallback={<>never finished</>}>
                          <button
                            class="linklike"
                            onClick={() =>
                              setViewTake(x.take >= latestTakeN() ? null : x.take)
                            }
                          >
                            finished in Take {x.take} →
                          </button>
                        </Show>
                      </span>
                    )}
                  </For>
                  <span class="grow1" />
                  <button class="btn btn-ghost btn-sm" onClick={() => setViewTake(null)}>
                    jump to latest
                  </button>
                </div>
              )}
            </Show>

            <div class="run-toolbar">
              {/* Restart mutates THIS run — it closes the current Take and
                  opens the next; Re-run mints a whole NEW run. Distinct verbs,
                  distinct icons (ADR-0056). While time-traveling, everything
                  that would mutate this run is disabled; "New run" stays (it
                  touches nothing here) and debug-pod stays (evidence-only). */}
              <button
                class="btn btn-ghost btn-sm"
                onClick={() => sel() && onRestart(sel()!)}
                disabled={restarting() !== null || !sel() || timeTraveling()}
                title={
                  timeTraveling()
                    ? "read-only take view — jump to latest to restart"
                    : sel()
                      ? `restart ${sel()} and its descendants — opens a new take`
                      : "select a step"
                }
              >
                <Icon icon="rotate-ccw" size={13} /> {restarting() ? "restarting…" : "Restart step"}
              </button>
              <button
                class="btn btn-ghost btn-sm"
                onClick={() =>
                  nav(`/${org()}/${repo()}/run`, { state: { prefillParams: runParams(r()) } })
                }
                title="launch a NEW run of this pipeline, pre-filled with these parameters"
              >
                <Icon icon="play" size={13} /> New run
              </button>
              <button
                class="btn btn-ghost btn-sm"
                onClick={() => void onCancel()}
                disabled={
                  cancelling() || timeTraveling() || (run() ? isTerminal(run()!.status) : true)
                }
                title={
                  timeTraveling() ? "read-only take view" : "cancel this run and tear down its steps"
                }
              >
                {cancelling() ? "cancelling…" : "Cancel"}
              </button>
              <button
                class="btn btn-ghost btn-sm"
                onClick={() => setShellOpen(true)}
                disabled={!canShell()}
                title={
                  !canShell()
                    ? "select a running or finished step"
                    : selectedRunning()
                      ? `shell into the running ${sel()} Pod`
                      : `reproduce ${sel()} in a fresh debug Pod and shell in`
                }
              >
                <Icon icon="terminal" size={13} />{" "}
                {selectedRunning() ? "Debug shell" : "Debug pod"}
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
                    {r().steps.length} steps
                    {!timeTraveling() && runningCount() ? ` · ${runningCount()} running` : ""}
                    {timeTraveling() ? ` · as of take ${viewing()!.n}` : ""}
                  </span>
                </div>
                <Dag steps={dagSteps()} selected={sel()} onSelect={setSel} />
              </div>

              <StepPane
                runId={id()}
                step={selectedStep()}
                events={visibleEvents()}
                attempt={selAttempt()}
                onAttemptSelect={setSelAttempt}
                frontierAttempt={sel() ? takeView()?.frontier[sel()!] ?? null : null}
                deadLettered={r().status === "dead_lettered"}
              />
            </div>

            {/* Artifacts — run-level files of record, immutable per attempt
                (ADR-0056): every version listed with its provenance; the bare
                download is the of-record resolution, shadowed/failed versions
                download by pinned version. */}
            <div class="panel artifacts-panel">
              <div class="panel-h">
                <span>Artifacts</span>
                <span class="subtle">{visibleArtifacts().length} versions</span>
              </div>
              <Show
                when={visibleArtifacts().length > 0}
                fallback={<p class="empty">no artifacts published by this run</p>}
              >
                <ul class="filelist">
                  <For each={visibleArtifacts()}>
                    {(a) => (
                      <li class="filerow" classList={{ shadowed: !a.of_record }}>
                        <span class="fname">
                          <Icon icon="package" size={14} />
                          <a
                            href={
                              a.of_record || !a.step
                                ? artifactUrl(id(), a.name)
                                : artifactUrl(id(), a.name, { step: a.step, attempt: a.attempt })
                            }
                            download={a.name}
                          >
                            {a.name}
                          </a>
                        </span>
                        <Show when={a.step}>
                          <span class="aprov mono" classList={{ failed: !a.succeeded }}>
                            {a.step}@{a.attempt}
                            {a.succeeded ? "" : " ✗"}
                          </span>
                        </Show>
                        <Show when={a.of_record}>
                          <span
                            class="aofrecord"
                            title="what the bare name-addressed download serves"
                          >
                            of record
                          </span>
                        </Show>
                        <span class="fsize mono">{fmtSize(a.size)}</span>
                        <span class="fmeta mono">{a.content_type}</span>
                      </li>
                    )}
                  </For>
                </ul>
              </Show>
            </div>

            <div class="panel activity-panel">
              <div class="panel-h">
                <span>Activity</span>
                <span class="subtle">
                  {visibleEvents().length} events
                  {timeTraveling()
                    ? ` · ${events().length - visibleEvents().length} later hidden by take view`
                    : ""}
                </span>
              </div>
              <div class="tl">
                <For each={visibleEvents()} fallback={<div class="tl-empty">no events yet</div>}>
                  {(e) => {
                    const cat = eventCategory(e);
                    const parts = eventParts(e);
                    return (
                      <div class="tl-item">
                        <div class="tl-time" title={absTime(e.at)}>{relTime(e.at)}</div>
                        <div class="tl-rail">
                          <div class={`tl-glyph ${cat}`}>{EVENT_GLYPH[cat]}</div>
                        </div>
                        <div class="tl-body">
                          <div class="tl-msg">
                            <Show when={parts.step}>
                              <b class="tl-step mono">{parts.step}</b>
                            </Show>
                            {parts.text}
                          </div>
                        </div>
                      </div>
                    );
                  }}
                </For>
              </div>
            </div>
            <Show when={shellOpen() && sel()}>
              <DebugShell
                runId={id()}
                step={sel()!}
                mode={shellMode()}
                onClose={() => setShellOpen(false)}
              />
            </Show>
          </>
        )}
      </Show>
    </section>
  );
}
