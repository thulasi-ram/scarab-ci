// Browser side of docs versioning: the site-wide manifest (see
// scripts/docs-version.mjs) and how to move between lines.

export interface Manifest {
  latest: string;
  versions: { version: string; tag: string }[];
}

let pending: Promise<Manifest | null> | undefined;

/** versions.json, fetched once per page; null when absent (local builds). */
export function loadManifest(root: string): Promise<Manifest | null> {
  pending ??= fetch(`${root}versions.json`, { cache: 'no-cache' })
    .then((r) => (r.ok ? (r.json() as Promise<Manifest>) : null))
    .catch(() => null);
  return pending;
}

/** The latest line is served from the root; every other line from its folder. */
export function baseOf(root: string, manifest: Manifest, version: string): string {
  return version === manifest.latest ? root : `${root}${version}/`;
}

/**
 * Go to the same page in another line, or to that line's home when the page
 * does not exist there (renamed, or newer than the line).
 */
export async function switchTo(root: string, base: string, manifest: Manifest, version: string) {
  const target = baseOf(root, manifest, version);
  const rest = location.pathname.startsWith(base) ? location.pathname.slice(base.length) : '';
  let dest = target;
  if (rest) {
    try {
      if ((await fetch(target + rest, { method: 'HEAD' })).ok) dest = target + rest;
    } catch {
      // Offline or blocked: the line's home is still a correct answer.
    }
  }
  location.assign(dest);
}
