// Offline doodle generator — ADR-0040 + docs/DESIGN.md §5.
//
// Takes clean Lucide line icons and re-draws them DOTTED: zero-length dashes
// with round caps turn every stroke into a run of round dots — the same
// dot-matrix language as the pixel display voice, the page dot-grid texture,
// and the ASCII beetle scenes. (The rough.js "hand-sketched" era is retired;
// sketchiness was a different accent than the dotted/pixel identity.)
//
// Output is a static SVG per motif in a committed assets dir; the site ships
// no generator code. Regenerate on demand (`npm run gen:doodles`) and commit —
// output is fully deterministic.
import { writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  Bug, GitBranch, GitCommitHorizontal, Container, Boxes, Workflow,
  Waypoints, Package, Terminal, KeyRound, ShieldCheck, Timer, Network,
} from 'lucide';

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, '..', 'src', 'assets', 'doodles');

// Canonical dotted-stroke settings (DESIGN.md §5). Copper ink; `stroke` is a
// literal hex — SVG presentation attributes can't resolve CSS custom props.
// dasharray "0 1.2" + round caps = round dots Ø strokeWidth at a 1.2-unit
// pitch — the same grain as the 10 Pixel display face.
const STROKE = '#c0873f';
const STROKE_WIDTH = 0.6;
const DASH = '0 1.2';

// The motif set (DESIGN.md §5). Name -> Lucide IconNode. `bug` is the house motif.
const MOTIFS = {
  bug: Bug, 'git-branch': GitBranch, 'git-commit': GitCommitHorizontal,
  container: Container, boxes: Boxes, workflow: Workflow, waypoints: Waypoints,
  package: Package, terminal: Terminal, 'key-round': KeyRound,
  'shield-check': ShieldCheck, timer: Timer, network: Network,
};

/** Serialize one Lucide primitive to an SVG element (attrs pass through). */
function primitive(tag, attrs) {
  const a = Object.entries(attrs)
    .filter(([k]) => k !== 'key')
    .map(([k, v]) => `${k}="${v}"`)
    .join(' ');
  return `<${tag} ${a}/>`;
}

mkdirSync(outDir, { recursive: true });
let n = 0;

for (const [name, iconNode] of Object.entries(MOTIFS)) {
  const els = iconNode.map(([tag, attrs]) => `  ${primitive(tag, attrs)}`);
  const svg =
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" ` +
    `fill="none" stroke="${STROKE}" stroke-width="${STROKE_WIDTH}" ` +
    `stroke-dasharray="${DASH}" stroke-linecap="round" stroke-linejoin="round">\n` +
    `${els.join('\n')}\n</svg>\n`;
  writeFileSync(join(outDir, `${name}.svg`), svg, 'utf8');
  n++;
}

console.log(`gen:doodles: ${n} dotted doodles -> src/assets/doodles/`);
