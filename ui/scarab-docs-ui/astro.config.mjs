// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightOpenAPI, { openAPISidebarGroups } from 'starlight-openapi';
import lucode from 'lucode-starlight';

// Scarab documentation site — ADR-0040.
// - Operator-primary IA, published honestly (stubs are explicit).
// - Sources the API reference from the repo's canonical ../../openapi.json in
//   place (starlight-openapi, build time). ADRs + CONTEXT.md are synced from
//   ../../docs by scripts/sync-content.mjs (a gitignored prebuild step — the
//   canonical source stays in docs/; nothing is committed twice).
// - GitHub Pages project subpath: base '/scarab-ci/'.
export default defineConfig({
  site: 'https://thulasi-ram.github.io',
  base: '/scarab-ci/',
  integrations: [
    starlight({
      title: 'Scarab',
      tagline: 'Your pipeline is a workflow that survives crashes.',
      logo: { src: './src/assets/logo.svg', alt: 'Scarab' },
      customCss: [
        // Geist is the face shadcn/ui (and thus the Lucode base) is drawn with —
        // JetBrains Mono stays for machine tokens (ids, logs, durations).
        '@fontsource-variable/geist/index.css',
        '@fontsource/jetbrains-mono/400.css',
        '@fontsource/jetbrains-mono/500.css',
        './src/styles/scarab.css',
      ],
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/thulasi-ram/scarab-ci' },
      ],
      plugins: [
        // Lucode: shadcn/ui-inspired Starlight theme — light-default, restrained
        // typography, a subtle active-nav rail (no solid fill). Scarab identity
        // (emerald links, copper fence-bracket edge) is layered in scarab.css.
        lucode({
          // Links are relative to the site root; Starlight prepends `base`
          // (/scarab-ci/) itself — including it here would double it.
          navLinks: [
            { label: 'Get Started', link: '/get-started/run-locally/' },
            { label: 'Guides', link: '/guides/authoring/' },
            { label: 'Tech', link: '/tech/context/' },
          ],
          footerText:
            'Scarab — durable-execution, Kubernetes-native CI. Themed with [Lucode Starlight](https://github.com/lucas-labs/lucode-starlight-theme).',
        }),
        // API reference generated from the canonical spec, in place.
        starlightOpenAPI([
          { base: 'reference/api', label: 'API', schema: '../../openapi.json' },
        ]),
      ],
      sidebar: [
        {
          label: 'Get Started',
          items: [
            { label: 'Run locally', slug: 'get-started/run-locally' },
            { label: 'Deploy with Helm', slug: 'get-started/deploy-helm', badge: { text: 'stub', variant: 'caution' } },
          ],
        },
        {
          label: 'Guides',
          items: [{ label: 'Pipeline authoring', slug: 'guides/authoring', badge: { text: 'wip', variant: 'note' } }],
        },
        {
          label: 'Configure',
          items: [{ label: 'Configuration reference', slug: 'configure/reference', badge: { text: 'wip', variant: 'note' } }],
        },
        {
          label: 'Reference',
          items: [...openAPISidebarGroups],
        },
        {
          label: 'Tech',
          items: [
            { label: 'Thesis & language (CONTEXT)', slug: 'tech/context' },
            { label: 'Architecture decisions', items: [{ autogenerate: { directory: 'tech/adr' } }] },
          ],
        },
      ],
    }),
  ],
});
