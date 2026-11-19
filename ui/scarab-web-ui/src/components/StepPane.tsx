// The step pane: one pane for the selected step. Its header names the step,
// the selected try ("Try 2 of 3"), what that try did ("✕ Failed after
// 2m06s: step error"), a link to the step's artifacts and the step's actions
// (Retry step, Rerun from here, Debug pod, handed in by the page). A step with
// two or more tries gets a row of try tabs under the header; on a narrow
// screen the header's "Try 2 of 3 ▾" dropdown picks instead. A box below
// says why the try failed, what holds it back, or where a step that did not
// run in this version was reused from. Every content tab (Logs, Results, Workspace) is
// scoped to the selected try and says so; Outputs reads the try of record and
// names it when it is a different one. Causes (first run, automatic retry,
// retried by, rerun of) and outcomes come from the event log; a crash
// re-adoption is a marker inside its try, never a new execution.
import {
  createSignal,
  createEffect,
  createMemo,
  createResource,
  on,
  For,
  Show,
  onCleanup,
  type JSX,
} from "solid-js";
import {
  getStepResults,
  getConsumed,
  listWorkspace,
  workspaceFileUrl,
  streamStepLogs,
  streamStepSidecarLogs,
  stepOrigin,
  type StepStatus,
  type Attempt,
} from "../api/client";
import type { RunEvent } from "../api/client";
import { attemptPlaces, carriedOfRecord, ofRecordAttemptId, tryLabel } from "../takes";
import {
  windowAttemptList,
  scopedAttempt,
  stepNotRun,
  ofRecordIndexOf,
  causeText,
} from "../attempts";
import { heldBeforeStart } from "../events";
import { failureBox, notLatestNote, statusWord, stepName, tryStatusWords } from "../run-page";
import { levelOf, logBodyState } from "../logs";
import { approvalReleaseIn, type GateStatus } from "../gates";
import { clock, clockLow, expiryPercent, isYankRefusal, pluginGateState } from "../run-origin";
import { shortHash } from "../libraries";
import { absTime } from "../fmt";
import Icon from "./Icon";
import AttemptsDropdown, { type FilmstripTry } from "./AttemptsDropdown";
import TryTabs from "./TryTabs";
import Pill from "./Pill";

export type PaneTab = "logs" | "results" | "outputs" | "workspace" | "source" | "gate";

// Cap rendered lines so a giant attempt can't blow up the DOM (windowing v1).
const MAX_LINES = 1500;

/** A compact display of a result value — scalars inline, structures as JSON. */
function showValue(v: unknown): string {
  if (typeof v === "string") return v;
  return JSON.stringify(v);
}

