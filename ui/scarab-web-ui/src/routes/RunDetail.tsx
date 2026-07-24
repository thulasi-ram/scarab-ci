// Run detail — the operator view. A provenance header answers "what is this
// run"; below it, ONE Pipeline component (ADR-0056 + amendment) holds the DAG,
// the selected step's evidence, and a version-aware Artifacts footer. Its header
// carries the always-present version dropdown — the run's history as a row per
// Rerun (derived purely from the event log; "Take"/"attempt" never surface).
// Zoom out = which whole-run version; zoom in = which try of a step (the strip
// inside the step pane). Picking an older version turns the whole component into
// a read-only snapshot-at-boundary. The Activity rail stays separate — the
// unfiltered, cross-version event log where reruns, retries, and crash
// re-adoptions are witnessed.
import { createSignal, createEffect, createResource, onMount, onCleanup, For, Show } from "solid-js";
import { A, useParams, useNavigate } from "@solidjs/router";
import {
  getRun,
  fetchEvents,
  rerunStep,
  retryStep,
  cancelRun,
  isTerminal,
  runParams,
  listArtifacts,
  artifactUrl,
  repoForgeUrl,
  listServices,
  type RunStatus,
  type RunEvent,
  type StepStatus,
  type Artifact,
  type Service,
} from "../api/client";
import { eventParts, eventCategory, EVENT_GLYPH } from "../events";
import {
  deriveTakes,
  replayTake,
  rowLabel,
  versionRows,
  stepTiming,
  visibleArtifacts as visibleArtifactsIn,
  type Take,
  type TakeView,
} from "../takes";
import { stripTries as stripTriesOf } from "../attempts";
import { relTime, absTime, duration } from "../fmt";
import { forgeCommitUrl, forgePrUrl } from "../forge";
import StatusBadge from "../components/StatusBadge";
import Icon from "../components/Icon";
import Doodle from "../components/Doodle";
import Dag, { type DagStep, type DagService, type DagControls } from "../components/Dag";
import VersionDropdown, { type VersionRow } from "../components/VersionDropdown";
import StepPane from "../components/StepPane";
import ServicePane from "../components/ServicePane";
import { type FilmstripTry } from "../components/AttemptsDropdown";
import DebugShell from "../components/DebugShell";
import TriggerCell from "../components/TriggerCell";

const POLL_MS = 1200;

