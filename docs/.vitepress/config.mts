import { defineConfig } from 'vitepress'

// https://vitepress.dev/reference/site-config
export default defineConfig({
  title: 'EdgeZero',
  description: 'Production-ready toolkit for portable edge HTTP workloads',
  base: '/edgezero/',
  // `superpowers/` holds internal design docs (specs + plans) that are not
  // part of the published site. They sit in `docs/` so the doc tooling
  // (prettier, eslint) covers them, but VitePress should skip them: the
  // raw spec text contains literal `{{ … }}` interpolations inside inline
  // code that Vue's compiler would otherwise try to evaluate.
  srcExclude: ['superpowers/**'],
  themeConfig: {
    // https://vitepress.dev/reference/default-theme-config
    nav: [
      { text: 'Home', link: '/' },
      { text: 'Guide', link: '/guide/getting-started' },
      { text: 'Adapters', link: '/guide/adapters/overview' },
      { text: 'Reference', link: '/guide/configuration' },
    ],

    sidebar: [
      {
        text: 'Introduction',
        items: [
          { text: 'What is EdgeZero?', link: '/guide/what-is-edgezero' },
          { text: 'Getting Started', link: '/guide/getting-started' },
          { text: 'Architecture', link: '/guide/architecture' },
          { text: 'Roadmap', link: '/guide/roadmap' },
        ],
      },
      {
        text: 'Core Concepts',
        items: [
          { text: 'Routing', link: '/guide/routing' },
          { text: 'Handlers & Extractors', link: '/guide/handlers' },
          { text: 'Middleware', link: '/guide/middleware' },
          { text: 'Streaming', link: '/guide/streaming' },
          { text: 'Proxying', link: '/guide/proxying' },
        ],
      },
      {
        text: 'Adapters',
        items: [
          { text: 'Overview', link: '/guide/adapters/overview' },
          { text: 'Fastly Compute', link: '/guide/adapters/fastly' },
          { text: 'Cloudflare Workers', link: '/guide/adapters/cloudflare' },
          { text: 'Fermyon Spin', link: '/guide/adapters/spin' },
          { text: 'Axum (Native)', link: '/guide/adapters/axum' },
        ],
      },
      {
        text: 'Reference',
        items: [
          {
            text: 'Configuration (edgezero.toml)',
            link: '/guide/configuration',
          },
          { text: 'CLI Reference', link: '/guide/cli-reference' },
          { text: 'CLI Walkthrough', link: '/guide/cli-walkthrough' },
          {
            text: 'Deploying from GitHub Actions',
            link: '/guide/deploy-github-actions',
          },
          {
            text: 'Manifest Store Migration',
            link: '/guide/manifest-store-migration',
          },
          {
            text: 'Blob App-Config Migration',
            link: '/guide/blob-app-config-migration',
          },
        ],
      },
    ],

    socialLinks: [
      { icon: 'github', link: 'https://github.com/stackpop/edgezero' },
    ],

    footer: {
      message: 'Released under the Apache License 2.0.',
      copyright: 'Copyright 2025-present Stackpop',
    },
  },
})
