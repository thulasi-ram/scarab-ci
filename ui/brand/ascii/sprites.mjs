// The traced beetle, composed into the bodies the scenes use, plus the
// cell-exact primitives they draw with.
//
// WHY CELL-EXACT. The scenes bake onto a canvas where one pixel IS one cell,
// and the bake maps luminance to dot SIZE on " .·•●". A stroked line therefore
// does not render as a line of the width you asked for: its antialiased fringe
// is sub-cell, and every grazed cell still crosses the ramp and bakes to a
// dot. A 1.65-cell leg comes out a three-dot band; a 2-cell ball rim comes out
// three rows thick at the poles however you tune it. So limbs and the rim are
// rasterized cell by cell, like the sprite itself.
import { createCanvas } from "@napi-rs/canvas";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const SHEET = JSON.parse(readFileSync(join(here, "sprites", "beetle-poses.json"), "utf8"));
const TAU = Math.PI * 2;

/** Brand layer glyphs used by the composed bodies. */
export const INK = {
  "#": "#4ecfa2",   // elytra
  "%": "#d9b45e",   // the shell's highlight slab — gold, see below
  o: "#d9b45e",     // pronotum, head, horn
  "-": "#9bb4a7",   // limbs — see below
};
const RIM = "#d9b45e", RIM_DIM = "#a8873f", FLECK = "#b3924a";

// THE LIMB INK IS A THRESHOLD, NOT A SHADE. The bake maps luminance to dot
// SIZE, and a colour must reach 0.6665 to land on the biggest dot on the ramp.
// The old #8fa89b measured 0.6343 — just under it — so the legs baked one ramp
// step below the body and the ball, and at the size the site actually renders
// (a 3px cell) they were not there at all. #9bb4a7 is 0.6814: same cell count,
// every one a size bigger, and still gray to classify(). Widening the limb was
// the obvious fix and it is the wrong one; this costs nothing.


// The shell is #4ecfa2, not the roller's old #27b584. Inside ONE layer the
// only thing a colour can say is dot SIZE: #27b584 bakes to '•' while the gold
// head bakes to '●', so at ship size the beetle read as a gold head with a
// faint green smudge behind it. #4ecfa2 puts both on '●'. That in turn forces
// the shell's highlight into the GOLD layer — a brighter emerald has nowhere
// left on the ramp to go.

const faceRight = (rows) => rows.map((l) => [...l].reverse().join(""));
const sleep = faceRight(SHEET.poses.sleep).slice(7);   // rows 0..6 are the sheet's Zzz
const idle = faceRight(SHEET.poses.idle);

// ── the PONDER body ─────────────────────────────────────────────────────────
// The sheet's sleeper's carapace with the standing pose's front — pronotum,
// head and the horn ARCH — dropped so its belly meets the shell. The sleeper's
// own head is tucked and its horn curled to the floor, which at this pitch is
// a blank snout; the arch is the one cue that survives the bake and a settled
// beetle still has to read as a beetle. Then MIRRORED, so it lies with its
// back to the ball and the horn points into open space.
const PW = 40, PH = 17, PAD_L = 2, SEAM = 11, FRONT = 13;
function ponderBody() {
  const g = Array.from({ length: PH }, () => Array(PW).fill("."));
  for (let r = 0; r < 11; r++) for (let c = 0; c <= SEAM; c++) {
    const ch = sleep[r]?.[c]; if (!ch || ch === ".") continue;
    g[r + 1][c + PAD_L] = ch === "@" ? "%" : "#";
  }
  for (let r = 0; r < 12; r++) for (let c = FRONT; c < idle[r].length; c++) {
    const ch = idle[r]?.[c]; if (!ch || ch === ".") continue;
    g[r + 1][c - FRONT + PAD_L + SEAM + 1] = "o";
  }
  for (let r = 11; r < 13; r++) for (let c = 0; c < sleep[r].length; c++)   // tucked legs
    if (sleep[r]?.[c] && sleep[r][c] !== ".") g[r + 1][c + PAD_L] = "-";
  return g.map((r) => [...r].reverse().join("").replace(/\s+$/, ""));
}

