// The pieces of a Runs list row: the status mark at the left edge, line 1
// (pipeline / headline), line 2 (where the Run came from), and the status
// phrase over the time at the right. The words come from ../run-row.
import { For, Match, Show, Switch } from "solid-js";
import { cutName, runHeadline, statusGlyph, statusText, type PhrasePart, type StatusPhrase } from "../run-row";
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

/** Line 1: the pipeline name, a slash, and the headline. The name is cut to
 *  24 characters in code and never shrinks; the headline wraps to two lines
 *  and is what gives way. On a phone the full name moves above the headline
 *  (`.rr-pfull`). */
export function RowTitle(props: { pipeline?: string | null; kind?: string | null; title?: string | null }) {
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
          <>
            <span class="rr-pipe rr-pcut" title={`pipeline ${p()}`}>
              {cutName(p())}
            </span>
            <span class="rr-pipe rr-pfull" title={`pipeline ${p()}`}>
              {p()}
            </span>
          </>
        )}
      </Show>
      <span class="rr-slash">/</span>
      <span class="rr-title" classList={{ none: head().muted }} title={head().muted ? undefined : head().text}>
        {head().text}
      </span>
    </div>
  );
}

/** Line 2's parts. */
export type OriginProps = {
  kind?: string | null;
  /** Where the Run came from, without the commit and the author. */
  where: PhrasePart[];
  /** A deploy Run's Environment. */
  environment?: string | null;
  /** A deploy Run with no Environment. */
  deploy?: boolean;
  sha?: string | null;
  author: { text: string; muted: boolean };
  commitUrl?: string | null;
  prUrl?: string | null;
};

const stop = (e: MouseEvent) => e.stopPropagation();

/** Line 2: where the Run came from, a deploy's Environment, the commit and
 *  the author. Its layout lives here alone, so it can change in one place;
 *  this version puts the hash and the author in two columns at the right. */
export function OriginLine(props: OriginProps) {
  return (
    <div class="rr-l2">
      <span class="rr-from">
        <Icon icon={triggerIcon(props.kind)} size={13} />
        <span class="rr-where">
          <WhereParts parts={props.where} prUrl={props.prUrl} />
        </span>
        <Show
          when={props.environment}
          fallback={
            <Show when={props.deploy}>
              <span class="rr-into" title="deploy run: its pipeline or a step is marked `deploy: true`">
                <span class="sep">·</span> deploy run
              </span>
            </Show>
          }
        >
          {(env) => (
            <span class="rr-into">
              <span class="sep">·</span> into{" "}
              <span class="rr-env" title={`deploy run into ${env()}`}>
                {cutName(env())}
              </span>
            </span>
          )}
        </Show>
      </span>
      <span class="rr-sha">
        <Show when={props.sha}>
          {(sha) => (
            <Show when={props.commitUrl} fallback={<span class="sha">{sha().slice(0, 7)}</span>}>
              {(href) => (
                <a class="sha" href={href()} target="_blank" rel="noopener noreferrer" onClick={stop} title="commit on the forge">
                  {sha().slice(0, 7)}
                </a>
              )}
            </Show>
          )}
        </Show>
      </span>
      <span class="rr-who" classList={{ muted: props.author.muted }} title={props.author.muted ? undefined : props.author.text}>
        {props.author.text}
      </span>
    </div>
  );
}

function WhereParts(props: { parts: PhrasePart[]; prUrl?: string | null }) {
  return (
    <For each={props.parts}>
      {(p) => (
        <Switch>
          <Match when={p.t === "text" && p}>{(x) => x().text}</Match>
          <Match when={p.t === "mono" && p}>{(x) => <span class="mono">{x().text}</span>}</Match>
          <Match when={p.t === "muted" && p}>{(x) => <span class="muted">{x().text}</span>}</Match>
          <Match when={p.t === "arrow"}>
            <span class="sep"> → </span>
          </Match>
          <Match when={p.t === "pr" && p}>
            {(x) => (
              <Show when={props.prUrl} fallback={<span class="mono">#{x().n}</span>}>
                {(href) => (
                  <a class="mono" href={href()} target="_blank" rel="noopener noreferrer" onClick={stop} title="pull request on the forge">
                    #{x().n}
                  </a>
                )}
              </Show>
            )}
          </Match>
        </Switch>
      )}
    </For>
  );
}

/** The right column: the status phrase over `HH:MM · duration`. A passed Run
 *  shows no phrase (its check mark says it). */
export function RowStatus(props: {
  phrase: StatusPhrase;
  quiet?: boolean;
  clock: string;
  /** Relative and absolute time, for the hover title. */
  when: string;
  duration?: string | null;
  live?: boolean;
}) {
  return (
    <div class="rr-right">
      <Show when={!props.quiet}>
        <span class={`rr-word ${props.phrase.tone}`} title={statusText(props.phrase)}>
          {props.phrase.lead}
          <Show when={props.phrase.step}>
            {(s) => (
              <>
                {" "}
                <span class="rr-step">{s()}</span>
              </>
            )}
          </Show>
        </span>
      </Show>
      <span class="rr-time" classList={{ live: props.live }} title={props.when}>
        <span class="mono">{props.clock}</span>
        <Show when={props.duration}>
          {(d) => (
            <>
              {" · "}
              <span class="mono">{d()}</span>
            </>
          )}
        </Show>
      </span>
    </div>
  );
}
