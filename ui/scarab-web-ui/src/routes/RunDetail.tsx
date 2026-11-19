// Run detail, top to bottom:
// - the run header: breadcrumb, `#N` and the run's title, status, New run or
//   Cancel run, a ⋯ menu (Keep workspace snapshots, Copy run ID), and a
//   provenance line (trigger, branch and commit, pipeline, timing, and the
//   version selector);
// - a notice slot: gate approvals, the read-only banner of an older version,
//   the yank note, the retention note and the run's policy decisions;
// - a summary line: which steps failed first, or what is running and waiting;
// - the run tabs: Pipeline, Artifacts and Activity;
// - in Pipeline, the step graph or the step list, then the step pane for the
//   selected step: its header and actions, its try tabs, the selected try's
//   failure box, and its content tabs. The step pane stays mounted while
//   another run tab is open, so a log stream does not restart;
// - in Artifacts, the run's files grouped by step; in Activity, its events.
// Versions and tries are derived from the event log; "Take" and "Attempt"
// never show. Picking an older version turns the page into a read-only view
// of the run as it stood then. The selected step, version, try and tab live in
// the URL (steps by index, never by name).
import {
  batch,
  createSignal,
  createEffect,
  createMemo,
  createResource,
  on,
  onMount,
  onCleanup,
  For,
  Show,
} from "solid-js";
import { useParams, useNavigate, useSearchParams } from "@solidjs/router";
import {
  getRun,
  fetchEvents,
  rerunStep,
  retryStep,
  cancelRun,
  approveGate,
  rejectGate,
  isTerminal,
  runParams,
  listArtifacts,
  artifactUrl,
  repoForgeUrl,
  listServices,
  fetchRerunPlan,
  pinSnapshots,
  unpinSnapshots,
  type RunStatus,
  type RunEvent,
  type StepStatus,
  type Artifact,
  type Service,
  type RerunPlan,
  type RetentionInfo,
} from "../api/client";
import { activeInfraConditions, infraReasonText, NODE_WAIT_REASONS, type ActiveInfraCondition } from "../events";
import {
  deriveTakes,
  replayTake,
  versionRows,
  stepTiming,
  attemptPlaces,
  visibleArtifacts as visibleArtifactsIn,
  type Take,
  type TakeView,
} from "../takes";
import { stripTries as stripTriesOf } from "../attempts";
import {
  actionableGateIds,
  actionableGates,
  addedByEnvironment,
  addedByLabel,
  approvalNote,
  approvalReleaseIn,
  environmentGateLabel,
  gateBlockers,
  gateStatus,
  gateWaitHint,
  isApprovable,
  isRejectable,
  rejectReasonError,
} from "../gates";
import { isWidened, rerunLabel, rerunTitle, retentionNote, widenedNote } from "../snapshot-retention";
import { planDivergence, type ConfirmKind } from "../rerun-confirm";
import { clock, clockLow, isYankRefusal, rerunYank, yankNote } from "../run-origin";
import {
  clampRunUrl,
  defaultSelection,
  parseRunUrl,
  runSummary,
  runUrlParams,
  stepDisplayName,
  type PaneTabParam,
  type RunTabParam,
  type RunUrlState,
} from "../run-page";
import { activityView, artifactFilterCounts, artifactsInScope, triggerLine } from "../run-tabs";
import { relTime, absTime, duration } from "../fmt";
import { forgeCommitUrl, forgePrUrl } from "../forge";
import { triggerIcon, triggerText } from "../trigger";
import StatusBadge from "../components/StatusBadge";
import RunPolicies from "../components/RunPolicies";
import Icon from "../components/Icon";
import { HomeCrumb, RepoCrumb } from "../components/PageCrumbs";
import Doodle from "../components/Doodle";
import Dag, { type DagStep, type DagService, type DagControls } from "../components/Dag";
import StepList from "../components/StepList";
import VersionDropdown, { type VersionRow } from "../components/VersionDropdown";
import StepPane, { type PaneTab } from "../components/StepPane";
import ServicePane from "../components/ServicePane";
import { type FilmstripTry } from "../components/AttemptsDropdown";
import DebugShell from "../components/DebugShell";
import RerunConfirm from "../components/RerunConfirm";
import CancelRunConfirm from "../components/CancelRunConfirm";
import RunMoreMenu from "../components/RunMoreMenu";
import RunArtifacts from "../components/RunArtifacts";
import RunActivity from "../components/RunActivity";

const POLL_MS = 1200;

/** The step tab as the URL names it (absent = Logs; a gate's tab is not kept). */
const paneTabParam = (t: PaneTab): PaneTabParam | undefined =>
  t === "results" || t === "workspace" || t === "outputs" || t === "source" ? t : undefined;

/** A run tab: Pipeline (absent from the URL), Artifacts or Activity. */
type RunTab = "pipeline" | RunTabParam;