// ── the ROLLER body ─────────────────────────────────────────────────────────
// The standing pose rotated 55° into the head-down push. 45° was the first
// cut and reads as a beetle leaning; 55° stands it up into a shove — the
// silhouette goes narrower and taller (22×18 cells to 21×20) and the raised
// back and the ball start to read as one gesture.
//
// Rotating pixel art is normally wrong, and it is safe HERE because the final
// medium is a dot matrix, not crisp pixels: what has to survive is the
// silhouette and the region boundaries, not the staircase. The sprite is
// painted as flat single-channel KEYS — one pure colour per brand layer —
// rotated with smoothing OFF, and each output cell resolved by a MAJORITY VOTE
// over those keys. No blended colour is ever produced, so no cell can land
// between layers, which is the actual failure mode for a role-split bake.
//
// A shear was tried first and tears the art: it slants each column
// independently, and the horn's arch is twelve columns wide, so even a mild
// shear drags it four rows across itself and it stops being an arch. Rotation
// moves it as one rigid piece.
const RW = 36, RH = 26, PAD_T = 1;
function rollerBody(deg = 55) {
  const src = SHEET.poses.idle;                      // the sheet faces LEFT: the push direction
  const seam = SHEET.parts.idle.seam, bodyRows = SHEET.parts.idle.legRow;
  const K = 12, sw = src[0].length, diag = Math.ceil(Math.hypot(sw, bodyRows)) + 2;
  const o = createCanvas(diag * K, diag * K), ox = o.getContext("2d");
  ox.imageSmoothingEnabled = false;
  const s2 = createCanvas(sw * K, bodyRows * K), sx = s2.getContext("2d");
  for (let r = 0; r < bodyRows; r++) for (let c = 0; c < sw; c++) {
    if (src[r][c] === ".") continue;
    sx.fillStyle = c >= seam ? "rgb(255,0,0)" : "rgb(0,0,255)";   // shell / body
    sx.fillRect(c * K, r * K, K, K);
  }
  ox.translate(diag * K / 2, diag * K / 2);
  ox.rotate(-deg * Math.PI / 180);                   // face low, back high
  ox.drawImage(s2, -sw * K / 2, -bodyRows * K / 2);
  const im = ox.getImageData(0, 0, diag * K, diag * K).data, g = [];
  for (let r = 0; r < diag; r++) {
    const row = [];
    for (let c = 0; c < diag; c++) {
      let red = 0, blue = 0, tot = 0;
      for (let j = 0; j < K; j++) for (let i = 0; i < K; i++) {
        const q = ((r * K + j) * diag * K + (c * K + i)) * 4;
        if (im[q + 3] < 128) continue;
        tot++; if (im[q] > 127) red++; else blue++;
      }
      row.push(tot / (K * K) >= 0.42 ? (red >= blue ? "#" : "o") : ".");
    }
    g.push(row);
  }
  let ra = 1e9, rb = -1, ca = 1e9, cb = -1;
  g.forEach((row, r) => row.forEach((ch, c) => {
    if (ch === ".") return;
    if (r < ra) ra = r; if (r > rb) rb = r; if (c < ca) ca = c; if (c > cb) cb = c;
  }));
  const out = Array.from({ length: RH }, () => Array(RW).fill("."));
  g.slice(ra, rb + 1).forEach((row, r) => row.slice(ca, cb + 1).forEach((ch, c) => {
    if (ch !== ".") out[r + PAD_T][c + PAD_L] = ch;
  }));
  // Re-cut the shell's highlight AFTER the rotation. The sheet draws it as a
  // hard slab across the shell's upper front; turned into this pose that slab lands as an
  // amorphous patch mid-shell and reads as damage. Drawn fresh along the
  // shell's top contour it becomes a shine down the beetle's raised back,
  // which is what this pose wants anyway.
  const cols = [];
  for (let c = 0; c < RW; c++) { const top = out.findIndex((row) => row[c] === "#"); if (top >= 0) cols.push([c, top]); }
  cols.slice(Math.round(cols.length * 0.18), Math.round(cols.length * 0.86))
    .forEach(([c, top]) => { for (let k = 0; k < 2; k++) if (out[top + k]?.[c] === "#") out[top + k][c] = "%"; });
  return out.map((r) => r.join("").replace(/\s+$/, ""));
}

export const BODIES = { ponder: ponderBody(), roller: rollerBody() };

// ── cell-exact primitives ───────────────────────────────────────────────────
/** Blit a body. The art is authored in CELLS, so it is drawn with the
 *  transform reset — never through a scene's design-unit transform.
 *
 *  `dy` shifts it by a FRACTION of a cell, which is how this art breathes.
 *  The bake maps coverage to dot SIZE, so a sub-cell offset makes every cell
 *  on the silhouette swell and shrink through the ramp while the interior
 *  holds solid — the body reads as soft-edged and alive instead of as a
 *  bitmap being translated. (A WHOLE-cell offset can't do this: it is a jump,
 *  and at gait frequency it reads as vibration.)
 *
 *  Each output cell is RESOLVED, never blended: it takes its coverage from
 *  the two source rows it straddles but its INK from whichever of them covers
 *  it more. Blending is not an option here — laying the shell's gold
 *  highlight over the emerald at 0.73 of a cell mixes to rgb(170,189,117),
 *  which classify() reads as the gray LIMB layer, so the seam would come out
 *  as a gray stripe across the shell at every peak of the bob. */
