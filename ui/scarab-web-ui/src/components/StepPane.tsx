// The merged step pane (ADR-0056): ONE pane for the DAG-selected step, with an
// attempt strip on top that scopes EVERY tab — Logs, Results, Outputs,
// Workspace — to the selected (step, attempt). This replaces the old stacked
// Logs panel + Inspector, whose asymmetry (only logs knew about attempts) let
// a rerun silently shadow the evidence the other tabs showed. Chips carry each
// try's cause (auto-retry / you reran / ⟵ cascade) and outcome (failed ✗ /
// superseded ⊘ / shadowed) from the event log (ADR-0056 amendment); a crash
// re-adoption renders as a marker INSIDE its chip — same attempt, same fence,
// never a new execution.
import { createSignal, createEffect, createResource, For, Show, onCleanup } from "solid-js";
import {
  getStepResults,
  getConsumed,
  listWorkspace,
  workspaceFileUrl,
  streamStepLogs,
  type StepStatus,
  type Attempt,
} from "../api/client";
import type { RunEvent } from "../api/client";
import { attemptCauses } from "../takes";
import Icon from "./Icon";

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
  /** Selected attempt id (the strip's selection); null = latest. */
  attempt: string | null;
  onAttemptSelect: (id: string | null) => void;
  /** Viewing a closed Take: pin the strip's default to this frontier attempt
   * and mark attempts beyond it as from a later take. */
  frontierAttempt?: string | null;
  /** The run was dead-lettered — the step strip's terminal ⊘ marker. */
  deadLettered?: boolean;
  /** Open a Debug pod for this step. The Workspace tab offers it as the fallback
   * when no snapshot exists — a fresh reproduction is the robust way to explore
   * a filesystem the CAS can't hand back (note: it re-runs, so it's NOT the
   * attempt's immutable bytes). Absent/`canDebug:false` ⇒ the affordance hides. */
  onDebugPod?: () => void;
  canDebug?: boolean;
}) {
  const [tab, setTab] = createSignal<Tab>("logs");
  const [wsPath, setWsPath] = createSignal("");
  const [openFile, setOpenFile] = createSignal<string | null>(null);
  const [wrap, setWrap] = createSignal(true);

  const stepId = () => props.step?.id ?? null;
  const attemptsOf = (): Attempt[] => props.step?.attempt_list ?? [];

  // The attempt every tab is scoped to: explicit selection, else the Take
  // frontier (closed-take view), else the latest attempt.
  const scoped = (): Attempt | null => {
    const list = attemptsOf();
    if (!list.length) return null;
    const want = props.attempt ?? props.frontierAttempt ?? null;
    return list.find((a) => a.id === want) ?? list[list.length - 1];
  };

  // Reset per-step view state when the DAG selection moves.
  createEffect(() => {
    void stepId();
    setWsPath("");
    setOpenFile(null);
  });

  const causes = () => (stepId() ? attemptCauses(props.events, stepId()!) : null);

  const isSuperseded = (a: Attempt) => causes()?.superseded.has(a.id) ?? false;
  const isShadowed = (a: Attempt) => causes()?.shadowed.has(a.id) ?? false;

  // Plain-english cause suffix (ADR-0056 amendment): the machine's own retry vs
  // the human reran-this-step vs a rerun of an ancestor that dragged this along.
  const causeSuffix = (a: Attempt): string => {
    const cause = causes()?.causes[a.id];
    switch (cause) {
      case "rerun":
        return " · you reran";
      case "cascade":
        return " · ⟵ rerun";
      case "retry":
        return " · auto-retry";
      default:
        return "";
    }
  };

  // Outcome glyph: superseded (cut short) ranks before failed; both before ok.
  const chipLabel = (a: Attempt, i: number) => {
    const outcome = isSuperseded(a)
      ? " ⊘ superseded"
      : a.failed
        ? ` ✗ ${a.failure ?? ""}`
        : "";
    return `try ${i + 1}${outcome}${causeSuffix(a)}`;
  };

  // --- Logs: one buffered SSE stream per (step, attempt); the scoped attempt's
  // buffer renders. Historical attempts replay and close; the live one tails. ---
  const [buffers, setBuffers] = createSignal<Record<string, string>>({});
  const streams = new Map<string, () => void>();
  const logKey = (step: string, attempt: string) => `${step} ${attempt}`;

  createEffect(() => {
    const s = stepId();
    const a = scoped();
    if (!s || !a || tab() !== "logs") return;
    const key = logKey(s, a.id);
    if (streams.has(key)) return;
    const close = streamStepLogs(props.runId, s, {
      attempt: a.id,
      onChunk: (t) => setBuffers((prev) => ({ ...prev, [key]: (prev[key] ?? "") + t + "\n" })),
    });
    streams.set(key, close);
  });
  onCleanup(() => streams.forEach((close) => close()));

  const logText = () => {
    const s = stepId();
    const a = scoped();
    return s && a ? (buffers()[logKey(s, a.id)] ?? "waiting for output…") : "";
  };
  const logRows = () => {
    const all = logText().split("\n");
    const sliced = all.length > MAX_LINES ? all.slice(all.length - MAX_LINES) : all;
    const base = all.length > MAX_LINES ? all.length - MAX_LINES : 0;
    return sliced
      .map((line, i) => ({ n: base + i + 1, line, lvl: levelOf(line) }))
      .filter((r) => r.line.length > 0);
  };

  // --- Results / Outputs / Workspace: attempt-scoped fetches (ADR-0056). ---
  const evidenceArg = () => {
    const s = stepId();
    const a = scoped();
    return s ? { run: props.runId, step: s, attempt: a?.id } : null;
  };
  const [results] = createResource(evidenceArg, (a) =>
    getStepResults(a.run, a.step, a.attempt).catch(() => []),
  );
  const [consumed] = createResource(evidenceArg, (a) =>
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
            {/* Attempt strip: every tab below is scoped to the lit chip. */}
            <div class="astrip">
              <span class="astrip-step mono">{s().id}</span>
              <For each={attemptsOf()}>
                {(a, i) => (
                  <button
                    class="achip"
                    classList={{
                      failed: a.failed && !isSuperseded(a),
                      superseded: isSuperseded(a),
                      shadowed: isShadowed(a),
                      ok: !a.failed && !isSuperseded(a),
                      sel: scoped()?.id === a.id,
                    }}
                    onClick={() => props.onAttemptSelect(a.id)}
                    title={`try ${a.id}${
                      isSuperseded(a)
                        ? " — cut short by a rerun of an upstream step (superseded)"
                        : isShadowed(a)
                          ? " — succeeded, but a newer try replaced it as the of-record version (shadowed)"
                          : ""
                    }${
                      causes()?.readopted.has(a.id)
                        ? " — re-adopted after a control-plane restart (same attempt, same fence)"
                        : ""
                    }`}
                  >
                    {chipLabel(a, i())}
                    <Show when={isShadowed(a)}>
                      <span class="ashadow" title="no longer the of-record version">
                        shadowed
                      </span>
                    </Show>
                    <Show when={causes()?.readopted.has(a.id)}>
                      <span class="areadopt" title="re-adopted after control-plane restart">
                        ⟲
                      </span>
                    </Show>
                  </button>
                )}
              </For>
              <Show when={props.deadLettered && attemptsOf().some((a) => a.failed)}>
                <span class="adeadletter" title="no verdict obtainable — the operator signal">
                  ⊘ dead-lettered
                </span>
              </Show>
              <Show when={attemptsOf().length === 0}>
                <span class="subtle">not started</span>
              </Show>
            </div>

            <div class="tabs">
              <TabBtn id="logs" label="Logs" />
              <TabBtn id="results" label="Results" count={results()?.length ?? 0} />
              <TabBtn id="outputs" label="Outputs" count={results()?.length ?? 0} />
              <TabBtn id="workspace" label="Workspace" />
            </div>

            {/* Logs — the scoped attempt's stream. */}
            <Show when={tab() === "logs"}>
              <div class="tabpane logpane">
                <div class="steplogs-tools">
                  <span class="subtle mono">
                    {scoped() ? `try ${scoped()!.id}` : "no tries"}
                  </span>
                  <span class="grow1" />
                  <button class={`lgtog ${wrap() ? "on" : ""}`} onClick={() => setWrap((v) => !v)}>
                    wrap
                  </button>
                </div>
                <div class="lgbody" classList={{ nowrap: !wrap() }}>
                  <For
                    each={logRows()}
                    fallback={
                      <div class="lgrow empty">
                        <span class="lgln" />
                        <span class="lgtx">no output</span>
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
              </div>
            </Show>

            {/* Results — the scoped attempt's typed values. */}
            <Show when={tab() === "results"}>
              <div class="tabpane">
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
              </div>
            </Show>

            {/* Outputs — interpolation view + consumption provenance. */}
            <Show when={tab() === "outputs"}>
              <div class="tabpane">
                <Show when={consumed() && Object.keys(consumed()!.consumed).length > 0}>
                  <div class="consumed mono">
                    built on{" "}
                    <For each={Object.entries(consumed()!.consumed)}>
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
                  when={(results()?.length ?? 0) > 0}
                  fallback={
                    <p class="empty">nothing for a downstream step to read from this try</p>
                  }
                >
                  <div class="exprs">
                    <For each={results()}>
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
              </div>
            </Show>

            {/* Workspace — the scoped attempt's immutable snapshot. */}
            <Show when={tab() === "workspace"}>
              <div class="tabpane">
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
              </div>
            </Show>
          </>
        )}
      </Show>
    </div>
  );
}
