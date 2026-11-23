// The pieces of a Runs list row: the status mark at the left edge, line 1
// (pipeline / headline, then a deploy Run's Environment) and line 2 (where the
// Run came from, as one phrase). The words come from ../run-row.
import { For, Match, Show, Switch } from "solid-js";
import { runHeadline, statusGlyph, type PhrasePart } from "../run-row";
import { statusWord } from "../run-page";
import { triggerIcon } from "../trigger";
import Icon from "./Icon";

/** A round mark per status: a glyph for a settled Run, a spinning ring for a
 *  running one, a dashed ring for a pending one. */
export function StatusMark(props: { status: string }) {
  return (
    <span class={`rr-mark ${props.status}`} title={statusWord(props.status)}>
      <Show when={statusGlyph(props.status)}>{(g) => <Icon icon={g()} size={11} />}</Show>
    </span>
  );
}

/** Line 1: the pipeline as plain text, a slash, the headline, and for a deploy
 *  Run the Environment it deploys into. */
export function RowTitle(props: {
  pipeline?: string | null;
  kind?: string | null;
  title?: string | null;
  deploy?: boolean;
  environment?: string | null;
}) {
  const head = () => runHeadline(props.kind, props.title);
  return (
    <div class="rr-l1">
      <Show
        when={props.pipeline}
        fallback={
          <span class="rr-pipe none" title="started with an inline pipeline">
            inline
          </span>
        }
      >
        {(p) => (
          <span class="rr-pipe" title={`pipeline ${p()}`}>
            {p()}
          </span>
        )}
      </Show>
      <span class="rr-slash">/</span>
      {/* The headline and the Environment take the rest of the line, and the
          headline truncates; the Environment follows its last word. */}
      <span class="rr-head">
        <span class="rr-title" classList={{ none: head().muted }} title={head().muted ? undefined : head().text}>
          {head().text}
        </span>
        <Show
          when={props.environment}
          fallback={
            <Show when={props.deploy}>
              <span class="rr-env" title="deploy run: its pipeline or a step is marked `deploy: true`">
                <Icon icon="rocket" size={12} />
                deploy
              </span>
            </Show>
          }
        >
          {(env) => (
            <span class="rr-env" title={`deploy run into ${env()}`}>
              → {env()}
            </span>
          )}
        </Show>
      </span>
    </div>
  );
}

/** Line 2: the trigger kind's icon, then the origin phrase. The commit hash
 *  and the pull request number link to the forge when it is known. */
export function OriginLine(props: {
  kind?: string | null;
  parts: PhrasePart[];
  commitUrl?: string | null;
  prUrl?: string | null;
}) {
  const stop = (e: MouseEvent) => e.stopPropagation();
  return (
    <div class="rr-l2">
      <Icon icon={triggerIcon(props.kind)} size={13} />
      <span class="rr-phrase">
        <For each={props.parts}>
          {(p) => (
            <Switch>
              <Match when={p.t === "text" && p}>{(x) => x().text}</Match>
              <Match when={p.t === "mono" && p}>{(x) => <span class="mono">{x().text}</span>}</Match>
              <Match when={p.t === "muted" && p}>{(x) => <span class="muted">{x().text}</span>}</Match>
              <Match when={p.t === "arrow"}>
                <span class="sep"> → </span>
              </Match>
              <Match when={p.t === "sep"}>
                <span class="sep"> · </span>
              </Match>
              <Match when={p.t === "sha" && p}>
                {(x) => (
                  <Show when={props.commitUrl} fallback={<span class="mono sha">{x().text}</span>}>
                    {(href) => (
                      <a
                        class="mono sha"
                        href={href()}
                        target="_blank"
                        rel="noopener noreferrer"
                        onClick={stop}
                        title="commit on the forge"
                      >
                        {x().text}
                      </a>
                    )}
                  </Show>
                )}
              </Match>
              <Match when={p.t === "pr" && p}>
                {(x) => (
                  <Show when={props.prUrl} fallback={<span class="mono">#{x().n}</span>}>
                    {(href) => (
                      <a
                        class="mono"
                        href={href()}
                        target="_blank"
                        rel="noopener noreferrer"
                        onClick={stop}
                        title="pull request on the forge"
                      >
                        #{x().n}
                      </a>
                    )}
                  </Show>
                )}
              </Match>
            </Switch>
          )}
        </For>
      </span>
    </div>
  );
}
