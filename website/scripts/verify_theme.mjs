import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import assert from 'node:assert/strict';
const html = readFileSync(new URL('../dist/index.html', import.meta.url), 'utf8');
const bootstrapMatch = [...html.matchAll(/<script\b[^>]*>([\s\S]*?)<\/script>/g)]
  .find(match => match[1].includes('repotunnel-theme'));
assert.ok(bootstrapMatch, 'The built page must include a theme bootstrap');
const bodyStart = html.search(/<body\b/i);
const firstExternalScript = html.search(/<script\b[^>]*\bsrc\s*=/i);
assert.ok(bodyStart >= 0 && bootstrapMatch.index < bodyStart, 'Theme must initialize before body content can paint');
assert.ok(firstExternalScript < 0 || bootstrapMatch.index < firstExternalScript, 'Theme must initialize before external page scripts');
const bootstrap = bootstrapMatch[1];
for (const [stored, expected] of [['dark', 'dark'], ['light', 'light'], ['invalid', 'light'], [null, 'light']]) {
  const document = { documentElement: { dataset: {} } };
  runInNewContext(bootstrap, { document, localStorage: { getItem: () => stored } });
  assert.equal(document.documentElement.dataset.theme, expected, 'Stored theme must apply before external page scripts');
}
const document = { documentElement: { dataset: {} } };
runInNewContext(bootstrap, { document, localStorage: { getItem: () => { throw new Error('Storage unavailable'); } } });
assert.equal(document.documentElement.dataset.theme, 'light', 'Blocked storage must preserve a readable default');
process.stdout.write('PASS: early theme bootstrap handles all five storage scenarios.\n');
