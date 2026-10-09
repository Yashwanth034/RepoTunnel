#!/usr/bin/env python3
from pathlib import Path
import json
import sys
import hashlib
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / 'src'
errors = []

astro_files = sorted(SRC.rglob('*.astro'))
for path in astro_files:
    text = path.read_text(encoding='utf-8')
    rel = path.relative_to(ROOT)
    if '<style' in text:
        errors.append(f'{rel}: inline/global style block found; keep presentation in src/styles')
    if 'style=' in text:
        errors.append(f'{rel}: inline style attribute found; keep presentation in src/styles')
    for marker in ('TODO', 'FIXME', 'HACK', 'debugger', 'console.log('):
        if marker in text:
            errors.append(f'{rel}: debug/placeholder marker {marker!r}')
    control_chars = [(index, ord(ch)) for index, ch in enumerate(text) if ord(ch) < 32 and ch not in '\n\r\t']
    if control_chars:
        errors.append(f'{rel}: unexpected control characters {control_chars[:5]}')

for path in sorted(SRC.rglob('*')):
    if not path.is_file() or path.suffix not in {'.astro', '.css', '.js', '.mjs', '.json', '.ts'}:
        continue
    text = path.read_text(encoding='utf-8')
    rel = path.relative_to(ROOT)
    control_chars = [(index, ord(ch)) for index, ch in enumerate(text) if ord(ch) < 32 and ch not in '\n\r\t']
    if control_chars:
        errors.append(f'{rel}: unexpected control characters {control_chars[:5]}')
    if 'mask-image:' in text:
        errors.append(f'{rel}: mask-image spelling triggers RepoTunnel secret protection; use mask shorthand')
    if 'Task-scoped' in text or 'task-scoped' in text:
        errors.append(f'{rel}: task-scoped token triggers RepoTunnel secret protection; use task scoped')

styles = {p.name for p in (SRC / 'styles').glob('*.css')}
allowed = {'site.css', 'responsive.css'}
extra = sorted(styles - allowed)
missing = sorted(allowed - styles)
if extra:
    errors.append('unexpected legacy stylesheets: ' + ', '.join(extra))
if missing:
    errors.append('missing canonical stylesheets: ' + ', '.join(missing))

request_flow = SRC / 'components' / 'RequestFlow.astro'
if request_flow.exists():
    errors.append('src/components/RequestFlow.astro: unused legacy component should be removed')



def _hex_luminance(value: str) -> float:
    value = value.lstrip('#')
    if len(value) == 3:
        value = ''.join(ch * 2 for ch in value)
    channels = [int(value[i:i + 2], 16) / 255 for i in (0, 2, 4)]
    linear = [channel / 12.92 if channel <= 0.04045 else ((channel + 0.055) / 1.055) ** 2.4 for channel in channels]
    return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]


def contrast_ratio(foreground: str, background: str) -> float:
    high, low = sorted((_hex_luminance(foreground), _hex_luminance(background)), reverse=True)
    return (high + 0.05) / (low + 0.05)

site_css = (SRC / 'styles' / 'site.css').read_text(encoding='utf-8')
homepage = (SRC / 'pages' / 'index.astro').read_text(encoding='utf-8')
footer = (SRC / 'components' / 'Footer.astro').read_text(encoding='utf-8')

# Marketing/design invariants: keep the public landing page clean, readable and brand-consistent.
if 'RepoTunnel</strong><span>R.</span>' in homepage:
    errors.append('homepage: stray R. shorthand must not appear in the hero gateway card')
if 'Give AI access to the work.<br' in homepage:
    errors.append('homepage: display headline should not end its first line with decorative sentence punctuation')
if 'RepoTunnel<span>.</span>' in footer:
    errors.append('footer: wordmark must not append a decorative full stop')
if '--display-font:' not in site_css:
    errors.append('site.css: missing dedicated display-font stack for headings')
if '.hero-proof span+span{border-left:' in site_css:
    errors.append('site.css: hero proof points must not be split by rigid vertical-rule separators')

