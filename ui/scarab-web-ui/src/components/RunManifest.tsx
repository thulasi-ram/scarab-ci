// The Run page's Manifest tab: the Pipeline file the Run was created from, as
// recorded at creation. A bar names the file, then the commit it was read at,
// the branch or tag, its lines and size as one dotted group, with Go to
// failure, Wrap, Copy, Download and a link to the file on the forge. Each top-level step's `id:` line carries the result of the Run Steps
// it became in a gutter left of the line numbers (click opens the step on the
// Pipeline tab); a failed step's lines are tinted; a matrix step lists its legs
// under its id line; an `invoke:` line is followed by what it pulled in. The
// file is repo content: every line renders as text nodes, never as markup, and
// the rows added under lines are not part of what Copy and Download give.
import { createEffect, createMemo, createResource, createSignal, For, Show, on, onCleanup } from "solid-js";
import { runManifest, type RunManifest as RunManifestDto } from "../api/client";
import {
  baseName,
  fileLines,
  fileSteps,
  firstFailure,
  invokeBoxes,
  isCut,
  LINE_CHARS,
  markOf,
  markWords,
  matrixLegs,
  SHOWN_LINES,
  shortRef,
  sizeWords,
  stepMark,
  tokenizeFile,
  unmappedSteps,
  type FileStep,
  type MarkKind,
} from "../manifest";
import { shortHash } from "../libraries";
import { stepDisplayName, stepName } from "../run-page";
import { forgeCommitUrl, forgeFileUrl } from "../forge";
import Icon from "./Icon";
import Pill from "./Pill";
import "../run-manifest.css";

// A Run's file never changes once recorded: read it once per Run, and again
// only after a failed read.
const cache = new Map<string, Promise<RunManifestDto>>();

/** The Run's manifest, read once per page load. */
export function loadRunManifest(runId: string): Promise<RunManifestDto> {
  let p = cache.get(runId);
  if (!p) {
    p = runManifest(runId);
    cache.set(runId, p);
    p.catch(() => cache.delete(runId));
  }
  return p;
}

type Loaded = { ok: true; m: RunManifestDto } | { ok: false; error: string };

export type ManifestStep = { id: string; status: string; addedBy?: string | null };

const MARK_ICON: Partial<Record<MarkKind, string>> = { passed: "check", failed: "x", cancelled: "minus" };

function Mark(props: { kind: MarkKind; title: string; onClick?: () => void }) {
  const glyph = () => <Show when={MARK_ICON[props.kind]}>{(i) => <Icon icon={i()} size={9} />}</Show>;
  return (
    <Show
      when={props.onClick}
      fallback={
        <span class={`mf-mark ${props.kind}`} role="img" title={props.title} aria-label={props.title}>
          {glyph()}
        </span>
      }
    >
      {(click) => (
        <button
          type="button"
          class={`mf-mark ${props.kind}`}
          title={props.title}
          aria-label={props.title}
          onClick={() => click()()}
        >
          {glyph()}
        </button>
      )}
    </Show>
  );
}

const KIND_LABEL: Record<string, string> = { library: "lib", file: "file", plugin: "plugin" };

/** A count with thousands separators: `5,000`. */
const count = (n: number) => n.toLocaleString("en-US");

