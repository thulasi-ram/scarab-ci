import { test } from 'node:test';
import assert from 'node:assert/strict';
import { rewriteRepoLinks, rewriteTarget } from './repo-links.mjs';

test('CONTEXT.md links to ADRs land on their pages', () => {
  assert.equal(
    rewriteTarget('docs/adr/0001-ci-as-durable-execution.md', '.'),
    '/scarab-ci/tech/adr/0001-ci-as-durable-execution/',
  );
  assert.equal(rewriteTarget('docs/adr/', '.'), '/scarab-ci/tech/adr/readme/');
});

test('an ADR linking a sibling ADR keeps the anchor', () => {
  assert.equal(
    rewriteTarget('0061-workspace-data-path.md#decision', 'docs/adr'),
    '/scarab-ci/tech/adr/0061-workspace-data-path/#decision',
  );
});

test('an ADR linking CONTEXT.md lands on the context page', () => {
  assert.equal(rewriteTarget('../../CONTEXT.md', 'docs/adr'), '/scarab-ci/tech/context/');
});

test('a repo file with no page goes to GitHub', () => {
  assert.equal(
    rewriteTarget('../positioning.md', 'docs/adr'),
    'https://github.com/thulasi-ram/scarab-ci/blob/main/docs/positioning.md',
  );
  assert.equal(
    rewriteTarget('docs/positioning.md', '.'),
    'https://github.com/thulasi-ram/scarab-ci/blob/main/docs/positioning.md',
  );
});

test('absolute, external and anchor-only links are left alone', () => {
  for (const t of ['https://example.com/a.md', '#section', '/scarab-ci/usage/secrets/', 'mailto:a@b.c']) {
    assert.equal(rewriteTarget(t, 'docs/adr'), t);
  }
});

test('links in code are left alone, links beside it are rewritten', () => {
  const body = [
    'See [ADR-0005](0005-tenancy.md "Tenancy") and `[x](0006-y.md)`.',
    '```md',
    '[kept](0007-z.md)',
    '```',
  ].join('\n');
  assert.equal(
    rewriteRepoLinks(body, 'docs/adr'),
    [
      'See [ADR-0005](/scarab-ci/tech/adr/0005-tenancy/ "Tenancy") and `[x](0006-y.md)`.',
      '```md',
      '[kept](0007-z.md)',
      '```',
    ].join('\n'),
  );
});

test('a link to a missing file is reported', () => {
  const dead = [];
  const exists = (p) => p !== 'docs/adr/0007-step-outputs.md';
  rewriteRepoLinks('[a](0007-step-outputs.md) [b](0008-step-contract.md)', 'docs/adr', { exists, dead });
  assert.deepEqual(dead, ['docs/adr/0007-step-outputs.md']);
});