light_match = __import__('re').search(r':root\{([^}]*)\}', site_css)
dark_match = __import__('re').search(r'html\[data-theme=dark\]\{([^}]*)\}', site_css)
for theme_name, match in [('light', light_match), ('dark', dark_match)]:
    if not match:
        errors.append(f'{theme_name} theme variable block missing')
        continue
    values = dict(__import__('re').findall(r'--([a-z0-9-]+):(#[0-9a-fA-F]{3,8})', match.group(1)))
    for foreground in ('text', 'muted', 'muted-2', 'accent'):
        for background in ('bg', 'paper'):
            if foreground not in values or background not in values:
                missing_token = foreground if foreground not in values else background
                errors.append(f'{theme_name} theme missing color token {missing_token}')
                continue
            ratio = contrast_ratio(values[foreground], values[background])
            if ratio < 4.5:
                errors.append(f'{theme_name} {foreground}/{background} contrast {ratio:.2f}:1 is below 4.5:1')

content_path = SRC / 'data' / 'site-content.json'
try:
    content = json.loads(content_path.read_text(encoding='utf-8'))
except (OSError, json.JSONDecodeError) as exc:
    errors.append(f'src/data/site-content.json: invalid JSON: {exc}')
    content = {}

seen_titles = {}
seen_descriptions = {}
for collection_name in ('products', 'solutions', 'docs'):
    pages = content.get(collection_name)
    if not isinstance(pages, list) or not pages:
        errors.append(f'site content collection {collection_name!r} must be a non-empty list')
        continue
    seen_slugs = set()
    for page in pages:
        slug = str(page.get('slug', '')).strip()
        title = str(page.get('title', '')).strip()
        description = str(page.get('description', '')).strip()
        label = f'{collection_name}/{slug or "<missing-slug>"}'
        if not slug:
            errors.append(f'{collection_name}: page missing slug')
        elif slug in seen_slugs:
            errors.append(f'{collection_name}: duplicate slug {slug!r}')
        seen_slugs.add(slug)
        if not title:
            errors.append(f'{label}: missing title')
        elif title in seen_titles:
            errors.append(f'{label}: duplicate title also used by {seen_titles[title]}')
        else:
            seen_titles[title] = label
        if not 50 <= len(description) <= 180:
            errors.append(f'{label}: description length {len(description)} must be 50..180 characters')
        elif description in seen_descriptions:
            errors.append(f'{label}: duplicate description also used by {seen_descriptions[description]}')
        else:
            seen_descriptions[description] = label
        sections = page.get('sections')
        if not isinstance(sections, list) or not sections:
            errors.append(f'{label}: must contain at least one content section')
            continue
        section_titles = set()
        paragraph_texts = set()
        for section in sections:
            section_title = str(section.get('title', '')).strip()
            if not section_title:
                errors.append(f'{label}: section missing title')
            elif section_title in section_titles:
                errors.append(f'{label}: duplicate section title {section_title!r}')
            section_titles.add(section_title)
            if not any((section.get('paragraphs'), section.get('bullets'), section.get('code'), section.get('note'), section.get('steps'), section.get('links'), section.get('image'))):
                errors.append(f'{label}: empty section {section_title!r}')
            for paragraph in section.get('paragraphs') or []:
                normalized = ' '.join(str(paragraph).split())
                if normalized in paragraph_texts:
                    errors.append(f'{label}: repeated paragraph in section {section_title!r}')
                paragraph_texts.add(normalized)


# App labels are the public UI contract, read from RepoTunnel's AppSidebar.tsx.
# Missing a screen from the website must fail independently of page/word counts.
expected_app_labels = [
    'Home', 'Projects', 'GPT Agents', 'Video', 'History', 'Checks', 'Git',
    'Connect', 'Commands', 'Phone', 'HTTPS Setup', 'Settings', 'Help',
]
areas = content.get('appAreas', [])
if [area.get('label') for area in areas] != expected_app_labels:
    errors.append('app guide must cover every actual sidebar label in order, including Settings and Help')
docs_by_slug = {page.get('slug'): page for page in content.get('docs', [])}
for area in areas:
    if area.get('guide') not in docs_by_slug:
        errors.append(f"app guide {area.get('label')!r}: missing walkthrough {area.get('guide')!r}")
    if not area.get('description'):
        errors.append(f"app guide {area.get('label')!r}: missing explanation")
tour = docs_by_slug.get('getting-started/app-tour')
if not tour:
    errors.append('missing screen-by-screen app walkthrough')
