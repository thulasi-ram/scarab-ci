// Animated brand beetle — plays a baked ASCII scene from ui/brand/ascii (see
// its README). State moments ONLY (all-clear, empty, loading) — never ambient
// behind live data. No rendering code: the loop swaps pre-baked text frames at
// the scene's fps; three <pre> layers are colored via the --ascii-* tokens.
//
// Bubble stages (ponder-*): the art is baked but the speech-bubble TEXT is not.
// A scene may carry `bubble: {from,to,col,row,place}`; pass a `line` prop and a
// fourth <pre> composites a box around it, shown only on frames [from,to).
// That <pre> is anchored at the bubble's origin and keeps a 1em line box — the
// art's square cells are right for a dot matrix but collide real text.
import { onCleanup, onMount } from "solid-js";

// `place` is typed as string (not a literal union) so a JSON-imported scene,
// whose place widens to string, satisfies the prop; the compositor branches on
// `=== "right"` and treats everything else as "above".
type Bubble = { from: number; to: number; col: number; row: number; place: string };

// frames: per frame, three text layers (emerald, gold, gray) — typed loosely
// because resolveJsonModule infers string[][] from the baked files.
type Baked = {
  cols: number;
  rows: number;
  fps: number;
  frames: string[][];
  bubble?: Bubble;
};

// Compose a speech bubble sized to `line`, tail pointing at the scene's baked
// anchor. Returns the bubble BLOCK plus its grid origin — not a rows-tall
// layer. The art is a dot matrix and renders on square cells (line-height =
// the 0.6em advance), but the bubble is real TEXT: 1em glyphs in a 0.6em line
// box collide, which mangled every bubble. Emitting just the block lets the
// player anchor it and give it its own 1em line box.
function composeBubble(line: string, b: Bubble, cols: number, rows: number) {
  const lines = line.split("\n");
  const maxw = Math.max(...lines.map((l) => l.length));
  const bw = maxw + 4;
  const bh = lines.length + 2;
  let top: number, left: number;
  if (b.place === "right") {
    left = Math.min(cols - bw, Math.max(0, b.col + 2));
    top = Math.max(0, Math.min(rows - bh, b.row - (bh >> 1)));
  } else {
    left = Math.max(0, Math.min(cols - bw, b.col - 2));
    top = Math.max(0, Math.min(rows - bh, b.row - bh - 1));
  }
  // The block spans the box PLUS the tail, which sits outside it: one row
  // below ("above") or one column to the left ("right").
  const c0 = b.place === "right" ? Math.max(0, left - 1) : left;
  const r0 = top;
  const w = left + bw - c0;
  const h = b.place === "right" ? bh : bh + 1;
  const grid: string[][] = Array.from({ length: h }, () => Array(w).fill(" "));
  const put = (r: number, c: number, ch: string) => {
    const rr = r - r0, cc = c - c0;
    if (rr >= 0 && rr < h && cc >= 0 && cc < w) grid[rr][cc] = ch;
  };
  put(top, left, "╭"); put(top, left + bw - 1, "╮");
  put(top + bh - 1, left, "╰"); put(top + bh - 1, left + bw - 1, "╯");
  for (let c = left + 1; c < left + bw - 1; c++) { put(top, c, "─"); put(top + bh - 1, c, "─"); }
  for (let r = top + 1; r < top + bh - 1; r++) { put(r, left, "│"); put(r, left + bw - 1, "│"); }
  lines.forEach((ln, i) => { for (let k = 0; k < ln.length; k++) put(top + 1 + i, left + 2 + k, ln[k]); });
  if (b.place === "right") put(Math.max(top + 1, Math.min(top + bh - 2, b.row)), left - 1, "╴");
  else put(top + bh, Math.max(left + 1, Math.min(left + bw - 2, b.col)), "╲");
  return { text: grid.map((r) => r.join("").replace(/\s+$/, "")).join("\n"), col: c0, row: r0 };
}

