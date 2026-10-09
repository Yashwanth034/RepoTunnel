from __future__ import annotations
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import urlparse, unquote
import json
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
DIST = ROOT / 'dist'

class Parser(HTMLParser):
    def __init__(self):
        super().__init__()
        self.links=[]; self.ids=set(); self.duplicate_ids=set(); self.unlabeled_images=0; self.title=''; self._in_title=False; self.h1=0; self.desc=None
    def handle_starttag(self, tag, attrs):
        a=dict(attrs)
        if 'id' in a:
            if a['id'] in self.ids: self.duplicate_ids.add(a['id'])
            self.ids.add(a['id'])
        if tag=='img' and 'alt' not in a: self.unlabeled_images += 1
        if tag=='a' and a.get('href'): self.links.append(a['href'])
        if tag=='h1': self.h1 += 1
        if tag=='meta' and a.get('name')=='description': self.desc=a.get('content','')
        if tag=='title': self._in_title=True
    def handle_endtag(self, tag):
        if tag=='title': self._in_title=False
    def handle_data(self,data):
        if self._in_title: self.title += data

def resolve_internal(current_file: Path, href: str):
    parsed=urlparse(href)
    if parsed.scheme or href.startswith('//') or href.startswith('mailto:') or href.startswith('tel:'):
        return None
    path=unquote(parsed.path)
    if not path:
        return current_file
    if path.startswith('/'):
        target=DIST/path.lstrip('/')
    else:
        target=current_file.parent/path
    if path.endswith('/'):
        target=target/'index.html'
    elif target.suffix=='':
        target=target/'index.html'
    return target.resolve()

errors=[]
html_files=list(DIST.rglob('*.html'))
if len(html_files) < 50: errors.append(f'Expected a complete site; found only {len(html_files)} HTML files')

for file in html_files:
    raw=file.read_text(encoding='utf-8')
    p=Parser(); p.feed(raw)
    rel=file.relative_to(DIST)
    if p.duplicate_ids: errors.append(f'{rel}: duplicate HTML IDs {sorted(p.duplicate_ids)}')
    if p.unlabeled_images: errors.append(f'{rel}: {p.unlabeled_images} images missing alt attributes')
    if not p.title.strip(): errors.append(f'{rel}: missing title')
    elif not 10 <= len(p.title.strip()) <= 65: errors.append(f'{rel}: title length {len(p.title.strip())} must be 10..65 characters')
    if not p.desc: errors.append(f'{rel}: missing meta description')
    elif not 50 <= len(p.desc.strip()) <= 180: errors.append(f'{rel}: meta description length {len(p.desc.strip())} must be 50..180 characters')
    if p.h1 != 1: errors.append(f'{rel}: expected exactly one h1, got {p.h1}')
    if '<html lang="en"' not in raw: errors.append(f'{rel}: missing html lang="en"')
    if 'name="viewport"' not in raw: errors.append(f'{rel}: missing viewport metadata')
    if 'id="main-content"' not in raw: errors.append(f'{rel}: missing main-content landmark')
    if 'class="skip-link"' not in raw: errors.append(f'{rel}: missing skip-to-content link')
    lower=raw.lower()
    if not lower.lstrip().startswith('<!doctype html>'): errors.append(f'{rel}: missing HTML5 doctype')
    for bad in ['lorem ipsum','todo:','coming soon','example.com','example.org']:
        if bad in lower: errors.append(f'{rel}: placeholder text found: {bad}')
    for href in p.links:
        parsed=urlparse(href)
        if href.startswith('#'):
            anchor=href[1:]
            if anchor and anchor not in p.ids: errors.append(f'{rel}: missing local anchor #{anchor}')
            continue
        target=resolve_internal(file,href)
        if target is not None and not target.exists():
            errors.append(f'{rel}: broken internal link {href} -> {target.relative_to(DIST) if str(target).startswith(str(DIST)) else target}')

idx=json.loads((DIST/'search-index.json').read_text())
for item in idx:
    target=resolve_internal(DIST/'index.html',item['url'])
    if target and not target.exists(): errors.append(f'search index broken URL: {item["url"]}')

expected_downloads = [
    'RepoTunnel_0.4.1_amd64.deb','RepoTunnel-0.4.1-1.x86_64.rpm','RepoTunnel_0.4.1_amd64.AppImage',
    'RepoTunnel_0.4.1_x64-setup.exe','RepoTunnel_0.4.1_x64_en-US.msi','RepoTunnel_0.4.1_aarch64.dmg','RepoTunnel_0.4.1_x64.dmg'
]
combined='\n'.join(f.read_text(encoding='utf-8') for f in [DIST/'install/index.html',DIST/'downloads/index.html'])
for name in expected_downloads:
    if name not in combined: errors.append(f'missing official release asset link: {name}')

