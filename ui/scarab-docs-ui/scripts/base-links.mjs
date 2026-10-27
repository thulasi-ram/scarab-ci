// Make hand-written `/scarab-ci/...` links follow the build's base.
//
// Content links to its own site as `/scarab-ci/...` (markdown links, raw
// `href=` in MDX, hero `link:` frontmatter). Astro prefixes `base` onto the
// links IT generates, but never onto these — so a build at /scarab-ci/v0.2/
// would quietly send every one of them to the latest docs at the root. This
// rewrites them in the built HTML, after the fact, which is the one place all
// three forms meet (Starlight's hero passes `link:` through untouched, so a
// markdown plugin would miss it).
//
// Anchored on the attribute: only an `href="` / `src="` VALUE that starts with
// the canonical base changes, and one already at the build's base (every link
// Astro generated) is left alone.
import { readdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const esc = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

export function rewriteBaseLinks(html, canonical, base) {
  if (base === canonical) return html;
  if (!base.startsWith(canonical)) {
    throw new Error(`base ${base} must live under ${canonical}`);
  }
  const rest = base.slice(canonical.length);
  const re = new RegExp(`(\\s(?:href|src)=["'])${esc(canonical)}(?!${esc(rest)})`, 'g');
  return html.replace(re, `$1${base}`);
}

async function* htmlFiles(dir) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* htmlFiles(path);
    else if (entry.name.endsWith('.html')) yield path;
  }
}

/** Astro integration: rewrite every built page once the build is done. */
export function baseLinks(canonical, base) {
  return {
    name: 'scarab-base-links',
    hooks: {
      'astro:build:done': async ({ dir, logger }) => {
        if (base === canonical) return;
        let changed = 0;
        for await (const file of htmlFiles(fileURLToPath(dir))) {
          const html = await readFile(file, 'utf8');
          const out = rewriteBaseLinks(html, canonical, base);
          if (out !== html) {
            await writeFile(file, out);
            changed++;
          }
        }
        logger.info(`rewrote ${canonical} links to ${base} in ${changed} page(s)`);
      },
    },
  };
}
