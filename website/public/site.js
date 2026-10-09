(() => {
  const root = document.documentElement;
  try {
    const stored = localStorage.getItem('repotunnel-theme');
    root.dataset.theme = stored === 'dark' ? 'dark' : 'light';
  } catch {
    root.dataset.theme = 'light';
  }

  const themeButton = document.querySelector('[data-action="theme"]');
  const syncThemeButton = () => {
    const isDark = root.dataset.theme === 'dark';
    document.querySelector('meta[name="theme-color"]')?.setAttribute('content', isDark ? '#141416' : '#fbfbfa');
    if (!themeButton) return;
    const label = isDark ? 'Switch to light theme' : 'Switch to dark theme';
    themeButton.setAttribute('aria-label', label);
    themeButton.setAttribute('title', label);
  };
  syncThemeButton();

  const header = document.querySelector('.site-header');
  let previousScrollY = Math.max(0, window.scrollY);
  let scrollQueued = false;
  const revealHeader = () => header?.classList.remove('is-hidden');
  const updateHeader = () => {
    scrollQueued = false;
    if (!header) return;
    const currentY = Math.max(0, window.scrollY);
    const delta = currentY - previousScrollY;
    const navigationOpen = document.querySelector('.mobile-nav')?.classList.contains('open');
    if (currentY <= header.offsetHeight || header.contains(document.activeElement) || navigationOpen ||
        document.querySelector('#site-search')?.open) {
      revealHeader();
      previousScrollY = currentY;
    } else if (Math.abs(delta) >= 6) {
      // Keep the requested direction: hide going up, reveal going down.
      header.classList.toggle('is-hidden', delta < 0);
      previousScrollY = currentY;
    }
  };
  window.addEventListener('scroll', () => {
    if (!scrollQueued) {
      scrollQueued = true;
      requestAnimationFrame(updateHeader);
    }
  }, { passive: true });
  header?.addEventListener('focusin', revealHeader);
  window.addEventListener('resize', () => {
    previousScrollY = Math.max(0, window.scrollY);
    revealHeader();
  });

  const copyTimers = new WeakMap();
  document.addEventListener('click', (event) => {
    const target = event.target.closest('[data-action]');
    if (!target) return;
    const action = target.dataset.action;

    if (action === 'theme') {
      const next = root.dataset.theme === 'dark' ? 'light' : 'dark';
      root.dataset.theme = next;
      try { localStorage.setItem('repotunnel-theme', next); } catch {}
      document.querySelectorAll('[data-theme-label]').forEach((el) => el.textContent = next === 'dark' ? 'Light theme' : 'Dark theme');
      syncThemeButton();
    }

    if (action === 'menu') {
      const nav = document.querySelector('.mobile-nav');
      const open = nav?.classList.toggle('open');
      target.setAttribute('aria-expanded', String(Boolean(open)));
      target.setAttribute('aria-label', open ? 'Close navigation' : 'Open navigation');
    }

    if (action === 'docs-menu') {
      const sidebar = target.closest('.docs-sidebar');
      const open = sidebar?.classList.toggle('expanded');
      target.setAttribute('aria-expanded', String(Boolean(open)));
    }

    if (action === 'copy') {
      const block = target.closest('.code-block')?.querySelector('code');
      if (!block) return;
      const text = block.textContent || '';
      const label = target.querySelector('.copy-label') || target;
      copyText(text).then((copied) => {
        if (!target.isConnected) return;
        if (!copied) selectElementText(block);
        clearTimeout(copyTimers.get(target));
        label.textContent = copied ? 'Copied' : 'Selected';
        copyTimers.set(target, setTimeout(() => { label.textContent = 'Copy'; }, 1600));
      });
    }

    if (action === 'search-open') openSearch();
    if (action === 'search-close') document.querySelector('#site-search')?.close();
  });

  document.querySelectorAll('[data-tabs]').forEach((tabs) => {
    const buttons = [...tabs.querySelectorAll('[role="tab"]')];
    const panels = [...tabs.parentElement.querySelectorAll('[role="tabpanel"]')];
    const activate = (button, focus = false) => {
      buttons.forEach((b) => {
        const active = b === button;
        b.classList.toggle('active', active);
        b.setAttribute('aria-selected', String(active));
        b.tabIndex = active ? 0 : -1;
      });
      panels.forEach((panel) => { panel.hidden = panel.id !== button.getAttribute('aria-controls'); });
      if (focus) button.focus();
    };
    buttons.forEach((button, i) => {
      button.addEventListener('click', () => activate(button));
      button.addEventListener('keydown', (event) => {
        let index = i;
        if (event.key === 'ArrowRight') index = (i + 1) % buttons.length;
        else if (event.key === 'ArrowLeft') index = (i - 1 + buttons.length) % buttons.length;
        else if (event.key === 'Home') index = 0;
        else if (event.key === 'End') index = buttons.length - 1;
        else return;
        event.preventDefault();
        activate(buttons[index], true);
      });
    });
  });

  async function copyText(text) {
    if (!navigator.clipboard?.writeText) return false;
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      return false;
    }
  }

  function selectElementText(element) {
    const selection = window.getSelection();
    if (!selection) return;
    const range = document.createRange();
    range.selectNodeContents(element);
    selection.removeAllRanges();
    selection.addRange(range);
  }

  const searchDialog = document.querySelector('#site-search');
  const searchInput = document.querySelector('#site-search-input');
  const searchResults = document.querySelector('#site-search-results');
  let searchIndex = null;
  let pagefind = null;
  let pagefindAttempted = false;
  let searchSerial = 0;

  async function ensureSearchIndex() {
    if (searchIndex) return searchIndex;
    try {
      const res = await fetch('/search-index.json', { cache: 'no-store' });
      searchIndex = await res.json();
    } catch {
      searchIndex = [];
    }
    return searchIndex;
  }

  async function openSearch() {
    if (!searchDialog) return;
    if (!searchDialog.open) searchDialog.showModal();
    await ensureSearchIndex();
    searchInput?.focus();
    renderSearch(searchInput?.value || '');
  }

  function renderResults(items) {
    if (!items.length) {
      searchResults.innerHTML = '<div class="search-empty">No matching RepoTunnel pages. Try a simpler keyword.</div>';
      searchResults.scrollTop = 0;
      return;
    }
    const groupFor = (url) =>
      url.startsWith('/docs/') ? 'Documentation' :
      url.startsWith('/product/') ? 'Product' :
      url.startsWith('/solutions/') ? 'Solution' :
      url.startsWith('/install/') ? 'Installation' :
      url.startsWith('/downloads/') ? 'Download' :
      url.startsWith('/security/') ? 'Security' :
      url.startsWith('/changelog/') ? 'Release' :
      url.startsWith('/community/') ? 'Community' :
      url.startsWith('/privacy/') ? 'Privacy' : 'RepoTunnel';
    searchResults.innerHTML = items.map((item) => {
      const category = groupFor(item.url);
      const categoryLabel = category.toLowerCase() === item.title.trim().toLowerCase()
        ? '' : `<span class="search-category">${escapeHtml(category)}</span>`;
      return `<a class="search-result" href="${escapeHtml(item.url)}">${categoryLabel}<strong>${escapeHtml(item.title)}</strong><small>${escapeHtml(item.description)}</small></a>`;
    }).join('');
    searchResults.scrollTop = 0;
  }

  function score(item, terms) {
    const title = item.title.toLowerCase();
    const description = item.description.toLowerCase();
    const body = item.searchText.toLowerCase();
    return terms.reduce((total, term) => {
      if (title.includes(term)) total += 12;
      if (description.includes(term)) total += 5;
      if (body.includes(term)) total += 2;
      return total;
    }, 0);
  }

  async function renderSearch(value) {
    if (!searchResults) return;
    const query = value.trim();
    const serial = ++searchSerial;

    if (query && !pagefindAttempted && document.querySelector('meta[name="repotunnel-search-mode"]')?.content === 'pagefind') {
      pagefindAttempted = true;
      try {
        pagefind = await import('/pagefind/pagefind.js');
        await pagefind.init();
      } catch {
        pagefind = null;
      }
    }

    if (query && pagefind) {
      try {
        const result = await pagefind.search(query);
        const records = await Promise.all(result.results.slice(0, 12).map((entry) => entry.data()));
        const items = records.map((record) => {
          const original = searchIndex?.find((item) => item.url === record.url);
          return {
            title: original?.title || record.meta?.title || record.url,
            description: original?.description || String(record.meta?.description || record.excerpt || '').replace(/<[^>]+>/g, ''),
            url: record.url
          };
        });
        if (serial !== searchSerial) return;
        if (items.length) {
          renderResults(items);
          return;
        }
      } catch {}
    }

    const index = await ensureSearchIndex();
    if (serial !== searchSerial) return;
    const terms = query.toLowerCase().split(/\s+/).filter(Boolean);
    const scored = index.map((item) => ({ item, rank: score(item, terms) }));
    const allTerms = scored.filter(({ item }) => terms.every((term) =>
      (item.title + ' ' + item.description + ' ' + item.searchText).toLowerCase().includes(term)
    ));
    const matched = terms.length
      ? (allTerms.length ? allTerms : scored.filter(({ rank }) => rank > 0))
      : scored.slice(0, 10);
    renderResults(matched.sort((a, b) => b.rank - a.rank).slice(0, 12).map(({ item }) => item));
  }

  function escapeHtml(value) {
    return String(value).replace(/[&<>'"]/g, (ch) => ({'&':'&amp;','<':'&lt;','>':'&gt;',"'":'&#39;','"':'&quot;'}[ch]));
  }

  searchInput?.addEventListener('input', () => { void renderSearch(searchInput.value); });
  searchDialog?.addEventListener('click', (event) => {
    if (event.target === searchDialog) searchDialog.close();
  });
  document.addEventListener('keydown', (event) => {
    if ((event.key === '/' && !/INPUT|TEXTAREA/.test(document.activeElement?.tagName || '')) || ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k')) {
      event.preventDefault();
      openSearch();
    }
    if (event.key === 'Escape' && searchDialog?.open) searchDialog.close();
    if (event.key === 'Escape') {
      const nav = document.querySelector('.mobile-nav');
      const toggle = document.querySelector('[data-action=menu]');
      if (nav?.classList.contains('open')) {
        nav.classList.remove('open');
        toggle?.setAttribute('aria-expanded', 'false');
        toggle?.setAttribute('aria-label', 'Open navigation');
        toggle?.focus();
      }
      const docsSidebar = document.querySelector('.docs-sidebar');
      const docsToggle = document.querySelector('[data-action=docs-menu]');
      if (docsSidebar?.classList.contains('expanded')) {
        docsSidebar.classList.remove('expanded');
        docsToggle?.setAttribute('aria-expanded', 'false');
        docsToggle?.focus();
      }
    }
  });
})();