# Validate discoverability, structured metadata and production assets, not just visible anchors.
indexed_urls = [item.get('url') for item in idx]
if len(indexed_urls) != len(set(indexed_urls)):
    errors.append('search index contains duplicate URLs')

expected_urls = set()
seen_titles = {}
for file in html_files:
    rel = file.relative_to(DIST).as_posix()
    if rel == '404.html':
        raw = file.read_text(encoding='utf-8')
        if 'name="robots" content="noindex' not in raw:
            errors.append('404 page must be noindex')
        if 'data-pagefind-body' in raw:
            errors.append('404 page must be excluded from Pagefind indexing')
        continue
    raw = file.read_text(encoding='utf-8')
    if 'data-pagefind-body' not in raw:
        errors.append(f'{rel}: normal page missing data-pagefind-body')
    url = '/' if rel == 'index.html' else '/' + rel.removesuffix('/index.html') + '/'
    expected_urls.add(url)
    raw = file.read_text(encoding='utf-8')
    page = Parser(); page.feed(raw)
    if page.title in seen_titles:
        errors.append(f'duplicate page title: {rel} and {seen_titles[page.title]}')
    seen_titles[page.title] = rel
    for metadata in ['property="og:title"', 'property="og:description"', 'name="twitter:card"']:
        if metadata not in raw:
            errors.append(f'{rel}: missing {metadata}')
    match = re.search(r'<script[^>]*type="application/ld\+json"[^>]*>(.*?)</script>', raw, re.S)
    if not match:
        errors.append(f'{rel}: missing structured data')
    else:
        try:
            schema = json.loads(match.group(1))
            if schema.get('@type') not in ('WebPage','TechArticle','SoftwareApplication'):
                errors.append(f'{rel}: unexpected structured data type')
        except ValueError:
            errors.append(f'{rel}: invalid JSON-LD')

if set(indexed_urls) != expected_urls:
    errors.append(f'search index mismatch: missing={sorted(expected_urls-set(indexed_urls))[:8]}, excess={sorted(set(indexed_urls)-expected_urls)[:8]}')

for asset in ['favicon.svg', 'favicon.ico', 'site.js', 'social-card.png', 'search-index.json', 'pagefind/pagefind.js', '_headers', '_redirects']:
    if not (DIST / asset).is_file():
        errors.append(f'missing production asset: {asset}')

headers_file = DIST / '_headers'
if headers_file.is_file():
    headers_text = headers_file.read_text(encoding='utf-8')
    if '/_astro/*' not in headers_text or 'max-age=31536000, immutable' not in headers_text:
        errors.append('_headers must give fingerprinted /_astro assets long-lived immutable caching')
    if '/assets/*' in headers_text:
        errors.append('_headers contains stale /assets/* cache rule; Astro emits fingerprinted assets under /_astro/')

redirects_file = DIST / '_redirects'
if redirects_file.is_file():
    for line in redirects_file.read_text(encoding='utf-8').splitlines():
        line = line.strip()
        if not line or line.startswith('#'):
            continue
        parts = line.split()
        if len(parts) < 2:
            errors.append(f'invalid redirect rule: {line}')
            continue
        destination = parts[1]
        if destination.startswith('/'):
            target = resolve_internal(DIST / 'index.html', destination)
            if target is not None and not target.exists():
                errors.append(f'broken redirect target: {line}')

if (DIST / 'sitemap-index.xml').exists():
    try:
        import xml.etree.ElementTree as ET
        sitemap_index = ET.parse(DIST / 'sitemap-index.xml')
        sitemap_urls = ET.parse(DIST / 'sitemap-0.xml')
        locations = [n.text for n in sitemap_urls.iter() if n.tag.endswith('loc')]
        if len(locations) != len(expected_urls):
            errors.append(f'sitemap URL count mismatch: {len(locations)} vs {len(expected_urls)}')
        if any(location and '404' in location for location in locations):
            errors.append('sitemap indexes the 404 page')
        if 'Sitemap:' not in (DIST / 'robots.txt').read_text():
            errors.append('sitemap exists but robots.txt has no sitemap directive')
    except (OSError, ET.ParseError) as exc:
        errors.append(f'invalid sitemap XML: {exc}')

if errors:
    print('FAILED')
    for err in errors[:100]: print('-',err)
    print(f'{len(errors)} error(s)')
    sys.exit(1)
print(f'PASS: {len(html_files)} HTML pages, {len(idx)} search entries, internal links/anchors and release link configuration checked (external reachability not tested).')