export default function StepPane(props: {
  runId: string;
  step: StepStatus | null;
  /** Full event log — the attempt-cause and re-adoption source. */
  events: RunEvent[];
  /** The try scoping every tab — chosen in the header filmstrip; null = latest. */
  attempt: string | null;
  /** The selected step's tries, resolved by the caller from the event log — the
   * header filmstrip renders these (the try axis). */
  tries: FilmstripTry[];
  /** The try the filmstrip highlights as active (resolved to a concrete id). */
  activeAttempt: string | null;
  /** Pick a try in the filmstrip — scopes every tab to `(step, attempt)`. */
  onAttemptSelect: (id: string | null) => void;
  /** The number of the version (Take) in view, latest included. Attempt ids
   * are shown as tries counted within their version, and an attempt from
   * another version names that version. */
  viewedTake?: number | null;
  /** Viewing a closed Take: pin the strip's default to this frontier attempt
   * and mark attempts beyond it as from a later take. */
  frontierAttempt?: string | null;
  /** The SELECTED take's attempt-id window —
   * the exact set of tries that belong to this take, scoping EVERY tab's
   * evidence in every view (the latest/live take included). `[]` = the step was
   * carried forward untouched (nothing ran here); `null`/absent = unscoped
   * (fall back to the frontier filter). Keeps the pane consistent with the
   * caller's already-windowed `tries` strip. */
  window?: string[] | null;
  /** The run was dead-lettered — the step strip's terminal ⊘ marker. */
  deadLettered?: boolean;
  /** Open a Debug pod for this step. The Workspace tab offers it as the fallback
   * when no snapshot exists — a fresh reproduction is the robust way to explore
   * a filesystem the CAS can't hand back (note: it re-runs, so it's NOT the
   * attempt's immutable bytes). Absent/`canDebug:false` ⇒ the affordance hides. */
  onDebugPod?: () => void;
  canDebug?: boolean;
  /** A sidecar container index to focus in the Logs tab on selection
   * — set when the user clicked this step's docked sidecar chip in the DAG.
   * `null`/absent = the step's main container. */
  focusSidecar?: number | null;
  /** The step's gate verdict (gates.ts `gateStatus`) — shown under the stamp. `null` = nothing to say (or viewing
   * an older version, which this line does not describe). */
  gateStatus?: GateStatus | null;
  /** The Run's pipeline name, for the origin line. */
  pipeline?: string | null;
  /** The page's clock tick, for a plugin gate's expiry countdown. */
  now?: number;
  /** For an Environment approval gate: "Environment approval gate · <Environment>"
   * (gates.ts `environmentGateLabel`). `null` for any other step. */
  gateLabel?: string | null;
  /** The step's actions (Retry step, Rerun from here, Debug pod), right-aligned
   * in the header. */
  actions?: JSX.Element;
  /** A note under the header (the widened-rerun note). */
  note?: JSX.Element;
  /** How many files this step published in the version in view. */
  artifacts?: number;
  /** Open the run's Artifacts tab filtered to this step. */
  onShowArtifacts?: () => void;
  /** The infra condition holding the step's current try back now (live view
   * only), in words, with the backend's message. */
  held?: { attempt: string; text: string; message?: string } | null;
  /** The tab to open on first mount (from the URL). */
  initialTab?: PaneTab;
  /** Told whenever the open tab changes, so the page can keep it in the URL. */
  onTabChange?: (t: PaneTab) => void;
  /** Show an earlier version's try: the "Reused from version 2, try 1" link. */
  onShowTry?: (take: number, attempt: string) => void;
}) {
  const [tab, setTab] = createSignal<PaneTab>(props.initialTab ?? "logs");
  // Below 640px the try tabs do not fit: the header's dropdown picks instead.
  const narrowQuery = typeof window !== "undefined" && window.matchMedia ? window.matchMedia("(max-width: 639px)") : null;
  const [narrow, setNarrow] = createSignal(narrowQuery?.matches ?? false);
  if (narrowQuery) {
    const onNarrow = (e: MediaQueryListEvent) => setNarrow(e.matches);
    narrowQuery.addEventListener("change", onNarrow);
    onCleanup(() => narrowQuery.removeEventListener("change", onNarrow));
  }
  const tryTabs = () => props.tries.length > 1 && !narrow();
  createEffect(() => props.onTabChange?.(tab()));
  // A plugin gate opens on its Gate tab; any other step never
  // stays on one.
  const isPluginGate = () => !!props.step?.plugin_gate;
  createEffect(
    on(
      () => props.step?.id,
      () => {
        if (!props.step) return;
        if (isPluginGate()) setTab("gate");
        else if (tab() === "gate" || (tab() === "source" && (props.step?.origin ?? []).length === 0)) setTab("logs");
      },
    ),
  );
  // Where the step comes from: the chain, its bound inputs, and its render.
  // Read when the Source or Gate tab is open.
  const [origin] = createResource(
    () => {
      const s = props.step;
      const t = tab();
      return s && (t === "source" || t === "gate") ? { run: props.runId, step: s.id, n: s.attempt_list?.length ?? 0 } : null;
    },
    (k) => stepOrigin(k.run, k.step).catch(() => null),
  );
  // A launch refused because a library version is yanked.
  const yankRefusal = () => {
    const a = props.step?.attempt_list?.[props.step.attempt_list.length - 1];
    return a?.failure === "config" && isYankRefusal(a.failure_detail) ? a.failure_detail : null;
  };
  const [wsPath, setWsPath] = createSignal("");
  const [openFile, setOpenFile] = createSignal<string | null>(null);
  const [wrap, setWrap] = createSignal(true);
  // Which container's logs the Logs tab shows: `null` = the step's main
  // container (default); a number = the sidecar at that `services:` index.
  // Reset per selection from `focusSidecar` below.
  const [container, setContainer] = createSignal<number | null>(null);

  // A sidecar chip's short label, matching the DAG chip: `redis:7.4` → `redis`.
  const shortImage = (image: string): string => {
    const lastSlash = image.lastIndexOf("/");
    const seg = lastSlash >= 0 ? image.slice(lastSlash + 1) : image;
    const colon = seg.indexOf(":");
    return colon >= 0 ? seg.slice(0, colon) : seg;
  };
  const sidecars = () => props.step?.services ?? [];

  const stepId = () => props.step?.id ?? null;
  // The attempts this pane's tabs are scoped to. Take-windowed in EVERY view:
  // when the caller hands a `window` (the
  // selected take's attempt-id set), keep only those tries — so a step carried
  // forward untouched (`[]`) reads as "didn't run in this version", and a
  // re-run step shows only THIS take's tries, on the latest/live take too. The
  // `frontierAttempt` ≤-filter stays as the fallback for callers that don't
  // pass a window (so a closed version never shows later tries).
  const attemptsOf = (): Attempt[] =>
    windowAttemptList(props.step?.attempt_list ?? [], props.window, props.frontierAttempt);

  // The attempt every tab is scoped to: explicit selection, else the Take
  // frontier (closed-take view), else the latest attempt.
  const scoped = (): Attempt | null =>
    scopedAttempt(attemptsOf(), props.attempt ?? props.frontierAttempt ?? null);

  // The selected step didn't run in the viewed version — the caller's filmstrip
  // (already windowed to this version's tries) is empty. Both a time-travel
  // `not_run` step (re-armed but never executed in a superseded version) and a
  // not-yet-launched step land here. Its tabs read a short "didn't run / nothing
  // of record" line instead of a stale or empty-looking shell (a blank tab
  // would read as success).
  const notRun = (): boolean => stepNotRun(props.step, props.tries);

  // Of-record: the Outputs tab shows the latest SUCCESSFUL attempt
  // WITHIN THE VIEWED VERSION, not the selected try — so a failed/superseded/
  // running selected try still surfaces the values a downstream step would read.
  // `attemptsOf()` is already windowed to this version; a not-run step (or one
  // with no success here) has nothing of record, except a carried-forward
  // step (below).
  //
  // Every attempt's version and try number, from the event log.
  const places = createMemo(() => attemptPlaces(props.events));
  /** "plan · try 2", or "plan · version 1 · try 2" for another version. */
  const label = (step: string, attempt: string): string =>
    tryLabel(places(), step, attempt, props.viewedTake ?? null);
  // A step that did not run in this version was carried forward: downstream
  // steps read the outputs of an earlier version's attempt, so the Outputs
  // tab shows that one, labelled with its version and try.
  const carried = () => {
    const id = stepId();
    if (!id || !notRun() || props.viewedTake == null) return null;
    return carriedOfRecord(props.step?.attempt_list ?? [], places()[id], props.viewedTake);
  };
  const ofRecordTry = (): string | null =>
    notRun() ? (carried()?.id ?? null) : ofRecordAttemptId(attemptsOf());
  const ofRecordIndex = (): number => ofRecordIndexOf(attemptsOf());

  // Why the selected try produced no output. A step whose Pod never
  // started — unschedulable, image unpullable, volume unmountable — has an
  // EMPTY log stream by construction: there was no process to print anything.
  // The diagnosis is on the attempt (`failure_detail`, the executor's own read
  // of the backend), and until now nothing rendered it, so the pane an operator
  // opens first said "no output for this try" and stopped there.
  const scopedFailure = (): { failure: string; detail?: string } | null => {
    const a = scoped();
    if (!a?.failed) return null;
    return {
      failure: a.failure ?? "failed",
      detail: a.failure_detail ?? undefined,
    };
  };

  // ── Header, failure box and tab scope ──
  // A gate's tries are its waits.
  const noun = () => (props.step?.gate ? "Wait" : "Try");
  // The selected try's index in this version's tries, null when none ran.
  const selIndex = (): number | null => {
    if (!props.tries.length) return null;
    const i = props.tries.findIndex((t) => t.id === props.activeAttempt);
    return i >= 0 ? i : props.tries.length - 1;
  };
  const selTry = () => {
    const i = selIndex();
    return i === null ? null : props.tries[i];
  };
  // The held condition applies only while it holds the selected try.
  const heldHere = () => (props.held && props.held.attempt === selTry()?.id ? props.held : null);
  // What the selected try did; with no try in this version, the step's own
  // status (pending, waiting) or that it was reused from an earlier version.
  const status = () => {
    const t = selTry();
    if (t) return tryStatusWords(t, props.now ?? Date.now(), heldHere()?.text ?? null);
    if (carried()) return { text: "○ Not run in this version", tone: "muted" };
    return { text: `○ ${statusWord(props.step?.status ?? "")}`, tone: "muted" };
  };
  const box = createMemo(() =>
    failureBox({
      tries: props.tries.map((t) => ({
        outcome: t.outcome,
        failure: t.failure,
        failureDetail: t.failureDetail,
        cause: causeText(t.cause),
      })),
      selected: selIndex(),
      noun: noun(),
      heldBeforeMs: stepId() && selTry() ? heldBeforeStart(props.events, stepId()!, selTry()!.id) : null,
      held: heldHere(),
      carried: carried(),
      viewedTake: props.viewedTake ?? null,
    }),
  );
  const notLatest = () => notLatestNote(props.tries, selIndex(), noun());
  // The scope each content tab carries: "try 2".
  const tryScope = () => {
    const i = selIndex();
    return i === null ? null : `${noun().toLowerCase()} ${i + 1}`;
  };
  // Outputs read the try of record: name it when it is not the selected one.
  const outputsScope = (): string | null => {
    const c = carried();
    if (c) return `from version ${c.take}, ${noun().toLowerCase()} ${c.try}`;
    const rec = ofRecordTry();
    if (!rec) return "none";
    if (rec === scoped()?.id) return null;
    return `from ${noun().toLowerCase()} ${ofRecordIndex() + 1}`;
  };

  // Reset per-step view state when the DAG selection moves.
  createEffect(() => {
    void stepId();
    setWsPath("");
    setOpenFile(null);
  });

  // The Logs container follows the selection: a clicked sidecar chip
  // (`focusSidecar` set) opens that sidecar's container in the Logs tab; any
  // other step selection resets to the main container. Tracks stepId so
  // re-selecting a step returns to `step`, and focusSidecar so clicking a chip
  // on the current step re-focuses it.
  createEffect(() => {
    void stepId();
    const f = props.focusSidecar ?? null;
    setContainer(f);
    if (f != null) setTab("logs");
  });

  // A brief "changed / loading" pulse whenever the scoped (step, try) or tab
  // moves. The evidence pane no longer repeats the step name or try in a header
  // (the graph shows both, and highlights the active try in the fan) — this
  // spinner is the only cue that a switch took effect and fresh evidence is
  // arriving.
  const [switching, setSwitching] = createSignal(false);
  let switchTimer: ReturnType<typeof setTimeout> | undefined;
  // The selection identity — which step, which try, which tab — as a stable
  // string. The memo only notifies when that string actually changes, so the
  // 1.2s run-status poll (which hands us a fresh `props.step` object every tick
  // while the run is live, but with the same ids) no longer re-fires the pulse.
  // Tracking the reactive props objects directly made the log skeleton flicker
  // on every poll while a step was running.
  const selKey = createMemo(
    () => `${stepId() ?? ""} ${scoped()?.id ?? ""} ${tab()} ${container() ?? "main"}`,
  );
  createEffect(
    on(selKey, () => {
      setSwitching(true);
      clearTimeout(switchTimer);
      switchTimer = setTimeout(() => setSwitching(false), 450);
    }),
  );
  onCleanup(() => clearTimeout(switchTimer));

  // --- Logs: one buffered SSE stream per (step, attempt); the scoped attempt's
  // buffer renders. Historical attempts replay and close; the live one tails. ---
  const [buffers, setBuffers] = createSignal<Record<string, string>>({});
  // Streams the server has closed: nothing more will arrive for that key.
  const [ended, setEnded] = createSignal<Record<string, true>>({});
  const streams = new Map<string, () => void>();
  // Keyed by (step, attempt, container) — the container axis (main vs a sidecar
  // index) keeps each container's tail in its own buffer/stream.
  const logKey = (step: string, attempt: string, c: number | null) =>
    `${step} ${attempt} ${c ?? "main"}`;

  createEffect(() => {
    const s = stepId();
    const a = scoped();
    const c = container();
    // A not-run step's `scoped()` may resolve to a stale attempt from a LATER
    // version — never stream it; the Logs tab shows the not-run line instead.
    if (!s || !a || notRun() || tab() !== "logs") return;
    const key = logKey(s, a.id, c);
    if (streams.has(key)) return;
    const onChunk = (t: string) =>
      setBuffers((prev) => ({ ...prev, [key]: (prev[key] ?? "") + t + "\n" }));
    const onEnd = () => setEnded((prev) => ({ ...prev, [key]: true }));
    // Sidecar logs share the step's real attempt ids, so the attempt scoping is
    // identical — only the endpoint (`?sidecar=<index>`) differs.
    const close =
      c == null
        ? streamStepLogs(props.runId, s, { attempt: a.id, onChunk, onEnd })
        : streamStepSidecarLogs(props.runId, s, c, { attempt: a.id, onChunk, onEnd });
    streams.set(key, close);
  });
  onCleanup(() => streams.forEach((close) => close()));

  const logText = () => {
    const s = stepId();
    const a = scoped();
    return s && a ? (buffers()[logKey(s, a.id, container())] ?? "") : "";
  };
  // With no lines: still waiting (stream open, try running), or nothing was
  // captured (stream ended, or the try finished).
  const logState = () => {
    const s = stepId();
    const a = scoped();
    if (!s || !a) return "waiting" as const;
    const key = logKey(s, a.id, container());
    return logBodyState({ text: buffers()[key], ended: !!ended()[key], outcome: a.outcome });
  };
  const logRows = () => {
    const all = logText().split("\n");
    const sliced = all.length > MAX_LINES ? all.slice(all.length - MAX_LINES) : all;
    const base = all.length > MAX_LINES ? all.length - MAX_LINES : 0;
    return sliced
      .map((line, i) => ({ n: base + i + 1, line, lvl: levelOf(line) }))
      .filter((r) => r.line.length > 0);
  };

  // --- Results / Logs / Workspace: fetches scoped to the SELECTED try
  // (`scoped()`). Outputs read a DIFFERENT (of-record) attempt below. ---
  const evidenceArg = () => {
    const s = stepId();
    const a = scoped();
    // Not-run in this version → no per-try evidence to fetch (the tabs render a
    // "didn't run" line); skip the request rather than pull a stale attempt.
    return s && !notRun() ? { run: props.runId, step: s, attempt: a?.id } : null;
  };
  const [results] = createResource(evidenceArg, (a) =>
    getStepResults(a.run, a.step, a.attempt).catch(() => []),
  );

  // Outputs are OF-RECORD, not per-try: they resolve to the of-
  // record attempt (latest success in this version) via a dedicated arg/resource
  // pair. Null (no success, or not-run) simply doesn't fetch — the tab shows the
  // of-record empty copy. The interpolation view AND the "built on …" consumed
  // line both read these, so what a downstream step would see is what shows.
  const outputsArg = () => {
    const s = stepId();
    const rec = ofRecordTry();
    return s && rec ? { run: props.runId, step: s, attempt: rec } : null;
  };
  const [outResults] = createResource(outputsArg, (a) =>
    getStepResults(a.run, a.step, a.attempt).catch(() => []),
  );
  const [outConsumed] = createResource(outputsArg, (a) =>
    getConsumed(a.run, a.step, a.attempt).catch(() => null),
  );
  const wsArg = () => {
    const base = evidenceArg();
    return base ? { ...base, path: wsPath() } : null;
  };
  // Swallow browse failures into an unavailable listing — an uncaught reject
  // here would re-throw while rendering and tear down the whole pane (tabs and
  // all), stranding the user with no way back to Logs/Results.
  const [ws] = createResource(wsArg, (a) =>
    listWorkspace(a.run, a.step, a.path, a.attempt).catch(
      () => ({ available: false, entries: [], path: a.path }),
    ),
  );

  const fileArg = () => {
    const base = evidenceArg();
    return base && openFile() ? { ...base, path: openFile()! } : null;
  };
  const [fileBody] = createResource(fileArg, async (a) => {
    // Like the listing above, never let a failed read re-throw during render —
    // a file the CAS can no longer hand back would otherwise tear down the pane.
    try {
      const res = await fetch(workspaceFileUrl(a.run, a.step, a.path, a.attempt));
      if (!res.ok) return { binary: false, text: "", error: true };
      const ct = res.headers.get("content-type") ?? "";
      if (!ct.startsWith("text/")) return { binary: true, text: "", error: false };
      return { binary: false, text: await res.text(), error: false };
    } catch {
      return { binary: false, text: "", error: true };
    }
  });

  const crumbs = () => wsPath().split("/").filter(Boolean);
  const navTo = (path: string) => {
    setOpenFile(null);
    setWsPath(path);
  };
  const goto = (i: number) => navTo(crumbs().slice(0, i + 1).join("/"));
  const enter = (name: string) => navTo([...crumbs(), name].join("/"));
  const filePath = (name: string) => [...crumbs(), name].join("/");

  // Fresh evidence is on the way when the pulse is live or any tab's fetch is in
  // flight — drives the tab-bar spinner.
  const loading = () =>
    switching() ||
    results.loading ||
    outResults.loading ||
    outConsumed.loading ||
    ws.loading;

  // Shimmer stand-in for the log body during a switch. Cached tries swap
  // instantly and often look near-identical (both cargo output) — the skeleton
  // makes it unmistakable that the content reloaded. Reuses the `.lgrow` gutter
  // grid so it lands exactly where real lines will. Varied widths read as text.
  const SKELETON_WIDTHS = [46, 72, 58, 83, 35, 64, 77, 50, 69, 40, 61, 74, 44, 67];
  const LogSkeleton = () => (
    <div class="lgbody lgskel" aria-hidden="true">
      <For each={SKELETON_WIDTHS}>
        {(w, i) => (
          <div class="lgrow">
            <span class="lgln">{i() + 1}</span>
            <span class="lgskel-bar" style={{ width: `${w}%`, "animation-delay": `${i() * 40}ms` }} />
          </div>
        )}
      </For>
    </div>
  );

  const TabBtn = (p: { id: PaneTab; label: string; scope?: string | null }) => (
    <button class={`tab ${tab() === p.id ? "active" : ""}`} onClick={() => setTab(p.id)}>
      {p.label}
      <Show when={p.scope}>
        {" "}
        <span class="tscope">{p.scope}</span>
      </Show>
    </button>
  );

  return (
    // `inspector` carries the shared tab styling; `steppane` adds the strip +
    // in-pane log treatment.
    <div class="panel inspector steppane">
      <Show
        when={props.step}
        fallback={
          <div class="tabpane">
            <p class="empty">select a step to inspect it</p>
          </div>
        }
      >
        {(s) => (
          <>
            {/* Step header: the step, the selected try, what it did, the
                step's files, and its actions. */}
            <div class="sp-head">
              <span class="sp-name" title={s().id}>
                {stepName(s().id).base}
                <Show when={stepName(s().id).cell}>
                  {(cell) => <span class="sp-cell"> · {cell()}</span>}
                </Show>
              </span>
              <AttemptsDropdown
                tries={props.tries}
                active={props.activeAttempt}
                onSelect={props.onAttemptSelect}
                noun={noun()}
                static={tryTabs()}
              />
              <span class={`sp-status ${status().tone}`}>{status().text}</span>
              <Show when={(props.artifacts ?? 0) > 0}>
                <button
                  type="button"
                  class="linklike sp-files"
                  onClick={() => props.onShowArtifacts?.()}
                  title="Open the Artifacts tab, showing this step's files"
                >
                  {props.artifacts} {props.artifacts === 1 ? "artifact" : "artifacts"}
                </button>
              </Show>
              <span class="grow1" />
              <span class="sp-actions">{props.actions}</span>
            </div>
            <Show when={tryTabs()}>
              <TryTabs
                tries={props.tries}
                active={props.activeAttempt}
                ofRecord={notRun() ? null : ofRecordTry()}
                onSelect={props.onAttemptSelect}
                noun={noun()}
                now={props.now ?? Date.now()}
              />
            </Show>
            {props.note}
            <Show when={props.gateLabel}>
              {(label) => (
                <div class="gate-status gate-env-line">
                  {label()}
                  <Show when={approvalReleaseIn(s(), props.now ?? Date.now())}>
                    {(left) => (
                      <>
                        {" · approvals in, releases at "}
                        <span class="mono">{new Date(s().approval_release_at!).toLocaleTimeString()}</span>
                        {` (in ${clock(left())})`}
                      </>
                    )}
                  </Show>
                </div>
              )}
            </Show>
            <Show when={props.gateStatus}>
              {(g) => (
                <div class={`gate-status ${g().tone}`} role="status">
                  {g().text}
                </div>
              )}
            </Show>
            <Show when={!props.gateStatus && isPluginGate() && pluginGateState(s()) !== "released"}>
              <div
                class={`gate-status ${pluginGateState(s()) === "expired" || pluginGateState(s()) === "rejected" ? "failed" : ""}`}
                role="status"
              >
                {pluginGateState(s()) === "killed"
                  ? "waiting for its plugin · the plugin is killed"
                  : "waiting for its plugin"}
              </div>
            </Show>
            <Show when={yankRefusal()}>
              {(d) => (
                <div class="gate-status failed" role="status">
                  config error · refused at launch: {d()}
                </div>
              )}
            </Show>
            {/* Why the selected try failed, what holds it back, or where a
                step that did not run here was reused from. */}
            <Show when={box() || notLatest()}>
              <div class={`sp-box ${box()?.tone ?? "info"}`} role="status">
                <Show when={box()}>
                  {(b) => (
                    <>
                      <span class="sp-box-h">{b().head}</span>
                      <Show when={b().detail}>
                        <span class="sp-box-d">{b().detail}</span>
                      </Show>
                    </>
                  )}
                </Show>
                <Show when={!selTry() && carried() && props.onShowTry}>
                  <span class="sp-box-d">
                    <button
                      class="linklike"
                      onClick={() => props.onShowTry!(carried()!.take, carried()!.id)}
                    >
                      Show version {carried()!.take}, {noun().toLowerCase()} {carried()!.try}
                    </button>
                  </span>
                </Show>
                <Show when={notLatest()}>
                  {(n) => (
                    <span class="sp-box-d">
                      {n().text} ·{" "}
                      <button
                        class="linklike"
                        onClick={() => props.onAttemptSelect(props.tries[n().latest].id)}
                      >
                        Show {noun().toLowerCase()} {n().latest + 1}
                      </button>
                    </span>
                  )}
                </Show>
              </div>
            </Show>
            <div class="pane-tabbar">
              <div class="tabs">
                <Show
                  when={isPluginGate()}
                  fallback={
                    <>
                      <TabBtn id="logs" label="Logs" scope={tryScope()} />
                      <TabBtn id="results" label="Results" scope={tryScope()} />
                      <TabBtn id="workspace" label="Workspace" scope={tryScope()} />
                      <TabBtn id="outputs" label="Outputs" scope={outputsScope()} />
                      <Show when={(s().origin ?? []).length > 0}>
                        <TabBtn id="source" label="Source" />
                      </Show>
                    </>
                  }
                >
                  <TabBtn id="gate" label="Gate" />
                  <TabBtn id="outputs" label="Outputs" scope={outputsScope()} />
                </Show>
              </div>
              <span class="grow1" />
              <Show when={props.deadLettered && attemptsOf().some((a) => a.failed)}>
                <Pill tone="danger" class="adeadletter" title="no verdict obtainable — the operator signal">
                  ⊘ dead-lettered
                </Pill>
              </Show>
              <span class="pane-load" classList={{ on: loading() }} aria-hidden="true">
                <Icon icon="rotate-cw" size={13} />
              </span>
            </div>

            {/* Logs — the scoped attempt's stream (per-try). */}
            <Show when={tab() === "logs"}>
              <Show
                when={!notRun()}
                fallback={
                  <div class="tabpane">
                    <p class="empty">this step didn't run in this version</p>
                  </div>
                }
              >
              <div class="tabpane logpane">
                <div class="steplogs-tools">
                  {/* Container selector: when the step declares
                      sidecars, choose whose logs the pane tails — the step's
                      main container (default) or one sidecar. Same (step,
                      attempt) scope; only the streamed container changes. */}
                  <Show when={sidecars().length > 0}>
                    <div class="logsrc-seg" role="group" aria-label="log source">
                      <button
                        class={`lseg ${container() == null ? "on" : ""}`}
                        onClick={() => setContainer(null)}
                        title="the step's main container"
                      >
                        step
                      </button>
                      <For each={sidecars()}>
                        {(svc) => (
                          <button
                            class={`lseg ${container() === svc.index ? "on" : ""}`}
                            onClick={() => setContainer(svc.index)}
                            title={`${svc.image} · service-${svc.index}`}
                          >
                            {shortImage(svc.image)}
                          </button>
                        )}
                      </For>
                    </div>
                  </Show>
                  <span class="grow1" />
                  <button class={`lgtog ${wrap() ? "on" : ""}`} onClick={() => setWrap((v) => !v)}>
                    Wrap
                  </button>
                </div>
                <Show when={!switching()} fallback={<LogSkeleton />}>
                  <div class="lgbody" classList={{ nowrap: !wrap() }}>
                    <For
                      each={logRows()}
                      fallback={
                        <Show
                          when={logState() !== "waiting"}
                          fallback={
                            <div class="lgrow empty">
                              <span class="lgln" />
                              <span class="lgtx">waiting for output…</span>
                            </div>
                          }
                        >
                          <div class="lgrow empty">
                            <span class="lgln" />
                            <span class="lgtx">No log output was captured for this try.</span>
                          </div>
                          {/* A Pod that never started (unschedulable, an image
                              that will not pull, a volume that will not mount)
                              printed nothing, so the reason is on the attempt:
                              its failure class and the executor's own account. */}
                          <Show when={scopedFailure()}>
                            {(f) => (
                              <div class="lgrow empty lgwhy">
                                <span class="lgln" />
                                <span class="lgtx">
                                  <span class="lgwhy-reason">{f().failure} failure</span>
                                  {f().detail ? `: ${f().detail}` : ""}
                                </span>
                              </div>
                            )}
                          </Show>
                        </Show>
                      }
                    >
                      {(r) => (
                        <div class={`lgrow ${r.lvl}`}>
                          <span class="lgln">{r.n}</span>
                          <span class="lgtx">{r.line}</span>
                        </div>
                      )}
                    </For>
                  </div>
                </Show>
              </div>
              </Show>
            </Show>

            {/* Results — the scoped attempt's typed values (per-try). */}
            <Show when={tab() === "results"}>
              <div class="tabpane">
                <Show
                  when={!notRun()}
                  fallback={<p class="empty">nothing of record here</p>}
                >
                <Show
                  when={(results()?.length ?? 0) > 0}
                  fallback={<p class="empty">no results published by this try</p>}
                >
                  <div class="kvgrid">
                    <For each={results()}>
                      {(r) => (
                        <div class="kvcard">
                          <div class="kvk">
                            <span>{r.name}</span>
                            <Pill tone="copper">{r.type_name}</Pill>
                          </div>
                          <div class="kvv mono">{showValue(r.value)}</div>
                          <div class="kvfrom">
                            from{" "}
                            <b>{scoped() ? label(s().id, scoped()!.id) : s().id}</b>
                          </div>
                        </div>
                      )}
                    </For>
                  </div>
                </Show>
                </Show>
              </div>
            </Show>

            {/* Outputs — OF-RECORD: the interpolation view + the
                "built on …" consumed line both read the latest SUCCESSFUL attempt
                in this version (`ofRecordTry()`), NOT the selected try. So a
                failed / superseded / running selected try still shows what a
                downstream step would read, and a coordinate note names which try
                that is — with a "try N →" button that jumps the selection there.
                Never a blank-that-reads-as-success: no success ⇒ explicit copy. */}
            <Show when={tab() === "outputs"}>
              <div class="tabpane">
                <Show
                  when={ofRecordTry()}
                  fallback={
                    <p class="empty">
                      {notRun()
                        ? "nothing of record here"
                        : "nothing of record here — no successful attempt in this version"}
                    </p>
                  }
                >
                  {/* Coordinate note: is the of-record try the one selected? A
                      carried-forward step names the earlier version's try. */}
                  <div class="ofrec-note mono">
                    <Show when={carried()}>
                      {(c) => (
                        <span class="ofrec-here">
                          of record · from version {c().take}, try {c().try}
                        </span>
                      )}
                    </Show>
                    <Show when={!carried()}>
                    <Show
                      when={ofRecordTry() === scoped()?.id}
                      fallback={
                        <button
                          class="ofrec-jump"
                          onClick={() => props.onAttemptSelect(ofRecordTry())}
                          title="jump the selection to the of-record try"
                        >
                          of record · try {ofRecordIndex() + 1} →
                        </button>
                      }
                    >
                      <span class="ofrec-here">of record · this try</span>
                    </Show>
                    </Show>
                  </div>
                  <Show when={outConsumed() && Object.keys(outConsumed()!.consumed).length > 0}>
                    <div class="consumed mono">
                      built on{" "}
                      <For each={Object.entries(outConsumed()!.consumed)}>
                        {([up, at], i) => (
                          <>
                            <Show when={i() > 0}> · </Show>
                            <span class="consumed-edge">{label(up, at)}</span>
                          </>
                        )}
                      </For>
                    </div>
                  </Show>
                  <Show
                    when={(outResults()?.length ?? 0) > 0}
                    fallback={
                      <p class="empty">nothing for a downstream step to read of record</p>
                    }
                  >
                    <div class="exprs">
                      <For each={outResults()}>
                        {(r) => (
                          <div class="expr mono">
                            <span class="tok">{`\${{ outputs.${s().id}.${r.name} }}`}</span>
                            <span class="arrow">→</span>
                            <span class="val">{showValue(r.value)}</span>
                          </div>
                        )}
                      </For>
                    </div>
                  </Show>
                </Show>
              </div>
            </Show>

            {/* Workspace — the scoped attempt's immutable snapshot (per-try). */}
            <Show when={tab() === "workspace"}>
              <div class="tabpane">
                <Show
                  when={!notRun()}
                  fallback={<p class="empty">this step didn't run in this version</p>}
                >
                <div class="ws-crumbs mono">
                  <button class="crumb" onClick={() => navTo("")}>
                    {scoped() ? label(s().id, scoped()!.id) : s().id}
                  </button>
                  <For each={crumbs()}>
                    {(c, i) => (
                      <>
                        <span class="sep">/</span>
                        <button class="crumb" onClick={() => goto(i())}>
                          {c}
                        </button>
                      </>
                    )}
                  </For>
                  <Show when={openFile()}>
                    <span class="sep">/</span>
                    <span class="crumb file">{openFile()!.split("/").pop()}</span>
                  </Show>
                </div>

                <Show when={openFile()}>
                  <div class="fileview">
                    <div class="fileview-h">
                      <span class="mono">{openFile()!.split("/").pop()}</span>
                      <a
                        class="fv-dl mono"
                        href={workspaceFileUrl(props.runId, s().id, openFile()!, scoped()?.id)}
                        download={openFile()!.split("/").pop()}
                      >
                        download ↓
                      </a>
                      <button class="fv-close" onClick={() => setOpenFile(null)} title="close">
                        ✕
                      </button>
                    </div>
                    <Show
                      when={!fileBody.loading}
                      fallback={
                        <div class="fileview-body">
                          <span class="lgln" />
                          loading…
                        </div>
                      }
                    >
                      <Show
                        when={!fileBody()?.error}
                        fallback={
                          <div class="fileview-body binary">
                            could not read this file — it may no longer exist in the snapshot
                          </div>
                        }
                      >
                      <Show
                        when={!fileBody()?.binary}
                        fallback={
                          <div class="fileview-body binary">binary file — use download</div>
                        }
                      >
                        <div class="fileview-body mono">
                          <For each={(fileBody()?.text ?? "").replace(/\n$/, "").split("\n")}>
                            {(line, i) => (
                              <div class="fvrow">
                                <span class="lgln">{i() + 1}</span>
                                <span class="lgtx">{line}</span>
                              </div>
                            )}
                          </For>
                        </div>
                      </Show>
                      </Show>
                    </Show>
                  </div>
                </Show>

                <Show when={!openFile()}>
                  <Show
                    when={ws()?.available}
                    fallback={
                      <Show when={!ws.loading} fallback={<p class="empty">loading…</p>}>
                        <div class="ws-fallback">
                          <p class="empty">
                            No snapshot for this try — it's still running, a gate,
                            a backend that doesn't snapshot, or the CAS was cleared.
                          </p>
                          <Show when={props.canDebug && props.onDebugPod}>
                            <button class="btn btn-ghost btn-sm" onClick={() => props.onDebugPod!()}>
                              <Icon icon="terminal" size={13} /> Explore in a Debug pod →
                            </button>
                            <p class="subtle">
                              <small>
                                A Debug pod reproduces the step in a fresh Pod — it re-runs,
                                so it shows a reproduction, not this attempt's exact bytes.
                              </small>
                            </p>
                          </Show>
                        </div>
                      </Show>
                    }
                  >
                    <ul class="filelist">
                      <For
                        each={ws()?.entries}
                        fallback={<li class="filerow empty">empty directory</li>}
                      >
                        {(e) => (
                          <li class="filerow">
                            <Show
                              when={e.kind === "dir"}
                              fallback={
                                <button
                                  class="fname asdir"
                                  onClick={() => setOpenFile(filePath(e.name))}
                                >
                                  <Icon icon="file" size={14} class="fico-file" />
                                  {e.name}
                                </button>
                              }
                            >
                              <button class="fname asdir" onClick={() => enter(e.name)}>
                                <Icon icon="folder" size={14} class="fico-dir" />
                                {e.name}
                              </button>
                            </Show>
                            <span class="fsize mono">{e.kind}</span>
                            {/* A symlink's identity (git-bug 1344d1d):
                                the snapshot stores only the target PATH, so
                                show it — opening the entry streams that path,
                                not the target's content. */}
                            <span class="fmeta">
                              {e.kind === "symlink" && e.target ? `→ ${e.target}` : ""}
                            </span>
                          </li>
                        )}
                      </For>
                    </ul>
                  </Show>
                </Show>
                </Show>
              </div>
            </Show>

            {/* Source: the invoke chain, the inputs bound in this Step's
                invocation, and the Step as its launch renders it. */}
            <Show when={tab() === "source"}>
              <div class="tabpane">
                <Show when={origin()} fallback={<p class="empty">{origin.loading ? "loading…" : "Could not load where this Step comes from."}</p>}>
                  {(o) => (
                    <div class="split2">
                      <div>
                        <p class="pane-sub">Invoke chain</p>
                        <ol class="chain">
                          <li>
                            <span class="dot">
                              <Icon icon="file-code" size={12} />
                            </span>
                            <div class="t">
                              <div class="l1">
                                <Pill tone="muted" look="dotted">pipeline</Pill>
                                <span class="mono">{props.pipeline ? `.scarab/${props.pipeline}.yaml` : "the Pipeline file"}</span>
                              </div>
                              <div class="lib-src">the Run's own file</div>
                            </div>
                          </li>
                          <For each={o().chain}>
                            {(c, k) => (
                              <li>
                                <span class="dot">
                                  <Icon icon={c.kind === "plugin" ? "shield-check" : "package"} size={12} />
                                </span>
                                <div class="t">
                                  <div class="l1">
                                    <Pill tone="muted" look="dotted">{c.kind === "library" ? "lib" : c.kind}</Pill>
                                    <span class="mono">{c.name}</span>
                                    <span class="lib-hash">{shortHash(c.content_hash)}</span>
                                    <Show when={c.kind !== "plugin"}>
                                      <span class="subtle">
                                        invoked as <span class="mono">{s().id.split("/").slice(0, k() + 1).join("/")}</span>
                                      </span>
                                    </Show>
                                  </div>
                                  <Show when={c.source}>{(src) => <div class="lib-src">{src()}</div>}</Show>
                                  <Show when={c.kind === "library"}>
                                    <div class="subtle">
                                      {c.yanked ? <span class="plg-killed">yanked in this org</span> : "not yanked"}
                                    </div>
                                  </Show>
                                </div>
                              </li>
                            )}
                          </For>
                          <li>
                            <span class="dot" style={{ "border-color": "var(--emerald)", color: "var(--emerald)" }}>
                              <Icon icon="container" size={12} />
                            </span>
                            <div class="t">
                              <div class="l1">
                                <Pill tone="muted" look="dotted">step</Pill>
                                <span class="mono">{s().id.split("/").pop()}</span>
                                <span class="subtle">
                                  → <span class="mono">{s().id}</span> in this Run
                                </span>
                              </div>
                            </div>
                          </li>
                        </ol>
                      </div>
                      <div>
                        <p class="pane-sub">Inputs in this invocation's scope</p>
                        <Show when={o().inputs.length > 0} fallback={<p class="empty">none: a Step in the Pipeline file reads the Run's parameters</p>}>
                          <table class="table inputs-tbl">
                            <thead>
                              <tr>
                                <th>Input</th>
                                <th>Bound</th>
                                <th>Kind</th>
                              </tr>
                            </thead>
                            <tbody>
                              <For each={o().inputs}>
                                {(i) => (
                                  <tr>
                                    <td class="mono">{i.name}</td>
                                    <td class="mono">{typeof i.bound === "string" ? i.bound : JSON.stringify(i.bound)}</td>
                                    <td class="subtle">{i.kind}</td>
                                  </tr>
                                )}
                              </For>
                            </tbody>
                          </table>
                        </Show>
                        <p class="pane-sub" style={{ "margin-top": "14px" }}>Rendered at launch</p>
                        <Show when={o().rendered} fallback={<p class="empty">{o().render_note ?? "not rendered"}</p>}>
                          {(r) => (
                            <pre class="rendered">
                              {`image    ${r().image}\ncommand  ${r().command.join(" ")}${r()
                                .env.map(([k, v], n) => `\n${n === 0 ? "env      " : "         "}${k}=${v}`)
                                .join("")}`}
                            </pre>
                          )}
                        </Show>
                      </div>
                    </div>
                  )}
                </Show>
              </div>
            </Show>

            {/* A plugin gate: who releases it, its expiry, the
                inputs rendered when it was reached, and its typed outputs. */}
            <Show when={tab() === "gate" ? s().plugin_gate : null}>
              {(g) => {
                const state = () => pluginGateState(s());
                const left = () => (g().expires_at != null ? g().expires_at! - (props.now ?? Date.now()) : null);
                const bound = (name: string) => origin()?.inputs.find((i) => i.name === name);
                return (
                  <div class="tabpane">
                    <div class="gp">
                      <div class="gp-card">
                        <h4>Who releases it</h4>
                        <div class="gp-big gp-name">
                          {g().releaser}
                        </div>
                        <div class="gp-line">
                          <Show
                            when={g().plugin_state === "killed"}
                            fallback={
                              <>
                                <span class="plg-enabled">● {g().plugin_state ?? "not installed"}</span> · gate{" "}
                                <span class="mono">{g().gate}</span>
                              </>
                            }
                          >
                            <span class="plg-killed">● killed</span>
                            {g().killed_by ? ` by ${g().killed_by}` : ""}
                            {g().killed_at ? ` · ${absTime(g().killed_at!)}` : ""}
                          </Show>
                        </div>
                        <div class="gp-line">
                          Only {g().installation} can pass this gate. Anyone with write access to the
                          Project can reject it with a reason. Nobody can approve it in the plugin's
                          place.
                        </div>
                      </div>
                      <div class="gp-card">
                        <h4>Expiry</h4>
                        <Show
                          when={g().expires_at != null}
                          fallback={<div class="gp-line">The wait has not started: the gate is not reached yet.</div>}
                        >
                          <div
                            class={`gp-big ${state() === "released" ? "good" : state() === "expired" || state() === "rejected" ? "bad" : "warn"}`}
                          >
                            {state() === "released"
                              ? "released"
                              : state() === "expired"
                                ? "expired"
                                : state() === "rejected"
                                  ? "rejected"
                                  : `${clock(left()!)} left`}
                          </div>
                          <div class="bar">
                            <i
                              style={{
                                width: `${state() === "expired" ? 100 : expiryPercent(g().waiting_since!, g().expires_at!, state() === "released" || state() === "rejected" ? (s().gate_resolution?.at ?? props.now ?? 0) : (props.now ?? Date.now()))}%`,
                                background: left() != null && clockLow(left()!) && state() === "waiting" ? "var(--danger)" : undefined,
                              }}
                            />
                          </div>
                          <div class="gp-line">
                            reached {absTime(g().waiting_since!)} ·{" "}
                            <span class="mono">gate_expires_after: {g().expires_after}</span>
                            {state() === "waiting" || state() === "killed"
                              ? ` · fails at ${absTime(g().expires_at!)} if not released`
                              : s().gate_resolution
                                ? ` · ${state()} ${absTime(s().gate_resolution!.at)}`
                                : ""}
                          </div>
                        </Show>
                      </div>
                      <div class="gp-card">
                        <h4>Inputs · rendered when the gate was reached</h4>
                        <Show
                          when={g().inputs && typeof g().inputs === "object"}
                          fallback={<div class="gp-line">Rendered when the gate is reached.</div>}
                        >
                          <table class="table">
                            <tbody>
                              <For each={Object.entries((g().inputs ?? {}) as Record<string, unknown>)}>
                                {([k, v]) => (
                                  <tr>
                                    <td class="mono">{k}</td>
                                    <td class="mono">{typeof v === "string" ? v : JSON.stringify(v)}</td>
                                    <td class="subtle mono">
                                      {bound(k)?.kind === "template" ? String(bound(k)!.bound) : bound(k) ? "literal" : ""}
                                    </td>
                                  </tr>
                                )}
                              </For>
                            </tbody>
                          </table>
                        </Show>
                      </div>
                      <div class="gp-card">
                        <h4>Outputs</h4>
                        <Show when={g().outputs.length > 0} fallback={<div class="gp-line">This gate returns no outputs.</div>}>
                          <For each={g().outputs}>
                            {(o) => (
                              <div class="gp-line">
                                <span class="mono">{o.name}</span>{" "}
                                <Pill tone="copper" look="dotted">
                                  {o.type}
                                </Pill>
                                {o.value !== undefined && o.value !== null ? (
                                  <>
                                    {" "}= <span class="mono">{typeof o.value === "string" ? o.value : JSON.stringify(o.value)}</span> · by{" "}
                                    <b>{g().releaser}</b>
                                  </>
                                ) : state() === "waiting" || state() === "killed" ? (
                                  <>: set by {g().installation} when it releases the gate</>
                                ) : (
                                  <> was never set; Steps that read it did not run</>
                                )}
                                {" "}· read as <span class="mono">{`\${{ outputs.${s().id}.${o.name} }}`}</span>
                              </div>
                            )}
                          </For>
                        </Show>
                      </div>
                    </div>
                  </div>
                );
              }}
            </Show>
          </>
        )}
      </Show>
    </div>
  );
}
