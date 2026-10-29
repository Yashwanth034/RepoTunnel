(() => {
  const VERSION = chrome.runtime.getManifest().version;
  if (globalThis.__repoTunnelContinuationContentVersion === VERSION) return;
  globalThis.__repoTunnelContinuationContentVersion = VERSION;

  const COMPLETION_SETTLE_MS = 2000;
  const HUMAN_IDLE_MS = 1500;
  // Runtime messages wake the worker; hidden-page timers are not used for
  // operational waits during preparation, recovery, or acceptance checks.
  const sleep = async (ms) => {
    const result = await chrome.runtime.sendMessage({ type: 'REPOTUNNEL_BACKGROUND_DELAY', ms });
    if (!result?.ok) throw new Error('Extension background worker is unavailable.');
  };

  let bridgePollRunning = false;
  let completionCandidateSince = 0;
  let lastTrustedComposerInputAt = 0;
  let repoTunnelComposerWriteDepth = 0;
  let repoTunnelOwnWriteUntil = 0;
  let preparedDelivery = null;
  let interruptedRecoveryRunning = false;
  let messageTimeoutRecoveryRunning = false;
  let manualDefaultActionRunning = false;
  let interruptedRefreshRecoveryRunning = false;
  let recoveryHumanInputRevision = 0;
  const MESSAGE_TIMEOUT_RECOVERY_MESSAGE = "Resume the unfinished work from RepoTunnel's live continuity state. Inspect the current state first and do not repeat completed actions.";

  // Only the explicit ChatGPT UI notice qualifies. Quoted response text,
  // older turns, and ordinary slow generation must never trigger Stop.
  function latestAssistantTurnWithoutHas() {
    // A plain query preserves document order and supports Chrome 102, where
    // :has selectors are unavailable. Check assistant descendants in JavaScript.
    const turns = [...document.querySelectorAll(
      '[data-testid^="conversation-turn-"], [data-turn-key], section[data-turn="assistant"], article'
    )].filter((el) => {
      if (!isVisible(el)) return false;
      const hasAssistantMessage = Boolean(el.querySelector('[data-message-author-role="assistant"]'));
      return ((el.getAttribute('data-testid') || '').startsWith('conversation-turn-') && (
        el.getAttribute('data-turn') === 'assistant' ||
        el.getAttribute('data-message-author-role') === 'assistant' || hasAssistantMessage
      )) || (el.getAttribute('data-turn-key') !== null && Boolean(el.querySelector(
        '[data-conversation-role="assistant"], [data-chatgpt-agent-turn-start]'
      ))) || (el.tagName === 'SECTION' && el.getAttribute('data-turn') === 'assistant') ||
        (el.tagName === 'ARTICLE' && hasAssistantMessage);
    });
    if (turns.length) return turns.at(-1);
    const message = [...document.querySelectorAll('[data-message-author-role="assistant"]')].filter(isVisible).at(-1);
    return message?.closest(
      'article, section[data-turn="assistant"], [data-testid^="conversation-turn-"], [data-turn-key]'
    ) || message || null;
  }

  function latestRecoveryAssistantTurn() {
    return latestAssistantTurnWithoutHas();
  }

  // Original same-turn notice matching: do not scan separate panels or other turns.
  function hasExactTurnNotice(turn, pattern) {
    for (const el of turn.querySelectorAll('[role="alert"], [role="status"], div, p, span')) {
      if (!isVisible(el) ||
          el.closest('.markdown, [class*="markdown"], pre, code, [contenteditable="true"]')) continue;
      const role = el.getAttribute('role');
      if (el.closest('[data-message-author-role], [data-conversation-role]') &&
          role !== 'alert' && role !== 'status') continue;
      if (el.querySelector('[data-message-author-role], [data-conversation-role]')) continue;
      const text = (el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim();
      if (pattern.test(text)) return true;
    }
    return false;
  }

  function hasExactStandaloneNotice(pattern) {
    // ChatGPT renders delivery-timeout banners beside, rather than inside,
    // the assistant turn on some layouts. Match the exact visible notice
    // outside message content so quoted text cannot trigger automatic sending.
    const selectors = '[role="alert"], [role="status"], [data-testid*="error"], [data-testid*="toast"], div, p, span';
    for (const el of document.querySelectorAll(selectors)) {
      if (!isVisible(el) ||
          el.closest('.markdown, [class*="markdown"], pre, code, [contenteditable="true"]') ||
          el.closest('[data-message-author-role], [data-conversation-role]')) continue;
      const text = (el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim();
      if (pattern.test(text)) return true;
    }
    return false;
  }

  function findInterruptedResponse() {
    // Detect the notice even after generation has stopped. The live Stop
    // path is preserved; the idle case needs a one-time page refresh first.
    const turn = latestRecoveryAssistantTurn();
    if (!turn) return null;
    const latestUser = getUserMessages().at(-1);
    if (latestUser && (turn.compareDocumentPosition(latestUser) & Node.DOCUMENT_POSITION_FOLLOWING)) {
      return null;
    }

    const interruptedNotice = /^connection interrupted[.!]?\s+waiting for (?:the )?complete (?:answer|message)[.!]?$/i;
    if (!hasExactTurnNotice(turn, interruptedNotice) && !hasExactStandaloneNotice(interruptedNotice)) return null;
    const turnId = turn.getAttribute('data-turn-key') || turn.getAttribute('data-message-id') ||
      turn.querySelector('[data-message-id]')?.getAttribute('data-message-id') ||
      turn.getAttribute('data-testid') || turn.id;
    // History counts change when ChatGPT virtualizes/loading earlier turns.
    // Without stable turn identity, fail closed instead of risking a replay.
    return turnId ? { turn, eventKey: turnId } : null;
  }

  function findMessageDeliveryTimeout() {
    const turn = latestRecoveryAssistantTurn();
    if (!turn) return null;
    const latestUser = getUserMessages().at(-1);
    if (latestUser && (turn.compareDocumentPosition(latestUser) & Node.DOCUMENT_POSITION_FOLLOWING)) {
      return null;
    }

    const timeoutNotice = /^message delivery timed out[.!]?\s+please try again[.!]?$/i;
    if (!hasExactTurnNotice(turn, timeoutNotice) && !hasExactStandaloneNotice(timeoutNotice)) return null;
    const turnId = turn.getAttribute('data-turn-key') || turn.getAttribute('data-message-id') ||
      turn.querySelector('[data-message-id]')?.getAttribute('data-message-id') ||
      turn.getAttribute('data-testid') || turn.id;
    return turnId ? { turn, eventKey: turnId } : null;
  }

  function composerIsHumanIdle() {
    return !lastTrustedComposerInputAt || Date.now() - lastTrustedComposerInputAt >= HUMAN_IDLE_MS;
  }

  async function recoverInterruptedResponse(expectedUrl, eventKey) {
    const interrupted = findInterruptedResponse();
    const editor = findEditor();
    if (
      interruptedRecoveryRunning || messageTimeoutRecoveryRunning || preparedDelivery ||
      normalizedCurrentUrl() !== expectedUrl || interrupted?.eventKey !== eventKey ||
      !editor || editorText(editor).trim() || !composerIsHumanIdle()
    ) return { ok: false, notSent: true, error: 'Interrupted-response recovery is no longer safe.' };

    interruptedRecoveryRunning = true;
    const baselineUserCount = getUserMessages().length;
    const baselineHumanInputAt = lastTrustedComposerInputAt;
    const baselineHumanInputRevision = recoveryHumanInputRevision;
    let sendAttempted = false;
    const stillSameChat = () => (
      normalizedCurrentUrl() === expectedUrl && latestRecoveryAssistantTurn() === interrupted.turn &&
      findEditor() === editor &&
      getUserMessages().length === baselineUserCount &&
      lastTrustedComposerInputAt === baselineHumanInputAt &&
      recoveryHumanInputRevision === baselineHumanInputRevision
    );

    try {
      // The background has checked exact saved-chat ownership and reserved this
      // incident before either click. Stop only the response with this notice.
      const stopButton = getStopButton();
      if (!stopButton || !stillSameChat() || !findInterruptedResponse()) {
        return { ok: false, notSent: true, error: 'The interrupted response changed before Stop.' };
      }
      stopButton.click();

      const deadline = Date.now() + 10000;
      let idleSince = 0;
      let sendButton = null;
      while (Date.now() < deadline) {
        if (!stillSameChat() || editorText(editor).trim()) {
          return { ok: false, notSent: true, error: 'Chat navigation or human input cancelled recovery.' };
        }
        if (chatIsActivelyGenerating()) idleSince = 0;
        else {
          if (!idleSince) idleSince = Date.now();
          if (Date.now() - idleSince >= COMPLETION_SETTLE_MS) break;
        }
        await sleep(100);
      }
      if (chatIsActivelyGenerating() || !idleSince || Date.now() - idleSince < COMPLETION_SETTLE_MS) {
        return { ok: false, notSent: true, error: 'ChatGPT did not stop safely; no continuation was sent.' };
      }

      const authorization = await chrome.runtime.sendMessage({
        type: 'CONNECTION_RECOVERY_READY', eventKey
      });
      if (!authorization?.ok || !authorization.authorized || !stillSameChat() || editorText(editor).trim()) {
        return { ok: false, notSent: true, error: 'The saved chat changed during recovery.' };
      }
      editor.focus();
      setEditorText(editor, 'continue');
      await sleep(80);
      sendButton = await waitForRecoverySendButton(editor, 'continue', stillSameChat);
      const finalAuthorization = await chrome.runtime.sendMessage({
        type: 'CONNECTION_RECOVERY_READY', eventKey
      });
      if (!finalAuthorization?.ok || !finalAuthorization.authorized || !sendButton || !stillSameChat() || chatIsActivelyGenerating() || normalizeText(editorText(editor)) !== 'continue') {
        if (lastTrustedComposerInputAt === baselineHumanInputAt && recoveryHumanInputRevision === baselineHumanInputRevision && normalizeText(editorText(editor)) === 'continue') {
          setEditorText(editor, '');
        }
        return { ok: false, notSent: true, error: 'The final recovery send check failed.' };
      }

      // One attempt only, including when the Send result cannot be verified.
      sendAttempted = true;
      sendButton.click();
      const accepted = await waitForAcceptedTurn({ message: 'continue', baselineUserCount });
      return accepted
        ? { ok: true, accepted: true }
        : { ok: false, uncertain: true, error: 'Recovery Send was attempted but acceptance could not be verified.' };
    } catch (error) {
      return { ok: false, notSent: !sendAttempted, uncertain: sendAttempted, error: error?.message || String(error) };
    } finally {
      interruptedRecoveryRunning = false;
      completionCandidateSince = 0;
    }
  }

  async function recoverMessageDeliveryTimeout(expectedUrl, eventKey) {
    const timeout = findMessageDeliveryTimeout();
    const editor = findEditor();
    if (
      messageTimeoutRecoveryRunning || interruptedRecoveryRunning || preparedDelivery ||
      normalizedCurrentUrl() !== expectedUrl || timeout?.eventKey !== eventKey ||
      !editor || editorText(editor).trim() || !composerIsHumanIdle()
    ) return { ok: false, notSent: true, error: 'Message-timeout recovery is no longer safe.' };

    messageTimeoutRecoveryRunning = true;
    const baselineUserCount = getUserMessages().length;
    const baselineHumanInputAt = lastTrustedComposerInputAt;
    const baselineHumanInputRevision = recoveryHumanInputRevision;
    let sendAttempted = false;
    const stillSameChat = () => (
      normalizedCurrentUrl() === expectedUrl && latestRecoveryAssistantTurn() === timeout.turn &&
      findEditor() === editor &&
      getUserMessages().length === baselineUserCount &&
      lastTrustedComposerInputAt === baselineHumanInputAt &&
      recoveryHumanInputRevision === baselineHumanInputRevision &&
      findMessageDeliveryTimeout()?.eventKey === eventKey
    );

    try {
      if (!stillSameChat()) {
        return { ok: false, notSent: true, error: 'The timed-out response changed before recovery.' };
      }

      const stopButton = getStopButton();
      if (stopButton) {
        stopButton.click();
        const deadline = Date.now() + 10000;
        let idleSince = 0;
        while (Date.now() < deadline) {
          if (!stillSameChat() || editorText(editor).trim()) {
            return { ok: false, notSent: true, error: 'Chat navigation or human input cancelled timeout recovery.' };
          }
          if (chatIsActivelyGenerating()) idleSince = 0;
          else {
            if (!idleSince) idleSince = Date.now();
            // Empty modern composers show voice controls, not Send.
            // Wait for Stop to settle; Send is checked after typing.
            if (Date.now() - idleSince >= COMPLETION_SETTLE_MS) break;
          }
          await sleep(100);
        }
        if (chatIsActivelyGenerating() || !idleSince || Date.now() - idleSince < COMPLETION_SETTLE_MS) {
          return { ok: false, notSent: true, error: 'ChatGPT did not stop safely; timeout recovery message was not sent.' };
        }
      }

      if (!stillSameChat() || chatIsActivelyGenerating()) {
        return { ok: false, notSent: true, error: 'The timed-out response changed before recovery could be sent.' };
      }

      const authorization = await chrome.runtime.sendMessage({
        type: 'MESSAGE_TIMEOUT_RECOVERY_READY', eventKey
      });
      if (!authorization?.ok || !authorization.authorized || !stillSameChat() || editorText(editor).trim()) {
        return { ok: false, notSent: true, error: 'The saved chat changed during timeout recovery.' };
      }

      editor.focus();
      setEditorText(editor, MESSAGE_TIMEOUT_RECOVERY_MESSAGE);
      await sleep(80);
      const sendButton = await waitForRecoverySendButton(
        editor, MESSAGE_TIMEOUT_RECOVERY_MESSAGE, stillSameChat
      );
      const finalAuthorization = await chrome.runtime.sendMessage({
        type: 'MESSAGE_TIMEOUT_RECOVERY_READY', eventKey
      });
      if (
        !finalAuthorization?.ok || !finalAuthorization.authorized || !sendButton || !stillSameChat() ||
        normalizeText(editorText(editor)) !== normalizeText(MESSAGE_TIMEOUT_RECOVERY_MESSAGE)
      ) {
        if (
          lastTrustedComposerInputAt === baselineHumanInputAt &&
          recoveryHumanInputRevision === baselineHumanInputRevision &&
          normalizeText(editorText(editor)) === normalizeText(MESSAGE_TIMEOUT_RECOVERY_MESSAGE)
        ) setEditorText(editor, '');
        return { ok: false, notSent: true, error: 'The final timeout-recovery send check failed.' };
      }

      // Never click ChatGPT's Try again button. Resume from RepoTunnel's live state once.
      sendAttempted = true;
      sendButton.click();
      const accepted = await waitForAcceptedTurn({ message: MESSAGE_TIMEOUT_RECOVERY_MESSAGE, baselineUserCount });
      return accepted
        ? { ok: true, accepted: true }
        : { ok: false, uncertain: true, error: 'Timeout recovery Send was attempted but acceptance could not be verified.' };
    } catch (error) {
      return { ok: false, notSent: !sendAttempted, uncertain: sendAttempted, error: error?.message || String(error) };
    } finally {
      messageTimeoutRecoveryRunning = false;
      completionCandidateSince = 0;
    }
  }

  function isVisible(el) {
    if (!el) return false;
    const style = getComputedStyle(el);
    return el.getClientRects().length > 0 && style.visibility !== 'hidden' && style.display !== 'none';
  }

  function getStopButton() {
    const composer = document.querySelector('form[data-chatgpt-composer]') ||
      findEditor()?.closest('form') || null;
    const scope = composer || document;
    const direct = [
      scope.querySelector('[data-testid="stop-button"]'),
      scope.querySelector('button[type="button"][aria-label="Stop"]'),
      scope.querySelector('button[aria-label*="Stop generating" i]'),
      scope.querySelector('button[title*="Stop generating" i]')
    ].find((button) => button && isVisible(button) && !button.disabled);
    if (direct) return direct;

    return [...scope.querySelectorAll('button')].find((button) => {
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
      let matches;
      try { matches = document.querySelectorAll(selector); }
      catch { return latestAssistantTurnWithoutHas(); }
      for (const el of matches) {
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
      // Unsupported execCommand / selection APIs are normal in some pages.
    }
    // Chromium may return false from execCommand without throwing, leaving
    // Lexical's contenteditable untouched. Verify the actual editor DOM
    // before dispatching input so React never receives a phantom insertion.
    if (normalizeText(editorText(el)) !== normalizeText(value)) {
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

  // Modern ChatGPT can render Send asynchronously after the extension writes
  // into an otherwise empty composer. Do not fail a reserved one-shot action
  // just because React has not painted that button within the first 80ms.
  async function waitForRecoverySendButton(editor, message, stillAuthorized, timeoutMs = 3000) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      const currentEditor = findEditor();
      // ChatGPT may remount its composer after Input. Preserve authorization
      // only when the replacement contains the exact message we inserted.
      if (!stillAuthorized() || !currentEditor || chatIsActivelyGenerating() ||
          normalizeText(editorText(currentEditor)) !== normalizeText(message)) return null;
      const send = findSendButton();
      if (send) return send;
      await sleep(100);
    }
    return null;
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

  function stableAssistantTurnKey(turn) {
    if (!turn) return '';
    return turn.getAttribute('data-turn-key') || turn.getAttribute('data-message-id') ||
      turn.querySelector('[data-message-id]')?.getAttribute('data-message-id') ||
      turn.getAttribute('data-testid') || turn.id || '';
  }

  function manualDefaultCandidate() {
    if (!readyForDelivery()) return null;
    const turn = latestAssistantTurn();
    if (!turn) return null;
    const latestUser = getUserMessages().at(-1);
    if (latestUser && (turn.compareDocumentPosition(latestUser) & Node.DOCUMENT_POSITION_FOLLOWING)) return null;
    const eventKey = stableAssistantTurnKey(turn);
    return eventKey ? { turn, eventKey } : null;
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
    if (interruptedRecoveryRunning) {
      const error = new Error('Interrupted-response recovery is active; RepoTunnel will retry later.');
      error.retryable = true;
      throw error;
    }
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

  async function performManualDefaultAction(expectedUrl, eventKey, exactMessage, action) {
    const message = String(exactMessage || '');
    const candidate = action === 'interrupted'
      ? findInterruptedResponse()
      : action === 'timeout'
        ? findMessageDeliveryTimeout()
        : action === 'normal'
          ? manualDefaultCandidate()
          : null;
    const editor = findEditor();
    if (
      manualDefaultActionRunning || interruptedRecoveryRunning || messageTimeoutRecoveryRunning || preparedDelivery ||
      !message.trim() || normalizedCurrentUrl() !== expectedUrl || candidate?.eventKey !== eventKey ||
      !editor || editorText(editor).trim() || !composerIsHumanIdle()
    ) return { ok: false, notSent: true, error: 'Default-message action is no longer safe.' };

    manualDefaultActionRunning = true;
    const baselineUserCount = getUserMessages().length;
    const baselineHumanInputAt = lastTrustedComposerInputAt;
    const baselineHumanInputRevision = recoveryHumanInputRevision;
    let sendAttempted = false;
    let messageWasPrepared = false;
    const sameBaseState = () => (
      normalizedCurrentUrl() === expectedUrl &&
      (findEditor() === editor || (messageWasPrepared &&
        normalizeText(editorText(findEditor())) === normalizeText(message))) &&
      getUserMessages().length === baselineUserCount &&
      lastTrustedComposerInputAt === baselineHumanInputAt &&
      recoveryHumanInputRevision === baselineHumanInputRevision &&
      // React may remount the same assistant turn while activating Send.
      // Compare its stable identity, not the previous DOM element instance.
      stableAssistantTurnKey(latestAssistantTurn()) === eventKey
    );

    try {
      if (action === 'interrupted') {
        const stopButton = getStopButton();
        if (!stopButton || !sameBaseState() || findInterruptedResponse()?.eventKey !== eventKey) {
          return { ok: false, notSent: true, error: 'The interrupted response changed before Stop.' };
        }
        stopButton.click();
        const deadline = Date.now() + 10000;
        let idleSince = 0;
        while (Date.now() < deadline) {
          if (!sameBaseState() || editorText(editor).trim()) {
            return { ok: false, notSent: true, error: 'Chat navigation or human input cancelled the default-message recovery.' };
          }
          if (chatIsActivelyGenerating()) idleSince = 0;
          else {
            if (!idleSince) idleSince = Date.now();
            if (Date.now() - idleSince >= COMPLETION_SETTLE_MS) break;
          }
          await sleep(100);
        }
        if (chatIsActivelyGenerating() || !idleSince || Date.now() - idleSince < COMPLETION_SETTLE_MS) {
          return { ok: false, notSent: true, error: 'ChatGPT did not stop safely; the default message was not sent.' };
        }
      } else if (action === 'timeout') {
        if (!sameBaseState() || findMessageDeliveryTimeout()?.eventKey !== eventKey) {
          return { ok: false, notSent: true, error: 'The delivery-timeout response changed before recovery.' };
        }
        const stopButton = getStopButton();
        if (stopButton) {
          stopButton.click();
          const deadline = Date.now() + 10000;
          let idleSince = 0;
          while (Date.now() < deadline) {
            if (!sameBaseState() || editorText(editor).trim() || findMessageDeliveryTimeout()?.eventKey !== eventKey) {
              return { ok: false, notSent: true, error: 'Chat navigation or human input cancelled the default-message timeout recovery.' };
            }
            if (chatIsActivelyGenerating()) idleSince = 0;
            else {
              if (!idleSince) idleSince = Date.now();
              if (Date.now() - idleSince >= COMPLETION_SETTLE_MS) break;
            }
            await sleep(100);
          }
          if (chatIsActivelyGenerating() || !idleSince || Date.now() - idleSince < COMPLETION_SETTLE_MS) {
            return { ok: false, notSent: true, error: 'ChatGPT did not stop safely; the default timeout message was not sent.' };
          }
        }
      } else if (action === 'normal') {
        if (chatIsActivelyGenerating() || !sameBaseState() || manualDefaultCandidate()?.eventKey !== eventKey) {
          return { ok: false, notSent: true, error: 'The completed response changed before the default message could be sent.' };
        }
      }

      const authorization = await chrome.runtime.sendMessage({
        type: 'MANUAL_DEFAULT_ACTION_READY', eventKey, action
      });
      if (!authorization?.ok || !authorization.authorized || !sameBaseState() || editorText(editor).trim()) {
        return { ok: false, notSent: true, error: 'The manual chat changed during default-message delivery.' };
      }

      editor.focus();
      setEditorText(editor, message);
      messageWasPrepared = true;
      await sleep(80);
      const sendButton = await waitForRecoverySendButton(editor, message, sameBaseState);
      const finalAuthorization = await chrome.runtime.sendMessage({
        type: 'MANUAL_DEFAULT_ACTION_READY', eventKey, action
      });
      const modeStillValid = action === 'interrupted'
        ? !chatIsActivelyGenerating()
        : action === 'timeout'
          ? !chatIsActivelyGenerating() && findMessageDeliveryTimeout()?.eventKey === eventKey
          : !chatIsActivelyGenerating() && latestAssistantIsComplete() && stableAssistantTurnKey(latestAssistantTurn()) === eventKey;
      if (
        !finalAuthorization?.ok || !finalAuthorization.authorized || !sendButton || !sameBaseState() ||
        !modeStillValid || normalizeText(editorText(findEditor())) !== normalizeText(message)
      ) {
        if (
          lastTrustedComposerInputAt === baselineHumanInputAt &&
          recoveryHumanInputRevision === baselineHumanInputRevision &&
          normalizeText(editorText(findEditor())) === normalizeText(message)
        ) setEditorText(findEditor(), '');
        return { ok: false, notSent: true, error: 'The final default-message send check failed.' };
      }

      sendAttempted = true;
      sendButton.click();
      const accepted = await waitForAcceptedTurn({ message, baselineUserCount });
      return accepted
        ? { ok: true, accepted: true }
        : { ok: false, uncertain: true, error: 'Default-message Send was attempted but acceptance could not be verified.' };
    } catch (error) {
      return { ok: false, notSent: !sendAttempted, uncertain: sendAttempted, error: error?.message || String(error) };
    } finally {
      manualDefaultActionRunning = false;
      completionCandidateSince = 0;
    }
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

  async function performInterruptedRefreshRecovery(recovery) {
    if (interruptedRefreshRecoveryRunning || !recovery ||
        normalizedCurrentUrl() !== recovery.url || !String(recovery.message || '').trim()) return;
    interruptedRefreshRecoveryRunning = true;
    const baselineHumanInputAt = lastTrustedComposerInputAt;
    const baselineHumanInputRevision = recoveryHumanInputRevision;
    let baselineUserCount = null;
    let baselineLatestUserText = '';
    let editor = null;
    let stopClicked = false;
    let stableSince = 0;
    let previousTurnKey = '';
    let messageWasPrepared = false;
    let sendAttempted = false;
    let outcome = 'not_sent';
    let retryable = false;
    let failure = '';

    const finish = async (nextOutcome, canRetry, reason = '') => {
      outcome = nextOutcome;
      retryable = Boolean(canRetry);
      failure = reason;
      await chrome.runtime.sendMessage({
        type: 'CHAT_INTERRUPTED_REFRESH_COMPLETE',
        recovery,
        outcome,
        retryable,
        error: failure
      }).catch(() => {});
    };

    const safePageState = () => {
      const currentEditor = findEditor();
      if (normalizedCurrentUrl() !== recovery.url || !currentEditor) return false;
      if (currentEditor !== editor &&
          !(messageWasPrepared && normalizeText(editorText(currentEditor)) === normalizeText(recovery.message))) return false;
      if (lastTrustedComposerInputAt !== baselineHumanInputAt ||
          recoveryHumanInputRevision !== baselineHumanInputRevision) return false;
      if (baselineUserCount !== null) {
        const users = getUserMessages();
        if (users.length !== baselineUserCount ||
            normalizeText(getUserText(users.at(-1))) !== baselineLatestUserText) return false;
      }
      return true;
    };

    try {
      const deadline = Date.now() + 120000;
      while (Date.now() < deadline) {
        if (normalizedCurrentUrl() !== recovery.url) {
          await finish('not_sent', false, 'Conversation changed during post-refresh recovery.');
          return;
        }
        if (document.readyState !== 'complete') {
          stableSince = 0;
          await sleep(500);
          continue;
        }

        editor = findEditor();
        if (!editor) {
          stableSince = 0;
          await sleep(500);
          continue;
        }
        if (editorText(editor).trim()) {
          await finish('not_sent', false, 'Composer has a draft; recovery will not overwrite it.');
          return;
        }
        if (!composerIsHumanIdle()) {
          await finish('not_sent', false, 'Recent human composer input prevented recovery.');
          return;
        }

        if (baselineUserCount === null) {
          const users = getUserMessages();
          baselineUserCount = users.length;
          baselineLatestUserText = normalizeText(getUserText(users.at(-1)));
        }

        const stopButton = getStopButton();
        if (stopButton) {
          stableSince = 0;
          if (!stopClicked) {
            if (!safePageState()) {
              await finish('not_sent', false, 'Page state changed before the post-refresh Stop action.');
              return;
            }
            const currentStop = getStopButton();
            if (!currentStop) continue;
            currentStop.click();
            stopClicked = true;
          }
          await sleep(400);
          continue;
        }

        const turnKey = stableAssistantTurnKey(latestRecoveryAssistantTurn());
        if (turnKey && previousTurnKey && turnKey !== previousTurnKey) stableSince = 0;
        if (turnKey) previousTurnKey = turnKey;
        if (!stableSince) stableSince = Date.now();

        // After a refresh, wait for the app to become quiet. The interrupted banner
        // may no longer exist, so recovery is keyed by the persisted incident instead.
        if (Date.now() - stableSince >= 5000) break;
        await sleep(400);
      }

      if (!editor || document.readyState !== 'complete' || chatIsActivelyGenerating() ||
          !stableSince || Date.now() - stableSince < 5000) {
        await finish('not_sent', true, 'ChatGPT did not become stably ready after refresh; recovery can retry without another refresh.');
        return;
      }
      editor = findEditor();
      if (!editor || editorText(editor).trim() || !safePageState()) {
        await finish('not_sent', false, 'Post-refresh composer or conversation state changed; no message was sent.');
        return;
      }

      const authorization = await chrome.runtime.sendMessage({
        type: 'CHAT_INTERRUPTED_REFRESH_AUTHORIZE',
        recovery
      });
      if (!authorization?.ok || !authorization.authorized || !safePageState()) {
        await finish('not_sent', false, 'The saved chat changed or recovery was paused before sending.');
        return;
      }

      editor.focus();
      setEditorText(editor, recovery.message);
      messageWasPrepared = true;
      await sleep(100);

      const sendButton = await waitForRecoverySendButton(editor, recovery.message, safePageState);
      const currentEditor = findEditor();
      const ownMessageStillInComposer = currentEditor &&
        lastTrustedComposerInputAt === baselineHumanInputAt &&
        recoveryHumanInputRevision === baselineHumanInputRevision &&
        normalizeText(editorText(currentEditor)) === normalizeText(recovery.message);
      if (!sendButton) {
        if (ownMessageStillInComposer) setEditorText(currentEditor, '');
        await finish('not_sent', true, 'ChatGPT send control is not ready after refresh; recovery can retry without refreshing again.');
        return;
      }

      const finalAuthorization = await chrome.runtime.sendMessage({
        type: 'CHAT_INTERRUPTED_REFRESH_AUTHORIZE',
        recovery
      });
      if (!finalAuthorization?.ok || !finalAuthorization.authorized ||
          !safePageState() || normalizeText(editorText(findEditor())) !== normalizeText(recovery.message)) {
        if (ownMessageStillInComposer) setEditorText(currentEditor, '');
        await finish('not_sent', false, 'Final post-refresh authorization or human-input checks failed.');
        return;
      }
      if (chatIsActivelyGenerating()) {
        if (ownMessageStillInComposer) setEditorText(currentEditor, '');
        await finish('not_sent', true, 'ChatGPT resumed generation before Send; recovery can retry after it settles.');
        return;
      }

      const beginSend = await chrome.runtime.sendMessage({
        type: 'CHAT_INTERRUPTED_REFRESH_BEGIN_SEND',
        recovery
      });
      if (!beginSend?.ok || !beginSend.authorized || !safePageState()) {
        const currentEditor = findEditor();
        if (currentEditor && lastTrustedComposerInputAt === baselineHumanInputAt &&
            recoveryHumanInputRevision === baselineHumanInputRevision &&
            normalizeText(editorText(currentEditor)) === normalizeText(recovery.message)) setEditorText(currentEditor, '');
        await finish('not_sent', false, 'Final recovery authorization expired before Send.');
        return;
      }

      sendAttempted = true;
      sendButton.click();
      const accepted = await waitForAcceptedTurn({ message: recovery.message, baselineUserCount });
      await finish(accepted ? 'delivered' : 'uncertain', false,
        accepted ? '' : 'Send was attempted after refresh, but ChatGPT acceptance could not be verified.');
    } catch (error) {
      await finish(sendAttempted ? 'uncertain' : 'not_sent', !sendAttempted,
        error?.message || String(error));
    } finally {
      interruptedRefreshRecoveryRunning = false;
      completionCandidateSince = 0;
    }
  }

  async function pollBridge() {
    if (bridgePollRunning || interruptedRecoveryRunning || messageTimeoutRecoveryRunning ||
        manualDefaultActionRunning || interruptedRefreshRecoveryRunning) return;
    bridgePollRunning = true;
    try {
      const claimedRefresh = await chrome.runtime.sendMessage({
        type: 'CHAT_INTERRUPTED_REFRESH_CLAIM'
      });
      if (claimedRefresh?.recovery) {
        await performInterruptedRefreshRecovery(claimedRefresh.recovery);
        return;
      }

      const interrupted = findInterruptedResponse();
      const editor = findEditor();
      // Two distinct cases: if generation is still running, stop and continue
      // in-place. Refresh only when the exact interruption notice remains and
      // ChatGPT is already stopped; post-refresh recovery is handled by the claim above.
      if (interrupted && !preparedDelivery && editor && !editorText(editor).trim() && composerIsHumanIdle()) {
        const userCount = getUserMessages().length;
        if (getStopButton()) {
          const continuationRecovery = await chrome.runtime.sendMessage({
            type: 'CHAT_CONNECTION_INTERRUPTED',
            eventKey: interrupted.eventKey,
            userCount
          });
          if (continuationRecovery?.handled) return;
        } else {
          const refreshRecovery = await chrome.runtime.sendMessage({
            type: 'CHAT_INTERRUPTED_REFRESH_BEGIN',
            eventKey: interrupted.eventKey,
            userCount
          });
          if (refreshRecovery?.handled) return;

          const continuationRecovery = await chrome.runtime.sendMessage({
            type: 'CHAT_CONNECTION_INTERRUPTED',
            eventKey: interrupted.eventKey,
            userCount
          });
          if (continuationRecovery?.handled) return;
        }
        const manualRecovery = await chrome.runtime.sendMessage({
          type: 'CHAT_MANUAL_DEFAULT_ACTION', action: 'interrupted', eventKey: interrupted.eventKey,
          userCount: getUserMessages().length
        });
        if (manualRecovery?.handled) return;
      }

      const messageTimeout = findMessageDeliveryTimeout();
      if (messageTimeout && !preparedDelivery && editor && !editorText(editor).trim() && composerIsHumanIdle()) {
        const continuationRecovery = await chrome.runtime.sendMessage({
          type: 'CHAT_MESSAGE_DELIVERY_TIMED_OUT', eventKey: messageTimeout.eventKey,
          userCount: getUserMessages().length
        });
        if (continuationRecovery?.handled) return;
        const manualRecovery = await chrome.runtime.sendMessage({
          type: 'CHAT_MANUAL_DEFAULT_ACTION', action: 'timeout', eventKey: messageTimeout.eventKey,
          userCount: getUserMessages().length
        });
        if (manualRecovery?.handled) return;
      }

      const manualCandidate = manualDefaultCandidate();
      if (manualCandidate && !preparedDelivery && editor && !editorText(editor).trim() && composerIsHumanIdle()) {
        const manualDelivery = await chrome.runtime.sendMessage({
          type: 'CHAT_MANUAL_DEFAULT_ACTION', action: 'normal', eventKey: manualCandidate.eventKey,
          userCount: getUserMessages().length
        });
        if (manualDelivery?.handled) return;
      }

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
    if ((interruptedRecoveryRunning || messageTimeoutRecoveryRunning || manualDefaultActionRunning || interruptedRefreshRecoveryRunning) && event.isTrusted && repoTunnelComposerWriteDepth === 0) {
      const editor = findEditor();
      if (editor && event.target instanceof Node && (event.target === editor || editor.contains(event.target))) recoveryHumanInputRevision += 1;
    }
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
    if (message?.type === 'REPOTUNNEL_BACKGROUND_TICK') {
      (async () => {
        if (normalizedCurrentUrl() !== String(message.expectedUrl || '')) return { ok: true, ignored: true };
        await pollBridge();
        return { ok: true };
      })().then(sendResponse).catch(error => sendResponse({ ok: false, error: error?.message || String(error) }));
      return true;
    }
    if (message?.type === 'RECOVER_INTERRUPTED_RESPONSE') {
      recoverInterruptedResponse(String(message.expectedUrl || ''), String(message.eventKey || ''))
        .then(sendResponse)
        .catch(error => sendResponse({ ok: false, notSent: true, error: error?.message || String(error) }));
      return true;
    }
    if (message?.type === 'RECOVER_MESSAGE_DELIVERY_TIMEOUT') {
      recoverMessageDeliveryTimeout(String(message.expectedUrl || ''), String(message.eventKey || ''))
        .then(sendResponse)
        .catch(error => sendResponse({ ok: false, notSent: true, error: error?.message || String(error) }));
      return true;
    }
    if (message?.type === 'REPOTUNNEL_MANUAL_DEFAULT_ACTION') {
      performManualDefaultAction(
        String(message.expectedUrl || ''),
        String(message.eventKey || ''),
        String(message.message || ''),
        String(message.action || '')
      ).then(sendResponse)
        .catch(error => sendResponse({ ok: false, notSent: true, error: error?.message || String(error) }));
      return true;
    }

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

  // Fast foreground checks complement background alarms for hidden tabs.
  setInterval(pollBridge, 1500);
  setTimeout(pollBridge, 750);
})();
