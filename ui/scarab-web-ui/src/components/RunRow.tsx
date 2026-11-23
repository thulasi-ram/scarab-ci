// The pieces of a Runs list row: the status mark at the left edge, line 1
// (pipeline / headline), line 2 (the trigger, where the Run came from, the
// commit, the author and a deploy Run's Environment), line 3 (what the status
// word leaves out, when there is something), and the status word over the time
// at the right. The words come from ../run-row; every text style is a class in
// styles.css.
import { Index, Match, Show, Switch } from "solid-js";
import { cutName, runHeadline, statusGlyph, type DeployTag, type KindCell, type PhrasePart, type StatusDetail } from "../run-row";
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
  line: { kind: KindCell; where: PhrasePart[]; tight: boolean };
  sha?: string | null;
  author: { text: string; muted: boolean };
  commitUrl?: string | null;
  prUrl?: string | null;
  /** A deploy Run's Environment (`deployTag`), last on the line. */
  deploy?: DeployTag | null;
};

const stop = (e: MouseEvent) => e.stopPropagation();

/** Line 2: the trigger in a fixed first column (`PR #213`, `push`, `tag
 *  v0.7.2`), where the Run came from (which gives way first), the hash and
 *  the author at the same x on every row, then a deploy Run's Environment.
 *  A `tight` phrase (`→ main`) follows the trigger in its cell, with no column
 *  gap. On a phone it flows as one wrapped line in the same order. */
export function OriginLine(props: OriginProps) {
  return (
    <div class="rr-l2">
      <span class="rr-kind" classList={{ wide: props.line.kind.muted || props.line.tight }}>
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
        <Show when={props.line.tight}>
          <span class="rr-where">
            <WhereParts parts={props.line.where} prUrl={props.prUrl} />
          </span>
        </Show>
      </span>
      <Show when={!props.line.kind.muted && !props.line.tight}>
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
      <Show when={props.deploy}>
        {(d) => (
          <span class="rr-env" title={d().title}>
            <Icon icon="rocket" size={12} />
            <span class="rr-envname">{d().text}</span>
          </span>
        )}
      </Show>
    </div>
  );
}

/** Line 3: what the status word leaves out (`failed at test · macos +1`,
 *  `waiting for approval`, `replaced by #130, a newer push`), behind its
 *  icon. Drawn only when there is one. */
export function StatusLine(props: { detail: StatusDetail }) {
  return (
    <div class={`rr-l3 ${props.detail.tone}`} title={props.detail.text}>
      <Icon icon={props.detail.icon} size={12} />
      <span class="rr-l3txt">{props.detail.text}</span>
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

/** The right column: one status word over `HH:MM · duration`. A passed Run
 *  shows no word (its check mark says it). The word's title carries line 3's
 *  detail when there is one. */
export function RowStatus(props: {
  word: string;
  /** The status whose ink the word takes. */
  tone: string;
  /** The word's hover title: the full detail, else the word. */
  title?: string | null;
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
        <span class={`rr-word ${props.tone}`} title={props.title ?? props.word}>
          {props.word}
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
