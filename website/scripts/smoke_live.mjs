import { readFileSync } from 'node:fs';

const origin = (process.env.SITE_PREVIEW_ORIGIN || 'http://127.0.0.1:4321').replace(/\/$/, '');
const index = JSON.parse(readFileSync(new URL('../public/search-index.json', import.meta.url), 'utf8'));
const urls = [...new Set(index.map(({ url }) => url))];
const failures = [];
const batchSize = 8;

for (let start = 0; start < urls.length; start += batchSize) {
  await Promise.all(urls.slice(start, start + batchSize).map(async (path) => {
    try {
      const response = await fetch(origin + path, { signal: AbortSignal.timeout(8000) });
      const html = await response.text();
      if (!response.ok) failures.push(`${path}: HTTP ${response.status}`);
      else if (!html.includes('<main ') || !html.includes('<h1')) failures.push(`${path}: incomplete page shell`);
      else if (!html.includes('application/ld+json')) failures.push(`${path}: missing structured data`);
      else if (!html.includes('name="description"')) failures.push(`${path}: missing description`);
    } catch (error) {
      failures.push(`${path}: ${error.message}`);
    }
  }));
}

console.log(`Live route smoke test: ${urls.length} routes; ${failures.length} failures`);
if (failures.length) {
  for (const failure of failures) console.error(failure);
  process.exitCode = 1;
}
