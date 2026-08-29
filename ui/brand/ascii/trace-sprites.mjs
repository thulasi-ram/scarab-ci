// Trace the supplied beetle sprite sheet into a committed asset.
//
// `sprites/beetle-sheet.png` is a restoration of a pixel-art sheet: three
// poses, drawn in four reds, upscaled and speckled with noise. Everything the
// scenes need is the underlying PIXEL GRID, so this reads the sheet back at
// its native pitch and writes:
//
//   sprites/beetle-poses.json  the three poses as cell grids, in the sheet's
//                              own four shades — a faithful transcription, no
//                              brand decisions baked in
//   sprites/beetle-poses.png   the same poses in BRAND colours, 8x, for review
//
// Measured off the sheet once, recorded here so nobody re-derives them:
//   • binary alpha — background < 128, ink >= 128
//   • native pixel pitch 15.5 source px, grid phase (12.5, 6). Found by
//     autocorrelating ink-transition edges, then confirmed by a cell-purity
//     scan: 97% of cells are unanimous at this pitch and phase.
//   • four ink colours, from k-means over per-cell median RGB. Ordered dark to
//     light they are outline, shadow, body and highlight.
// The sprites are ~25x20 native px. That is the whole design: at 1.5x the
// horn's arch breaks into disconnected cells, so the scenes use it 1:1.
import { createCanvas, loadImage } from "@napi-rs/canvas";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dir = join(here, "sprites");

export const PITCH = 15.5, PHASE_X = 12.5, PHASE_Y = 6;
/** Source-x span of each pose on the sheet. */
export const POSES = { sleep: [27, 415], idle: [449, 867], rear: [906, 1268] };
/** The sheet's four inks, dark -> light, and the glyph each is written as. */
export const SHADES = [
  { key: "#", rgb: [133, 6, 3] },     // outline
  { key: "+", rgb: [196, 8, 5] },     // shadow
  { key: "o", rgb: [214, 27, 19] },   // body
  { key: "@", rgb: [254, 135, 130] }, // highlight
];
/** Where each pose's anatomy divides, in the sheet's own LEFT-facing columns.
 *  `seam` is the pronotum/elytra joint: columns at or past it are the shell.
 *  `legRow` is the first row that is legs rather than carapace. Both are
 *  features OF THE DRAWING — they are what the scenes colour by, since the
 *  brand layers are anatomical (shell = emerald, head/pronotum/horn = gold,
 *  limbs = gray) and not a function of the sheet's shading.
 *  `rear` is unannotated: the scenes do not use it. */
export const PARTS = { sleep: { seam: 13, legRow: 18 }, idle: { seam: 13, legRow: 12 } };
/** Brand layer colours. */
export const BRAND = { shell: "#4ecfa2", shine: "#d9b45e", body: "#d9b45e", limb: "#8fa89b" };

export async function trace(sheet = join(dir, "beetle-sheet.png")) {
  const img = await loadImage(sheet);
  const W = img.width, H = img.height;
  const cv = createCanvas(W, H), cx = cv.getContext("2d");
  cx.drawImage(img, 0, 0);
  const d = cx.getImageData(0, 0, W, H).data;
  const med = (a) => { a.sort((p, q) => p - q); return a.length ? a[a.length >> 1] : 0; };
  const nearest = (p) => {
    let best = SHADES[0], bd = Infinity;
    for (const s of SHADES) {
      const t = (s.rgb[0] - p[0]) ** 2 + (s.rgb[1] - p[1]) ** 2 + (s.rgb[2] - p[2]) ** 2;
      if (t < bd) { bd = t; best = s; }
    }
    return best.key;
  };
  const out = {};
  for (const [name, [x0, x1]] of Object.entries(POSES)) {
    const ia = Math.floor((x0 - PHASE_X) / PITCH), ib = Math.floor((x1 - PHASE_X) / PITCH);
    const ny = Math.ceil((H - PHASE_Y) / PITCH), grid = [];
    for (let j = 0; j < ny; j++) {
      const row = [];
      for (let i = ia; i <= ib; i++) {
        const sx = PHASE_X + i * PITCH, sy = PHASE_Y + j * PITCH;
        const R = [], G = [], B = []; let on = 0, tot = 0;
        // inset by 2px: the restoration's edges are soft, the middles are not
        for (let py = Math.round(sy + 2); py < Math.round(sy + PITCH - 2); py++)
          for (let px = Math.round(sx + 2); px < Math.round(sx + PITCH - 2); px++) {
            if (px < 0 || py < 0 || px >= W || py >= H) continue;
            tot++;
            const q = (py * W + px) * 4;
            if (d[q + 3] >= 128) { on++; R.push(d[q]); G.push(d[q + 1]); B.push(d[q + 2]); }
          }
        row.push(tot && on / tot >= 0.5 ? nearest([med(R), med(G), med(B)]) : ".");
      }
      grid.push(row);
    }
    let ra = 1e9, rb = -1, ca = 1e9, cb = -1;
    grid.forEach((row, j) => row.forEach((ch, i) => {
      if (ch === ".") return;
      if (j < ra) ra = j; if (j > rb) rb = j; if (i < ca) ca = i; if (i > cb) cb = i;
    }));
    out[name] = grid.slice(ra, rb + 1).map((r) => r.slice(ca, cb + 1).join(""));
  }
  return out;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const poses = await trace();
  writeFileSync(join(dir, "beetle-poses.json"), JSON.stringify({
    source: "beetle-sheet.png",
    pitch: PITCH, phase: [PHASE_X, PHASE_Y],
    shades: SHADES, parts: PARTS, brand: BRAND,
    note: "Every beetle on the sheet faces LEFT. Scenes mirror as needed.",
    poses,
  }, null, 1) + "\n");
  // Brand-coloured review sheet, 8x, poses laid out on the app ground.
  const S = 8, gap = 4;
  const wide = Object.values(poses).reduce((a, p) => a + p[0].length + gap, gap);
  const tall = Math.max(...Object.values(poses).map((p) => p.length)) + gap * 2;
  const o = createCanvas(wide * S, tall * S), ox = o.getContext("2d");
  ox.fillStyle = "#0d1411"; ox.fillRect(0, 0, wide * S, tall * S);
  let col = gap;
  for (const [name, p] of Object.entries(poses)) {
    const part = PARTS[name];
    p.forEach((line, r) => [...line].forEach((ch, c) => {
      if (ch === ".") return;
      ox.fillStyle = !part ? BRAND.shell
        : r >= part.legRow ? BRAND.limb
        : c < part.seam ? BRAND.body
        : ch === "@" ? BRAND.shine : BRAND.shell;
      ox.fillRect((col + c) * S, (gap + r) * S, S, S);
    }));
    col += p[0].length + gap;
  }
  writeFileSync(join(dir, "beetle-poses.png"), o.toBuffer("image/png"));
  for (const [k, v] of Object.entries(poses)) console.log(`${k}: ${v[0].length}x${v.length}`);
  console.log("wrote sprites/beetle-poses.json + beetle-poses.png");
}