else:
    headings = {section.get('title') for section in tour.get('sections', [])}
    if not set(expected_app_labels) <= headings:
        errors.append('app walkthrough must explain all sidebar screens')
valid_urls = {'/' + prefix + '/' + page['slug'] + '/'
              for prefix, collection in (('product', 'products'), ('solutions', 'solutions'), ('docs', 'docs'))
              for page in content.get(collection, [])}
valid_urls.update({'/', '/docs/', '/product/', '/solutions/', '/install/', '/downloads/', '/security/'})
manifest_path = ROOT / 'docs/platform-screenshots.json'
try:
    manifest = json.loads(manifest_path.read_text())
    image_records = {record['path']: record for record in manifest['images']}
except (OSError, ValueError, KeyError) as exc:
    errors.append(f'guide screenshot provenance unavailable: {exc}')
    image_records = {}
used_images = set()
for collection in ('products', 'solutions', 'docs'):
    for page in content.get(collection, []):
        for section in page.get('sections', []):
            label = f"{collection}/{page['slug']} / {section['title']}"
            steps = section.get('steps')
            if steps is not None and (not isinstance(steps, list) or not steps or any(not isinstance(step, str) or not step.strip() for step in steps)):
                errors.append(f'{label}: steps must be non-empty text')
            for link in section.get('links', []):
                href = link.get('href', '')
                if not link.get('label', '').strip():
                    errors.append(f'{label}: link must have a descriptive label')
                if href.startswith('/') and not href.startswith('//'):
                    if href.split('#', 1)[0] not in valid_urls:
                        errors.append(f'{label}: missing related guide {href}')
                elif urlsplit(href).scheme != 'https' or not urlsplit(href).hostname:
                    errors.append(f'{label}: unsafe or unsupported link {href}')
            image = section.get('image')
            if not image:
                continue
            src = image.get('src', '')
            if not src.startswith('/images/guides/') or '..' in src or not src.endswith('.png'):
                errors.append(f'{label}: screenshot must use a local guide PNG')
                continue
            # Guide identifiers resolve to originals imported by Astro's image pipeline.
            path = ROOT / 'src' / 'assets' / 'guides' / Path(src).name
            relative = str(path.relative_to(ROOT))
            used_images.add(relative)
            if not all(isinstance(image.get(field), str) and image[field].strip() for field in ('alt', 'caption', 'sourceHref', 'sourceLabel')):
                errors.append(f'{label}: missing screenshot description or credit')
            if urlsplit(image.get('sourceHref', '')).scheme != 'https':
                errors.append(f'{label}: screenshot source must be HTTPS')
            try:
                payload = path.read_bytes()
                dimensions = (int.from_bytes(payload[16:20], 'big'), int.from_bytes(payload[20:24], 'big'))
                if payload[:8] != bytes((137, 80, 78, 71, 13, 10, 26, 10)) or dimensions != (image.get('width'), image.get('height')):
                    errors.append(f'{label}: screenshot dimensions do not match its PNG')
                record = image_records.get(relative)
                if not record or record.get('sha256') != hashlib.sha256(payload).hexdigest():
                    errors.append(f'{label}: screenshot has missing or stale provenance')
            except OSError as exc:
                errors.append(f'{label}: screenshot is unavailable: {exc}')
for slug in ('connections/ngrok', 'connections/cloudflare', 'connections/direct-https'):
    sections = docs_by_slug.get(slug, {}).get('sections', [])
    if not any(section.get('steps') for section in sections):
        errors.append(f'{slug}: missing ordered setup walkthrough')
    if not any(section.get('image') for section in sections):
        errors.append(f'{slug}: missing practical platform screenshot')
    if not any(link.get('href', '').startswith('https://') for section in sections for link in section.get('links', [])):
        errors.append(f'{slug}: missing direct official platform links')
if set(image_records) != used_images:
    errors.append('screenshot manifest must contain exactly the images used by guides')

if errors:
    print('SOURCE AUDIT FAIL')
    for error in errors:
        print('-', error)
    sys.exit(1)
print(f'SOURCE AUDIT PASS: {len(astro_files)} Astro files; canonical CSS and source hygiene verified.')
