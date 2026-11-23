// The pieces of a Runs list row: the status mark at the left edge, line 1
// (pipeline / headline), line 2 (the trigger, where the Run came from, the
// commit and the author), and the status phrase over the time at the right,
// with a deploy Run's chip before the time. The words come from ../run-row;
// every text style is a class in styles.css.
import { Index, Match, Show, Switch } from "solid-js";
import { cutName, runHeadline, statusGlyph, statusText, type KindCell, type PhrasePart, type StatusPhrase } from "../run-row";
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
  /** The trigger in words, and where the Run came from (`kindFirst`). */
  line: { kind: KindCell; where: PhrasePart[] };
  sha?: string | null;
  author: { text: string; muted: boolean };
  commitUrl?: string | null;
  prUrl?: string | null;
};

const stop = (e: MouseEvent) => e.stopPropagation();

/** Line 2: the trigger in a fixed first column (`PR #213`, `push`, `tag
 *  v0.7.2`), where the Run came from (which gives way first), then the hash
 *  and the author at the right edge, at the same x on every row. On a phone
 *  it flows as one wrapped line in the same order. */
export function OriginLine(props: OriginProps) {
  return (
    <div class="rr-l2">
      <span class="rr-kind" classList={{ wide: props.line.kind.muted }}>
        <Icon icon={triggerIcon(props.kind)} size={13} />
        <span class="rr-kword" classList={{ muted: props.line.kind.muted }}>
          {props.line.kind.word}
        </span>
        <Show when={props.line.kind.token}>
          {(t) => (
            <span class="rr-ktok">
              {" "}
              <WhereParts parts={[t()]} prUrl={props.prUrl} />
            </span>
          )}
        </Show>
      </span>
      <Show when={!props.line.kind.muted}>
        <span class="rr-where">
          <WhereParts parts={props.line.where} prUrl={props.prUrl} />
        </span>
      </Show>
      <span class="rr-sha">
        <Show when={props.sha} fallback={<span class="muted">—</span>}>
          {(sha) => (
            <>
              <Icon icon="git-commit-horizontal" size={12} />
              <Show when={props.commitUrl} fallback={<span class="sha">{sha().slice(0, 7)}</span>}>
                {(href) => (
                  <a class="sha" href={href()} target="_blank" rel="noopener noreferrer" onClick={stop} title="commit on the forge">
                    {sha().slice(0, 7)}
                  </a>
                )}
              </Show>
            </>
          )}
        </Show>
      </span>
      <span class="rr-who" classList={{ muted: props.author.muted }} title={props.author.muted ? undefined : props.author.text}>
        <Show when={!props.author.muted}>
          <Icon icon="user" size={12} />
        </Show>
        <span class="rr-whoname">{props.author.text}</span>
      </span>
    </div>
  );
}

/** The parts of a phrase. `<Index>`, so a poll that hands over equal parts
 *  keeps the nodes (a focused PR link stays focused). */
function WhereParts(props: { parts: PhrasePart[]; prUrl?: string | null }) {
  return (
    <Index each={props.parts}>
      {(p, i) => (
        <Switch>
          <Match when={p().t === "text" && (p() as Extract<PhrasePart, { t: "text" }>)}>{(x) => x().text}</Match>
          <Match when={p().t === "mono" && (p() as Extract<PhrasePart, { t: "mono" }>)}>
            {(x) => <span class="mono">{x().text}</span>}
          </Match>
          <Match when={p().t === "muted" && (p() as Extract<PhrasePart, { t: "muted" }>)}>
            {(x) => <span class="muted">{x().text}</span>}
          </Match>
          <Match when={p().t === "arrow"}>
            {/* No break after the arrow: it stays with the branch it points to. */}
            <span class="sep">{i === 0 ? "→\u00a0" : " →\u00a0"}</span>
          </Match>
          <Match when={p().t === "pr" && (p() as Extract<PhrasePart, { t: "pr" }>)}>
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
    </Index>
  );
}

/** A deploy Run's chip, before its time: the Environment it deploys into,
 *  or `Deploy` when it names none. The only chip on a row. */
export function DeployChip(props: { environment?: string | null }) {
  return (
    <span class="rr-dchip" title={props.environment ? `deploy run into ${props.environment}` : "deploy run"}>
      <Icon icon="rocket" size={11} />
      <Show when={props.environment} fallback={<span class="rr-dlabel">Deploy</span>}>
        {(env) => <span class="rr-denv">{cutName(env(), 18)}</span>}
      </Show>
    </span>
  );
}

/** The right column: the status phrase over `HH:MM · duration`, with a
 *  deploy Run's chip before the time. A passed Run shows no phrase (its
 *  check mark says it). */
export function RowStatus(props: {
  phrase: StatusPhrase;
  quiet?: boolean;
  clock: string;
  /** Relative and absolute time, for the hover title. */
  when: string;
  duration?: string | null;
  live?: boolean;
  /** Set on a deploy Run: its Environment, or null for none. */
  deploy?: { environment: string | null } | null;
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
      <span class="rr-tline">
        <Show when={props.deploy}>{(d) => <DeployChip environment={d().environment} />}</Show>
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
      </span>
    </div>
  );
}

/** A placeholder the shape of a row while the list loads. `n` varies the
 *  bar widths so the rows do not read as one repeated block. */
export function SkeletonRow(props: { n: number }) {
  const w = (base: number, step: number, span: number) => `${base + ((props.n * step) % span)}%`;
  return (
    <div class="runrow rr-skel" aria-hidden="true">
      <i class="rr-skel-mark" />
      <i class="rr-skel-num" />
      <div class="rr-main">
        <i style={{ width: w(55, 13, 35) }} />
        <i style={{ width: w(30, 17, 30) }} />
      </div>
      <div class="rr-right">
        <i class="rr-skel-word" />
        <i class="rr-skel-time" />
      </div>
    </div>
  );
}
