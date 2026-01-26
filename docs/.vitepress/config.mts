import { defineConfig } from 'vitepress'

// https://vitepress.dev/reference/site-config
export default defineConfig({
  title: "EdgeZero",
  description: "Production-ready toolkit for portable edge HTTP workloads",
  base: "/edgezero/",
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
          { text: 'Architecture', link: '/guide/architecture' }
        ]
      },
      {
        text: 'Core Concepts',
        items: [
          { text: 'Routing', link: '/guide/routing' },
          { text: 'Handlers & Extractors', link: '/guide/handlers' },
          { text: 'Middleware', link: '/guide/middleware' },
          { text: 'Streaming', link: '/guide/streaming' },
          { text: 'Proxying', link: '/guide/proxying' }
        ]
      },
      {
        text: 'Adapters',
        items: [
          { text: 'Overview', link: '/guide/adapters/overview' },
          { text: 'Fastly Compute', link: '/guide/adapters/fastly' },
          { text: 'Cloudflare Workers', link: '/guide/adapters/cloudflare' },
          { text: 'Axum (Native)', link: '/guide/adapters/axum' }
        ]
      },
      {
        text: 'Reference',
        items: [
          { text: 'Configuration (edgezero.toml)', link: '/guide/configuration' },
          { text: 'CLI Reference', link: '/guide/cli-reference' }
        ]
      }
    ],

    socialLinks: [
      { icon: 'github', link: 'https://github.com/stackpop/edgezero' }
    ],

    footer: {
      message: 'Released under the MIT License.',
      copyright: 'Copyright 2024-present Stackpop'
    }
  }
})