export default function RunManifest(props: {
  runId: string;
  /** The Run's Steps with their status in the version in view. */
  steps: ManifestStep[];
  /** Library versions this Run uses that are yanked now. */
  yanks: { name: string; content_hash: string }[];
  pipeline: string | null;
  /** The trigger's commit and branch or ref, when the event log has them. */
  triggerSha: string | null;
  branch: string | null;
  /** The trigger's ref (`refs/tags/v0.7.2`). Not `ref`: Solid keeps that
   *  name for element refs and drops it from props. */
  gitRef: string | null;
  repoUrl: string | null;
  /** The Run can still change: not finished, and not an older version in
   *  view. On a finished Run a step that did not run reads `not run`. */
  live: boolean;
  onOpenStep: (id: string) => void;
  onShowPipeline: () => void;
}) {
  const [tries, setTries] = createSignal(0);
  const [loaded] = createResource(
    () => [props.runId, tries()] as const,
    ([id]): Promise<Loaded> =>
      loadRunManifest(id).then(
        (m) => ({ ok: true as const, m }),
        (e: unknown) => ({ ok: false as const, error: e instanceof Error ? e.message : String(e) }),
      ),
  );

  const manifest = () => {
    const l = loaded();
    return l?.ok ? l.m : null;
  };
  const error = () => {
    const l = loaded();
    if (!l || l.ok) return null;
    // The server's words as a sentence.
    const e = l.error.trim() || "The server did not answer";
    return `${e.charAt(0).toUpperCase()}${e.slice(1)}${/[.!?]$/.test(e) ? "" : "."}`;
  };

  return (
    <Show
      when={manifest()}
      fallback={
        <div class="panel mf">
          <Show
            when={error()}
            fallback={
              <div class="mf-empty" role="status">
                Loading the pipeline file…
              </div>
            }
          >
            {(e) => (
              <div class="mf-empty" role="alert">
                <b>Could not load the file</b>
                {e()}{" "}
                <button type="button" class="linklike" onClick={() => setTries((n) => n + 1)}>
                  Try again
                </button>
              </div>
            )}
          </Show>
        </div>
      }
    >
      {(m) => (
        <Show when={m().recorded} fallback={<NotRecorded {...props} />}>
          <ManifestFile {...props} m={m()} />
        </Show>
      )}
    </Show>
  );
}

/** No file was kept: an inline Run never had one; an older Run's was not
 * recorded. */
function NotRecorded(props: {
  pipeline: string | null;
  triggerSha: string | null;
  repoUrl: string | null;
  onShowPipeline: () => void;
}) {
  const folder = () => forgeFileUrl(props.repoUrl, props.triggerSha, ".scarab");
  return (
    <div class="panel mf">
      <Show
        when={props.pipeline}
        fallback={
          <div class="mf-empty">
            <b>Started without a file</b>
            This Run was sent through the API as a compiled pipeline, so there is no file to show. Its steps are on
            the{" "}
            <button type="button" class="linklike" onClick={() => props.onShowPipeline()}>
              Pipeline tab
            </button>
            .
          </div>
        }
      >
        {(name) => (
          <div class="mf-empty">
            <b>No file recorded for this Run</b>
            This Run of <span class="mono">{name()}</span>
            <Show when={props.triggerSha}>
              {(sha) => (
                <>
                  {" "}
                  at <span class="mono mf-sha">{sha().slice(0, 8)}</span>
                </>
              )}
            </Show>{" "}
            was created before Scarab kept each Run's pipeline file.
            <Show when={folder()}>
              {(href) => (
                <>
                  {" "}
                  <a href={href()} target="_blank" rel="noopener noreferrer">
                    Open .scarab/ on the forge at that commit
                  </a>
                </>
              )}
            </Show>
          </div>
        )}
      </Show>
    </div>
  );
}

/** One row of the code view: a line of the file, or a row added under one. */
type Row =
  | { kind: "line"; i: number }
  | { kind: "legs"; step: FileStep }
  | { kind: "invoke"; step: FileStep };