export function blit(x, body, col, row, dy = 0) {
  x.save(); x.setTransform(1, 0, 0, 1, 0, 0);
  const k = Math.floor(dy), f = dy - k;             // whole cells, then the fraction
  const at = (r, c) => { const ch = body[r]?.[c]; return !ch || ch === "." || ch === " " ? null : ch; };
  const h = body.length + (f > 0 ? 1 : 0);
  for (let r = 0; r < h; r++) {
    const wide = Math.max(body[r - k]?.length ?? 0, body[r - k - 1]?.length ?? 0);
    for (let c = 0; c < wide; c++) {
      const near = at(r - k, c), far = at(r - k - 1, c);   // weights 1-f and f
      const a = (near ? 1 - f : 0) + (far ? f : 0);
      if (a <= 0) continue;
      x.globalAlpha = a;
      x.fillStyle = INK[(near && (!far || f < 0.5)) ? near : far];
      x.fillRect(col + c, row + r, 1, 1);
    }
  }
  x.globalAlpha = 1;
  x.restore();
}

/** A limb: a 1-cell path ending in a 2-cell foot. The sheet ends every leg in
 *  a foot, and without it a bare diagonal reads as a scratch, not a limb.
 *  Draw limbs BEFORE the body — a braced leg leaves from under the carapace,
 *  and stroked afterwards it lays a gray band straight across the shell. */
export function limb(x, pts, foot = "flat") {
  x.save(); x.setTransform(1, 0, 0, 1, 0, 0); x.fillStyle = INK["-"];
  const put = (c, r) => x.fillRect(Math.round(c), Math.round(r), 1, 1);
  for (let i = 0; i < pts.length - 1; i++) {
    const [ax, ay] = pts[i], [bx, by] = pts[i + 1];
    const n = Math.max(Math.abs(bx - ax), Math.abs(by - ay)) || 1;
    for (let k = 0; k <= n; k++) put(ax + (bx - ax) * k / n, ay + (by - ay) * k / n);
  }
  const [fx, fy] = pts[pts.length - 1];
  if (foot === "flat") { put(fx - 1, fy); put(fx, fy); }   // on the ground
  else { put(fx, fy - 1); put(fx, fy); }                   // gripping the rim
  x.restore();
}

/** The dung ball (DUNG.md). `roll` is the only input: rim rhythm and fleck
 *  angles are both derived from it. */
export function ball(x, cx, cy, r, roll) {
  x.save(); x.setTransform(1, 0, 0, 1, 0, 0);
  const seen = new Set();
  const put = (c, w) => { const k = c + "," + w; if (seen.has(k)) return; seen.add(k); x.fillRect(c, w, 1, 1); };
  const period = (TAU * r) / 21;                  // 21 dashes wrap the circle exactly
  const steps = Math.ceil(TAU * r * 4);
  let prev = null;
  for (let i = 0; i <= steps; i++) {
    const th = (i / steps) * TAU;
    const arc = th * r + roll * r;                // phase-locked to the roll
    // THE RHYTHM IS DRAWN AS WEIGHT, NOT PRESENCE. The committed rim's 21
    // dashes at 62% duty are intact, but the "off" arcs are dimmed rather than
    // omitted: on a dot matrix a gap is a HOLE, and once the cells are spaced
    // (see the player's cell factor) the eye no longer closes them, so a
    // dashed ring stops being a circle. A lighter dot marches just as well and
    // the circle never opens.
    x.fillStyle = ((arc % period) + period) % period < period * 0.62 ? RIM : RIM_DIM;
    const c = Math.round(cx + Math.cos(th) * r), w = Math.round(cy + Math.sin(th) * r);
    // 4-CONNECTED. Cells that touch only at a corner sit sqrt(2) apart, which
    // reads as a hole at this spacing; filling the corner closes it.
    if (prev && prev[0] !== c && prev[1] !== w) put(prev[0], w);
    put(c, w);
    prev = [c, w];
  }
  // Dung texture, turning with the roll. SIXTEEN flecks at THREE sizes — the
  // committed ball's own rule. Equal-sized flecks orbit as a pattern; varied
  // ones read as a surface turning, because the eye tracks the big ones and
  // the small ones fill between them.
  x.fillStyle = FLECK;
  for (let i = 0; i < 16; i++) {
    const a = roll + i * 2.39996, rr = r * (0.18 + 0.52 * ((i * 0.61) % 1));
    const fs = 1 + ((i * 7) % 3);
    x.fillRect(Math.round(cx + Math.cos(a) * rr), Math.round(cy + Math.sin(a) * rr), fs, fs);
  }
  x.restore();
}
