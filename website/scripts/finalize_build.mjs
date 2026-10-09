import { writeFileSync } from 'node:fs';

const site = process.env.PUBLIC_SITE_URL?.replace(/\/$/, '');
const lines = ['User-agent: *', 'Allow: /'];
if (site) lines.push('', `Sitemap: ${site}/sitemap-index.xml`);
writeFileSync(new URL('../dist/robots.txt', import.meta.url), lines.join('\n') + '\n');
console.log(site ? 'Production robots.txt points to the generated sitemap index' : 'Local robots.txt (no invented production sitemap)');
