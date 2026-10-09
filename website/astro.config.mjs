import { defineConfig } from 'astro/config';
import sitemap from '@astrojs/sitemap';

const site = process.env.PUBLIC_SITE_URL || undefined;
if (site) {
  const url = new URL(site);
  if (url.protocol !== 'https:' && url.hostname !== 'localhost') {
    throw new Error('PUBLIC_SITE_URL must be HTTPS for a public deployment');
  }
  if (url.pathname !== '/' || url.search || url.hash) {
    throw new Error('PUBLIC_SITE_URL must be the site origin, without a path, query or fragment');
  }
}
if (process.env.CF_PAGES && !site) {
  throw new Error('Cloudflare Pages needs PUBLIC_SITE_URL set to the real public site origin for canonical tags and sitemap');
}

export default defineConfig({
  site,
  output: 'static',
  compressHTML: true,
  integrations: site ? [sitemap()] : [],
  build: { format: 'directory' },
  vite: {
    build: { cssMinify: true }
  }
});
