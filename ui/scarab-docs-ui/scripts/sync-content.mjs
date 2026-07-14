// Sync canonical repo docs into the Starlight content collection — ADR-0040.
//
// The ADRs and CONTEXT.md live in ../../docs (the single source of truth) and
// have NO frontmatter (they open with `# NNNN. Title`). Starlight needs a
// frontmatter title, so we read the canonical files in place and emit collection
// entries with a title derived from the H1. Output goes to a GITIGNORED dir —
// nothing is committed twice; edit the originals in docs/, never these.
//
// Runs as `predev`/`prebuild` (npm), mirroring the openapi `gen` pattern.
import { readFileSync, writeFileSync, mkdirSync, readdirSync, rmSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '../../..'); // ui/scarab-docs-ui/scripts -> repo root
const docsAdr = join(repoRoot, 'docs', 'adr');
const contextMd = join(repoRoot, 'CONTEXT.md');
const outTech = resolve(here, '..', 'src', 'content', 'docs', 'tech');
const outAdr = join(outTech, 'adr');

/** First markdown H1 (`# ...`) → title, else fallback. */
function h1Title(body, fallback) {
  const m = body.match(/^#\s+(.+?)\s*$/m);
  return (m ? m[1] : fallback).replace(/"/g, '\\"');
}

/** Drop the leading H1 — Starlight renders the title from frontmatter, so the
 *  original `# NNNN. Title` would otherwise appear twice. */
function stripH1(body) {
  return body.replace(/^#\s+.+?\r?\n+/, '');
}

/** Emit a Starlight page: YAML frontmatter (title) + body (leading H1 removed). */
function emit(path, title, body, extra = '') {
  const fm = `---\ntitle: "${title}"\n${extra}---\n\n`;
  writeFileSync(path, fm + stripH1(body), 'utf8');
}

// Fresh output each run (it's generated + gitignored).
rmSync(outTech, { recursive: true, force: true });
mkdirSync(outAdr, { recursive: true });

// CONTEXT.md → tech/context.md
const ctx = readFileSync(contextMd, 'utf8');
emit(join(outTech, 'context.md'), h1Title(ctx, 'Context'), ctx, 'description: "The thesis, the durability contract, and the ubiquitous language."\n');

// docs/adr/*.md (skip the template) → tech/adr/*.md
let count = 0;
for (const f of readdirSync(docsAdr).sort()) {
  if (!f.endsWith('.md') || f.startsWith('_')) continue;
  const body = readFileSync(join(docsAdr, f), 'utf8');
  emit(join(outAdr, f), h1Title(body, f.replace(/\.md$/, '')), body);
  count++;
}

console.log(`sync-content: 1 context + ${count} ADRs -> src/content/docs/tech/`);
