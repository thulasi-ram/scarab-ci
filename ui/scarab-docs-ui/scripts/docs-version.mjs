// Docs versioning — which release line a tag publishes, and the site-wide
// manifest the version dropdown reads.
//
// The site is one build per release LINE (vX.Y), accumulated on the
// `gh-pages` branch by .github/workflows/docs.yml:
//
//   /scarab-ci/            the latest stable line (rebuilt at base /scarab-ci/)
//   /scarab-ci/v0.2/       every line, latest included, at its own base
//   /scarab-ci/versions.json   { latest, versions: [{ version, tag }] }
//
// A patch tag replaces its line's folder; it only touches the root when it is
// the highest stable tag overall, so a fix to an old line (or an rc) can never
// take over "latest". The manifest is read at page load, not baked in, because
// old lines are never rebuilt but must still list newer ones.
//
// CLI (used by docs.yml; both print KEY=VALUE lines for $GITHUB_OUTPUT):
//   node scripts/docs-version.mjs plan <tag>        (all tags on stdin, one per line)
//   node scripts/docs-version.mjs manifest <file> <tag> <latest:true|false>
import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const STABLE = /^v(\d+)\.(\d+)\.(\d+)$/;

/** `v0.2.1` → [0, 2, 1]; null for anything that is not a stable release tag. */
export function parseStable(tag) {
  const m = STABLE.exec(tag.trim());
  return m ? m.slice(1, 4).map(Number) : null;
}

function cmp(a, b) {
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    if ((a[i] ?? 0) !== (b[i] ?? 0)) return (a[i] ?? 0) - (b[i] ?? 0);
  }
  return 0;
}

/**
 * What publishing `tag` means, given every tag in the repo.
 * - `publish: false` for a non-stable tag (rc, beta, ...) or for a patch that
 *   is not the newest in its line — re-running an old patch must not roll the
 *   line's folder back.
 * - `latest: true` only for the highest stable tag overall.
 */
export function plan(tag, allTags) {
  const v = parseStable(tag);
  if (!v) return { publish: false, reason: `${tag} is not a stable vX.Y.Z tag` };
  const stable = allTags.map(parseStable).filter(Boolean);
  const version = `v${v[0]}.${v[1]}`;
  const newerInLine = stable.some((s) => s[0] === v[0] && s[1] === v[1] && cmp(s, v) > 0);
  if (newerInLine) {
    return { publish: false, reason: `${version} already has a newer patch than ${tag}` };
  }
  const latest = !stable.some((s) => cmp(s, v) > 0);
  return { publish: true, version, latest };
}

/** The manifest after publishing `tag`: its line upserted, sorted newest first. */
export function updateManifest(manifest, tag, latest) {
  const v = parseStable(tag);
  if (!v) throw new Error(`not a stable tag: ${tag}`);
  const version = `v${v[0]}.${v[1]}`;
  const versions = (manifest?.versions ?? []).filter((e) => e.version !== version);
  versions.push({ version, tag });
  versions.sort((a, b) => cmp(parseStable(b.tag), parseStable(a.tag)));
  return { latest: latest ? version : (manifest?.latest ?? version), versions };
}

function main([cmd, ...args]) {
  if (cmd === 'plan') {
    const tags = readFileSync(0, 'utf8').split('\n').filter(Boolean);
    const p = plan(args[0], tags);
    for (const [k, val] of Object.entries(p)) console.log(`${k}=${val}`);
  } else if (cmd === 'manifest') {
    const [file, tag, latest] = args;
    const current = existsSync(file) ? JSON.parse(readFileSync(file, 'utf8')) : null;
    const next = updateManifest(current, tag, latest === 'true');
    writeFileSync(file, JSON.stringify(next, null, 2) + '\n');
    console.log(`latest=${next.latest}`);
  } else {
    console.error('usage: docs-version.mjs plan <tag> | manifest <file> <tag> <latest>');
    process.exit(2);
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main(process.argv.slice(2));
