# Deployment and search indexing

## Hosting

The website is fully static and lives inside the **same `Yashwanth034/RepoTunnel` Git repository** as the desktop application and continuation extension, under `website/`. The current public site is https://repotunnel.pages.dev/. Cloudflare Pages builds this static site separately; it must not build, install, or publish desktop binaries.

1. In Cloudflare Pages, connect the **existing** `Yashwanth034/RepoTunnel` repository and use the `main` production branch. Do not create or split out a second website repository.
2. Set the Pages **root directory** to `website` (relative to the repository root), **build command** to `npm run build`, and **build output directory** to `dist` (relative to `website`). Install dependencies with the locked npm dependencies supported by your Pages Node runtime.
3. Set `PUBLIC_SITE_URL=https://repotunnel.pages.dev` as a production environment variable while that remains the canonical domain. Use an appropriate supported Node.js version (Node.js 22 or later with the current Astro toolchain).
4. Push an approved website-only change to `main` or explicitly trigger a Pages deployment; confirm the resulting build succeeds and inspect the actual production pages.
5. If you later add a custom domain, configure and verify DNS/redirects, update `PUBLIC_SITE_URL` to that actual canonical HTTPS origin, and rebuild.

Do not set the canonical origin to an invented address. Cloudflare Pages builds fail explicitly if `PUBLIC_SITE_URL` is missing, to prevent launching without canonical and sitemap metadata.

The website's `public/_headers` contains additional browser security headers.

## Search indexing

The production build contains a sitemap index at:

```text
https://repotunnel.pages.dev/sitemap-index.xml
```

and a generated sitemap file at `/sitemap-0.xml`. `robots.txt` references the sitemap index automatically.

**Google Search Console**

1. Add and verify your public website property.
2. Submit `/sitemap-index.xml` under Sitemaps.
3. Inspect the home page, installation page, product pages, solutions pages and key setup guides.
4. Request indexing for important pages as appropriate. Monitor indexed pages, crawl errors, queries, impressions and clicks.

**Bing Webmaster Tools**

1. Verify the same live website.
2. Submit its sitemap index.
3. Monitor indexing and discovered queries.

A sitemap and technical SEO make the site discoverable; they do **not** guarantee Google rankings, any particular placement, or immediate inclusion.

## Authority and update workflow

The existing RepoTunnel GitHub repository is the source of truth for the desktop code, `website/`, the continuation extension, release assets, and security policy. Its repository About URL already links to the deployed website. Website-only changes belong under `website/`; never stage or publish unfinished desktop changes alongside site updates.

The current public desktop release is **v0.4.2**. On every desktop release, update the website's release metadata, platform installers, download guides, and changelog together. Validate the configured asset names against GitHub using `npm run verify:release-assets` when online. This fetches release metadata only; it does not download installers. The offline `npm run check` verifies internal links, search, and the expected release URL structure but cannot prove remote files are available.

## Privacy

The website has no account system, analytics SDK, marketing tracking or third-party font dependencies. Hosting/CDN providers may still log standard request metadata. See `/privacy/`.

## Release checklist

- Check copy and installation steps against current RepoTunnel capabilities and release notes.
- Run `npm run check`.
- Visually inspect home, docs, install, security, downloads and both themes on desktop and mobile.
- Test search, theme, tab switching, keyboard navigation and code copy.
- Run `npm run verify:release-assets` against the live GitHub Release to confirm every configured OS installer and the SHA-256 checksum file exist.
- Build with the real public `PUBLIC_SITE_URL`.
- Confirm the sitemap, robots.txt, HTTPS, canonical metadata and OpenGraph preview work in production.
- Submit the verified site to search engines.

The public site currently exists at https://repotunnel.pages.dev/; independently verify the deployed version, sitemap, canonical URLs, and downloads after each publishing change. Search engine submissions or indexing cannot be assumed merely because the sitemap is reachable.
