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
  type RunStatus,
  type RunEvent,
  type StepStatus,
  type Artifact,
} from "../api/client";
import { eventParts, eventCategory, EVENT_GLYPH } from "../events";
import { deriveTakes, replayTake, attemptCauses, type Take, type TakeView } from "../takes";
import { relTime, absTime, duration } from "../fmt";
import { forgeCommitUrl, forgePrUrl } from "../forge";
import StatusBadge from "../components/StatusBadge";
import Icon from "../components/Icon";
import Doodle from "../components/Doodle";
import Dag, { type DagStep } from "../components/Dag";
import VersionRail, { type VersionRow, type OutcomeCounts } from "../components/VersionRail";
import ServicesPanel from "../components/ServicesPanel";
import StepPane from "../components/StepPane";
import { type FilmstripTry } from "../components/AttemptsFilmstrip";
import DebugShell from "../components/DebugShell";
import TriggerCell from "../components/TriggerCell";

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
  const [rerunning, setRerunning] = createSignal<string | null>(null);
  const [retrying, setRetrying] = createSignal<string | null>(null);
  const [cancelling, setCancelling] = createSignal(false);
  const [shellOpen, setShellOpen] = createSignal(false);
  // The viewed Take (1-based), or null = latest. Only a CLOSED take is a
  // time-travel view; selecting the latest take clears back to live. Driven by
  // the always-present VersionRail (ADR-0056 amendment): a persistent left rail,
  // a row per Rerun, replacing the former header dropdown (redesign stage 2).
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
  // to the rerun press that closed it.
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
      // Reused (ADR-0056 amendment): while time-traveling, a step with a known
      // (carried-forward) status but ZERO attempts in THIS Take wasn't part of
      // this rerun — render it muted, not as if it re-ran here.
      const reused =
        !!tv &&
        (tv.attempts[s.id] ?? 0) === 0 &&
        ["succeeded", "failed", "skipped"].includes(tv.status[s.id] ?? "");
      return {
        id: s.id,
        status: tv ? (tv.status[s.id] ?? "pending") : s.status,
        attempts: tv ? (tv.attempts[s.id] ?? 0) : s.attempts,
        reused,
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

  // The selected step's tries, resolved from the event log — the markers the
  // attempts filmstrip renders in the evidence-pane header (ADR-0056 amendment).
  // While viewing a closed Take, tries after the boundary are hidden (they
  // belong to a later version and never existed in this snapshot), and causes
  // are derived over the truncated log to match. Mirrors StepPane's
  // `attemptsOf()`/`scoped()`.
  const stripTries = (): FilmstripTry[] => {
    const s = selectedStep();
    if (!s) return [];
    const list = s.attempt_list ?? [];
    // While time-traveling, the tries strip shows only THIS Take's attempts
    // (per-Take scoping, ADR-0056 amendment 2026-07-22) — a step carried
    // forward untouched shows none. Live view shows the full history.
    const win = timeTraveling() && sel() ? (takeView()?.windowAttempts[sel()!] ?? []) : null;
    const visible = win ? list.filter((a) => win.includes(a.id)) : list;
    const c = attemptCauses(visibleEvents(), s.id);
    return visible.map((a, i) => ({
      id: a.id,
      index: i,
      cause: c.causes[a.id],
      // The backend's authoritative verdict (running/succeeded/failed/
      // superseded/cancelled) — the fan reads this so it never shows an
      // abandoned attempt green; the derived flags below stay as fallback.
      outcome: a.outcome,
      failed: a.failed,
      failure: a.failure ?? undefined,
      superseded: c.superseded.has(a.id),
      shadowed: c.shadowed.has(a.id),
      readopted: c.readopted.has(a.id),
    }));
  };
  // The try scoping the evidence pane: explicit selection, else the Take
  // frontier, else the latest — the same resolution StepPane's `scoped()` uses,
  // so the filmstrip's highlight and the pane always agree.
  const stripActiveAttempt = (): string | null => {
    const ts = stripTries();
    if (!ts.length) return null;
    const want = selAttempt() ?? (sel() ? (takeView()?.frontier[sel()!] ?? null) : null);
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

  const openedBy = (t: Take): string | null =>
    t.n <= 1 ? null : (takes()[t.n - 2]?.closedByTarget ?? null);
  const rowLabel = (t: Take): string => {
    const by = openedBy(t);
    return by ? `you reran ${by}` : "original run";
  };
  const rowTime = (t: Take): number | null =>
    t.n <= 1 ? startedAt() : (takes()[t.n - 2]?.closedAt ?? null);
  // Who pressed the Rerun that opened this Take (the acting principal, `null`
  // when auth is off or for the original run).
  const rowActor = (t: Take): string | null =>
    t.n <= 1 ? null : (takes()[t.n - 2]?.closedBy ?? null);
  // The row's second line: on the latest row show what opened it ("you reran b"),
  // then the actor and time — enough provenance without a banner.
  const rowSub = (t: Take): string => {
    const parts: string[] = [];
    if (t.n === latestTakeN() && t.n > 1) parts.push(rowLabel(t));
    const who = rowActor(t);
    if (who) parts.push(`by ${who}`);
    const at = rowTime(t);
    if (at) parts.push(relTime(at));
    return parts.join(" · ");
  };
  const viewedLabel = (): string => {
    const v = viewing();
    return v ? rowLabel(v) : "latest";
  };

  // Map a step status into an outcome bucket for the rail's mini-summary. Only
  // the five named statuses get their own accent; everything else (pending,
  // skipped, ready, waiting, cancelled, …) falls to `other`.
  const bucketOf = (status: string): keyof OutcomeCounts => {
    switch (status) {
      case "succeeded":
        return "succeeded";
      case "failed":
        return "failed";
      case "superseded":
        return "superseded";
      case "not_run":
        return "notRun";
      case "running":
        return "running";
      default:
        return "other";
    }
  };
  const tally = (statuses: string[]): OutcomeCounts => {
    const c: OutcomeCounts = {
      succeeded: 0,
      failed: 0,
      superseded: 0,
      notRun: 0,
      running: 0,
      other: 0,
    };
    for (const s of statuses) c[bucketOf(s)] += 1;
    return c;
  };
  // The version rail's rows (ADR-0056 amendment): one per Take, newest-first in
  // the rail. The latest/open Take tallies live `run().steps[].status`; a closed
  // Take tallies its snapshot-at-boundary replay. Takes are few — re-replaying
  // per closed row each render is cheap. Reuses the dropdown's label/sub helpers.
  const versions = (): VersionRow[] => {
    const ts = takes();
    const latest = latestTakeN();
    const selN = viewTake() ?? latest;
    return ts.map((t) => {
      const isLatest = t.n === latest;
      const statuses = isLatest
        ? stepList().map((s) => s.status)
        : Object.values(replayTake(events(), ts, t).status);
      return {
        n: t.n,
        label: isLatest ? "latest" : rowLabel(t),
        sub: rowSub(t),
        summary: tally(statuses),
        isLatest,
        isSelected: selN === t.n,
      };
    });
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
                onClick={() => sel() && onRerun(sel()!)}
                disabled={
                  rerunning() !== null ||
                  retrying() !== null ||
                  !sel() ||
                  timeTraveling() ||
                  !!prereqBlocker(sel())
                }
                title={
                  timeTraveling()
                    ? "read-only — back to latest to rerun"
                    : !sel()
                      ? "select a step"
                      : prereqBlocker(sel())
                        ? `blocked — prerequisite ${prereqBlocker(sel())} failed; rerun that first`
                        : `rerun ${sel()} and everything downstream — forks a new version`
                }
              >
                <Icon icon="rotate-ccw" size={13} />{" "}
                {rerunning() ? "rerunning…" : "Rerun pipeline from this step"}
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
                  timeTraveling()
                    ? "read-only — back to latest to act"
                    : "cancel this run and tear down its steps"
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

            {/* Pipeline component (ADR-0056 amendment): DAG + the selected
                step's evidence + a version-aware Artifacts footer, beside the
                always-present VersionRail (redesign stage 2). Zoom out = which
                whole-run version (the rail); zoom in = which try of a step (the
                strip inside StepPane). An older version turns the whole
                component read-only. On narrow viewports the rail collapses to a
                horizontal strip above the panel (see .pipeline-with-rail). */}
            <div class="pipeline-with-rail">
              <VersionRail
                rows={versions()}
                live={live() && !timeTraveling()}
                onSelect={(n) => setViewTake(n)}
              />
            <div class="panel pipeline-panel" classList={{ readonly: timeTraveling() }}>
              <div class="panel-h">
                <span>Pipeline</span>
                <span class="subtle">
                  {r().steps.length} steps
                  {!timeTraveling() && runningCount() ? ` · ${runningCount()} running` : ""}
                </span>
                <span class="grow1" />
              </div>
              {/* Read-only banner (redesign stage 2): while viewing an older
                  version the whole component is a snapshot-at-boundary — say so
                  explicitly, now that the dropdown's "👁" label is gone. */}
              <Show when={timeTraveling()}>
                <div class="readonly-banner">
                  <span class="rb-eye">👁</span>
                  <span>
                    Viewing <b>{viewedLabel()}</b> — read-only
                    <span class="rb-hint"> · return to latest to act</span>
                  </span>
                </div>
              </Show>

              <div class="rd-grid">
                <div class="dag-wrap">
                  <div class="dag-head">Steps</div>
                  <Dag steps={dagSteps()} selected={sel()} onSelect={setSel} />
                  {/* Shared services (ADR-0058) live BESIDE the DAG, never as
                      nodes in it — a compact evidence section beneath the graph
                      in the same left column. Renders nothing when there are
                      none (the common case). */}
                  <ServicesPanel runId={id()} live={live() && !timeTraveling()} />
                </div>

                <StepPane
                runId={id()}
                step={selectedStep()}
                events={visibleEvents()}
                attempt={selAttempt()}
                tries={stripTries()}
                activeAttempt={stripActiveAttempt()}
                onAttemptSelect={setSelAttempt}
                versionLabel={viewedLabel()}
                frontierAttempt={sel() ? takeView()?.frontier[sel()!] ?? null : null}
                deadLettered={r().status === "dead_lettered"}
                canDebug={canShell()}
                onDebugPod={() => setShellOpen(true)}
              />
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
