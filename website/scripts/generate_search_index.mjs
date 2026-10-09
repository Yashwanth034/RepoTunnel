import { readFileSync, writeFileSync } from 'node:fs';

const data = JSON.parse(readFileSync(new URL('../src/data/site-content.json', import.meta.url), 'utf8'));
const entries = [];
const add = (url, title, description, searchText) => {
  entries.push({ url, title, description, searchText: searchText.replace(/\s+/g, ' ').trim() });
};
const pageText = (page) => [
  page.eyebrow, ...(page.keywords || []),
  ...(page.sections || []).flatMap(section => [
    section.title, ...(section.paragraphs || []), ...(section.bullets || []),
    section.note || '', section.code || '', ...(section.steps || []),
    ...(section.links || []).map(link => link.label), section.image?.caption || ''
  ])
].filter(Boolean).join(' ');

add('/', data.site.name, data.site.description, 'local-first workspace MCP ChatGPT git browser phone AI development gateway');
add('/docs/', 'Documentation', 'Setup, security and workflows', data.docs.map(doc => doc.title).join(' '));
for (const [kind, pages] of [
  ['product', data.products],
  ['solutions', data.solutions],
  ['docs', data.docs]
]) for (const page of pages) add(`/${kind}/${page.slug}/`, page.title, page.description, pageText(page));
for (const [url, title, description, keywords] of [
  ['/product/', 'Product capabilities', 'Explore all RepoTunnel capabilities', 'MCP terminal git browser phone ai workspace team continuity video'],
  ['/solutions/', 'Solutions', 'Practical RepoTunnel use cases', 'ChatGPT local files Android AI controlled automation'],
  ['/install/', 'Installation', 'Install RepoTunnel on Linux Windows or macOS', 'deb rpm AppImage dmg exe msi'],
  ['/downloads/', 'Downloads', 'Official release packages for all supported platforms', 'Linux Windows macOS'],
  ['/security/', 'Security', 'RepoTunnel permission and isolation model', 'OAuth protected paths sandbox git device'],
  ['/changelog/', 'Changelog', 'RepoTunnel release history', 'new versions fixes releases'],
  ['/changelog/0.4.1/', 'RepoTunnel v0.4.1', 'RepoTunnel release notes', 'phone AI workspace continuity team video'],
  ['/community/', 'Community', 'Issue reports, feature requests and contributions', 'github issues contributing'],
  ['/privacy/', 'Website privacy', 'What the static RepoTunnel site collects', 'privacy localstorage cookies analytics']
]) add(url, title, description, keywords);
if (new Set(entries.map(x => x.url)).size !== entries.length) throw new Error('Duplicate search URL');
writeFileSync(new URL('../public/search-index.json', import.meta.url), JSON.stringify(entries, null, 2) + '\n');
console.log(`Search: ${entries.length} entries generated`);
