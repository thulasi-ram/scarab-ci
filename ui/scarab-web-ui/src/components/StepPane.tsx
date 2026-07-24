// The merged step pane (ADR-0056): ONE pane for the DAG-selected step, with an
// attempt strip on top that scopes EVERY tab — Logs, Results, Outputs,
// Workspace — to the selected (step, attempt). This replaces the old stacked
// Logs panel + Inspector, whose asymmetry (only logs knew about attempts) let
// a rerun silently shadow the evidence the other tabs showed. Chips carry each
// try's cause (auto-retry / you reran / ⟵ cascade) and outcome (failed ✗ /
// superseded ⊘ / shadowed) from the event log (ADR-0056 amendment); a crash
// re-adoption renders as a marker INSIDE its chip — same attempt, same fence,
// never a new execution.
import { createSignal, createEffect, createMemo, createResource, on, For, Show, onCleanup } from "solid-js";
import {
  getStepResults,
  getConsumed,
  listWorkspace,
  workspaceFileUrl,
  streamStepLogs,
  streamStepSidecarLogs,
  type StepStatus,
  type Attempt,
} from "../api/client";
import type { RunEvent } from "../api/client";
import { ofRecordAttemptId } from "../takes";
import Icon from "./Icon";
import AttemptsDropdown, { type FilmstripTry } from "./AttemptsDropdown";

type Tab = "logs" | "results" | "outputs" | "workspace";

// Cap rendered lines so a giant attempt can't blow up the DOM (windowing v1).
const MAX_LINES = 1500;

