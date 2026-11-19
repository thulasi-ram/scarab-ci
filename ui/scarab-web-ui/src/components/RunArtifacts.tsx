// The run's Artifacts tab: the files the run published, grouped by the step
// that published them. "Of record" (the default) shows one copy per file, the
// one a bare download serves; "Include earlier copies" adds the copies earlier
// tries wrote. Each row names its file (a download link), the step and try
// that wrote it, its size and type. Opened from a step header's "N artifacts"
// link, the tab shows that step's files until "Show all".
import { createSignal, For, Show } from "solid-js";
import { artifactUrl, type Artifact } from "../api/client";
import {
  artifactFilterCounts,
  artifactGroups,
  artifactsInScope,
  copyMark,
  shownArtifacts,
  type ArtifactFilter,
} from "../run-tabs";
import { stepDisplayName } from "../run-page";
import { tryLabel, type AttemptPlace } from "../takes";
import Icon from "./Icon";

/** Human byte size. */
function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export default function RunArtifacts(props: {
  runId: string;
  /** The copies visible in the version in view. */
  artifacts: Artifact[];
  /** The run's steps in order, for the grouping. */
  stepOrder: string[];
  /** Every attempt's version and try number. */
  places: Record<string, Record<string, AttemptPlace>>;
  /** The version in view, so a copy from another version names it. */
  viewedTake: number | null;
  /** Show only this step's files. */
  onlyStep: string | null;
  onShowAll: () => void;
  /** The outcome of the try that wrote a copy, when the run knows it. */
  outcomeOf?: (step: string, attempt: string) => string | undefined;
}) {
  const [filter, setFilter] = createSignal<ArtifactFilter>("record");
  const inScope = () => artifactsInScope(props.artifacts, props.onlyStep);
  const counts = () => artifactFilterCounts(inScope());
  const groups = () => artifactGroups(shownArtifacts(inScope(), filter()), props.stepOrder);

  return (
    <div class="panel run-tab-panel">
      <div class="rt-filters" role="group" aria-label="Artifact copies">
        <button type="button" class="chip" aria-pressed={filter() === "record"} onClick={() => setFilter("record")}>
          Of record <span class="mono">{counts().record}</span>
        </button>
        <button type="button" class="chip" aria-pressed={filter() === "all"} onClick={() => setFilter("all")}>
          Include earlier copies <span class="mono">{counts().all}</span>
        </button>
        <Show when={props.onlyStep}>
          {(s) => (
            <span class="rt-scope">
              Showing artifacts from <span class="mono">{stepDisplayName(s())}</span> ·{" "}
              <button type="button" class="linklike" onClick={() => props.onShowAll()}>
                Show all
              </button>
            </span>
          )}
        </Show>
      </div>
      <For
        each={groups()}
        fallback={
          <p class="empty rt-empty">
            {props.onlyStep
              ? "This step published no artifacts in this version."
              : "No artifacts published by this run."}
          </p>
        }
      >
        {(g) => (
          <div class="art-group">
            <div class="art-grp-h mono" title={g.step}>
              {g.step ? stepDisplayName(g.step) : "no step recorded"}
            </div>
            <ul class="filelist art-list">
              <For each={g.rows}>
                {(a) => (
                  <li class="art-row" classList={{ shadowed: !a.of_record }}>
                    <span class="art-name">
                      <Icon icon="package" size={14} />
                      <a
                        href={
                          a.of_record || !a.step
                            ? artifactUrl(props.runId, a.name)
                            : artifactUrl(props.runId, a.name, { step: a.step, attempt: a.attempt })
                        }
                        download={a.name}
                        title={a.name}
                      >
                        {a.name}
                      </a>
                    </span>
                    <span class="art-from">
                      {a.step
                        ? tryLabel(props.places, a.step, a.attempt, props.viewedTake, stepDisplayName(a.step))
                        : ""}
                    </span>
                    <span class="art-size mono">{fmtSize(a.size)}</span>
                    <span class="art-type mono">{a.content_type}</span>
                    <span class="art-mark">
                      <Show
                        when={a.of_record}
                        fallback={(() => {
                          const m = copyMark(a, a.step ? props.outcomeOf?.(a.step, a.attempt) : undefined);
                          return <span class={m.tone === "earlier" ? "art-earlier" : "art-failed"}>{m.text}</span>;
                        })()}
                      >
                        <span class="art-rec" title="the copy a download by name serves">
                          of record
                        </span>
                      </Show>
                    </span>
                  </li>
                )}
              </For>
            </ul>
          </div>
        )}
      </For>
    </div>
  );
}
