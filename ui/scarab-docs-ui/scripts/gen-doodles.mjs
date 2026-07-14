// Offline doodle generator — ADR-0040 + docs/DESIGN.md §5.
//
// Takes clean Lucide line icons, runs each through rough.js ONCE (DOM-free, via
// rough.generator()), and writes the hand-sketched result as a static SVG to a
// committed assets dir. The site serves those SVGs and has NO rough.js at
// runtime or in its build. Regenerate on demand (`npm run gen:doodles`) and
// commit the output; a fixed per-icon seed keeps it deterministic (no churn).
import { writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import rough from 'roughjs';
import {
  Bug, GitBranch, GitCommitHorizontal, Container, Boxes, Workflow,
  Waypoints, Package, Terminal, KeyRound, ShieldCheck, Timer, Network,
} from 'lucide';

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, '..', 'src', 'assets', 'doodles');

// Canonical rough.js settings (DESIGN.md §5). Copper ink, outlines only, barely
// sketchy. `stroke` is a literal hex — rough writes SVG presentation attributes
// where CSS custom properties don't resolve.
const OPTS = { roughness: 0.12, bowing: 0.5, curveStepCount: 5, strokeWidth: 1.6, stroke: '#c0873f', fill: 'none' };

// The motif set (DESIGN.md §5). Name -> Lucide IconNode. `bug` is the house motif.
const MOTIFS = {
  bug: Bug, 'git-branch': GitBranch, 'git-commit': GitCommitHorizontal,
  container: Container, boxes: Boxes, workflow: Workflow, waypoints: Waypoints,
  package: Package, terminal: Terminal, 'key-round': KeyRound,
  'shield-check': ShieldCheck, timer: Timer, network: Network,
};

const r = (n) => Math.round(n * 100) / 100;

/** Serialize one rough OpSet to an SVG path `d` string. */
function opsToPath(ops) {
  let d = '';
  for (const { op, data } of ops) {
    if (op === 'move') d += `M${r(data[0])} ${r(data[1])}`;
    else if (op === 'lineTo') d += `L${r(data[0])} ${r(data[1])}`;
    else if (op === 'bcurveTo') d += `C${r(data[0])} ${r(data[1])} ${r(data[2])} ${r(data[3])} ${r(data[4])} ${r(data[5])}`;
  }
  return d;
}

function toPairs(points) {
  const n = String(points).trim().split(/[\s,]+/).map(Number);
  const out = [];
  for (let i = 0; i + 1 < n.length; i += 2) out.push([n[i], n[i + 1]]);
  return out;
}

/** Draw one Lucide primitive via the generator; returns its OpSets. */
function drawablePrimitive(g, tag, a, opts) {
  const num = (v) => Number(v ?? 0);
  switch (tag) {
    case 'path': return a.d ? g.path(String(a.d), opts).sets : [];
    case 'circle': return g.circle(num(a.cx), num(a.cy), num(a.r) * 2, opts).sets;
    case 'ellipse': return g.ellipse(num(a.cx), num(a.cy), num(a.rx) * 2, num(a.ry) * 2, opts).sets;
    case 'line': return g.line(num(a.x1), num(a.y1), num(a.x2), num(a.y2), opts).sets;
    case 'rect': return g.rectangle(num(a.x), num(a.y), num(a.width), num(a.height), opts).sets;
    case 'polyline': return g.linearPath(toPairs(a.points), opts).sets;
    case 'polygon': return g.polygon(toPairs(a.points), opts).sets;
    default: return [];
  }
}

mkdirSync(outDir, { recursive: true });
const gen = rough.generator();
let n = 0;

for (const [name, iconNode] of Object.entries(MOTIFS)) {
  // Fixed seed per motif → deterministic output, no churn between runs.
  const opts = { ...OPTS, seed: (n + 1) * 9973 };
  const paths = [];
  for (const [tag, attrs] of iconNode) {
    for (const set of drawablePrimitive(gen, tag, attrs, opts)) {
      if (set.type !== 'path') continue; // fill:none -> only stroke paths
      const d = opsToPath(set.ops);
      if (d) paths.push(`<path d="${d}"/>`);
    }
  }
  const svg =
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" ` +
    `fill="none" stroke="${OPTS.stroke}" stroke-width="${OPTS.strokeWidth}" ` +
    `stroke-linecap="round" stroke-linejoin="round">\n  ${paths.join('\n  ')}\n</svg>\n`;
  writeFileSync(join(outDir, `${name}.svg`), svg, 'utf8');
  n++;
}

console.log(`gen:doodles: ${n} rough.js doodles -> src/assets/doodles/`);