type Level = "err" | "warn" | "ok" | "cmd" | "";
function levelOf(line: string): Level {
  if (/^\s*\$ /.test(line)) return "cmd";
  if (/\b(error|panic|fatal)\b/i.test(line) || /^error(\[|:)/i.test(line)) return "err";
  if (/\bwarn(ing)?\b/i.test(line)) return "warn";
  if (/\b(finished|passed|ok|success(ful)?)\b/i.test(line)) return "ok";
  return "";
}

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
   * header filmstrip renders these (the try axis, ADR-0056 amendment). */
  tries: FilmstripTry[];
  /** The try the filmstrip highlights as active (resolved to a concrete id). */
  activeAttempt: string | null;
  /** Pick a try in the filmstrip — scopes every tab to `(step, attempt)`. */
  onAttemptSelect: (id: string | null) => void;
  /** The viewed version's label — the trailing coordinate in the pane's stamp.
   * Only carries a value while TIME-TRAVELLING (a rerun label like "you reran
   * b"); the caller passes `null` on the live/latest view so the stamp doesn't
   * tag every attempt with a redundant "latest" (the toolbar version dropdown
   * already says so). */
  versionLabel: string | null;
  /** Viewing a closed Take: pin the strip's default to this frontier attempt
   * and mark attempts beyond it as from a later take. */
  frontierAttempt?: string | null;
  /** The SELECTED take's attempt-id window (ADR-0056 amendment 2026-07-24) —
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
  /** A sidecar container index to focus in the Logs tab on selection (ADR-0058)
   * — set when the user clicked this step's docked sidecar chip in the DAG.
   * `null`/absent = the step's main container. */
  focusSidecar?: number | null;
}) {
  const [tab, setTab] = createSignal<Tab>("logs");
  const [wsPath, setWsPath] = createSignal("");
  const [openFile, setOpenFile] = createSignal<string | null>(null);
  const [wrap, setWrap] = createSignal(true);
  // Which container's logs the Logs tab shows: `null` = the step's main
  // container (default); a number = the sidecar at that `services:` index
  // (ADR-0058). Reset per selection from `focusSidecar` below.
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
  const attemptN = (id: string) => parseInt(id.replace(/^a/, ""), 10) || 0;
  // The attempts this pane's tabs are scoped to. Take-windowed in EVERY view
  // (ADR-0056 amendment 2026-07-24): when the caller hands a `window` (the
  // selected take's attempt-id set), keep only those tries — so a step carried
  // forward untouched (`[]`) reads as "didn't run in this version", and a
  // re-run step shows only THIS take's tries, on the latest/live take too. The
  // `frontierAttempt` ≤-filter stays as the fallback for callers that don't
  // pass a window (snapshot-at-boundary honesty).
  const attemptsOf = (): Attempt[] => {
    const list = props.step?.attempt_list ?? [];
    const w = props.window;
    if (w) return list.filter((a) => w.includes(a.id));
    const f = props.frontierAttempt;
    return f ? list.filter((a) => attemptN(a.id) <= attemptN(f)) : list;
  };

  // The attempt every tab is scoped to: explicit selection, else the Take
  // frontier (closed-take view), else the latest attempt.
  const scoped = (): Attempt | null => {
    const list = attemptsOf();
    if (!list.length) return null;
    const want = props.attempt ?? props.frontierAttempt ?? null;
    return list.find((a) => a.id === want) ?? list[list.length - 1];
  };

  // The selected step didn't run in the viewed version — the caller's filmstrip
  // (already windowed to this version's tries) is empty. Both a time-travel
  // `not_run` step (re-armed but never executed in a superseded version) and a
  // not-yet-launched step land here. Its tabs read a short "didn't run / nothing
  // of record" line instead of a stale or empty-looking shell (ADR-0056 §3:
  // never render blank-that-reads-as-success).
  const notRun = (): boolean => !!props.step && props.tries.length === 0;

  // Of-record (ADR-0056 §3): the Outputs tab shows the latest SUCCESSFUL attempt
  // WITHIN THE VIEWED VERSION, not the selected try — so a failed/superseded/
  // running selected try still surfaces the values a downstream step would read.
  // `attemptsOf()` is already windowed to this version; a not-run step (or one
  // with no success here) has nothing of record.
  const ofRecordTry = (): string | null =>
    notRun() ? null : ofRecordAttemptId(attemptsOf());
  const ofRecordIndex = (): number => {
    const rec = ofRecordTry();
    return rec ? attemptsOf().findIndex((a) => a.id === rec) : -1;
  };

  // Reset per-step view state when the DAG selection moves.
  createEffect(() => {
    void stepId();
    setWsPath("");
    setOpenFile(null);
  });

  // The Logs container follows the selection (ADR-0058): a clicked sidecar chip
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
  const streams = new Map<string, () => void>();
  // Keyed by (step, attempt, container) — the container axis (main vs a sidecar
  // index) keeps each container's tail in its own buffer/stream (ADR-0058).
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
    // Sidecar logs share the step's real attempt ids, so the attempt scoping is
    // identical — only the endpoint (`?sidecar=<index>`) differs.
    const close =
      c == null
        ? streamStepLogs(props.runId, s, { attempt: a.id, onChunk })
        : streamStepSidecarLogs(props.runId, s, c, { attempt: a.id, onChunk });
    streams.set(key, close);
  });
  onCleanup(() => streams.forEach((close) => close()));

  const logText = () => {
    const s = stepId();
    const a = scoped();
    return s && a ? (buffers()[logKey(s, a.id, container())] ?? "waiting for output…") : "";
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
  // (`scoped()`), ADR-0056. Outputs read a DIFFERENT (of-record) attempt below. ---
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

  // Outputs are OF-RECORD, not per-try (ADR-0056 §3): they resolve to the of-
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

  const TabBtn = (p: { id: Tab; label: string; count?: number }) => (
    <button class={`tab ${tab() === p.id ? "active" : ""}`} onClick={() => setTab(p.id)}>
      {p.label}
      <Show when={p.count !== undefined}>
        <span class="tcount">{p.count}</span>
      </Show>
    </button>
  );

  return (
    // `inspector` carries the shared tab styling; `steppane` adds the strip +
    // in-pane log treatment (ADR-0056).
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
            {/* Evidence header. A permanent coordinate stamp names WHERE you are
                — step · [attempts] · version — so the evidence below never floats
                context-free (redesign stage 4). The attempts dropdown occupies
                the "try N" position of the stamp: it names the active try (the
                one scoping every tab) and, when a step has more than one try,
                opens a menu to switch between them (design feedback — the compact
                dropdown replaced the noisier filmstrip). It renders nothing when
                nothing ran here, so the stamp reads `step · version` for a
                not-run step. A spinner on the right of the tab row is the only
                cue that a switch took effect and fresh evidence is loading;
                dead-letter, being terminal + rare, keeps a badge there. */}
            <div class="coord-stamp mono">
              {/* The attempts dropdown leads the stamp (left-most), sitting under
                  the toolbar's version dropdown so the two run-detail dropdowns
                  align into one column. The step id follows, then the version
                  segment — shown ONLY while time-travelling (a redundant
                  "latest" on the live view is dropped). */}
              <Show when={props.tries.length > 0}>
                <AttemptsDropdown
                  tries={props.tries}
                  active={props.activeAttempt}
                  onSelect={props.onAttemptSelect}
                />
                <span class="cs-dot">·</span>
              </Show>
              <span class="cs-step">{s().id}</span>
              <Show when={props.versionLabel}>
                <span class="cs-dot">·</span>
                <span class="cs-ver">{props.versionLabel}</span>
              </Show>
            </div>
            <div class="pane-tabbar">
              <div class="tabs">
                <TabBtn id="logs" label="Logs" />
                <TabBtn id="results" label="Results" count={results()?.length ?? 0} />
                <TabBtn id="outputs" label="Outputs" count={outResults()?.length ?? 0} />
                <TabBtn id="workspace" label="Workspace" />
              </div>
              <span class="grow1" />
              <Show when={props.deadLettered && attemptsOf().some((a) => a.failed)}>
                <span class="adeadletter" title="no verdict obtainable — the operator signal">
                  ⊘ dead-lettered
                </span>
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
                  {/* Container selector (ADR-0058): when the step declares
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
                    wrap
                  </button>
                </div>
                <Show when={!switching()} fallback={<LogSkeleton />}>
                  <div class="lgbody" classList={{ nowrap: !wrap() }}>
                    <For
                      each={logRows()}
                      fallback={
                        <div class="lgrow empty">
                          <span class="lgln" />
                          <span class="lgtx">no output for this try</span>
                        </div>
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
                            <span class="type">{r.type_name}</span>
                          </div>
                          <div class="kvv mono">{showValue(r.value)}</div>
                          <div class="kvfrom">
                            from <b>{s().id}</b>
                            <Show when={scoped()}> @ {scoped()!.id}</Show>
                          </div>
                        </div>
                      )}
                    </For>
                  </div>
                </Show>
                </Show>
              </div>
            </Show>

            {/* Outputs — OF-RECORD (ADR-0056 §3): the interpolation view + the
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
                  {/* Coordinate note: is the of-record try the one selected? */}
                  <div class="ofrec-note mono">
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
                  </div>
                  <Show when={outConsumed() && Object.keys(outConsumed()!.consumed).length > 0}>
                    <div class="consumed mono">
                      built on{" "}
                      <For each={Object.entries(outConsumed()!.consumed)}>
                        {([up, at], i) => (
                          <>
                            <Show when={i() > 0}> · </Show>
                            <span class="consumed-edge">
                              {up}@{at}
                            </span>
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
                    {s().id}
                    <Show when={scoped()}>@{scoped()!.id}</Show>
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
                            <span class="fmeta" />
                          </li>
                        )}
                      </For>
                    </ul>
                  </Show>
                </Show>
                </Show>
              </div>
            </Show>
          </>
        )}
      </Show>
    </div>
  );
}
