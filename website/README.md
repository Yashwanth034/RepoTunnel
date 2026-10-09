# RepoTunnel Website

Website checkpoint: **1.2.0** (see `WEBSITE_VERSION`). This version preserves the complete guides, provider screenshots and UI corrections after the saved 1.1.0 checkpoint. Website versions are independent of the RepoTunnel desktop release versions.

The independent public website for [RepoTunnel](https://github.com/Yashwanth034/RepoTunnel). It explains installation, controlled MCP access, real workflows, security boundaries, release downloads and common problems.

**This repository is not the RepoTunnel desktop application.** It contains only the static website and does not modify, install or bundle the desktop application.

## Stack

- Astro 5 + TypeScript
- Content source: `src/data/site-content.json`
- Lightweight custom CSS
- Pagefind static production search, with locally generated fallback search data
- Cloudflare Pages or any static file host

No accounts, backend, database, paid CMS or tracking SDK.

## Run locally

Requires Node.js 20.3+ and npm:

```bash
npm install
npm run dev
```

Open the localhost URL shown by Astro. The development server defaults to port 4321 unless another port is specified or already occupied.

## Validate and build

```bash
npm run check
```

This command generates the current search index, builds all static pages, builds Pagefind search and runs the offline HTML/link/release-URL configuration validator. The verifier also checks page metadata, structured data, duplicate IDs, image alternatives and search URLs.

Check Astro types and the saved-theme startup behavior after building:

```bash
./node_modules/.bin/astro check
node scripts/verify_theme.mjs
```

Optional browser QA requires Python 3, the `websockets` package, and a managed Chrome tab already showing `http://127.0.0.1:3010/`. Use that browser's debug port and the website tab's ID:

```bash
python3 scripts/verify_browser.py --port CHROME_DEBUG_PORT --target WEBSITE_TAB_ID
```

The browser check visits all generated pages at 320, 375, 640, 768, 1024, 1440 and 1920 pixels. It checks page overflow, clipped and overlapping controls, repeated primary actions, copy-toolbar alignment and arrow-free labels, then tests clipboard success/fallback, repeated copy clicks, search, keyboard dismissal, mobile menus, installation tabs, saved themes and reduced motion. It restores the original theme and viewport when finished.

To smoke-test every published route against a running local server:

```bash
npm run preview -- --host 127.0.0.1 --port 3010
```

In another terminal:

```bash
SITE_PREVIEW_ORIGIN=http://127.0.0.1:3010 node scripts/smoke_live.mjs
```

This performs live HTTP checks for all indexed pages. Real cross-browser/mobile visual review and external release-download availability still require separate verification.

For a production build, use the **actual** public HTTPS origin:

```bash
PUBLIC_SITE_URL=https://YOUR_ACTUAL_DOMAIN npm run build
python3 scripts/verify_static.py
```

Replace the value with the real provider URL or custom domain, not an example placeholder. If the domain is not known yet, do not publish an indexable website build without setting it.

Output: `dist/`. When `PUBLIC_SITE_URL` is set, Astro generates `sitemap-index.xml` and `sitemap-0.xml`, and the build writes the correct sitemap directive to `robots.txt`.

To preview the production build:

```bash
npm run preview
```

## Website sections

- Home — controlled local AI access and architecture
- Product — capability descriptions and safe setup links
- Solutions — practical, problem-oriented explanations
- Install & Downloads — OS packages and release information
- Docs — all guides, navigation and search
- Security, Changelog, Community and Privacy

Source information is based on the approved RepoTunnel code/documentation. This site uses a **labeled architecture diagram**, not a fabricated app screenshot. Current feature limitations are documented instead of hidden.

## Design references

The design review covered [Posts](https://posts.design/), [Recent](https://recent.design/), [Siteinspire](https://www.siteinspire.com/), [Collect UI](https://collectui.com/), [Designspo](https://designspo.com/), [Land-book](https://land-book.com/), [Sent](https://www.sent.dm/en), [Best AI Builder](https://bestaibuilder.website/), [Modulify templates](https://modulify.ai/templates) and [Modulify Logo Maker](https://logomaker.modulify.ai/).

Sent provides the primary developer-focused reference: compact headline and actions, thin outlined surfaces and a restrained request-path diagram. The product copy and capability names remain grounded in RepoTunnel's existing source documentation.

## Content maintenance

Edit `src/data/site-content.json`, the Astro pages, and relevant guide text. Search data is regenerated automatically on `npm run dev` and `npm run build`.

Use **Astro as the sole build system**. Do not run the removed legacy Python site generators: they cannot reproduce the current page components, design system or search implementation.

Keep downloads and changelog tied to actual official GitHub releases. The static validator checks URL configuration and internal links; it **does not** prove third-party GitHub assets are reachable at build time.

## Deployment and indexing

Read [DEPLOYMENT.md](DEPLOYMENT.md) for Cloudflare Pages, URL configuration, Google Search Console, Bing Webmaster Tools and privacy considerations.

Read [CONTENT_SOURCES.md](CONTENT_SOURCES.md) for the project source map.
