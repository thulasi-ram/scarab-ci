// `node --test scripts/*.test.mjs` — run by docs.yml before it publishes anything.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { plan, updateManifest } from './docs-version.mjs';
import { rewriteBaseLinks } from './base-links.mjs';

const TAGS = ['v0.1.0', 'v0.1.1', 'v0.2.0', 'v0.3.0-rc.1'];

test('the highest stable tag publishes its line as latest', () => {
  assert.deepEqual(plan('v0.2.0', TAGS), { publish: true, version: 'v0.2', latest: true });
});

test('a patch to an older line publishes that line but never takes the root', () => {
  const tags = [...TAGS, 'v0.1.2'];
  assert.deepEqual(plan('v0.1.2', tags), { publish: true, version: 'v0.1', latest: false });
});

test('an rc, or an old patch re-run, publishes nothing', () => {
  assert.equal(plan('v0.3.0-rc.1', TAGS).publish, false);
  assert.equal(plan('v0.1.0', TAGS).publish, false);
});

test('the manifest upserts the line, sorts newest first, and keeps latest on an old-line patch', () => {
  let m = updateManifest(null, 'v0.2.0', true);
  m = updateManifest(m, 'v0.10.0', true);
  m = updateManifest(m, 'v0.2.1', false);
  assert.deepEqual(m, {
    latest: 'v0.10',
    versions: [
      { version: 'v0.10', tag: 'v0.10.0' },
      { version: 'v0.2', tag: 'v0.2.1' },
    ],
  });
});

test('hand-written root links move to the build base; generated ones are left alone', () => {
  const html =
    '<a href="/scarab-ci/usage/secrets/">x</a> <a href="/scarab-ci/">home</a> ' +
    '<script src="/scarab-ci/v0.2/_astro/a.js"></script> ' +
    '<a href="https://github.com/thulasi-ram/scarab-ci/issues">gh</a> ' +
    '<code>href=&quot;/scarab-ci/x&quot;</code>';
  assert.equal(
    rewriteBaseLinks(html, '/scarab-ci/', '/scarab-ci/v0.2/'),
    '<a href="/scarab-ci/v0.2/usage/secrets/">x</a> <a href="/scarab-ci/v0.2/">home</a> ' +
      '<script src="/scarab-ci/v0.2/_astro/a.js"></script> ' +
      '<a href="https://github.com/thulasi-ram/scarab-ci/issues">gh</a> ' +
      '<code>href=&quot;/scarab-ci/x&quot;</code>',
  );
  assert.equal(rewriteBaseLinks(html, '/scarab-ci/', '/scarab-ci/'), html);
});