/** Human byte size. */
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
  const [live, setLive] = createSignal(true);
  const [sel, setSel] = createSignal<string | null>(null);
  const [selAttempt, setSelAttempt] = createSignal<string | null>(null);
  // A step's docked sidecar the user clicked (ADR-0058): its `services:` index,
  // or null. Focuses that container in the StepPane Logs tab. Cleared whenever
  // the selection moves to a node (a plain step/service click) or the version
  // changes — so a stale sidecar focus never carries across.
  const [focusSidecar, setFocusSidecar] = createSignal<number | null>(null);

  // Selecting a node (step OR shared service) clears any sidecar focus; clicking
  // a docked sidecar chip selects its step AND focuses that container. Keeping
  // the clear here (not in the sel-tracking effect below) avoids clobbering the
  // focus we set in the same handler.
  const selectNode = (target: string) => {
    setSel(target);
    setFocusSidecar(null);
  };
  const selectSidecar = (stepId: string, index: number) => {
    setSel(stepId);
    setFocusSidecar(index);
  };
  // The DAG's zoom controls (Proposal B, stage 3), handed up by <Dag> on mount
  // so the band toolbar's fit / ＋ / － cluster can drive fit-to-view + zoom.
  const [dagControls, setDagControls] = createSignal<DagControls | null>(null);
  const [rerunning, setRerunning] = createSignal<string | null>(null);
  const [retrying, setRetrying] = createSignal<string | null>(null);
  const [cancelling, setCancelling] = createSignal(false);
  const [shellOpen, setShellOpen] = createSignal(false);
  // The viewed Take (1-based), or null = latest. Only a CLOSED take is a
  // time-travel view; selecting the latest take clears back to live. Driven by
  // the VersionDropdown in the Pipeline band toolbar (Proposal B): a row per
  // Rerun (ADR-0056 amendment), folded from the former persistent left rail.
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
  // to the rerun press that closed it. `takeView()` is CLOSED-take only (null on
  // the latest) — it gates the time-travel-only affordances (read-only banner,
  // artifact horizon, replayed DAG statuses).
  const takeView = (): TakeView | null => {
    const t = viewing();
    return t ? replayTake(events(), takes(), t) : null;
  };
  // The take whose lens scopes the attempt-grain reads (tries strip, evidence
  // pane, ×N badges) — the SELECTED take INCLUDING the latest/open one, unlike
  // `viewing()`/`takeView()` which are null on the latest so the header can tell
  // live from time-travel. Attempts are Take-scoped in EVERY view (ADR-0056
  // amendment 2026-07-24): each Take shows only the attempts that belong to it,
  // never tries carried over from prior Takes — the latest included.
  const selectedTake = (): Take | null => {
    const ts = takes();
    if (!ts.length) return null;
    const n = viewTake();
    if (n === null) return ts[ts.length - 1];
    return ts.find((t) => t.n === n) ?? ts[ts.length - 1];
  };
  // The scoped lens for the selected take — computed for the latest/open take
  // too. The latest take's window runs from the last rerun boundary to now, so
  // `replayTake` folds streaming attempts (AttemptStarted within the window)
  // into `windowAttempts`/`frontier`/`attempts` live as the poll advances the
  // log. Never null while there are events (deriveTakes always yields ≥1 take).
  const scopedView = (): TakeView | null => {
    const t = selectedTake();
    return t ? replayTake(events(), takes(), t) : null;
  };
  // The selected take's window for a given step — the exact attempt-id set to
  // render, `[]` for a step carried forward untouched (so it reads as "didn't
  // run in this version", consistent across closed and latest takes).
  const stepWindow = (stepId: string | null): string[] | null =>
    stepId ? (scopedView()?.windowAttempts[stepId] ?? []) : null;
  // Events visible in the Activity rail: truncated at the boundary while
  // time-traveling — the rail shows what had happened AS OF that instant.
  const visibleEvents = () => {
    const t = viewing();
    return t ? events().slice(0, t.endIdx) : events();
  };

  // Selection id space (ADR-0058): a step is `"<stepId>"`; a shared service is
  // `"service:<name>"`. A service selection routes to ServicePane, not StepPane,
  // and disables the step-only mutations (rerun/retry/debug).
  const SERVICE_PREFIX = "service:";
  const isServiceSel = () => sel()?.startsWith(SERVICE_PREFIX) ?? false;
  const selectedService = (): DagService | null => {
    if (!isServiceSel()) return null;
    const name = sel()!.slice(SERVICE_PREFIX.length);
    return dagServices().find((s) => s.name === name) ?? { name, status: "pending" };
  };

  const selectedStep = () => stepList().find((s) => s.id === sel()) ?? null;
  const selectedStatus = () => {
    const tv = takeView();
    const s = sel();
    if (tv && s) return tv.status[s];
    return selectedStep()?.status;
  };
  const selectedRunning = () => selectedStatus() === "running" && !timeTraveling();
  // Rerun/retry are blocked when a prerequisite FAILED (ADR-0056 amendment):
  // only {succeeded, skipped} prerequisites permit the action. Uses live
  // statuses (the action targets the latest version).
  const prereqBlocker = (stepId: string | null): string | null => {
    if (!stepId) return null;
    const by = new Map(stepList().map((s) => [s.id, s.status]));
    const s = stepList().find((x) => x.id === stepId);
    return (
      (s?.needs ?? []).find((d) => {
        const st = by.get(d);
        return st !== "succeeded" && st !== "skipped";
      }) ?? null
    );
  };
  // A running step attaches to its live Pod; a finished one is reproduced in a
  // fresh debug Pod. Debug-pod is the ONE action allowed while viewing a
  // closed Take (it reproduces immutable evidence and mutates nothing durable).
  const shellMode = (): "attach" | "debug-pod" => (selectedRunning() ? "attach" : "debug-pod");
  const canShell = () =>
    !!sel() && ["running", "succeeded", "failed"].includes(String(selectedStatus() ?? ""));
  const runningCount = () => stepList().filter((s) => s.status === "running").length;

  // Per-step wall-clock from the event log: first AttemptStarted → last
  // AttemptFinished — Take-scoped (ADR-0056). Only THIS take's attempts count,
  // so a step's duration is the current take's wall-clock, not summed across
  // takes (a rerun must not keep growing the displayed step time). Scoped via
  // the take's `windowAttempts` so it matches the ×N attempt badge; while
  // time-traveling, `visibleEvents()` is already truncated to the boundary too.
  // Pure derivation, extracted to takes.ts.
  const timingOf = (): Record<string, { start?: number; end?: number }> =>
    stepTiming(visibleEvents(), scopedView()?.windowAttempts ?? null);

  // DAG nodes: the graph shape + status. Live: the run object. Time-traveling:
  // the replayed statuses/attempt-counts as of the boundary.
  const dagSteps = (): DagStep[] => {
    const timing = timingOf();
    const tv = takeView();
    const sv = scopedView();
    return stepList().map((s) => {
      const t = timing[s.id];
      // Status stays LIVE (backend-authoritative) on the latest take; a closed
      // take replays it as of the boundary.
      const status = tv ? (tv.status[s.id] ?? "pending") : s.status;
      // Attempt count is Take-scoped in EVERY view now (ADR-0056 amendment):
      // the ×N badge counts only THIS take's tries, not every take's summed. For
      // a single-take run the window covers everything, so this equals
      // `s.attempts` — no change to the common case.
      const attempts = sv ? (sv.attempts[s.id] ?? 0) : s.attempts;
      // Reused: a step with a known (carried-forward) verdict but ZERO attempts
      // in THIS take's window wasn't part of this take's run (a partial rerun
      // left it alone) — render it muted. Applies to the latest take too now,
      // not only while time-traveling, so a carried-forward step reads the same
      // in the live view as in the closed-take snapshot.
      const reused =
        !!sv &&
        attempts === 0 &&
        ["succeeded", "failed", "skipped"].includes(status ?? "");
      return {
        id: s.id,
        status,
        attempts,
        reused,
        needs: s.needs ?? [],
        gate: s.gate,
        runningSince:
          !tv && s.status === "running"
            ? s.attempt_list?.[s.attempt_list.length - 1]?.started_at ?? null
            : null,
        durationMs: t?.start != null && t?.end != null ? t.end - t.start : null,
        // Sidecars dock ON the node; `uses` names source the dotted service edges
        // (ADR-0058). Both come straight off the step's live status projection.
        services: s.services,
        uses: s.uses,
      };
    });
  };

  // The selected step's tries, resolved from the event log — the markers the
  // attempts filmstrip renders in the evidence-pane header (ADR-0056 amendment).
  // While viewing a closed Take, tries after the boundary are hidden (they
  // belong to a later version and never existed in this snapshot), and causes
  // are derived over the truncated log to match. Mirrors StepPane's
  // `attemptsOf()`/`scoped()`.
  const stripTries = (): FilmstripTry[] => {
    const s = selectedStep();
    if (!s) return [];
    // The tries strip shows only the SELECTED Take's attempts in EVERY view —
    // the latest/live take included (per-Take scoping, ADR-0056 amendment
    // 2026-07-24). A step carried forward untouched shows none; a re-run step
    // shows only this take's tries, numbered per-take. The latest take's window
    // grows with the streaming log, so new attempts appear live. Pure
    // derivation, extracted to attempts.ts.
    return stripTriesOf(visibleEvents(), s.id, s.attempt_list ?? [], stepWindow(sel()));
  };
  // The try scoping the evidence pane: explicit selection, else the Take
  // frontier, else the latest — the same resolution StepPane's `scoped()` uses,
  // so the filmstrip's highlight and the pane always agree.
  const stripActiveAttempt = (): string | null => {
    const ts = stripTries();
    if (!ts.length) return null;
    const want = selAttempt() ?? (sel() ? (scopedView()?.frontier[sel()!] ?? null) : null);
    return ts.find((t) => t.id === want)?.id ?? ts[ts.length - 1].id;
  };

  // The repo's forge web base (for commit/PR deep links), from the registry.
  const [repoUrl] = createResource(
    () => [org(), repo()] as const,
    ([o, r]) => repoForgeUrl(o, r).catch(() => null),
  );

  const [artifacts, { refetch: refetchArtifacts }] = createResource(id, (rid) =>
    listArtifacts(rid).catch(() => [] as Artifact[]),
  );

  // Shared services (ADR-0058) now render as PEER NODES in the DAG (a lane at the
  // top) rather than a panel beside it. Poll alongside a live run for lifecycle
  // updates; a terminal run's set is fixed. Empty (the common case) → no lane.
  const [svcTick, setSvcTick] = createSignal(0);
  createEffect(() => {
    if (!(live() && !timeTraveling())) return;
    const t = setInterval(() => setSvcTick((n) => n + 1), 2000);
    onCleanup(() => clearInterval(t));
  });
  const [services] = createResource(
    () => [id(), svcTick()] as const,
    ([rid]) => listServices(rid).catch(() => [] as Service[]),
  );

  // The DAG's shared-service nodes: the `/services` projection (name → lifecycle
  // status) UNION every name any step declares in `uses:` — so a service that is
  // declared but not yet started still renders as a pending peer node. The
  // /services shape carries no ports, so the node's meta reads status only.
  const dagServices = (): DagService[] => {
    const fromApi = services() ?? [];
    const byName = new Map(fromApi.map((s) => [s.name, s]));
    const names: string[] = fromApi.map((s) => s.name);
    for (const s of stepList()) {
      for (const u of s.uses ?? []) if (!names.includes(u)) names.push(u);
    }
    return names.map((name) => ({ name, status: byName.get(name)?.status ?? "pending" }));
  };

  // Artifact versions visible in the current view: while time-traveling, only
  // versions from attempts that existed as of the boundary — and of-record is
  // recomputed within that horizon (the server's flag is latest-global). Pure
  // derivation, extracted to takes.ts.
  const visibleArtifacts = (): Artifact[] => visibleArtifactsIn(artifacts() ?? [], takeView());

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

  async function onRerun(step: string) {
    setRerunning(step);
    try {
      await rerunStep(id(), step);
      setLive(true);
      await refresh();
      if (!poll && live()) poll = setInterval(() => void refresh(), POLL_MS);
    } finally {
      setRerunning(null);
    }
  }

  // Retry (ADR-0056 amendment): another Attempt in the CURRENT Take, no fork.
  // Failed steps only (the button is gated so; the server also 409s otherwise).
  async function onRetry(step: string) {
    setRetrying(step);
    try {
      await retryStep(id(), step);
      setLive(true);
      await refresh();
      if (!poll && live()) poll = setInterval(() => void refresh(), POLL_MS);
    } finally {
      setRetrying(null);
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

  // Version-dropdown row copy (ADR-0056 amendment): the run history is a row per
  // Rerun, never surfacing "Take"/"attempt". Take 1 is the "original run"; every
  // later Take is named by the Rerun that OPENED it — the previous Take's closing
  // target (deriveTakes records it as `closedByTarget`) and time.
  // Run provenance lives in the event log's trigger event (a `Raw` payload) —
  // the thin GET /v1/runs/{id} DTO carries only id/status/steps — so the top bar
  // reads trigger/actor/branch/commit from there.
  const triggerInfo = (): {
    kind?: string;
    actor?: string;
    branch?: string;
    ref?: string;
    sha?: string;
    /** Forge coordinate the run was born from (for forge links). */
    owner?: string;
    name?: string;
    /** PR number, when the trigger was a pull request. */
    pr?: number;
  } | null => {
    for (const e of events()) {
      const k = e.kind as unknown;
      if (k && typeof k === "object" && "Raw" in (k as Record<string, unknown>)) {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const t = (k as any).Raw?.trigger?.event;
        if (!t) continue;
        return {
          kind: t.kind,
          actor: t.actor,
          branch: t.branch,
          ref: t.ref,
          sha: t.sha,
          owner: t.repo?.owner,
          name: t.repo?.name,
          pr: t.pr ?? t.pr_number ?? t.number ?? undefined,
        };
      }
    }
    return null;
  };

  const viewedLabel = (): string => {
    const v = viewing();
    return v ? rowLabel(takes(), v) : "latest";
  };

  // The version rail's rows (ADR-0056 amendment): one per Take. The latest/open
  // Take tallies live `run().steps[].status`; a closed Take tallies its
  // snapshot-at-boundary replay. Pure derivation (row copy, outcome tallies,
  // selection flags), extracted to takes.ts.
  const versions = (): VersionRow[] =>
    versionRows(
      events(),
      takes(),
      stepList().map((s) => s.status),
      viewTake(),
      startedAt(),
    );

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
                {/* The per-repo run number `#N` is the human handle (ADR-0057);
                    fall back to the short internal id for untenanted runs. */}
                <span class="crumb-head-title mono" title={id()}>
                  {r().run_number != null ? `#${r().run_number}` : `run ${id().slice(0, 8)}`}
                </span>
              </h1>
            </div>

            <div class="run-toolbar">
              {/* Two "agains" (ADR-0056 amendment): "Retry step" gives a FAILED
                  step another attempt in the CURRENT version (no fork); "Rerun
                  pipeline from this step" re-runs this step + everything
                  downstream as a NEW version. "New run" mints a whole new run.
                  Both are blocked when a prerequisite failed. While viewing an
                  older version everything that would mutate this run is disabled;
                  "New run" and debug-pod stay. */}
              <Show when={selectedStatus() === "failed" && !timeTraveling()}>
                <button
                  class="btn btn-ghost btn-sm"
                  onClick={() => sel() && onRetry(sel()!)}
                  disabled={retrying() !== null || rerunning() !== null || !!prereqBlocker(sel())}
                  title={
                    prereqBlocker(sel())
                      ? `blocked — prerequisite ${prereqBlocker(sel())} failed; retry that first`
                      : `retry ${sel()} — another attempt in this version (no new version)`
                  }
                >
                  <Icon icon="rotate-cw" size={13} /> {retrying() ? "retrying…" : "Retry step"}
                </button>
              </Show>
              <button
                class="btn btn-ghost btn-sm"
                onClick={() => sel() && !isServiceSel() && onRerun(sel()!)}
                disabled={
                  rerunning() !== null ||
                  retrying() !== null ||
                  !sel() ||
                  isServiceSel() ||
                  timeTraveling() ||
                  !!prereqBlocker(sel())
                }
                title={
                  timeTraveling()
                    ? "read-only — back to latest to rerun"
                    : !sel()
                      ? "select a step"
                      : isServiceSel()
                        ? "a shared service isn't a step — select a step to rerun"
                        : prereqBlocker(sel())
                          ? `blocked — prerequisite ${prereqBlocker(sel())} failed; rerun that first`
                          : `rerun ${sel()} and everything downstream — forks a new version`
                }
              >
                <Icon icon="rotate-ccw" size={13} />{" "}
                {rerunning() ? "rerunning…" : "Rerun pipeline from this step"}
              </button>
              {/* One contextual slot: while the run is ACTIVE (on latest) the
                  useful lifecycle action is to STOP it → "Cancel run" (danger);
                  once terminal — or while time-traveling an older version, where
                  cancel is meaningless — it becomes "New run", minting a fresh run
                  pre-filled with these params. Replaces the old always-present
                  "New run" + a perpetually-greyed "Cancel". */}
              <Show
                when={run() && !isTerminal(run()!.status) && !timeTraveling()}
                fallback={
                  <button
                    class="btn btn-ghost btn-sm"
                    onClick={() =>
                      nav(`/${org()}/${repo()}/run`, {
                        state: { prefillParams: runParams(r()) },
                      })
                    }
                    title="launch a NEW run of this pipeline, pre-filled with these parameters"
                  >
                    <Icon icon="play" size={13} /> New run
                  </button>
                }
              >
                <button
                  class="btn btn-danger btn-sm"
                  onClick={() => void onCancel()}
                  disabled={cancelling()}
                  title="cancel this run and tear down its steps"
                >
                  {cancelling() ? "cancelling…" : "Cancel run"}
                </button>
              </Show>
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

            {/* Provenance bar (ADR-0057 A·2): the SAME two-line body as a runs-
                list row — the Headline on top, an icon-chip fact strip beneath
                (no labels; the icon + hover title carry the meaning). The `#N`
                handle lives in the breadcrumb; status floats right. Commit + PR
                deep-link to the forge via the repo's registry web base. */}
            <div class="prov">
              <div class="prov-main">
                {/* Line 1: kind + Headline — the shared runs-list cell. */}
                <div class="rr-head">
                  <TriggerCell kind={triggerInfo()?.kind} title={r().trigger_title} />
                </div>
                {/* Line 2: origin facts — pipeline · commit · base ← head · PR ·
                    who · when · id. */}
                <div class="rr-facts">
                  <Show when={r().pipeline}>
                    {(name) => (
                      <span class="pfact strong" title="pipeline">
                        <Icon icon="workflow" size={12} />
                        <span class="mono">{name()}</span>
                      </span>
                    )}
                  </Show>
                  <Show when={triggerInfo()}>
                    {(t) => {
                      const commitUrl = () => forgeCommitUrl(repoUrl(), t().sha);
                      const prUrl = () => forgePrUrl(repoUrl(), t().pr);
                      return (
                        <>
                          <Show when={t().sha}>
                            <a
                              class="pfact"
                              classList={{ link: !!commitUrl() }}
                              href={commitUrl() ?? undefined}
                              target="_blank"
                              rel="noopener noreferrer"
                              title="commit on the forge"
                            >
                              <Icon icon="git-commit-horizontal" size={12} />
                              <span class="mono sha">{t().sha!.slice(0, 8)}</span>
                            </a>
                          </Show>
                          {/* base ← head on PR runs (ADR-0057); else branch/ref. */}
                          <Show
                            when={r().origin_pr_base}
                            fallback={
                              <Show when={t().branch && t().branch !== t().sha}>
                                <span class="pfact" title="branch / ref">
                                  <Icon icon="git-branch" size={12} />
                                  <span class="mono">{t().branch}</span>
                                </span>
                              </Show>
                            }
                          >
                            {(base) => (
                              <span class="pfact" title="base ← head">
                                <Icon icon="git-branch" size={12} />
                                <span class="mono">{base()}</span>
                                <span class="pr-arrow">←</span>
                                <span class="mono sha">{(t().sha ?? "").slice(0, 8)}</span>
                              </span>
                            )}
                          </Show>
                          <Show when={t().pr != null}>
                            <a
                              class="pfact"
                              classList={{ link: !!prUrl() }}
                              href={prUrl() ?? undefined}
                              target="_blank"
                              rel="noopener noreferrer"
                              title="pull request on the forge"
                            >
                              <Icon icon="git-pull-request" size={12} />
                              <span class="mono">#{t().pr}</span>
                            </a>
                          </Show>
                          <Show when={t().actor}>
                            {(actor) => (
                              <span class="pfact" title="triggered by">
                                <Icon icon="user" size={12} />
                                <span>{actor()}</span>
                              </span>
                            )}
                          </Show>
                        </>
                      );
                    }}
                  </Show>
                  <Show when={startedAt()}>
                    <span
                      class="pfact"
                      classList={{ live: live() && !timeTraveling() }}
                      title={absTime(startedAt()!)}
                    >
                      <Icon icon="timer" size={12} />
                      <span>
                        {relTime(startedAt()!)} ·{" "}
                        <span class="mono">
                          {duration(startedAt()!, finishedAt() ?? Date.now())}
                        </span>
                      </span>
                    </span>
                  </Show>
                  <Show when={Object.keys(runParams(r())).length > 0}>
                    <span class="pfact" title="launch parameters">
                      <span class="mono">
                        {Object.entries(runParams(r()))
                          .map(([k, v]) => `${k}=${String(v)}`)
                          .join(" · ")}
                      </span>
                    </span>
                  </Show>
                  {/* Opaque internal id (UUIDv7) — copyable; a small inline `id`
                      tag marks it apart from the shas. `#N` is in the breadcrumb. */}
                  <span
                    class="pfact idfact"
                    title={`run id ${id()} — click to copy`}
                    onClick={() => navigator.clipboard?.writeText(id())}
                  >
                    <span class="idcap">id</span>
                    <span class="mono subtle">{id().slice(0, 8)}</span>
                  </span>
                </div>
              </div>

              <div class="prov-right">
                <StatusBadge status={r().status} />
                <Show when={live() && !timeTraveling()}>
                  <span class="live-dot" title="live">
                    <span class="dot" /> live
                  </span>
                </Show>
              </div>
            </div>

            {/* Pipeline component (Proposal B — stack): a full-width blueprint
                band on TOP, with the selected step's (or service's) evidence
                pinned full-width directly BELOW it — always visible, no drawer;
                selecting a node just swaps the pinned evidence. The band toolbar
                carries the version dropdown (folded from the former left rail):
                zoom out = which whole-run version (the dropdown); zoom in = which
                try of a step (the dropdown inside StepPane). Viewing an older
                version turns the whole component read-only. */}
            <div class="panel pipeline-panel" classList={{ readonly: timeTraveling() }}>
              <div class="panel-h pipeline-toolbar">
                {/* Versions rail folded into a compact toolbar dropdown
                    (◈ latest ▾) — preserves the time-travel behavior; only the
                    presentation changed from a left column to a dropdown. */}
                <VersionDropdown
                  rows={versions()}
                  live={live() && !timeTraveling()}
                  onSelect={(n) => {
                    setViewTake(n);
                    setFocusSidecar(null);
                  }}
                />
                <span class="pt-label">Pipeline</span>
                <span class="pt-count">
                  · {r().steps.length} steps
                  {!timeTraveling() && runningCount() ? ` · ${runningCount()} running` : ""}
                </span>
                <span class="grow1" />
                {/* Fit-to-view + manual zoom (Proposal B, stage 3) — driven
                    through the controls <Dag> hands up on mount. */}
                <span class="dag-zoomctl">
                  <button type="button" title="fit to view" onClick={() => dagControls()?.fit()}>
                    ⤢
                  </button>
                  <button type="button" title="zoom in" onClick={() => dagControls()?.zoomIn()}>
                    ＋
                  </button>
                  <button type="button" title="zoom out" onClick={() => dagControls()?.zoomOut()}>
                    －
                  </button>
                </span>
              </div>
              {/* Read-only banner: while viewing an older version the whole
                  component is a snapshot-at-boundary — say so explicitly. */}
              <Show when={timeTraveling()}>
                <div class="readonly-banner">
                  <span class="rb-eye">👁</span>
                  <span>
                    Viewing <b>{viewedLabel()}</b> — read-only
                    <span class="rb-hint"> · return to latest to act</span>
                  </span>
                </div>
              </Show>

              {/* Blueprint band (full-width, always-dark canvas): shared services
                  + sidecars live AS NODES in the graph (ADR-0058) — shared
                  services as copper peers in a lane at the top with dotted `uses`
                  edges; a step's own sidecars as docked chips on its node.
                  Selecting a service opens ServicePane in the pinned evidence
                  area below. */}
              <div class="dag-band">
                <Dag
                  steps={dagSteps()}
                  services={dagServices()}
                  selected={sel()}
                  onSelect={selectNode}
                  onSelectSidecar={selectSidecar}
                  sidecarFocus={focusSidecar()}
                  onControls={setDagControls}
                />
              </div>

              {/* Evidence pinned directly below the band, full-width, always
                  visible (Proposal B). A shared-service node opens ServicePane
                  (readiness + logs); every other selection is a step → StepPane
                  (ADR-0058) — the same slot, switched by the `service:` selection
                  id. */}
              <div class="evidence-pinned">
                <Show
                  when={isServiceSel()}
                  fallback={
                    <StepPane
                      runId={id()}
                      step={selectedStep()}
                      events={visibleEvents()}
                      attempt={selAttempt()}
                      tries={stripTries()}
                      activeAttempt={stripActiveAttempt()}
                      onAttemptSelect={setSelAttempt}
                      versionLabel={timeTraveling() ? viewedLabel() : null}
                      window={stepWindow(sel())}
                      frontierAttempt={sel() ? scopedView()?.frontier[sel()!] ?? null : null}
                      deadLettered={r().status === "dead_lettered"}
                      canDebug={canShell()}
                      onDebugPod={() => setShellOpen(true)}
                      focusSidecar={focusSidecar()}
                    />
                  }
                >
                  <ServicePane runId={id()} service={selectedService()!} />
                </Show>
              </div>

              {/* Artifacts — run-level files of record, immutable per try
                  (ADR-0056): version-aware footer of the Pipeline component; the
                  bare download is the of-record resolution, shadowed/failed
                  versions download by pinned version. */}
              <div class="artifacts-foot">
                <div class="af-h">
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
            </div>

            <div class="panel activity-panel">
              <div class="panel-h">
                <span>Activity</span>
                <span class="subtle">
                  {visibleEvents().length} events
                  {timeTraveling()
                    ? ` · ${events().length - visibleEvents().length} later hidden by this version`
                    : ""}
                </span>
              </div>
              <div class="tl">
                {/* Newest-first: the rail reads top-down as most-recent → oldest,
                    so a live run's latest activity is always in view without
                    scrolling. `visibleEvents()` stays chronological for StepPane;
                    only this display copy is reversed. */}
                <For
                  each={[...visibleEvents()].reverse()}
                  fallback={<div class="tl-empty">no events yet</div>}
                >
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
