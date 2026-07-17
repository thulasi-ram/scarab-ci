// Dot-matrix doodle generator — DESIGN.md §5.
//
// Rasterizes each Lucide line icon onto a coarse grid and emits one dot per
// lit cell, so a stroke is SEVERAL dots wide — a true dot-matrix rendering
// (not a dotted outline), the same language as the ASCII beetle scenes and
// the page dot-grid. Output SVGs are committed; both UIs consume them
// verbatim (docs Doodle.astro, web-ui Doodle.tsx).
//
// Run via `npm run bake` (deterministic — reruns don't churn).
import { createCanvas, Path2D } from "@napi-rs/canvas";
import { writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  Bug, GitBranch, GitCommitHorizontal, Container, Boxes, Workflow,
  Waypoints, Package, Terminal, KeyRound, ShieldCheck, Timer, Network,
} from "lucide";

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, "generated", "doodles");
mkdirSync(outDir, { recursive: true });

// The motif set (DESIGN.md §5). `bug` is the house motif.
const MOTIFS = {
  bug: Bug, "git-branch": GitBranch, "git-commit": GitCommitHorizontal,
  container: Container, boxes: Boxes, workflow: Workflow, waypoints: Waypoints,
  package: Package, terminal: Terminal, "key-round": KeyRound,
  "shield-check": ShieldCheck, timer: Timer, network: Network,
};

// Icon space is 24×24 with stroke-width 2 → on a 24-cell grid every stroke
// lights ~2 cells across ("multiple rows of dots"). Rendered oversampled.
const GRID = 24;
const SS = 10;                    // supersample: 10px per cell
const STROKE_W = 2;               // lucide's native stroke width
const THRESHOLD = 0.28;           // min cell coverage to earn a dot
const DOT_R = 0.34;               // dot radius in grid units
const INK = "#c0873f";            // copper — a literal hex (SVG attribute)

function toPairs(points) {
  const n = String(points).trim().split(/[\s,]+/).map(Number);
  const out = [];
  for (let i = 0; i + 1 < n.length; i += 2) out.push([n[i], n[i + 1]]);
  return out;
}

/** Stroke one Lucide primitive onto the canvas. */
function strokePrimitive(x, tag, a) {
  const num = (v) => Number(v ?? 0);
  switch (tag) {
    case "path": if (a.d) x.stroke(new Path2D(String(a.d))); break;
    case "circle":
      x.beginPath(); x.arc(num(a.cx), num(a.cy), num(a.r), 0, 7); x.stroke(); break;
    case "ellipse":
      x.beginPath(); x.ellipse(num(a.cx), num(a.cy), num(a.rx), num(a.ry), 0, 0, 7); x.stroke(); break;
    case "line":
      x.beginPath(); x.moveTo(num(a.x1), num(a.y1)); x.lineTo(num(a.x2), num(a.y2)); x.stroke(); break;
    case "rect": {
      x.beginPath();
      const r = num(a.rx);
      if (r) x.roundRect(num(a.x), num(a.y), num(a.width), num(a.height), r);
      else x.rect(num(a.x), num(a.y), num(a.width), num(a.height));
      x.stroke(); break;
    }
    case "polyline": case "polygon": {
      const pts = toPairs(a.points);
      if (!pts.length) break;
      x.beginPath(); x.moveTo(pts[0][0], pts[0][1]);
      for (const [px, py] of pts.slice(1)) x.lineTo(px, py);
      if (tag === "polygon") x.closePath();
      x.stroke(); break;
    }
  }
}

let n = 0;

for (const [name, iconNode] of Object.entries(MOTIFS)) {
  const px = GRID * SS;
  const canvas = createCanvas(px, px);
  const x = canvas.getContext("2d");
  x.scale(SS, SS);
  x.strokeStyle = "#ffffff";
  x.lineWidth = STROKE_W;
  x.lineCap = "round";
  x.lineJoin = "round";
  for (const [tag, attrs] of iconNode) strokePrimitive(x, tag, attrs);

  // coverage per grid cell -> dot
  const data = x.getImageData(0, 0, px, px).data;
  const dots = [];
  for (let gy = 0; gy < GRID; gy++) {
    for (let gx = 0; gx < GRID; gx++) {
      let sum = 0;
      for (let sy = 0; sy < SS; sy++) {
        const row = ((gy * SS + sy) * px + gx * SS) * 4 + 3;
        for (let sx = 0; sx < SS; sx++) sum += data[row + sx * 4];
      }
      if (sum / (SS * SS * 255) >= THRESHOLD) {
        dots.push(`<circle cx="${gx + 0.5}" cy="${gy + 0.5}" r="${DOT_R}"/>`);
      }
    }
  }

  const svg =
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${GRID} ${GRID}" fill="${INK}">\n` +
    `  ${dots.join("\n  ")}\n</svg>\n`;
  writeFileSync(join(outDir, `${name}.svg`), svg, "utf8");
  n++;
}

console.log(`dot-icons: ${n} dot-matrix doodles -> generated/doodles/`);