// Widest occupied column across every frame. The baked grid is padded to a
// uniform `cols` so a family of scenes can share one canvas — ponder-kingofhill
// puts a `place: "right"` bubble in columns 76..95, which the other ponder
// scenes leave empty. Sizing the box to `cols` therefore centres the BOX while
// the art sits left of centre. Measured once per baked scene and cached: the
// frames are frozen, so the answer never changes.
const contentColsCache = new WeakMap<Baked, number>();
function contentCols(scene: Baked): number {
  let w = contentColsCache.get(scene);
  if (w === undefined) {
    w = 0;
    for (const frame of scene.frames)
      for (const layer of frame)
        for (const ln of layer.split("\n")) w = Math.max(w, ln.trimEnd().length);
    contentColsCache.set(scene, w);
  }
  return w;
}

export default function AsciiScene(props: {
  scene: Baked;
  /** px per cell column; glyph advance is ~0.602 × this */
  fontSize?: number;
  /** accessible name; omit → decorative (aria-hidden) */
  label?: string;
  class?: string;
  /** reactive gate: false freezes the loop on its current frame */
  playing?: boolean;
  /** speech-bubble line for a bubble stage (ponder-*); ignored otherwise */
  line?: string;
}) {
  const pres: HTMLPreElement[] = [];
  const { frames, fps, bubble, cols, rows } = props.scene;
  const cell = (props.fontSize ?? 8) * 0.6;
  // Composed once: neither the text nor its anchor changes per frame — only
  // whether the bubble is showing.
  const bub = bubble && props.line ? composeBubble(props.line, bubble, cols, rows) : null;
  const bubbleAt = (f: number) =>
    bub && f >= bubble!.from && f < bubble!.to ? bub.text : "";
  // A right-placed bubble is real content and can run to the last column, so
  // it widens the box; `cols` stays the ceiling.
  const bubRight = bub
    ? bub.col + Math.max(...bub.text.split("\n").map((l) => l.length))
    : 0;
  const boxCols = Math.min(cols, Math.max(contentCols(props.scene), bubRight));

  onMount(() => {
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      // Hold a fully-open frame instead of animating (frame 0 is closed).
      const m = Math.floor(frames.length / 2);
      pres.forEach((p, i) => (p.textContent = i < 3 ? frames[m][i] : bubbleAt(m)));
      return;
    }
    let f = 0;
    const t = setInterval(() => {
      if (document.hidden || props.playing === false) return;
      f = (f + 1) % frames.length;
      for (let i = 0; i < 3; i++) pres[i].textContent = frames[f][i];
      if (pres[3]) pres[3].textContent = bubbleAt(f);
    }, 1000 / fps);
    onCleanup(() => clearInterval(t));
  });

  // Explicit box: layers are absolutely positioned, and the trimmed text lines
  // must not size the scene (a layer can be much narrower than the grid). The
  // width is the OCCUPIED grid, not the declared one — see contentCols.
  // JetBrains Mono's advance is exactly 0.6em and the CSS sets line-height to
  // 0.6em too, so cells are SQUARE — width AND height use the same 0.6 factor.
  return (
    <div
      class={`ascii-scene ${props.class ?? ""}`}
      style={{
        "--ascii-fs": `${props.fontSize ?? 8}px`,
        width: `${boxCols * cell}px`,
        height: `${props.scene.rows * cell}px`,
      }}
      role={props.label ? "img" : undefined}
      aria-label={props.label}
      aria-hidden={props.label ? undefined : "true"}
    >
      <pre class="ascii-em" ref={(el) => (pres[0] = el)}>{frames[0][0]}</pre>
      <pre class="ascii-au" ref={(el) => (pres[1] = el)}>{frames[0][1]}</pre>
      <pre class="ascii-fe" ref={(el) => (pres[2] = el)}>{frames[0][2]}</pre>
      {bub && (
        <pre
          class="ascii-bubble"
          ref={(el) => (pres[3] = el)}
          style={{ left: `${bub.col * cell}px`, top: `${bub.row * cell}px` }}
        />
      )}
    </div>
  );
}
