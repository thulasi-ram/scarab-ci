// The pieces of a Runs list row: the status mark at the left edge, line 1
// (pipeline / headline), line 2 (the trigger, where the Run came from, then
// the commit, the author and a deploy Run's Environment as one group), and
// the status word over the time at the right, with a hover card for what the
// word leaves out. The words come from ../run-row; every text style is a class
// in styles.css.
import { createEffect, createSignal, createUniqueId, For, Index, Match, onCleanup, Show, Switch } from "solid-js";
import { Portal } from "solid-js/web";
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

/** Line 2: the trigger in a fixed first column (`pr #213`, `push`, `tag
 *  v0.7.2`), where the Run came from (which gives way first), then the
 *  commit, the author and a deploy Run's Environment as one group that starts
 *  at the same x on every row. A `tight` phrase (`→ main`) follows the
 *  trigger in its cell, with no column gap. On a phone it flows as one
 *  wrapped line, the group wrapping as a unit. */
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
      {/* A break opportunity on a phone; a grid drops it. */}{" "}
      <RunMeta sha={props.sha} author={props.author} commitUrl={props.commitUrl} deploy={props.deploy} />
    </div>
  );
}

/** The commit, the author and a deploy Run's Environment, each behind its
 *  icon, as one group with even spacing. */
export function RunMeta(props: {
  sha?: string | null;
  author: { text: string; muted: boolean };
  commitUrl?: string | null;
  deploy?: DeployTag | null;
}) {
  return (
    <span class="rr-meta">
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
    </span>
  );
}

/** The status word. With a detail it is underlined dotted and opens a small
 *  card under it: on hover (after 200ms), on keyboard focus, and on a tap
 *  where there is no hover (the tap does not open the Run). The card closes
 *  on leave, blur, Escape, a scroll, and a tap elsewhere. */
export function StatusWord(props: { word: string; tone: string; detail: StatusDetail | null }) {
  return (
    <Show when={props.detail} fallback={<span class={`rr-word ${props.tone}`}>{props.word}</span>}>
      {(d) => <WordWithCard word={props.word} tone={props.tone} detail={d()} />}
    </Show>
  );
}

const OPEN_DELAY_MS = 200;
const EDGE = 8;

function WordWithCard(props: { word: string; tone: string; detail: StatusDetail }) {
  const id = createUniqueId();
  const [open, setOpen] = createSignal(false);
  const [pos, setPos] = createSignal({ left: 0, top: 0 });
  let word: HTMLSpanElement | undefined;
  let card: HTMLDivElement | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  // The last pointer was a finger or a pen, so a click toggles the card.
  let touch = false;

  // Below the word and starting at its left edge; moved left, then above,
  // when that would leave the viewport.
  const place = () => {
    if (!word || !card) return;
    const w = word.getBoundingClientRect();
    const c = card.getBoundingClientRect();
    const vw = document.documentElement.clientWidth;
    const vh = window.innerHeight;
    let left = w.left;
    if (left + c.width > vw - EDGE) left = Math.max(EDGE, w.right - c.width);
    let top = w.bottom + 6;
    if (top + c.height > vh - EDGE) top = Math.max(EDGE, w.top - 6 - c.height);
    setPos({ left, top });
  };
  const show = () => {
    clearTimeout(timer);
    setOpen(true);
    place();
  };
  const hide = () => {
    clearTimeout(timer);
    setOpen(false);
  };
  onCleanup(() => clearTimeout(timer));

  createEffect(() => {
    if (!open()) return;
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && hide();
    const onDown = (e: PointerEvent) => {
      if (!word?.contains(e.target as Node)) hide();
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("pointerdown", onDown, true);
    window.addEventListener("scroll", hide, true);
    window.addEventListener("resize", hide);
    onCleanup(() => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("scroll", hide, true);
      window.removeEventListener("resize", hide);
    });
  });

  return (
    <>
      <span
        ref={word}
        class={`rr-word ${props.tone} has-card`}
        tabindex="0"
        aria-describedby={id}
        onPointerDown={(e) => (touch = e.pointerType !== "mouse")}
        onPointerEnter={(e) => {
          if (e.pointerType !== "mouse") return;
          clearTimeout(timer);
          timer = setTimeout(show, OPEN_DELAY_MS);
        }}
        onPointerLeave={(e) => e.pointerType === "mouse" && hide()}
        // Keyboard focus only: a tap focuses too, and its click toggles.
        onFocus={(e) => e.currentTarget.matches(":focus-visible") && show()}
        onBlur={hide}
        onClick={(e) => {
          if (!touch) return;
          e.stopPropagation();
          if (open()) hide();
          else show();
        }}
      >
        {props.word}
      </span>
      {/* Always in the page (hidden when closed), so aria-describedby
          resolves; in a portal, so the list's overflow does not clip it. */}
      <Portal>
        <div
          ref={card}
          id={id}
          role="tooltip"
          class="rr-card"
          hidden={!open()}
          style={{ left: `${pos().left}px`, top: `${pos().top}px` }}
        >
          <For each={props.detail.parts}>
            {(p) => (p.t === "mono" ? <span class="rr-card-mono">{p.text}</span> : p.text)}
          </For>
        </div>
      </Portal>
    </>
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
 *  shows no word (its check mark says it). A word with a detail opens it in
 *  a hover card. */
export function RowStatus(props: {
  word: string;
  /** The status whose ink the word takes. */
  tone: string;
  /** What the word leaves out, for its hover card. */
  detail?: StatusDetail | null;
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
        <StatusWord word={props.word} tone={props.tone} detail={props.detail ?? null} />
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