export default function RunDetail() {
  const params = useParams();
  const nav = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const id = () => params.id!;
  const org = () => params.org!;
  const repo = () => params.repo!;

  // The URL's selection, read once: it beats the default selection on load.
  const urlInit: RunUrlState = parseRunUrl(searchParams);
  // The open run tab, and the step the Artifacts tab is narrowed to (set by a
  // step header's "N artifacts" link or the URL's `from`, cleared by "Show
  // all").
  const [runTab, setRunTab] = createSignal<RunTab>(urlInit.tab ?? "pipeline");
  const [artifactStep, setArtifactStep] = createSignal<string | null>(null);
  const openArtifacts = (step: string | null) => {
    setArtifactStep(step);
    setRunTab("artifacts");
  };

  const [run, setRun] = createSignal<RunStatus | null>(null);
  // A 1s tick for plugin gate expiry clocks and the step header's timers.
  const [nowTick, setNowTick] = createSignal(Date.now());
  const clockTimer = setInterval(() => setNowTick(Date.now()), 1000);
  onCleanup(() => clearInterval(clockTimer));
  /** Time left on an open plugin gate's wait, in ms, or null. */
  const gateLeft = (s: StepStatus): number | null => {
    const at = s.plugin_gate?.expires_at;
    return at != null ? at - nowTick() : null;
  };
  /** The Environment the Run names, with its display name. */
  const runEnv = () => run()?.environment ?? null;
  const [events, setEvents] = createSignal<RunEvent[]>([]);
  const [live, setLive] = createSignal(true);
  const [sel, setSel] = createSignal<string | null>(null);
  const [approving, setApproving] = createSignal<string | null>(null);
  const [gateNotes, setGateNotes] = createSignal<Record<string, string>>({});
  // Reject: the gate whose in-page reason form is open, its draft reason, and
  // the gate a reject is in flight for. Confirmation is the form itself (a
  // required reason and an explicit second click), never a browser
  // `confirm()`, which the embedded viewers refuse.
  const [rejectFor, setRejectFor] = createSignal<string | null>(null);
  const [rejectReason, setRejectReason] = createSignal("");
  const [rejecting, setRejecting] = createSignal<string | null>(null);
  // The try the user picked, or null to follow the latest.
  const [selAttempt, setSelAttempt] = createSignal<string | null>(null);
  // A step's docked sidecar the user clicked: its `services:` index, or null.
  // Focuses that container in the StepPane Logs tab. Cleared whenever the
  // selection moves to a node or the version changes.
  const [focusSidecar, setFocusSidecar] = createSignal<number | null>(null);
  // Graph or List in the Pipeline panel.
  const [view, setView] = createSignal<"graph" | "list">("graph");
  // The step tab, mirrored from StepPane for the URL.
  const [paneTab, setPaneTab] = createSignal<PaneTab>(urlInit.ptab ?? "logs");
  // The initial selection (URL, else default) has been applied; the URL is
  // written only after that.
  const [loaded, setLoaded] = createSignal(false);
  // A try to select once the step/version change below has reset the try.
  let pendingTry: string | null = null;
  // A shared service the URL named, applied once the services list loads.
  let pendingService: number | null = null;

  // Selecting a node (step OR shared service) clears any sidecar focus;
  // clicking a docked sidecar chip selects its step AND focuses that container.
  const selectNode = (target: string) => {
    setSel(target);
    setFocusSidecar(null);
  };
  const selectSidecar = (stepId: string, index: number) => {
    setSel(stepId);
    setFocusSidecar(index);
  };
  // The graph's zoom controls, handed up by <Dag> on mount so the panel
  // header's fit / ＋ / － buttons can drive them.
  const [dagControls, setDagControls] = createSignal<DagControls | null>(null);
  const [rerunning, setRerunning] = createSignal<string | null>(null);
  const [retrying, setRetrying] = createSignal<string | null>(null);
  const [cancelling, setCancelling] = createSignal(false);
  // The rerun PREVIEW for the selected step: whether expired workspace
  // snapshots widen the rerun upstream, resolved before the click so the
  // button can say which it is.
  const [plan, setPlan] = createSignal<RerunPlan | null>(null);
  // "Preview still loading" vs "preview FAILED", for the confirm popover: both
  // still allow confirm (disclosure, never prevention).
  const [planFailed, setPlanFailed] = createSignal(false);
  // The workspace-snapshot pin, mirrored locally so the toggle re-renders from
  // what the pin endpoint returned.
  const [pinState, setPinState] = createSignal<RetentionInfo | null>(null);
  const [pinning, setPinning] = createSignal(false);
  const [shellOpen, setShellOpen] = createSignal(false);
  // The viewed version (1-based), or null = latest. Only a CLOSED version is a
  // read-only view; picking the latest clears back to live.
  const [viewTake, setViewTake] = createSignal<number | null>(null);

  const stepList = (): StepStatus[] => run()?.steps ?? [];

  // --- Versions: derived from the event log, stored nowhere. ---
  const takes = (): Take[] => deriveTakes(events());
  const latestTakeN = () => takes().length;
  const viewing = (): Take | null => {
    const n = viewTake();
    if (n === null || n >= latestTakeN()) return null;
    return takes().find((t) => t.n === n) ?? null;
  };
  const timeTraveling = () => viewing() !== null;
  // A closed version's view is a replay of the log up to the rerun that
  // closed it. Null on the latest version.
  const takeView = (): TakeView | null => {
    const t = viewing();
    return t ? replayTake(events(), takes(), t) : null;
  };
  // The version whose tries the page shows, the latest included.
  const selectedTake = (): Take | null => {
    const ts = takes();
    if (!ts.length) return null;
    const n = viewTake();
    if (n === null) return ts[ts.length - 1];
    return ts.find((t) => t.n === n) ?? ts[ts.length - 1];
  };
  // The selected version's replay, computed for the latest version too: its
  // window runs from the last rerun to now, so streaming tries fold in live.
  const scopedView = (): TakeView | null => {
    const t = selectedTake();
    return t ? replayTake(events(), takes(), t) : null;
  };
  // The selected version's tries of a step: `[]` for a step carried forward
  // untouched, so it reads as "didn't run in this version".
  const stepWindow = (stepId: string | null): string[] | null =>
    stepId ? (scopedView()?.windowAttempts[stepId] ?? []) : null;
  // Events as of the version in view: truncated at the boundary while viewing
  // an older version.
  const visibleEvents = () => {
    const t = viewing();
    return t ? events().slice(0, t.endIdx) : events();
  };

  // Selection id space: a step is `"<stepId>"`; a shared service is
  // `"service:<name>"`. A service selection routes to ServicePane.
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
  // Rerun/retry are blocked when a prerequisite FAILED: only {succeeded,
  // skipped} prerequisites permit the action. Uses live statuses (the action
  // targets the latest version).
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
  // fresh debug Pod. Debug pod is the ONE action allowed while viewing an older
  // version (it mutates nothing durable).
  const shellMode = (): "attach" | "debug-pod" => (selectedRunning() ? "attach" : "debug-pod");
  const canShell = () =>
    !!sel() && ["running", "succeeded", "failed"].includes(String(selectedStatus() ?? ""));

  // Per-step wall-clock from the event log, scoped to the version in view.
  const timingOf = (): Record<string, { start?: number; end?: number }> =>
    stepTiming(visibleEvents(), scopedView()?.windowAttempts ?? null);

  // The infra conditions holding a running step's current try back right now
  // (no node, an image that will not pull). Live view only.
  const heldNow = createMemo((): Record<string, ActiveInfraCondition> => {
    if (timeTraveling()) return {};
    const infra = activeInfraConditions(events());
    const out: Record<string, ActiveInfraCondition> = {};
    for (const s of stepList()) {
      const h = infra[s.id];
      const latest = s.attempt_list?.[s.attempt_list.length - 1]?.id;
      if (s.status === "running" && h && (!latest || h.attempt === latest)) out[s.id] = h;
    }
    return out;
  });

  // Reused: a step with a carried-forward verdict but no tries in the version
  // in view (a partial rerun left it alone). A step with no tries that is
  // pending (re-armed by a rerun, not started yet) was not reused.
  const isReused = (status: string | undefined, attempts: number): boolean =>
    !!scopedView() && attempts === 0 && ["succeeded", "failed", "skipped"].includes(status ?? "");
  const selectedReused = (): boolean => {
    const id = sel();
    if (!id) return false;
    const sv = scopedView();
    const tv = takeView();
    const status = tv ? (tv.status[id] ?? "pending") : selectedStep()?.status;
    return isReused(status, sv ? (sv.attempts[id] ?? 0) : (selectedStep()?.attempts ?? 0));
  };

  // The selected step's Pod has no node yet: there is nothing to shell into.
  const unscheduled = (): boolean => {
    const h = sel() ? heldNow()[sel()!] : undefined;
    return !!h && NODE_WAIT_REASONS.has(h.reason);
  };

  // Graph nodes: the graph shape + status. Live: the run object. An older
  // version: the replayed statuses and try counts as of its boundary.
  const dagSteps = (): DagStep[] => {
    const timing = timingOf();
    const tv = takeView();
    const sv = scopedView();
    const held = heldNow();
    return stepList().map((s) => {
      const t = timing[s.id];
      const h = held[s.id];
      const status = tv ? (tv.status[s.id] ?? "pending") : s.status;
      // Tries in the version in view only.
      const win = sv ? (sv.windowAttempts[s.id] ?? []) : null;
      const attempts = sv ? (sv.attempts[s.id] ?? 0) : s.attempts;
      const failedTries = (s.attempt_list ?? []).filter(
        (a) => (win === null || win.includes(a.id)) && a.outcome === "failed",
      ).length;
      const reused = isReused(status, attempts);
      return {
        id: s.id,
        status,
        attempts,
        failedTries,
        reused,
        needs: s.needs ?? [],
        gate: s.gate,
        deploy: s.deploy,
        // The gate verdict line describes the run NOW, so it is dropped while
        // viewing an older version.
        note: tv
          ? null
          : ((h
              ? {
                  tone: "warning" as const,
                  text: infraReasonText(h.reason),
                  detail: h.message ? `${h.reason}: ${h.message}` : h.reason,
                  since: h.since,
                }
              : null) ??
            gateStatus(s) ??
            (s.attempt_list?.[s.attempt_list.length - 1]?.failure === "config" &&
            isYankRefusal(s.attempt_list[s.attempt_list.length - 1]!.failure_detail)
              ? { tone: "failed" as const, text: "refused · yanked" }
              : null) ??
            (s.plugin_gate && gateLeft(s) != null && ["pending", "ready"].includes(s.status)
              ? { tone: "waiting" as const, text: `${clock(gateLeft(s)!)} left` }
              : null) ??
            (approvalReleaseIn(s, nowTick()) != null
              ? { tone: "waiting" as const, text: `releases in ${clock(approvalReleaseIn(s, nowTick())!)}` }
              : null)),
        approvalGate: environmentGateLabel(s, runEnv()) !== null,
        addedBy: addedByLabel(s, runEnv()),
        runningSince:
          !tv && s.status === "running" ? (s.attempt_list?.[s.attempt_list.length - 1]?.started_at ?? null) : null,
        durationMs: t?.start != null && t?.end != null ? t.end - t.start : null,
        services: s.services,
        uses: s.uses,
        origin: s.origin,
      };
    });
  };

  // A library version this Run uses that is yanked now: a Rerun or Retry
  // that would launch a Step from it is refused every time.
  const yanks = () => run()?.yanked_libraries ?? [];
  const rerunBlockedByYank = () =>
    rerunYank(sel(), stepList().map((s) => ({ id: s.id, needs: s.needs ?? [] })), yanks());

  // The selected step's tries in the version in view, from the event log.
  const stripTries = (): FilmstripTry[] => {
    const s = selectedStep();
    if (!s) return [];
    return stripTriesOf(visibleEvents(), s.id, s.attempt_list ?? [], stepWindow(sel()));
  };
  // The try scoping the step pane: the picked one, else the version's
  // frontier, else the latest. The same resolution StepPane uses.
  const stripActiveAttempt = (): string | null => {
    const ts = stripTries();
    if (!ts.length) return null;
    const want = selAttempt() ?? (sel() ? (scopedView()?.frontier[sel()!] ?? null) : null);
    return ts.find((t) => t.id === want)?.id ?? ts[ts.length - 1].id;
  };
  // An older try is selected: Debug pod reproduces the latest one, so it is
  // disabled until the latest is selected.
  const olderTrySelected = () => {
    const ts = stripTries();
    return ts.length > 1 && stripActiveAttempt() !== ts[ts.length - 1].id;
  };

  // The repo's forge web base (for commit/PR deep links), from the registry.
  const [repoUrl] = createResource(
    () => [org(), repo()] as const,
    ([o, r]) => repoForgeUrl(o, r).catch(() => null),
  );

  const [artifacts, { refetch: refetchArtifacts }] = createResource(id, (rid) =>
    listArtifacts(rid).catch(() => [] as Artifact[]),
  );

  // Shared services render as peer nodes in the graph. Polled alongside a
  // live run; a terminal run's set is fixed.
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

  // The graph's shared-service nodes: the `/services` projection UNION every
  // name a step declares in `uses:`, so a declared but unstarted service still
  // shows as pending.
  const dagServices = (): DagService[] => {
    const fromApi = services() ?? [];
    const byName = new Map(fromApi.map((s) => [s.name, s]));
    const names: string[] = fromApi.map((s) => s.name);
    for (const s of stepList()) {
      for (const u of s.uses ?? []) if (!names.includes(u)) names.push(u);
    }
    return names.map((name) => ({ name, status: byName.get(name)?.status ?? "pending" }));
  };

  // Artifact copies visible in the version in view (pure, in takes.ts).
  const visibleArtifacts = (): Artifact[] => visibleArtifactsIn(artifacts() ?? [], takeView());
  const distinctNames = (rows: Artifact[]) => new Set(rows.map((a) => a.name)).size;
  // The step header's count: what the Artifacts tab shows when narrowed to
  // that step (one copy per file of that step).
  const stepArtifacts = (stepId: string | null): number =>
    stepId ? artifactFilterCounts(artifactsInScope(visibleArtifacts(), stepId)).record : 0;

  let poll: ReturnType<typeof setInterval> | undefined;

  // Timing from the event log: first event = created; the view's end is the
  // boundary instant while viewing an older version, else the latest event.
  const startedAt = () =>
    visibleEvents().length ? Math.min(...visibleEvents().map((e) => e.at)) : null;
  const finishedAt = () => {
    const t = viewing();
    if (t) return t.closedAt;
    return run() && isTerminal(run()!.status) && events().length
      ? Math.max(...events().map((e) => e.at))
      : null;
  };

  // Gates a person can act on now: open, and nothing before them unfinished.
  const readyGates = (steps: StepStatus[]): string[] => {
    const status = (sid: string) => steps.find((s) => s.id === sid)?.status;
    return actionableGates(steps)
      .filter((g) => gateBlockers(g, status).length === 0)
      .map((g) => g.id);
  };

  // The first selection: the URL's, where it exists in this run, else the
  // default (a gate waiting for a decision, the first root failure, …).
  function applyInitialSelection(status: RunStatus, evs: RunEvent[]) {
    const ts = deriveTakes(evs);
    const triesIn = (stepIdx: number, version: number): string[] => {
      const t = ts[version - 1];
      const step = status.steps[stepIdx];
      return t && step ? (replayTake(evs, ts, t).windowAttempts[step.id] ?? []) : [];
    };
    const want = clampRunUrl(urlInit, {
      steps: status.steps.length,
      services: null,
      versions: ts.length,
      triesOf: (i, v) => triesIn(i, v).length,
    });
    if (want.v !== undefined) setViewTake(want.v);
    if (want.from !== undefined) setArtifactStep(status.steps[want.from]!.id);
    if (want.step !== undefined) {
      if (want.try !== undefined) pendingTry = triesIn(want.step, want.v ?? ts.length)[want.try - 1] ?? null;
      setSel(status.steps[want.step]!.id);
      return;
    }
    if (want.service !== undefined) pendingService = want.service;
    const infra = activeInfraConditions(evs);
    const held = status.steps.filter((s) => s.status === "running" && infra[s.id]).map((s) => s.id);
    setSel(defaultSelection({ steps: status.steps, gates: readyGates(status.steps), held }));
  }

  // How many tries have finished, last time the run was read: the artifact
  // list is read again when it changes, since a finished try may have
  // published files.
  let finishedTries: number | null = null;

  async function refresh() {
    const [status, evs] = await Promise.all([
      getRun(id()),
      // A failed read keeps the events already shown (null), rather than
      // replacing them with none.
      fetchEvents(id()).catch(() => null),
    ]);
    batch(() => {
      setRun(status);
      if (evs) setEvents(evs);
      // First load only: later polls never move the selection. It waits for
      // the events: without them the run has one version and no tries, and
      // the URL's version and try would be dropped. The next poll tries again.
      if (!loaded() && status.steps.length && evs) {
        applyInitialSelection(status, evs);
        setLoaded(true);
      }
    });
    const finished = status.steps.reduce(
      (n, s) => n + (s.attempt_list ?? []).filter((a) => a.outcome !== "running").length,
      0,
    );
    let readArtifacts = finishedTries !== null && finished !== finishedTries;
    finishedTries = finished;
    if (isTerminal(status.status)) {
      if (live()) readArtifacts = true;
      setLive(false);
      // Keep polling until the events have loaded once.
      if (poll && (loaded() || !status.steps.length)) {
        clearInterval(poll);
        poll = undefined;
      }
    }
    if (readArtifacts) void refetchArtifacts();
  }

  onMount(async () => {
    await refresh();
    if (live() || !loaded()) poll = setInterval(() => void refresh(), POLL_MS);
  });
  onCleanup(() => {
    if (poll) clearInterval(poll);
  });

  // Selecting a step or switching versions resets the try to follow the
  // latest, unless a link asked for a particular try (`pendingTry`).
  createEffect(
    on([sel, viewTake], () => {
      setSelAttempt(pendingTry);
      pendingTry = null;
    }),
  );

  // A shared service the URL named: selected once the list has loaded, or
  // dropped when the run has no service at that index.
  createEffect(() => {
    if (pendingService === null || services.state !== "ready") return;
    const list = dagServices();
    const n = pendingService;
    pendingService = null;
    if (n < list.length) selectNode(`${SERVICE_PREFIX}${list[n]!.name}`);
  });

  // The URL follows the selection, with `replace` so a click adds no history
  // entry. Compared as a string so a poll that changes nothing writes nothing.
  // A key to remove is `undefined`, which JSON drops, so it travels as `null`
  // and turns back into `undefined` for the router.
  const urlState = createMemo((): string => {
    if (!loaded()) return "";
    const r = run();
    if (!r) return "";
    const s = sel();
    const st: RunUrlState = {};
    const rt = runTab();
    if (rt !== "pipeline") st.tab = rt;
    const only = artifactStep();
    if (rt === "artifacts" && only) {
      const i = r.steps.findIndex((x) => x.id === only);
      if (i >= 0) st.from = i;
    }
    const pt = paneTabParam(paneTab());
    if (pt) st.ptab = pt;
    if (s?.startsWith(SERVICE_PREFIX)) {
      const i = dagServices().findIndex((x) => `${SERVICE_PREFIX}${x.name}` === s);
      if (i >= 0) st.service = i;
    } else if (s) {
      const i = r.steps.findIndex((x) => x.id === s);
      if (i >= 0) st.step = i;
      const a = selAttempt();
      if (a) {
        const ti = stripTries().findIndex((t) => t.id === a);
        if (ti >= 0) st.try = ti + 1;
      }
    }
    const v = viewing();
    if (v) st.v = v.n;
    return JSON.stringify(runUrlParams(st), (_k, v: unknown) => (v === undefined ? null : v));
  });
  createEffect(
    on(urlState, (json) => {
      if (!json) return;
      const parsed = JSON.parse(json) as Record<string, string | null>;
      const next: Record<string, string | undefined> = {};
      for (const [k, v] of Object.entries(parsed)) next[k] = v ?? undefined;
      setSearchParams(next, { replace: true, scroll: false });
    }),
  );

  // Fetch the rerun PREVIEW for the selected step. Lazy and per selection: the
  // preview asks the object store whether each input snapshot is still there,
  // far too expensive for a 1.2s poll. Fetched for live runs too: the cascade
  // exists there as well, and the confirm popover has to name it.
  createEffect(() => {
    const step = sel();
    // Clear unconditionally, so a previous step's plan never renders under
    // this step's headline.
    setPlan(null);
    setPlanFailed(false);
    if (!step || isServiceSel() || timeTraveling()) {
      return;
    }
    const forRun = id();
    void fetchRerunPlan(forRun, step)
      .then((p) => {
        if (sel() === step && id() === forRun) setPlan(p);
      })
      // A failed preview must NOT relabel or block the rerun: unknown is not
      // "expired".
      .catch(() => {
        if (sel() === step && id() === forRun) setPlanFailed(true);
      });
  });

  // The retention promise: the server's value, overlaid by whatever the pin
  // endpoint last returned.
  const retention = (): RetentionInfo | null => pinState() ?? run()?.snapshot_retention ?? null;

  async function onTogglePin() {
    const r = retention();
    if (!r) return;
    setPinning(true);
    try {
      setPinState(r.pinned ? await unpinSnapshots(id()) : await pinSnapshots(id()));
    } finally {
      setPinning(false);
    }
  }
  // A different run drops the local overlay.
  createEffect(() => {
    void id();
    setPinState(null);
  });

  // The executed plan was wider than the one the user confirmed (a snapshot
  // expired between preview and click): say so after the fact.
  const [toast, setToast] = createSignal<string | null>(null);
  let toastTimer: ReturnType<typeof setTimeout> | undefined;
  function showToast(msg: string) {
    setToast(msg);
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => setToast(null), 10_000);
  }
  onCleanup(() => {
    if (toastTimer) clearTimeout(toastTimer);
  });

  // The confirm popover: the first click on Rerun/Retry opens it; the POST
  // fires only from its confirm button.
  const [confirming, setConfirming] = createSignal<{
    kind: ConfirmKind;
    step: string;
    top: number;
    left: number;
  } | null>(null);
  function openConfirm(kind: ConfirmKind, step: string, e: MouseEvent) {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    setConfirming({ kind, step, top: r.bottom + 6, left: r.left });
  }
  // The plan FOR this step, or null: a plan for another step is a stale
  // leftover mid-fetch.
  const planFor = (step: string): RerunPlan | null => {
    const p = plan();
    return p && p.target === step ? p : null;
  };
  // A moved selection or version switch invalidates what the popover named.
  createEffect(() => {
    void sel();
    void viewTake();
    setConfirming(null);
  });

  async function onRerun(step: string) {
    const previewed = planFor(step);
    setRerunning(step);
    try {
      const executed = await rerunStep(id(), step);
      const note = planDivergence(previewed, executed, "rerun");
      if (note) showToast(note);
      setLive(true);
      await refresh();
      if (!poll && live()) poll = setInterval(() => void refresh(), POLL_MS);
    } finally {
      setRerunning(null);
    }
  }

  // Retry: another try in the CURRENT version, no fork. Failed steps only.
  async function onRetry(step: string) {
    const previewed = planFor(step);
    setRetrying(step);
    try {
      const executed = await retryStep(id(), step);
      const note = planDivergence(previewed, executed, "retry");
      if (note) showToast(note);
      setLive(true);
      await refresh();
      if (!poll && live()) poll = setInterval(() => void refresh(), POLL_MS);
    } finally {
      setRetrying(null);
    }
  }

  // --- Gate approval ---
  // A manual gate holds StepStatus::Pending until release flips it to
  // Succeeded. A run can suspend on SEVERAL gates, so this is a list and every
  // gate carries its own action; there is no run-level "approve" button, which
  // would be ambiguous about what it approved. Reject is offered for every gate
  // kind; a gate whose needs are unmet lists with both disabled.
  const pendingGates = () => actionableGates(run()?.steps ?? []);
  // The gate bar iterates IDs, which survive a poll that replaces every step
  // object, so an open row (and its Reject form) is never re-mounted.
  const pendingGateIds = () => actionableGateIds(run()?.steps ?? []);
  const gateStep = (gid: string) => pendingGates().find((s) => s.id === gid);

  const statusOf = (step: string) => (run()?.steps ?? []).find((s) => s.id === step)?.status;

  const blockersOf = (g: StepStatus) => gateBlockers(g, statusOf);

  async function onApprove(step: string) {
    setApproving(step);
    try {
      const res = await approveGate(id(), step);
      // `released: false` is normal: the approval counted, and either more
      // approvals are needed or the wait timer still runs.
      setGateNotes({
        ...gateNotes(),
        [step]: approvalNote(res, (ms) => new Date(ms).toLocaleTimeString()),
      });
      setLive(true);
      await refresh();
      if (!poll && live()) poll = setInterval(() => void refresh(), POLL_MS);
    } catch (e) {
      setGateNotes({ ...gateNotes(), [step]: e instanceof Error ? e.message : "approve failed" });
    } finally {
      setApproving(null);
    }
  }

  function openReject(step: string) {
    setRejectFor(step);
    setRejectReason("");
  }

  async function onReject(step: string) {
    const reason = rejectReason();
    const invalid = rejectReasonError(reason);
    if (invalid) {
      setGateNotes({ ...gateNotes(), [step]: invalid });
      return;
    }
    setRejecting(step);
    try {
      await rejectGate(id(), step, reason.trim());
      setGateNotes({ ...gateNotes(), [step]: "rejected" });
      setRejectFor(null);
      setRejectReason("");
      setLive(true);
      await refresh();
      if (!poll && live()) poll = setInterval(() => void refresh(), POLL_MS);
    } catch (e) {
      // The server's own words; the reason form stays open to fix and retry.
      setGateNotes({ ...gateNotes(), [step]: e instanceof Error ? e.message : "reject failed" });
    } finally {
      setRejecting(null);
    }
  }

  // Cancel run asks first, like Rerun and Retry.
  const [confirmingCancel, setConfirmingCancel] = createSignal<{ top: number; left: number } | null>(
    null,
  );
  async function onCancel() {
    setConfirmingCancel(null);
    setCancelling(true);
    try {
      await cancelRun(id());
    } catch (e) {
      showToast(e instanceof Error ? e.message : "Could not cancel the run.");
    } finally {
      setCancelling(false);
    }
    // Read the run again either way: a refused cancel may mean it already
    // finished.
    await refresh().catch(() => undefined);
  }
  // A run that finished while the confirm was open has nothing to cancel.
  createEffect(() => {
    const r = run();
    if (r && isTerminal(r.status)) setConfirmingCancel(null);
  });

  // Run provenance lives in the event log's trigger event (a `Raw` payload).
  const triggerInfo = (): {
    kind?: string;
    actor?: string;
    branch?: string;
    ref?: string;
    sha?: string;
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
  // The trigger in words: "PR #131", "push", "manual".
  const triggerWords = (): string | null => {
    const t = triggerInfo();
    if (!t?.kind) return null;
    return t.kind === "pull_request" && t.pr != null ? `PR #${t.pr}` : triggerText(t.kind);
  };

  // A PR run's head branch for the "base ← head" fact; a branch that is only
  // the sha is not repeated.
  const headBranch = (t: { branch?: string; sha?: string }): string | null =>
    t.branch && t.branch !== t.sha ? t.branch : null;

  // Every attempt's version and try number, so an artifact names the try that
  // published it the way the step pane does ("plan · try 2").
  const places = createMemo(() => attemptPlaces(events()));

  // The version selector's rows: one per version.
  const versions = (): VersionRow[] =>
    versionRows(
      events(),
      takes(),
      stepList().map((s) => s.status),
      viewTake(),
      startedAt(),
      triggerWords(),
    );

  // The summary line's counts, over the version in view.
  const summary = () => {
    const held: Record<string, string> = {};
    for (const [sid, h] of Object.entries(heldNow())) held[sid] = infraReasonText(h.reason);
    return runSummary(
      dagSteps().map((s) => ({ id: s.id, status: s.status, needs: s.needs })),
      held,
    );
  };

  // A step's tries in the version in view, for the List view. Only a step
  // with more than one try is worth the event-log walk.
  const triesOfStep = (stepId: string): FilmstripTry[] => {
    const s = stepList().find((x) => x.id === stepId);
    if (!s || (s.attempt_list?.length ?? 0) < 2) return [];
    return stripTriesOf(visibleEvents(), s.id, s.attempt_list ?? [], stepWindow(stepId));
  };
  // Open one try of a step (a List view try row): the try is kept like a pick
  // in the try tabs.
  const selectTry = (stepId: string, attempt: string) => {
    if (sel() === stepId) {
      setSelAttempt(attempt);
      return;
    }
    pendingTry = attempt;
    selectNode(stepId);
  };

  // The Activity tab: its count (Run and steps), and on a live run a dot
  // while a condition holds a step back.
  const activityCount = createMemo(() => activityView(visibleEvents(), heldNow(), "steps").counts.steps);
  const activeCondition = () => Object.keys(heldNow()).length > 0;
  // The trigger event in words, for the Activity tab.
  const triggerSentence = (): string | null => {
    const t = triggerInfo();
    if (!t) return null;
    return triggerLine({ words: triggerWords(), branch: headBranch(t), sha: t.sha, actor: t.actor });
  };

  // Show an earlier version's try (the step pane's "Reused from" link).
  const showTry = (take: number, attempt: string) => {
    pendingTry = attempt;
    setViewTake(take >= latestTakeN() ? null : take);
  };

  return (
    <section class="page">
      <Doodle icon="container" size={230} rotate={14} opacity={0.16} top="52px" right="48px" />

      <Show when={run()} fallback={<p class="empty">loading…</p>}>
        {(r) => (
          <>
            {/* ── Run header ── */}
            <div class="rh">
              <div class="rh-row">
                <h1 class="crumb-head rh-crumbs">
                  <HomeCrumb size={20} sepClass="crumb-head-sep" />
                  <RepoCrumb org={org()} repo={repo()} link />
                  <Icon icon="chevron-right" size={20} class="crumb-head-sep" />
                  {/* The per-repo run number `#N` is the human handle; the
                      short internal id for untenanted runs. */}
                  <span class="rh-num" title={id()}>
                    {r().run_number != null ? `#${r().run_number}` : `run ${id().slice(0, 8)}`}
                  </span>
                  <Show when={r().trigger_title}>
                    {(t) => (
                      <span class="crumb-head-title rh-title" title={t()}>
                        {t()}
                      </span>
                    )}
                  </Show>
                </h1>
                <StatusBadge status={r().status} />
                <Show when={live() && !timeTraveling()}>
                  <span class="live-dot" title="live">
                    <span class="dot" /> Live
                  </span>
                </Show>
                <span class="grow1" />
                <span class="rh-actions">
                  {/* An active run's useful action is to stop it; a finished
                      run (or an older version, where cancel means nothing)
                      offers a new run pre-filled with these parameters. */}
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
                        title="Start a new run of this pipeline with these parameters"
                      >
                        <Icon icon="play" size={13} /> New run
                      </button>
                    }
                  >
                    <button
                      class="btn btn-danger btn-sm"
                      onClick={(e) => {
                        const b = (e.currentTarget as HTMLElement).getBoundingClientRect();
                        setConfirmingCancel({ top: b.bottom + 6, left: b.left });
                      }}
                      disabled={cancelling()}
                      title="Cancel this run and tear down its steps"
                    >
                      {cancelling() ? "cancelling…" : "Cancel run"}
                    </button>
                  </Show>
                  <RunMoreMenu
                    runId={id()}
                    retention={retention()}
                    pinning={pinning()}
                    onTogglePin={() => void onTogglePin()}
                  />
                </span>
              </div>

              {/* Provenance: trigger, branch and commit, pipeline, timing,
                  parameters, and the version selector. */}
              <div class="rh-prov">
                <Show when={triggerInfo()}>
                  {(t) => {
                    const commitUrl = () => forgeCommitUrl(repoUrl(), t().sha);
                    const prUrl = () => forgePrUrl(repoUrl(), t().pr);
                    return (
                      <>
                        <Show when={t().kind}>
                          {(kind) => (
                            <span class="pfact" title="trigger">
                              <Icon icon={triggerIcon(kind())} size={12} />
                              <Show when={t().pr != null && kind() === "pull_request"} fallback={<span>{triggerText(kind())}</span>}>
                                <a
                                  classList={{ plain: !prUrl() }}
                                  href={prUrl() ?? undefined}
                                  target="_blank"
                                  rel="noopener noreferrer"
                                  title="pull request on the forge"
                                >
                                  PR #{t().pr}
                                </a>
                              </Show>
                              <Show when={t().actor}>
                                {(actor) => <span class="subtle">by {actor()}</span>}
                              </Show>
                            </span>
                          )}
                        </Show>
                        {/* `base ← head` and the commit once. */}
                        <Show when={t().sha || t().branch || r().origin_pr_base}>
                          <span class="pfact" title={r().origin_pr_base ? "base ← head" : "branch and commit"}>
                            <Icon icon="git-branch" size={12} />
                            <Show
                              when={r().origin_pr_base}
                              fallback={
                                <Show when={headBranch(t())}>
                                  {(b) => <span class="mono">{b()}</span>}
                                </Show>
                              }
                            >
                              {(base) => (
                                <>
                                  <span class="mono">{base()}</span>
                                  <span class="pr-arrow">←</span>
                                  <Show when={headBranch(t())}>
                                    {(head) => <span class="mono">{head()}</span>}
                                  </Show>
                                </>
                              )}
                            </Show>
                            <Show when={t().sha}>
                              <a
                                class="mono sha"
                                classList={{ plain: !commitUrl() }}
                                href={commitUrl() ?? undefined}
                                target="_blank"
                                rel="noopener noreferrer"
                                title="commit on the forge"
                              >
                                {t().sha!.slice(0, 8)}
                              </a>
                            </Show>
                          </span>
                        </Show>
                      </>
                    );
                  }}
                </Show>
                <Show when={r().pipeline}>
                  {(name) => (
                    <span class="pfact" title="pipeline">
                      <Icon icon="workflow" size={12} />
                      <span class="mono">{name()}</span>
                    </span>
                  )}
                </Show>
                <Show when={startedAt()}>
                  <span class="pfact" classList={{ live: live() && !timeTraveling() }} title={absTime(startedAt()!)}>
                    <Icon icon="timer" size={12} />
                    <span>
                      started {relTime(startedAt()!)} · {finishedAt() ? "took" : "running for"}{" "}
                      <span class="mono">{duration(startedAt()!, finishedAt() ?? Date.now())}</span>
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
                <VersionDropdown
                  rows={versions()}
                  live={live() && !timeTraveling()}
                  onSelect={(n) => {
                    setViewTake(n);
                    setFocusSidecar(null);
                  }}
                />
              </div>
            </div>

            {/* ── Notice slot ── */}
            {/* Gates awaiting a decision, one row per gate: a run can suspend
                on more than one and "approve" has to say WHICH. Hidden while
                viewing an older version, which is read-only. */}
            <Show when={pendingGates().length > 0 && !timeTraveling()}>
              <div class="gate-bar">
                {/* Keyed by gate ID: a poll replaces every step object, and a
                    row keyed by object would re-mount an open Reject form. */}
                <For each={pendingGateIds()}>
                  {(gid) => (
                    <Show when={gateStep(gid)}>
                      {(g) => (
                        <div class="gate-row" classList={{ blocked: blockersOf(g()).length > 0 }}>
                          <Icon icon="shield-check" size={13} />
                          <span class="gate-step mono">{g().id}</span>
                          <span class="gate-kind mono">{g().gate}</span>
                          <Show when={environmentGateLabel(g(), runEnv())}>
                            {(label) => (
                              <span
                                class="gate-env"
                                title={
                                  addedByEnvironment(g()) !== null
                                    ? "The pipeline has no approval gate here: the environment added this one. Its approvers release it."
                                    : "A step after this gate uses the environment's authority (its secrets, a privilege grant, a k8s_overlay, a write forge token or the deploy mark): its approvers release it."
                                }
                              >
                                {label()}
                              </span>
                            )}
                          </Show>
                          <span class="gate-hint" title={gateWaitHint(g(), statusOf)}>
                            {gateWaitHint(g(), statusOf)}
                          </span>
                          <Show when={approvalReleaseIn(g(), nowTick())}>
                            {(left) => (
                              <span class="gate-clock">
                                releases at{" "}
                                <b class="mono">{new Date(g().approval_release_at!).toLocaleTimeString()}</b> · in{" "}
                                <b class="mono">{clock(left())}</b>
                              </span>
                            )}
                          </Show>
                          <Show when={g().plugin_gate}>
                            {(pg) => (
                              <>
                                <span class="gate-who">
                                  {pg().releaser}/{pg().gate}
                                </span>
                                <Show when={gateLeft(g()) != null}>
                                  <span class="gate-clock" classList={{ low: clockLow(gateLeft(g())!) }}>
                                    expires in <b class="mono">{clock(gateLeft(g())!)}</b>
                                  </span>
                                </Show>
                                <Show when={pg().plugin_state === "killed"}>
                                  <span class="gate-note" style={{ color: "var(--danger)" }}>
                                    {pg().installation} is killed: this gate waits for its expiry or a
                                    reject
                                  </span>
                                </Show>
                              </>
                            )}
                          </Show>
                          <Show when={gateNotes()[g().id]}>
                            {(note) => <span class="gate-note">{note()}</span>}
                          </Show>
                          <Show
                            when={rejectFor() === g().id}
                            fallback={
                              <span class="gate-actions">
                                <button
                                  class="btn btn-sm btn-ghost"
                                  onClick={() => openReject(g().id)}
                                  disabled={rejecting() !== null || !isRejectable(g(), statusOf)}
                                  title={
                                    blockersOf(g()).length > 0
                                      ? `blocked — ${blockersOf(g()).join(", ")} has not succeeded`
                                      : `reject ${g().id} — fail it with a reason; the run fails`
                                  }
                                >
                                  Reject
                                </button>
                                <Show when={isApprovable(g())}>
                                  <button
                                    class="btn btn-sm btn-primary"
                                    onClick={() => void onApprove(g().id)}
                                    disabled={approving() !== null || blockersOf(g()).length > 0}
                                    title={
                                      blockersOf(g()).length > 0
                                        ? `blocked — ${blockersOf(g()).join(", ")} has not succeeded`
                                        : `approve ${g().id}`
                                    }
                                  >
                                    {approving() === g().id ? "approving…" : "Approve"}
                                  </button>
                                </Show>
                              </span>
                            }
                          >
                            {/* The in-page confirm: a required reason, then an
                                explicit "Reject gate". */}
                            <form
                              class="gate-reject"
                              onSubmit={(e) => {
                                e.preventDefault();
                                void onReject(g().id);
                              }}
                            >
                              <input
                                class="input input-sm"
                                type="text"
                                required
                                maxlength={1024}
                                placeholder="why reject? (required)"
                                aria-label={`reason for rejecting ${g().id}`}
                                value={rejectReason()}
                                onInput={(e) => setRejectReason(e.currentTarget.value)}
                                ref={(el) => queueMicrotask(() => el.focus())}
                              />
                              <button
                                type="submit"
                                class="btn btn-sm btn-danger"
                                disabled={rejecting() !== null || rejectReasonError(rejectReason()) !== null}
                              >
                                {rejecting() === g().id ? "rejecting…" : "Reject gate"}
                              </button>
                              <button
                                type="button"
                                class="btn btn-sm btn-ghost"
                                onClick={() => setRejectFor(null)}
                                disabled={rejecting() !== null}
                              >
                                Cancel
                              </button>
                            </form>
                          </Show>
                        </div>
                      )}
                    </Show>
                  )}
                </For>
              </div>
            </Show>
            {/* Viewing an older version: the page is the run as it stood
                then, and nothing on it acts. */}
            <Show when={viewing()}>
              {(v) => (
                <div class="readonly-banner rd-notice" role="status">
                  <Icon icon="history" size={13} />
                  <span>
                    Viewing <b>version {v().n} of {latestTakeN()}</b>, read-only
                    <span class="rb-hint"> · pick the latest version to act</span>
                  </span>
                </div>
              )}
            </Show>
            {/* A version this Run uses was yanked: say what ran, what was
                refused, and offer a new run, which compiles against the
                installed version. */}
            <Show when={!timeTraveling() && yankNote(yanks(), stepList(), (ms) => new Date(ms).toLocaleTimeString())}>
              {(note) => (
                <div class="rd-toast rd-yank" role="status">
                  <Icon icon="ban" size={13} /> <span>{note()}</span>
                  <button
                    class="btn btn-primary btn-sm"
                    onClick={() =>
                      nav(`/${org()}/${repo()}/run`, {
                        state: { prefillParams: runParams(r()) },
                      })
                    }
                  >
                    <Icon icon="rotate-cw" size={13} /> New run
                  </button>
                </div>
              )}
            </Show>
            {/* The cold tier's time bound, said out loud once it matters: a
                pinned run, or one whose window has lapsed. */}
            <Show when={retention()} keyed>
              {(wr) => (
                <Show when={wr.pinned || wr.expired}>
                  <p class="subtle rd-retention rd-notice">
                    <Icon icon={wr.pinned ? "pin" : "clock"} size={12} />{" "}
                    {retentionNote(wr, isTerminal(run()!.status))}
                  </p>
                </Show>
              )}
            </Show>
            <RunPolicies runId={id()} live={live()} />

            {/* ── Summary line ── */}
            <div class="rd-summary" aria-label="Run summary">
              <Show when={summary().failed > 0}>
                <span class="rd-sum-part failed">
                  ✕ {summary().failed} of {summary().total} steps failed
                  {summary().roots.length ? ":" : ""}
                </span>
                <span class="rd-sum-roots">
                  <For each={summary().roots}>
                    {(sid, i) => (
                      <>
                        {i() > 0 ? ", " : ""}
                        <button
                          type="button"
                          class="linklike mono"
                          onClick={() => {
                            selectNode(sid);
                            setRunTab("pipeline");
                          }}
                        >
                          {stepDisplayName(sid)}
                        </button>
                      </>
                    )}
                  </For>
                </span>
              </Show>
              <Show when={summary().running > 0}>
                <span class="rd-sum-part running">● {summary().running} running</span>
              </Show>
              <For each={summary().held}>
                {(h) => (
                  <span class="rd-sum-part warning">
                    ◷ {h.count} {h.text}
                  </span>
                )}
              </For>
              <Show when={summary().failed === 0 && summary().passed > 0}>
                <span class="rd-sum-part passed">✓ {summary().passed} passed</span>
              </Show>
              <Show when={distinctNames(visibleArtifacts()) > 0}>
                <span class="rd-sum-part subtle">
                  ·{" "}
                  <button type="button" class="linklike" onClick={() => openArtifacts(null)}>
                    {distinctNames(visibleArtifacts())}{" "}
                    {distinctNames(visibleArtifacts()) === 1 ? "artifact" : "artifacts"}
                  </button>
                </span>
              </Show>
            </div>

            {/* ── Run tabs ── */}
            <div class="run-tabs" role="tablist" aria-label="Run">
              <button
                type="button"
                role="tab"
                class="run-tab"
                aria-selected={runTab() === "pipeline"}
                onClick={() => setRunTab("pipeline")}
              >
                Pipeline <span class="run-tab-n">{r().steps.length} {r().steps.length === 1 ? "step" : "steps"}</span>
              </button>
              <button
                type="button"
                role="tab"
                class="run-tab"
                aria-selected={runTab() === "artifacts"}
                onClick={() => openArtifacts(null)}
              >
                Artifacts{" "}
                <span class="run-tab-n" classList={{ zero: distinctNames(visibleArtifacts()) === 0 }}>
                  {distinctNames(visibleArtifacts())}
                </span>
              </button>
              <button
                type="button"
                role="tab"
                class="run-tab"
                aria-selected={runTab() === "activity"}
                onClick={() => setRunTab("activity")}
              >
                Activity{" "}
                <Show
                  when={!(live() && !timeTraveling())}
                  fallback={
                    <Show when={activeCondition()}>
                      <span class="run-tab-dot" role="img" aria-label="warning" title="A step is held back right now" />
                    </Show>
                  }
                >
                  <span class="run-tab-n">{activityCount()}</span>
                </Show>
              </button>
            </div>

            {/* ── Pipeline panel: the graph or the list ── */}
            <Show when={runTab() === "pipeline"}>
              <div class="panel pipeline-panel" classList={{ readonly: timeTraveling() }}>
                <div class="panel-h pipeline-toolbar">
                  <span class="pv-switch" role="group" aria-label="Pipeline view">
                    <button
                      type="button"
                      class="pv-tab"
                      aria-pressed={view() === "graph"}
                      onClick={() => setView("graph")}
                    >
                      Graph
                    </button>
                    <button
                      type="button"
                      class="pv-tab"
                      aria-pressed={view() === "list"}
                      onClick={() => setView("list")}
                    >
                      List <span class="pv-count">{r().steps.length}</span>
                    </button>
                  </span>
                  <span class="pt-count">select a step to open it below</span>
                  <span class="grow1" />
                  <Show when={view() === "graph"}>
                    <span class="dag-zoomctl">
                      <button type="button" title="Back to the fitted view" onClick={() => dagControls()?.fit()}>
                        ⤢
                      </button>
                      <button type="button" title="Zoom in" onClick={() => dagControls()?.zoomIn()}>
                        ＋
                      </button>
                      <button type="button" title="Zoom out" onClick={() => dagControls()?.zoomOut()}>
                        －
                      </button>
                    </span>
                  </Show>
                </div>
                <Show
                  when={view() === "graph"}
                  fallback={
                    <StepList
                      steps={dagSteps()}
                      services={dagServices()}
                      selected={sel()}
                      onSelect={selectNode}
                      now={nowTick()}
                      triesOf={triesOfStep}
                      activeTry={stripActiveAttempt()}
                      onSelectTry={selectTry}
                      resolutionOf={(sid) => stepList().find((x) => x.id === sid)?.gate_resolution ?? null}
                    />
                  }
                >
                  {/* Shared services are copper peers in a lane at the top with
                      dotted `uses` edges; a step's own sidecars are chips on its
                      node. */}
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
                </Show>
              </div>
            </Show>

            {/* ── Step pane: a shared service opens ServicePane; a step,
                StepPane with its actions. Hidden, never unmounted, while
                another run tab is open. ── */}
            <div
              class="rd-evidence"
              classList={{ readonly: timeTraveling() }}
              style={{ display: runTab() === "pipeline" ? undefined : "none" }}
            >
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
                    viewedTake={selectedTake()?.n ?? null}
                    window={stepWindow(sel())}
                    frontierAttempt={sel() ? (scopedView()?.frontier[sel()!] ?? null) : null}
                    deadLettered={r().status === "dead_lettered"}
                    canDebug={canShell() && !olderTrySelected() && !unscheduled()}
                    onDebugPod={() => setShellOpen(true)}
                    focusSidecar={focusSidecar()}
                    gateStatus={!timeTraveling() && selectedStep() ? gateStatus(selectedStep()!) : null}
                    pipeline={r().pipeline ?? null}
                    now={nowTick()}
                    gateLabel={
                      !timeTraveling() && selectedStep() ? environmentGateLabel(selectedStep()!, runEnv()) : null
                    }
                    artifacts={stepArtifacts(sel())}
                    onShowArtifacts={() => openArtifacts(sel())}
                    held={(() => {
                      const h = sel() ? heldNow()[sel()!] : undefined;
                      return h
                        ? { attempt: h.attempt, text: infraReasonText(h.reason), ...(h.message ? { message: h.message } : {}) }
                        : null;
                    })()}
                    initialTab={paneTab()}
                    onTabChange={setPaneTab}
                    onShowTry={showTry}
                    reused={selectedReused()}
                    note={
                      <Show when={widenedNote(plan())}>
                        {(note) => (
                          <p class="subtle rd-widened">
                            <Icon icon="alert-triangle" size={12} /> {note()}
                          </p>
                        )}
                      </Show>
                    }
                    actions={
                      <>
                        {/* Two "agains": Retry step gives a FAILED step another
                            try in this version; Rerun from here re-runs this
                            step and everything after it as a new version. Both
                            are blocked when a prerequisite failed, and while
                            viewing an older version. */}
                        <Show when={selectedStatus() === "failed" && !timeTraveling()}>
                          <button
                            class="btn btn-ghost btn-sm"
                            onClick={(e) => sel() && openConfirm("retry", sel()!, e)}
                            disabled={
                              retrying() !== null ||
                              rerunning() !== null ||
                              !!prereqBlocker(sel()) ||
                              !!rerunBlockedByYank()
                            }
                            title={
                              rerunBlockedByYank()
                                ? `refused: ${sel()} comes from lib:${rerunBlockedByYank()!.name}, which is yanked; start a new run`
                                : prereqBlocker(sel())
                                  ? `blocked — prerequisite ${prereqBlocker(sel())} failed; retry that first`
                                  : `retry ${sel()} — another try in this version (no new version)`
                            }
                          >
                            <Icon icon="rotate-cw" size={13} /> {retrying() ? "retrying…" : "Retry step"}
                          </button>
                        </Show>
                        {/* The label changes when the step's input snapshots
                            have expired: rerunning it alone is impossible, so
                            the button says where the rerun starts. */}
                        <button
                          class="btn btn-ghost btn-sm"
                          classList={{ widened: isWidened(plan()) }}
                          onClick={(e) => sel() && openConfirm("rerun", sel()!, e)}
                          disabled={
                            rerunning() !== null ||
                            retrying() !== null ||
                            !sel() ||
                            timeTraveling() ||
                            !!prereqBlocker(sel()) ||
                            !!rerunBlockedByYank()
                          }
                          title={
                            rerunBlockedByYank()
                              ? `refused: it would launch a Step from lib:${rerunBlockedByYank()!.name}, which is yanked; start a new run`
                              : timeTraveling()
                                ? "read-only — pick the latest version to rerun"
                                : prereqBlocker(sel())
                                  ? `blocked — prerequisite ${prereqBlocker(sel())} failed; rerun that first`
                                  : rerunTitle(plan(), sel() ?? "")
                          }
                        >
                          <Icon icon="rotate-ccw" size={13} /> {rerunning() ? "rerunning…" : rerunLabel(plan())}
                        </button>
                        <button
                          class="btn btn-ghost btn-sm"
                          onClick={() => setShellOpen(true)}
                          disabled={!canShell() || olderTrySelected() || unscheduled()}
                          title={
                            !canShell()
                              ? "Select a running or finished step"
                              : olderTrySelected()
                                ? "Select the latest try to debug; Debug pod reproduces the latest try."
                                : unscheduled()
                                  ? "The Pod has not been scheduled yet"
                                  : selectedRunning()
                                  ? `shell into the running ${sel()} Pod`
                                  : `reproduce ${sel()} in a fresh debug Pod and shell in`
                          }
                        >
                          <Icon icon="terminal" size={13} /> {selectedRunning() ? "Debug shell" : "Debug pod"}
                        </button>
                      </>
                    }
                  />
                }
              >
                <ServicePane runId={id()} service={selectedService()!} />
              </Show>
            </div>

            {/* The confirm popover: the whole cascade, named before the POST
                fires. Anchored under whichever button opened it. */}
            <Show when={confirming()} keyed>
              {(c) => (
                <RerunConfirm
                  kind={c.kind}
                  target={c.step}
                  plan={planFor(c.step)}
                  loading={!planFor(c.step) && !planFailed()}
                  anchor={{ top: c.top, left: c.left }}
                  busy={rerunning() !== null || retrying() !== null}
                  onCancel={() => setConfirming(null)}
                  onConfirm={() => {
                    setConfirming(null);
                    if (c.kind === "retry") void onRetry(c.step);
                    else void onRerun(c.step);
                  }}
                />
              )}
            </Show>
            <Show when={confirmingCancel()}>
              {(anchor) => (
                <CancelRunConfirm
                  anchor={anchor()}
                  busy={cancelling()}
                  onCancel={() => setConfirmingCancel(null)}
                  onConfirm={() => void onCancel()}
                />
              )}
            </Show>
            {/* The executed plan was wider than the previewed one. */}
            <Show when={toast()}>
              {(t) => (
                <div class="rd-toast" role="status">
                  <Icon icon="alert-triangle" size={13} /> {t()}
                  <button class="rd-toast-x" onClick={() => setToast(null)} title="dismiss">
                    ✕
                  </button>
                </div>
              )}
            </Show>

            <Show when={runTab() === "artifacts"}>
              <RunArtifacts
                runId={id()}
                artifacts={visibleArtifacts()}
                stepOrder={stepList().map((s) => s.id)}
                places={places()}
                viewedTake={selectedTake()?.n ?? null}
                onlyStep={artifactStep()}
                onShowAll={() => setArtifactStep(null)}
                outcomeOf={(step, attempt) =>
                  stepList()
                    .find((s) => s.id === step)
                    ?.attempt_list?.find((a) => a.id === attempt)?.outcome
                }
              />
            </Show>
            <Show when={runTab() === "activity"}>
              <RunActivity
                events={visibleEvents()}
                later={events().length - visibleEvents().length}
                active={heldNow()}
                trigger={triggerSentence()}
              />
            </Show>
            <Show when={shellOpen() && sel()}>
              <DebugShell runId={id()} step={sel()!} mode={shellMode()} onClose={() => setShellOpen(false)} />
            </Show>
          </>
        )}
      </Show>
    </section>
  );
}
