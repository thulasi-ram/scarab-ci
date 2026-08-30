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

// The occupied BOX across every frame: first/last column and row that any
// layer ever paints. The baked grid is padded to a uniform cols x rows so a
// family of scenes can share one canvas — ponder-kingofhill puts a
// `place: "right"` bubble in columns 76..95 that the other ponder scenes leave
// empty, and every scene reserves head-room its art never enters
// (ponder-ponder starts at row 6, dungroller at row 4 and column 13). Sizing
// the box to the DECLARED grid therefore pads the scene with dead space —
// which centres the box while the art sits off-centre inside it, and in a
// stacked layout pushes whatever follows down by rows of nothing.
//
// Measured once per baked scene and cached: the frames are frozen, so the
// answer never changes.
type Box = { c0: number; c1: number; r0: number; r1: number };
const contentBoxCache = new WeakMap<Baked, Box>();
function contentBox(scene: Baked): Box {
  let b = contentBoxCache.get(scene);
  if (b === undefined) {
    let c0 = Infinity, c1 = -1, r0 = Infinity, r1 = -1;
    for (const frame of scene.frames)
      for (const layer of frame)
        layer.split("\n").forEach((ln, r) => {
          const end = ln.trimEnd().length;
          if (!end) return;
          const start = ln.length - ln.trimStart().length;
          if (r < r0) r0 = r;
          if (r > r1) r1 = r;
          if (start < c0) c0 = start;
          if (end - 1 > c1) c1 = end - 1;
        });
    // An empty scene would leave the sentinels: fall back to the declared grid.
    b = c1 < 0
      ? { c0: 0, c1: scene.cols - 1, r0: 0, r1: scene.rows - 1 }
      : { c0, c1, r0, r1 };
    contentBoxCache.set(scene, b);
  }
  return b;
}

/** Cell size in em. MUST match --ascii-cell in styles.css: this sizes the box
 *  and .ascii-scene clips, so a mismatch crops the art (a layer can be a third
 *  of the scene's width, which is exactly how far it crops). */
const ASCII_CELL = 0.9;

/** Gray-layer glyph scale. MUST match --ascii-leg in styles.css, which is where
 *  the geometry lives. The legs are the only thing in that layer that reaches
 *  the top of the dot ramp, and the ramp's top two steps differ by 2.1x — so
 *  this is the only continuous control over how heavy a leg reads. It does NOT
 *  change the cell: the CSS takes the shrink back out of letter-spacing. */
const ASCII_LEG = 0.85;

export default function AsciiScene(props: {
  scene: Baked;
  /** px per glyph. A CELL is ASCII_CELL x this — bigger than the glyph box. */
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
  const cell = (props.fontSize ?? 8) * ASCII_CELL;
  // Composed once: neither the text nor its anchor changes per frame — only
  // whether the bubble is showing.
  const bub = bubble && props.line ? composeBubble(props.line, bubble, cols, rows) : null;
  const bubbleAt = (f: number) =>
    bub && f >= bubble!.from && f < bubble!.to ? bub.text : "";
  // The bubble is real content: it can sit outside the art's own box (a
  // `place: "right"` bubble runs past its right edge, an "above" one starts
  // above its top row), so the box is the union of the two. The declared grid
  // stays the ceiling.
  const art = contentBox(props.scene);
  const bubLines = bub ? bub.text.split("\n") : [];
  const box = {
    c0: bub ? Math.min(art.c0, bub.col) : art.c0,
    c1: bub
      ? Math.min(cols - 1, Math.max(art.c1, bub.col + Math.max(...bubLines.map((l) => l.length)) - 1))
      : art.c1,
    r0: bub ? Math.min(art.r0, bub.row) : art.r0,
    r1: bub ? Math.min(rows - 1, Math.max(art.r1, bub.row + bubLines.length - 1)) : art.r1,
  };
  const boxCols = box.c1 - box.c0 + 1;
  const boxRows = box.r1 - box.r0 + 1;

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
  // must not size the scene (a layer can be much narrower than the grid). Both
  // axes are the OCCUPIED grid, not the declared one — see contentBox. The
  // layers still paint in grid coordinates, so they are shifted up/left by the
  // box origin (`--ascii-dx/dy`) to bring the art flush into it.
  // Cells are SQUARE — width AND height use the same ASCII_CELL factor, which
  // the CSS applies as letter-spacing (x) and line-height (y). It is larger
  // than JetBrains Mono's 0.6em advance on purpose: the dots stand apart and a
  // solid fill reads as a dot matrix rather than a slab.
  return (
    <div
      class={`ascii-scene ${props.class ?? ""}`}
      style={{
        "--ascii-fs": `${props.fontSize ?? 8}px`,
        "--ascii-cell": `${ASCII_CELL}`,
        "--ascii-leg": `${ASCII_LEG}`,
        "--ascii-dx": `${-box.c0 * cell}px`,
        "--ascii-dy": `${-box.r0 * cell}px`,
        width: `${boxCols * cell}px`,
        height: `${boxRows * cell}px`,
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
          style={{
            left: `${(bub.col - box.c0) * cell}px`,
            top: `${(bub.row - box.r0) * cell}px`,
          }}
        />
      )}
    </div>
  );
}
