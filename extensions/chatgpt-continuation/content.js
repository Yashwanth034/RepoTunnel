(() => {
  const VERSION = chrome.runtime.getManifest().version;
  if (globalThis.__repoTunnelContinuationContentVersion === VERSION) return;
  globalThis.__repoTunnelContinuationContentVersion = VERSION;

  const COMPLETION_SETTLE_MS = 2000;
  const HUMAN_IDLE_MS = 1500;
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  let bridgePollRunning = false;
  let completionCandidateSince = 0;
  let lastTrustedComposerInputAt = 0;
  let repoTunnelComposerWriteDepth = 0;
  let repoTunnelOwnWriteUntil = 0;
  let preparedDelivery = null;

  function isVisible(el) {
    if (!el) return false;
    const style = getComputedStyle(el);
    return el.getClientRects().length > 0 && style.visibility !== 'hidden' && style.display !== 'none';
  }

  function getStopButton() {
    const direct = [
      document.querySelector('[data-testid="stop-button"]'),
      document.querySelector('form[data-chatgpt-composer] button[type="button"][aria-label="Stop"]'),
      document.querySelector('button[aria-label*="Stop generating" i]'),
      document.querySelector('button[title*="Stop generating" i]')
    ].find((button) => button && isVisible(button) && !button.disabled);
    if (direct) return direct;

    return [...document.querySelectorAll('button')].find((button) => {
      if (!isVisible(button) || button.disabled) return false;
      const label = [
        button.getAttribute('aria-label'),
        button.getAttribute('title'),
        button.innerText
      ].filter(Boolean).join(' ').replace(/\s+/g, ' ').trim();
      return /\b(stop generating|stop streaming|interrupt response|cancel generation)\b/i.test(label);
    }) || null;
  }

  function latestAssistantTurn() {
    const selectors = [
      '[data-testid^="conversation-turn-"][data-turn="assistant"]',
      '[data-testid^="conversation-turn-"][data-message-author-role="assistant"]',
      '[data-testid^="conversation-turn-"]:has([data-message-author-role="assistant"])',
      '[data-turn-key]:has([data-conversation-role="assistant"], [data-chatgpt-agent-turn-start])',
      'section[data-turn="assistant"]',
      'article:has([data-message-author-role="assistant"])'
    ];

    const turns = [];
    for (const selector of selectors) {
      for (const el of document.querySelectorAll(selector)) {
        if (isVisible(el) && !turns.includes(el)) turns.push(el);
      }
    }
    if (turns.length) return turns[turns.length - 1];

    const messages = [...document.querySelectorAll('[data-message-author-role="assistant"]')]
      .filter((el) => isVisible(el));
    const message = messages.at(-1) || null;
    return message?.closest(
      'article, section[data-turn="assistant"], [data-testid^="conversation-turn-"], [data-turn-key]'
    ) || message;
  }

  function latestAssistantIsComplete() {
    const turn = latestAssistantTurn();
    if (!turn) return false;
    const controls = [
      ...turn.querySelectorAll('button[data-testid="copy-turn-action-button"]'),
      ...turn.querySelectorAll('.turn-action-controls button')
    ];
    return controls.some((button) => !button.disabled && isVisible(button));
  }

  function chatIsActivelyGenerating() {
    return Boolean(getStopButton());
  }

  function findEditor() {
    const selectors = [
      '#prompt-textarea',
      'form[data-chatgpt-composer] textarea',
      'form[data-chatgpt-composer] [contenteditable="true"][data-lexical-editor="true"]',
      'form[data-chatgpt-composer] [contenteditable="true"]',
      'textarea[placeholder*="Message"]',
      '[contenteditable="true"][data-lexical-editor="true"]'
    ];

    for (const selector of selectors) {
      const candidates = [...document.querySelectorAll(selector)]
        .filter((el) => isVisible(el) && !el.closest('[aria-hidden="true"]'));
      if (candidates.length) return candidates[candidates.length - 1];
    }
    return null;
  }

  function editorText(editor) {
    if (!editor) return '';
    if (editor instanceof HTMLTextAreaElement || editor instanceof HTMLInputElement) {
      return editor.value || '';
    }
    return editor.innerText || editor.textContent || '';
  }

  function setTextareaValue(el, value) {
    const proto = Object.getPrototypeOf(el);
    const descriptor = Object.getOwnPropertyDescriptor(proto, 'value');
    if (descriptor?.set) descriptor.set.call(el, value);
    else el.value = value;
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  }

  function setContentEditable(el, value) {
    el.focus();
    try {
      const selection = window.getSelection();
      const range = document.createRange();
      range.selectNodeContents(el);
      selection.removeAllRanges();
      selection.addRange(range);
      document.execCommand('insertText', false, value);
      selection.removeAllRanges();
    } catch {
      el.textContent = value;
    }
    el.dispatchEvent(new InputEvent('input', {
      bubbles: true,
      inputType: 'insertText',
      data: value
    }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  }

  function setEditorText(editor, value) {
    repoTunnelComposerWriteDepth += 1;
    repoTunnelOwnWriteUntil = Date.now() + 250;
    try {
      if (editor instanceof HTMLTextAreaElement || editor instanceof HTMLInputElement) {
        setTextareaValue(editor, value);
      } else {
        setContentEditable(editor, value);
      }
    } finally {
      repoTunnelComposerWriteDepth = Math.max(0, repoTunnelComposerWriteDepth - 1);
      repoTunnelOwnWriteUntil = Math.max(repoTunnelOwnWriteUntil, Date.now() + 100);
    }
  }

  function findSendButton() {
    const candidates = [
      document.querySelector('[data-testid="send-button"]'),
      document.querySelector('button[aria-label="Send prompt"]'),
      document.querySelector('button[aria-label="Send message"]'),
      ...[...document.querySelectorAll('button')].filter((button) => {
        const label = (button.getAttribute('aria-label') || '').toLowerCase().trim();
        return label === 'send' || label.startsWith('send ');
      })
    ].filter(Boolean);
    return candidates.find((button) => !button.disabled && isVisible(button)) || null;
  }

  async function waitForEditor(timeoutMs = 10000) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      const editor = findEditor();
      if (editor) return editor;
      await sleep(250);
    }
    throw new Error('Chat input box was not found.');
  }

  function readyForDelivery() {
    const editor = findEditor();
    if (!editor || editorText(editor).trim()) {
      completionCandidateSince = 0;
      return false;
    }

    if (lastTrustedComposerInputAt && Date.now() - lastTrustedComposerInputAt < HUMAN_IDLE_MS) {
      completionCandidateSince = 0;
      return false;
    }

    if (chatIsActivelyGenerating() || !latestAssistantIsComplete()) {
      completionCandidateSince = 0;
      return false;
    }

    if (!completionCandidateSince) {
      completionCandidateSince = Date.now();
      return false;
    }

    return Date.now() - completionCandidateSince >= COMPLETION_SETTLE_MS;
  }

  async function waitUntilReady(timeoutMs = 20000) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (readyForDelivery()) return;
      await sleep(500);
    }
    const error = new Error('ChatGPT is not stably idle yet; RepoTunnel will retry later.');
    error.retryable = true;
    throw error;
  }

  function normalizeText(value) {
    return String(value || '').replace(/\s+/g, ' ').trim();
  }

  function normalizedCurrentUrl() {
    const url = new URL(location.href);
    const segments = url.pathname.split('/').filter(Boolean);
    if (
      url.protocol !== 'https:' ||
      url.hostname !== 'chatgpt.com' ||
      segments.length !== 2 ||
      segments[0] !== 'c'
    ) {
      return '';
    }
    return `https://chatgpt.com/c/${segments[1]}`;
  }

  function getUserMessages() {
    return [...document.querySelectorAll('[data-message-author-role="user"]')]
      .filter((el) => isVisible(el));
  }

  function getUserText(el) {
    if (!el) return '';
    const content = el.querySelector('.markdown, [class*="markdown"]') || el;
    return (content.innerText || content.textContent || '').trim();
  }

  async function clearPrepared(jobId, revision) {
    const prepared = preparedDelivery;
    if (
      !prepared ||
      prepared.jobId !== String(jobId || '') ||
      prepared.revision !== Number(revision)
    ) {
      return false;
    }

    const editor = findEditor();
    const unchanged =
      editor &&
      normalizeText(editorText(editor)) === normalizeText(prepared.message) &&
      lastTrustedComposerInputAt <= prepared.preparedAt;

    if (unchanged) setEditorText(editor, '');
    preparedDelivery = null;
    return unchanged;
  }

  async function prepareDelivery(jobId, revision, exactMessage) {
    const message = String(exactMessage || '');
    if (!message.trim()) throw new Error('RepoTunnel continuation message is empty.');

    if (preparedDelivery) {
      await clearPrepared(preparedDelivery.jobId, preparedDelivery.revision);
    }

    await waitUntilReady();
    const editor = await waitForEditor();

    if (editorText(editor).trim()) {
      const error = new Error('Chat input contains unsent text; RepoTunnel will not overwrite it.');
      error.retryable = true;
      throw error;
    }

    if (lastTrustedComposerInputAt && Date.now() - lastTrustedComposerInputAt < HUMAN_IDLE_MS) {
      const error = new Error('Recent human composer input detected; RepoTunnel will retry later.');
      error.retryable = true;
      throw error;
    }

    const preparedAt = Date.now();
    const baselineUserCount = getUserMessages().length;
    editor.focus();
    setEditorText(editor, message);
    preparedDelivery = {
      jobId: String(jobId || ''),
      revision: Number(revision),
      message,
      preparedAt,
      baselineUserCount
    };

    await sleep(80);

    if (normalizeText(editorText(editor)) !== normalizeText(message)) {
      await clearPrepared(jobId, revision);
      const error = new Error('RepoTunnel could not prepare the exact continuation text safely.');
      error.retryable = true;
      throw error;
    }

    if (lastTrustedComposerInputAt > preparedAt) {
      await clearPrepared(jobId, revision);
      const error = new Error('Human input raced with RepoTunnel message preparation.');
      error.retryable = true;
      throw error;
    }

    return { ok: true, prepared: true };
  }

  async function waitForAcceptedTurn(prepared, timeoutMs = 10000) {
    const deadline = Date.now() + timeoutMs;
    const expected = normalizeText(prepared.message);

    while (Date.now() < deadline) {
      const users = getUserMessages();
      if (users.length > prepared.baselineUserCount) {
        const latest = normalizeText(getUserText(users.at(-1)));
        if (latest === expected) return true;
      }

      const editor = findEditor();
      if (editor && !editorText(editor).trim() && chatIsActivelyGenerating()) {
        return true;
      }

      await sleep(100);
    }

    return false;
  }

  async function commitDelivery(jobId, revision, exactMessage) {
    const prepared = preparedDelivery;
    const message = String(exactMessage || '');

    if (
      !prepared ||
      prepared.jobId !== String(jobId || '') ||
      prepared.revision !== Number(revision) ||
      prepared.message !== message
    ) {
      return {
        ok: false,
        notSent: true,
        retryable: true,
        error: 'Prepared RepoTunnel delivery state was lost or superseded before Send.'
      };
    }

    const editor = findEditor();
    if (!editor) {
      preparedDelivery = null;
      return {
        ok: false,
        notSent: true,
        retryable: true,
        error: 'Chat input box disappeared before RepoTunnel could send.'
      };
    }

    if (
      lastTrustedComposerInputAt > prepared.preparedAt ||
      normalizeText(editorText(editor)) !== normalizeText(message)
    ) {
      preparedDelivery = null;
      return {
        ok: false,
        notSent: true,
        retryable: true,
        error: 'Human input or page state changed after RepoTunnel prepared its message.'
      };
    }

    if (chatIsActivelyGenerating() || !latestAssistantIsComplete()) {
      await clearPrepared(jobId, revision);
      return {
        ok: false,
        notSent: true,
        retryable: true,
        error: 'ChatGPT resumed generating before RepoTunnel clicked Send.'
      };
    }

    let sendButton = null;
    const deadline = Date.now() + 3000;
    while (Date.now() < deadline) {
      sendButton = findSendButton();
      if (sendButton) break;
      await sleep(100);
    }

    if (!sendButton) {
      await clearPrepared(jobId, revision);
      return {
        ok: false,
        notSent: true,
        retryable: true,
        error: 'Send button was not found or is disabled.'
      };
    }

    if (
      lastTrustedComposerInputAt > prepared.preparedAt ||
      chatIsActivelyGenerating() ||
      !latestAssistantIsComplete() ||
      normalizeText(editorText(editor)) !== normalizeText(message)
    ) {
      await clearPrepared(jobId, revision);
      return {
        ok: false,
        notSent: true,
        retryable: true,
        error: 'RepoTunnel final pre-send safety check failed.'
      };
    }

    sendButton.click();
    preparedDelivery = null;
    const sentAt = Date.now();
    const accepted = await waitForAcceptedTurn(prepared);

    if (accepted) return { ok: true, accepted: true, sentAt };

    return {
      ok: false,
      uncertain: true,
      sentAt,
      error: 'RepoTunnel clicked Send but could not positively verify ChatGPT accepted the turn.'
    };
  }

  async function pollBridge() {
    if (bridgePollRunning) return;
    bridgePollRunning = true;
    try {
      await chrome.runtime.sendMessage({
        type: 'REPOTUNNEL_BRIDGE_POLL',
        title: document.title,
        readyForDelivery: readyForDelivery()
      });
    } catch {
    } finally {
      bridgePollRunning = false;
    }
  }

  document.addEventListener('input', (event) => {
    if (
      !event.isTrusted ||
      repoTunnelComposerWriteDepth > 0 ||
      Date.now() <= repoTunnelOwnWriteUntil
    ) {
      return;
    }

    const editor = findEditor();
    const target = event.target;
    if (editor && target instanceof Node && (target === editor || editor.contains(target))) {
      lastTrustedComposerInputAt = Date.now();
    }
  }, true);

  chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
    if (message?.type === 'REPOTUNNEL_BRIDGE_PING') {
      sendResponse({ ok: true, version: VERSION });
      return;
    }

    if (message?.type === 'REPOTUNNEL_PREPARE_MESSAGE') {
      (async () => {
        const expectedUrl = String(message.expectedUrl || '').trim();
        if (expectedUrl && normalizedCurrentUrl() !== expectedUrl) {
          const error = new Error('ChatGPT tab changed before RepoTunnel preparation.');
          error.retryable = true;
          throw error;
        }

        const result = await prepareDelivery(message.jobId, message.revision, message.message);
        sendResponse({ ...result, jobId: message.jobId || null, revision: Number(message.revision) });
      })().catch((error) => {
        sendResponse({
          ok: false,
          retryable: Boolean(error?.retryable),
          error: error?.message || String(error)
        });
      });
      return true;
    }

    if (message?.type === 'REPOTUNNEL_ABORT_PREPARED') {
      (async () => {
        const expectedUrl = String(message.expectedUrl || '').trim();
        if (expectedUrl && normalizedCurrentUrl() !== expectedUrl) {
          sendResponse({ ok: true, cleared: false });
          return;
        }
        const cleared = await clearPrepared(message.jobId, message.revision);
        sendResponse({ ok: true, cleared });
      })().catch((error) => {
        sendResponse({ ok: false, retryable: false, error: error?.message || String(error) });
      });
      return true;
    }

    if (message?.type === 'REPOTUNNEL_COMMIT_MESSAGE') {
      (async () => {
        const expectedUrl = String(message.expectedUrl || '').trim();
        if (expectedUrl && normalizedCurrentUrl() !== expectedUrl) {
          sendResponse({
            ok: false,
            notSent: true,
            retryable: true,
            error: 'ChatGPT tab changed before RepoTunnel commit.'
          });
          return;
        }

        const result = await commitDelivery(message.jobId, message.revision, message.message);
        sendResponse({ ...result, jobId: message.jobId || null, revision: Number(message.revision) });
      })().catch((error) => {
        sendResponse({
          ok: false,
          uncertain: true,
          retryable: false,
          error: error?.message || String(error)
        });
      });
      return true;
    }
  });

  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) void pollBridge();
  });
  window.addEventListener('focus', () => void pollBridge());
  window.addEventListener('pageshow', () => void pollBridge());
  window.addEventListener('online', () => void pollBridge());

  setInterval(pollBridge, 1500);
  setTimeout(pollBridge, 750);
})();
