// Offline ASCII baker — the doodle-generator pattern (docs DESIGN.md §5)
// applied to the animated beetle scenes: render once here, commit the output,
// ship ZERO rendering code. The UIs play plain text frames by swapping
// textContent at 12 fps.
//
// Output format (generated/*.json):
//   { cols, rows, fps, frames: [[em, au, fe], ...] }
// Each frame is three same-shape text layers split by brand role — emerald
// (wings), gold (body/ball/nimbus), gray (legs/films/ground). Players stack
// three <pre> elements and color them with CSS custom properties, so the art
// follows the theme with no per-cell work at runtime.
//
// Also bakes generated/emblem-mark.txt: the TRACED emblem (../logo, winding
// kept intact) as a single static mono mark for faint background use.
//
// Run: npm install && npm run bake   (deterministic — reruns don't churn)
import { createCanvas, Path2D } from "@napi-rs/canvas";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  drawScarab, drawBeetle, VB_W, VB_H, BEETLE_VB_H, BEETLE_VB_H_BARE, CELL_ASPECT,
} from "./scenes.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, "generated");
mkdirSync(outDir, { recursive: true });

const RAMP = " .`:;i+=ox*XO#@";
const GAMMA = 0.8;
const FPS = 12;

// cell -> brand-role layer: 0 emerald, 1 gold, 2 gray
function classify(r, g, b) {
  if (g > r * 1.22 && g > b) return 0;
  if (r > g * 1.12 && r > b * 1.3) return 1;
  return 2;
}

/** Sample a cols×rows canvas into three text layers (em, au, fe). */
function toLayers(ctx, cols, rows) {
  const d = ctx.getImageData(0, 0, cols, rows).data;
  const layers = [[], [], []];
  for (let row = 0; row < rows; row++) {
    const lines = ["", "", ""];
    for (let col = 0; col < cols; col++) {
      const i = (row * cols + col) * 4;
      const a = d[i + 3] / 255;
      const lum = (0.2126 * d[i] * a + 0.7152 * d[i + 1] * a + 0.0722 * d[i + 2] * a) / 255;
      const ci = Math.min(RAMP.length - 1, Math.floor(Math.pow(lum, GAMMA) * RAMP.length));
      if (ci === 0) { lines[0] += " "; lines[1] += " "; lines[2] += " "; continue; }
      const layer = classify(d[i], d[i + 1], d[i + 2]);
      for (let l = 0; l < 3; l++) lines[l] += l === layer ? RAMP[ci] : " ";
    }
    for (let l = 0; l < 3; l++) layers[l].push(lines[l].replace(/ +$/, ""));
  }
  return layers.map((rowsArr) => rowsArr.join("\n"));
}

function bakeScene(name, { cols, rows, frames, draw }) {
  const canvas = createCanvas(cols, rows);
  const ctx = canvas.getContext("2d");
  const out = [];
  for (let f = 0; f < frames; f++) {
    draw(ctx, f / frames, cols, rows);
    out.push(toLayers(ctx, cols, rows));
  }
  const json = JSON.stringify({ cols, rows, fps: FPS, frames: out });
  writeFileSync(join(outDir, `${name}.json`), json, "utf8");
  console.log(`bake: ${name}.json — ${cols}×${rows}, ${frames} frames, ${(json.length / 1024).toFixed(0)} KB`);
}

// Scarab rows: cols * (VB_H/VB_W) * CELL_ASPECT keeps the nimbus round.
const scarabRows = (cols) => Math.round(cols * (VB_H / VB_W) * CELL_ASPECT);
const beetleRows = (cols, vbH) => Math.round(cols * (vbH / 96) * CELL_ASPECT);

// The wing-spread scene (drawScarab) is currently unbaked — the docs hero uses
// the square emblem SVG. Re-add a bakeScene line if a state moment wants it.
bakeScene("dungroller", { cols: 88, rows: beetleRows(88, BEETLE_VB_H), frames: 72, draw: drawBeetle });
// Traveling variant: no ground in the scene (crops at the feet) — the host
// provides the ground line and moves the beetle across it.
bakeScene("dungroller-bare", {
  cols: 88,
  rows: beetleRows(88, BEETLE_VB_H_BARE),
  frames: 72,
  draw: (x, u, c, r) => drawBeetle(x, u, c, r, { ground: false }),
});

// ---- static mark: the traced emblem, verbatim ------------------------------
// The dark layer is ONE compound path (holes by winding); it renders correctly
// as a whole — it just can't be split, which is why the animated scarab above
// is parametric.
{
  const svg = readFileSync(join(here, "..", "logo", "scarab-emblem.svg"), "utf8");
  const group = (fill) =>
    [...svg.matchAll(new RegExp(`<g fill="${fill}">([\\s\\S]*?)</g>`, "g"))]
      .flatMap((m) => [...m[1].matchAll(/<path d="([\s\S]*?)"\/>/g)].map((p) => p[1].replace(/\n/g, " ")));
  const wings = group("#1f9e74");
  const gold = group("#c9a94e");
  const dark = group("#111418");

  const cols = 64;
  const rows = scarabRows(cols);
  const canvas = createCanvas(cols, rows);
  const x = canvas.getContext("2d");
  const sx = cols / VB_W, sy = sx * CELL_ASPECT;
  x.setTransform(sx, 0, 0, sy, 0, (rows - VB_H * sy) / 2);
  x.translate(0, VB_H); x.scale(0.1, -0.1);
  x.fillStyle = "#27b584";
  for (const d of wings) x.fill(new Path2D(d));
  x.fillStyle = "#d9b45e";
  for (const d of gold) x.fill(new Path2D(d));
  x.fillStyle = "#6e7f76";
  x.fill(new Path2D(dark.join(" ")));

  const d = x.getImageData(0, 0, cols, rows).data;
  const lines = [];
  for (let row = 0; row < rows; row++) {
    let line = "";
    for (let col = 0; col < cols; col++) {
      const i = (row * cols + col) * 4;
      const a = d[i + 3] / 255;
      const lum = (0.2126 * d[i] * a + 0.7152 * d[i + 1] * a + 0.0722 * d[i + 2] * a) / 255;
      const ci = Math.min(RAMP.length - 1, Math.floor(Math.pow(lum, GAMMA) * RAMP.length));
      line += ci > 0 ? RAMP[ci] : " ";
    }
    lines.push(line.replace(/ +$/, ""));
  }
  writeFileSync(join(outDir, "emblem-mark.txt"), lines.join("\n") + "\n", "utf8");
  console.log(`bake: emblem-mark.txt — ${cols}×${rows} static traced emblem`);
}
