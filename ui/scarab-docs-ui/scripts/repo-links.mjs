// Point the repo-relative links in synced docs at pages that exist.
//
// CONTEXT.md and the ADRs are written to be read on GitHub, so they link by
// repo path: `docs/adr/0001-x.md` from CONTEXT.md, `0005-y.md` or
// `../../CONTEXT.md` from an ADR. Copied into the site as they are, a browser
// resolves those against the page URL and lands on a 404
// (`/tech/context/docs/adr/0001-x.md`). This rewrites each one, resolved from
// the file's own place in the repo:
//
// - an ADR → its page, `/scarab-ci/tech/adr/<name>/` (`docs/adr/` alone → the
//   ADR index);
// - CONTEXT.md → `/scarab-ci/tech/context/`;
// - any other repo file → the file on GitHub.
//
// The `/scarab-ci/` base is the canonical one; `base-links.mjs` moves it for a
// versioned build. Links inside code (fenced blocks, `inline code`) and links
// with a scheme or only an anchor are left alone.
import { posix } from 'node:path';

const BASE = '/scarab-ci/';
const REPO = 'https://github.com/thulasi-ram/scarab-ci/blob/main/';

/** Where a repo path is published, or null when the site has no page for it. */
function sitePath(repoPath) {
  if (repoPath === 'CONTEXT.md') return `${BASE}tech/context/`;
  if (repoPath === 'docs/adr' || repoPath === 'docs/adr/README.md') return `${BASE}tech/adr/readme/`;
  const adr = /^docs\/adr\/([^/_][^/]*)\.md$/.exec(repoPath);
  if (adr) return `${BASE}tech/adr/${adr[1].toLowerCase()}/`;
  return null;
}

/** One link target, written relative to `fromDir` (a repo-relative dir).
 *  `exists(repoPath)`, when given, is asked about every repo path a link names;
 *  one it says is missing is pushed onto `dead`. */
export function rewriteTarget(target, fromDir, { exists, dead } = {}) {
  if (/^([a-z][a-z0-9+.-]*:|#|\/)/i.test(target)) return target;
  const hash = target.indexOf('#');
  const path = hash === -1 ? target : target.slice(0, hash);
  const anchor = hash === -1 ? '' : target.slice(hash);
  const repoPath = posix.normalize(posix.join(fromDir, path)).replace(/\/$/, '');
  if (repoPath.startsWith('..')) return target;
  if (exists && !exists(repoPath)) dead?.push(repoPath);
  return (sitePath(repoPath) ?? REPO + repoPath) + anchor;
}

/** Rewrite every markdown link in `body`, a file that lives in `fromDir`. */
export function rewriteRepoLinks(body, fromDir, check = {}) {
  // Split on fenced blocks and inline code; odd entries are code, left as is.
  return body
    .split(/(```[\s\S]*?```|`[^`\n]*`)/)
    .map((part, i) =>
      i % 2 === 1
        ? part
        : part.replace(
            /(\]\()([^)\s]+)(\s+"[^"]*")?\)/g,
            (_, open, target, title = '') => `${open}${rewriteTarget(target, fromDir, check)}${title})`,
          ),
    )
    .join('');
}
