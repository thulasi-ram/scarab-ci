// The run detail Inspector — browse a step's outputs (ADR-0041/0029/0052). Four
// tabs: Results (typed values a step published), Outputs (how those read as
// `${{ outputs.<step>.<name> }}`), Artifacts (files of record, run-level), and
// Workspace (the step's output-snapshot filesystem, walked read-only from the
// content-addressed store). The per-step tabs follow the DAG selection.
import { createResource, createSignal, For, Show } from "solid-js";
import {
  getStepResults,
  listWorkspace,
  workspaceFileUrl,
  artifactUrl,
  type Artifact,
} from "../api/client";
import Icon from "./Icon";

type Tab = "results" | "outputs" | "artifacts" | "workspace";

/** Human byte size. */
function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** A compact display of a result value — scalars inline, structures as JSON. */
function showValue(v: unknown): string {
  if (typeof v === "string") return v;
  return JSON.stringify(v);
}

export default function Inspector(props: {
  runId: string;
  selectedStep: string | null;
  artifacts: Artifact[];
}) {
  const [tab, setTab] = createSignal<Tab>("results");
  const [wsPath, setWsPath] = createSignal("");
  // A file open for inline preview (path within the snapshot), or null = listing.
  const [openFile, setOpenFile] = createSignal<string | null>(null);

  // Fetch an opened file's bytes; text renders inline, binary offers download.
  const fileArg = () =>
    props.selectedStep && openFile()
      ? { run: props.runId, step: props.selectedStep, path: openFile()! }
      : null;
  const [fileBody] = createResource(fileArg, async (a) => {
    const res = await fetch(workspaceFileUrl(a.run, a.step, a.path));
    const ct = res.headers.get("content-type") ?? "";
    if (!ct.startsWith("text/")) return { binary: true, text: "" };
    return { binary: false, text: await res.text() };
  });

  const stepArg = () => (props.selectedStep ? { run: props.runId, step: props.selectedStep } : null);

  // Results (also the source the Outputs view derives from).
  const [results] = createResource(stepArg, (a) => getStepResults(a.run, a.step).catch(() => []));

  // Workspace listing at the current path, re-fetched on step/path change.
  const wsArg = () =>
    props.selectedStep ? { run: props.runId, step: props.selectedStep, path: wsPath() } : null;
  const [ws] = createResource(wsArg, (a) => listWorkspace(a.run, a.step, a.path));

  const crumbs = () => wsPath().split("/").filter(Boolean);
  // Navigating the tree closes any open file preview.
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
    <div class="panel inspector">
      <div class="tabs">
        <TabBtn id="results" label="Results" count={results()?.length ?? 0} />
        <TabBtn id="outputs" label="Outputs" count={results()?.length ?? 0} />
        <TabBtn id="artifacts" label="Artifacts" count={props.artifacts.length} />
        <TabBtn id="workspace" label="Workspace" />
      </div>

      <Show
        when={props.selectedStep || tab() === "artifacts"}
        fallback={<div class="tabpane"><p class="empty">select a step to inspect its outputs</p></div>}
      >
        {/* Results */}
        <Show when={tab() === "results"}>
          <div class="tabpane">
            <Show
              when={(results()?.length ?? 0) > 0}
              fallback={<p class="empty">no results published by {props.selectedStep}</p>}
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
                        from <b>{props.selectedStep}</b>
                      </div>
                    </div>
                  )}
                </For>
              </div>
            </Show>
          </div>
        </Show>

        {/* Outputs — the interpolation view over this step's results. */}
        <Show when={tab() === "outputs"}>
          <div class="tabpane">
            <Show
              when={(results()?.length ?? 0) > 0}
              fallback={<p class="empty">nothing for a downstream step to read from {props.selectedStep}</p>}
            >
              <div class="exprs">
                <For each={results()}>
                  {(r) => (
                    <div class="expr mono">
                      <span class="tok">{`\${{ outputs.${props.selectedStep}.${r.name} }}`}</span>
                      <span class="arrow">→</span>
                      <span class="val">{showValue(r.value)}</span>
                    </div>
                  )}
                </For>
              </div>
            </Show>
          </div>
        </Show>

        {/* Artifacts — run-level files of record. */}
        <Show when={tab() === "artifacts"}>
          <div class="tabpane">
            <Show
              when={props.artifacts.length > 0}
              fallback={<p class="empty">no artifacts published by this run</p>}
            >
              <ul class="filelist">
                <For each={props.artifacts}>
                  {(a) => (
                    <li class="filerow">
                      <span class="fname">
                        <Icon icon="package" size={14} />
                        <a href={artifactUrl(props.runId, a.name)} download={a.name}>
                          {a.name}
                        </a>
                      </span>
                      <span class="fsize mono">{fmtSize(a.size)}</span>
                      <span class="fmeta mono">{a.content_type}</span>
                    </li>
                  )}
                </For>
              </ul>
            </Show>
          </div>
        </Show>

        {/* Workspace — the step's output snapshot, browsed read-only. */}
        <Show when={tab() === "workspace"}>
          <div class="tabpane">
            <div class="ws-crumbs mono">
              <button class="crumb" onClick={() => navTo("")}>
                {props.selectedStep}
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

            {/* File preview */}
            <Show when={openFile()}>
              <div class="fileview">
                <div class="fileview-h">
                  <span class="mono">{openFile()!.split("/").pop()}</span>
                  <a
                    class="fv-dl mono"
                    href={workspaceFileUrl(props.runId, props.selectedStep!, openFile()!)}
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
                  fallback={<div class="fileview-body"><span class="lgln" />loading…</div>}
                >
                  <Show
                    when={!fileBody()?.binary}
                    fallback={<div class="fileview-body binary">binary file — use download</div>}
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
              </div>
            </Show>

            {/* Directory listing (hidden while a file is open) */}
            <Show when={!openFile()}>
              <Show
                when={ws()?.available}
                fallback={
                  <p class="empty">
                    {ws.loading
                      ? "loading…"
                      : "no workspace snapshot for this step — a still-running step, a gate, or a backend that doesn't snapshot"}
                  </p>
                }
              >
                <ul class="filelist">
                  <For each={ws()?.entries} fallback={<li class="filerow empty">empty directory</li>}>
                    {(e) => (
                      <li class="filerow">
                        <Show
                          when={e.kind === "dir"}
                          fallback={
                            <button class="fname asdir" onClick={() => setOpenFile(filePath(e.name))}>
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
      </Show>
    </div>
  );
}
