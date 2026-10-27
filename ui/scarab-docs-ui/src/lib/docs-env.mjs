// Which build this is — read once, by astro.config.mjs AND the components, so
// the base a page is built at and the version it claims cannot disagree.
// docs.yml sets both; a local `npm run dev` / `build` sets neither, which is
// the unversioned site at the canonical base with no version dropdown.

/** Where the site lives on GitHub Pages; also where versions.json sits. */
export const DOCS_ROOT = '/scarab-ci/';

/** `/scarab-ci/` for the latest line, `/scarab-ci/v0.2/` for a line's own copy. */
export const DOCS_BASE = process.env.SCARAB_DOCS_BASE || DOCS_ROOT;

/** The release line this build documents (`v0.2`), or '' for a local build. */
export const DOCS_VERSION = process.env.SCARAB_DOCS_VERSION || '';

if (!DOCS_BASE.startsWith(DOCS_ROOT) || !DOCS_BASE.endsWith('/')) {
  throw new Error(`SCARAB_DOCS_BASE=${DOCS_BASE} must be ${DOCS_ROOT} or a /-terminated path under it`);
}
