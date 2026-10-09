# Deployment and search indexing

## Hosting

The website is fully static. Cloudflare Pages is the intended zero-server setup; any host that serves static HTML and asset files works.

1. Create a separate website repository or upload the built `dist/` folder.
2. Configure the build command `npm run build` and output directory `dist`.
3. Set `PUBLIC_SITE_URL` to the **real** HTTPS origin you will serve, such as your Cloudflare-assigned Pages address.
4. Trigger a production build, publish its `dist/` directory and test HTTPS URLs.
5. If you later add a custom domain, update `PUBLIC_SITE_URL`, configure the DNS/redirects, and rebuild with the new canonical origin.

Do not set the canonical origin to an invented address. Cloudflare Pages builds fail explicitly if `PUBLIC_SITE_URL` is missing, to prevent launching without canonical and sitemap metadata.

The website's `public/_headers` contains additional browser security headers.

## Search indexing

The production build contains a sitemap index at:

```text
https://YOUR_REAL_SITE/sitemap-index.xml
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

The public desktop-app GitHub repository remains the source of truth for release assets, security policy and code. Once the website has a real public URL, an **independently authorized change** to the desktop repository may add the official website link to its repository About/README.

Review download URLs on every desktop release. The website currently documents v0.4.1 and must not call older versions the newest indefinitely; check the official GitHub release page before changing any release links. Build-time tests cannot verify live external assets when network access is unavailable.

## Privacy

The website has no account system, analytics SDK, marketing tracking or third-party font dependencies. Hosting/CDN providers may still log standard request metadata. See `/privacy/`.

## Release checklist

- Check copy and installation steps against current RepoTunnel capabilities and release notes.
- Run `npm run check`.
- Visually inspect home, docs, install, security, downloads and both themes on desktop and mobile.
- Test search, theme, tab switching, keyboard navigation and code copy.
- Verify actual GitHub downloads and checksum files through a normal network connection.
- Build with the real public `PUBLIC_SITE_URL`.
- Confirm the sitemap, robots.txt, HTTPS, canonical metadata and OpenGraph preview work in production.
- Submit the verified site to search engines.

Do not report the last two steps as complete until a public deployment exists.