function ManifestFile(props: {
  runId: string;
  m: RunManifestDto;
  steps: ManifestStep[];
  yanks: { name: string; content_hash: string }[];
  branch: string | null;
  gitRef: string | null;
  repoUrl: string | null;
  live: boolean;
  onOpenStep: (id: string) => void;
}) {
  const source = () => props.m.source ?? "";
  const lines = createMemo(() => fileLines(source()));
  // The page draws the first SHOWN_LINES lines: a longer file ends in a note
  // and the Download button. Copy and Download give the whole file.
  const shown = () => Math.min(lines().length, SHOWN_LINES);
  const tokens = createMemo(() => tokenizeFile(lines(), SHOWN_LINES));
  const items = createMemo(() => fileSteps(lines()));
  const byIdLine = createMemo(() => new Map(items().map((s) => [s.line, s])));
  const rows = createMemo((): Row[] => {
    const out: Row[] = [];
    const legsAt = new Map(items().map((s) => [s.line, s]));
    const invokeAt = new Map(items().filter((s) => s.invokeLine !== null).map((s) => [s.invokeLine!, s]));
    for (let i = 0; i < shown(); i++) {
      out.push({ kind: "line", i });
      const l = legsAt.get(i);
      if (l) out.push({ kind: "legs", step: l });
      const v = invokeAt.get(i);
      if (v) out.push({ kind: "invoke", step: v });
    }
    return out;
  });

  const mark = (s: FileStep) => stepMark(s.id, props.steps, props.live);
  const failing = (s: FileStep) => ["failed", "mixed"].includes(mark(s).kind);
  // The lines of every step with a failed Step, tinted.
  const bad = createMemo(() => {
    const out = new Set<number>();
    for (const s of items()) {
      if (!failing(s)) continue;
      for (let i = s.start; i <= Math.min(s.end, SHOWN_LINES - 1); i++) out.add(i);
    }
    return out;
  });
  // A failure past the lines drawn has nowhere to go.
  const failure = () => {
    const f = firstFailure(items(), props.steps);
    return f && f.line < SHOWN_LINES ? f : null;
  };
  const unmapped = () => unmappedSteps(items(), props.steps);

  const [wrap, setWrap] = createSignal(false);
  const [focus, setFocus] = createSignal<FileStep | null>(null);
  // Another Run's file: the last one's Go to failure no longer applies.
  createEffect(on(() => props.runId, () => setFocus(null), { defer: true }));
  const [copied, setCopied] = createSignal(false);
  let copiedTimer: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(copiedTimer));
  const lineEls: HTMLElement[] = [];

  const goToFailure = () => {
    const f = failure();
    if (!f) return;
    setFocus(f);
    lineEls[f.line]?.scrollIntoView({ block: "center", behavior: "smooth" });
  };
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(source());
      setCopied(true);
      clearTimeout(copiedTimer);
      copiedTimer = setTimeout(() => setCopied(false), 1500);
    } catch {
      // The clipboard can be refused (no permission, an insecure origin);
      // Download still gives the file.
    }
  };
  const download = () => {
    const url = URL.createObjectURL(new Blob([source()], { type: "text/yaml" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = baseName(props.m.path ?? "pipeline.yaml");
    a.click();
    setTimeout(() => URL.revokeObjectURL(url), 0);
  };

  const sha = () => props.m.sha ?? null;
  const commitUrl = () => forgeCommitUrl(props.repoUrl, sha());
  // The branch or tag the file was read on: a tag by its name, else the
  // trigger's branch, else its ref.
  const where = (): { text: string; tag: boolean } | null => {
    const ref = props.gitRef;
    if (ref?.startsWith("refs/tags/")) return { text: shortRef(ref), tag: true };
    if (props.branch) return { text: props.branch, tag: false };
    return ref ? { text: shortRef(ref), tag: false } : null;
  };
  const forgeUrl = () => forgeFileUrl(props.repoUrl, sha(), props.m.path);
  const bytes = () => new TextEncoder().encode(source()).length;
  const stepExists = (id: string) => props.steps.some((s) => s.id === id);

  return (
    <div class="panel mf">
      <div class="mf-bar">
        {/* The path, then one group: the commit, the branch or tag, the
            line count and the size, a dot between each. */}
        <div class="mf-info">
          <span class="mf-file">
            <Icon icon="file-code" size={13} />
            <span class="mf-path">{props.m.path}</span>
          </span>
          <span class="mf-group">
            <Show when={sha()}>
              {(s) => (
                <span class="mf-part" title="The file as read at this commit when the Run was created">
                  <Icon icon="git-commit-horizontal" size={12} />
                  <Show when={commitUrl()} fallback={<span class="mf-sha">{s().slice(0, 8)}</span>}>
                    {(href) => (
                      <a class="mf-sha" href={href()} target="_blank" rel="noopener noreferrer">
                        {s().slice(0, 8)}
                      </a>
                    )}
                  </Show>
                </span>
              )}
            </Show>
            <Show when={where()}>
              {(w) => (
                <span class="mf-part" title={w().tag ? `tag ${w().text}` : `branch ${w().text}`}>
                  <Icon icon={w().tag ? "tag" : "git-branch"} size={12} />
                  <span class="mf-where">{w().text}</span>
                </span>
              )}
            </Show>
            <span class="mf-part">
              {count(lines().length)} {lines().length === 1 ? "line" : "lines"}
            </span>
            <span class="mf-part">{sizeWords(bytes())}</span>
          </span>
        </div>
        <span class="mf-acts">
          <Show when={failure()}>
            <button type="button" class="btn btn-ghost btn-sm mf-btn mf-fail" onClick={goToFailure}>
              <Icon icon="x" size={12} /> Go to failure
            </button>
          </Show>
          <button
            type="button"
            class="btn btn-ghost btn-sm mf-btn"
            aria-pressed={wrap()}
            onClick={() => setWrap((w) => !w)}
            title="Wrap long lines"
          >
            Wrap
          </button>
          <button
            type="button"
            class="btn btn-ghost btn-sm mf-btn"
            onClick={() => void copy()}
            title={copied() ? "Copied" : "Copy the file"}
            aria-label={copied() ? "Copied" : "Copy the file"}
          >
            <Icon icon={copied() ? "check" : "copy"} size={13} />
          </button>
          <button
            type="button"
            class="btn btn-ghost btn-sm mf-btn"
            onClick={download}
            title={`Download as ${baseName(props.m.path ?? "pipeline.yaml")}`}
            aria-label={`Download as ${baseName(props.m.path ?? "pipeline.yaml")}`}
          >
            <Icon icon="download" size={13} />
          </button>
          <Show when={forgeUrl()}>
            {(href) => (
              <a
                class="btn btn-ghost btn-sm mf-btn"
                href={href()}
                target="_blank"
                rel="noopener noreferrer"
                title={`This file at ${sha()!.slice(0, 8)} on the forge`}
                aria-label={`This file at ${sha()!.slice(0, 8)} on the forge`}
              >
                <Icon icon="external-link" size={13} />
              </a>
            )}
          </Show>
        </span>
      </div>
      <Show when={!sha()}>
        <div class="mf-banner" role="note">
          <Icon icon="alert-triangle" size={13} />
          <span>
            <Show when={props.gitRef} fallback={<>The commit this file was read at was not recorded.</>}>
              The commit behind <span class="mono">{shortRef(props.gitRef!)}</span> was not recorded.
            </Show>{" "}
            The text below is what ran.
          </span>
        </div>
      </Show>
      <div class="mf-code" classList={{ wrap: wrap() }} role="region" aria-label="Pipeline file">
        <div class="mf-lines">
          <For each={rows()}>
            {(row) => {
              if (row.kind === "line") {
                const i = row.i;
                const own = byIdLine().get(i);
                const inFocus = () => {
                  const f = focus();
                  return !!f && i >= f.start && i <= f.end;
                };
                return (
                  <div
                    class="mf-ln"
                    classList={{ bad: bad().has(i), hl: inFocus() }}
                    ref={(el) => (lineEls[i] = el)}
                  >
                    <span class="mf-g">
                      <Show when={own}>
                        {(s) => {
                          const mk = () => mark(s());
                          return (
                            <Mark
                              kind={mk().kind}
                              title={
                                mk().open
                                  ? `${s().id}: ${mk().words} · open it on the Pipeline tab`
                                  : `${s().id}: ${mk().words}`
                              }
                              onClick={mk().open ? () => props.onOpenStep(mk().open!) : undefined}
                            />
                          );
                        }}
                      </Show>
                    </span>
                    <span class="mf-no" aria-hidden="true">
                      {i + 1}
                    </span>
                    <span class="mf-tx">
                      <For each={tokens()[i]} fallback={" "}>
                        {(t) => (t.cls ? <span class={`mf-${t.cls}`}>{t.text}</span> : t.text)}
                      </For>
                      <Show when={isCut(lines()[i] ?? "")}>
                        <span class="mf-cut">
                          {" "}
                          … the first {count(LINE_CHARS)} of {count(lines()[i]!.length)} characters
                        </span>
                      </Show>
                    </span>
                  </div>
                );
              }
              if (row.kind === "legs") {
                const legs = () => matrixLegs(row.step.id, props.steps);
                return (
                  <Show when={legs().length > 0}>
                    <div class="mf-anno" classList={{ bad: failing(row.step) }}>
                      <span />
                      <span />
                      <div class="mf-legs">
                        <For each={legs()}>
                          {(leg) => (
                            <span class="mf-leg">
                              <Mark
                                kind={markOf(leg.status, props.live)}
                                title={`${stepDisplayName(leg.id)}: ${markWords(leg.status, props.live)} · open it on the Pipeline tab`}
                                onClick={() => props.onOpenStep(leg.id)}
                              />
                              <button
                                type="button"
                                class="linklike mono"
                                title={leg.id}
                                onClick={() => props.onOpenStep(leg.id)}
                              >
                                {stepName(leg.id).cell ?? leg.id}
                              </button>
                              <span class="subtle">{markWords(leg.status, props.live)}</span>
                            </span>
                          )}
                        </For>
                      </div>
                    </div>
                  </Show>
                );
              }
              const boxes = () => invokeBoxes(row.step, props.m.invoked, props.yanks);
              return (
                <For each={boxes()}>
                  {(b) => (
                    <div class="mf-anno" classList={{ bad: failing(row.step) }}>
                      <span />
                      <span />
                      <div class="mf-box" classList={{ warn: b.yanked }}>
                        <Pill tone="muted" look="dotted">
                          {KIND_LABEL[b.entry.kind] ?? b.entry.kind}
                        </Pill>
                        <Show when={!b.direct}>
                          <span class="mono">{b.entry.name}</span>
                        </Show>
                        <span class="mono mf-hash" title="Content hash of the version that ran">
                          {shortHash(b.entry.content_hash)}
                        </span>
                        <span>
                          became{" "}
                          <For each={b.steps}>
                            {(id, k) => (
                              <>
                                {k() > 0 ? ", " : ""}
                                <Show when={stepExists(id)} fallback={<span class="mono">{id}</span>}>
                                  <button type="button" class="linklike mono" onClick={() => props.onOpenStep(id)}>
                                    {id}
                                  </button>
                                </Show>
                              </>
                            )}
                          </For>
                        </span>
                        <Show when={b.yanked}>
                          <span class="mf-yank">
                            <Icon icon="alert-triangle" size={12} /> yanked in this org since this Run
                          </span>
                        </Show>
                      </div>
                    </div>
                  )}
                </For>
              );
            }}
          </For>
        </div>
      </div>
      <Show when={lines().length > shown()}>
        <p class="mf-more">
          Showing the first {count(shown())} of {count(lines().length)} lines.
          <button type="button" class="btn btn-ghost btn-sm mf-btn" onClick={download}>
            <Icon icon="download" size={13} /> Download
          </button>
        </p>
      </Show>
      <Show when={unmapped().length > 0}>
        <p class="mf-foot">
          Not written in this file:{" "}
          <For each={unmapped()}>
            {(s, k) => (
              <>
                {k() > 0 ? ", " : ""}
                <button type="button" class="linklike mono" onClick={() => props.onOpenStep(s.id)}>
                  {s.id}
                </button>
                <Show when={s.addedBy}>{(a) => <span class="subtle"> ({a()})</span>}</Show>
              </>
            )}
          </For>
        </p>
      </Show>
    </div>
  );
}
