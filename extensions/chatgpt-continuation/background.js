const MAX_TARGETS = 5;
const BRIDGE_BASE = 'http://127.0.0.1:43185/v1';
const BRIDGE_PROTOCOL_VERSION = 2;
const EXTENSION_VERSION = chrome.runtime.getManifest().version;
const BRIDGE_POLL_MS = 1200;
const BRIDGE_RETRY_MAX_MS = 30000;
const BRIDGE_SESSION_KEY = 'repoTunnelContinuationRuntimeV1';
const HEARTBEAT_ALARM = 'repotunnel-continuation-heartbeat';
const TARGETS_KEY = 'repoTunnelContinuationTargetsV1';
const bridgePollState = new Map();
let bridgeLastOkAt = 0;
let targetMutationQueue = Promise.resolve();

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function conversationIdFromUrl(url) {
  const segments = url.pathname.split('/').filter(Boolean);
  if (
    segments.length !== 2 ||
    segments[0] !== 'c' ||
    !/^[A-Za-z0-9_-]{1,160}$/.test(segments[1])
  ) {
    throw new Error('Use an exact https://chatgpt.com/c/<conversation-id> URL.');
  }
  return segments[1];
}

function normalizeConversationUrl(raw) {
  const url = new URL(String(raw || '').trim());
  if (url.protocol !== 'https:' || url.hostname !== 'chatgpt.com') {
    throw new Error('Use an exact https://chatgpt.com/c/<conversation-id> URL.');
  }
  const conversationId = conversationIdFromUrl(url);
  return `https://chatgpt.com/c/${conversationId}`;
}

function isConversationUrl(raw) {
  try {
    normalizeConversationUrl(raw);
    return true;
  } catch {
    return false;
  }
}

function makeTargetId(url) {
  const conversationId = conversationIdFromUrl(new URL(normalizeConversationUrl(url)));
  if (conversationId.length <= 120) return `chat-${conversationId}`;

  // Keep the bridge target ID within RepoTunnel's 128-character limit while
  // retaining both ends of an unusually long conversation identifier.
  return `chat-${conversationId.slice(0, 58)}-${conversationId.slice(-58)}`;
}

function normalizeTarget(target) {
  const url = normalizeConversationUrl(target?.url || '');
  return {
    id: makeTargetId(url),
    url,
    title: String(target?.title || '').trim().slice(0, 160),
    lastStatus: String(target?.lastStatus || ''),
    lastStatusKind: String(target?.lastStatusKind || ''),
    lastRunAt: Number(target?.lastRunAt || 0)
  };
}

async function loadTargets() {
  const state = await chrome.storage.local.get(TARGETS_KEY);
  const source = state[TARGETS_KEY];

  const clean = [];
  for (const target of (Array.isArray(source) ? source : []).slice(0, MAX_TARGETS)) {
    try {
      clean.push(normalizeTarget(target));
    } catch {}
  }

  if (!Array.isArray(source)) {
    await chrome.storage.local.set({ [TARGETS_KEY]: clean });
  }

  return clean;
}

function mutateTargets(mutator) {
  const run = targetMutationQueue.then(async () => {
    const current = await loadTargets();
    const next = await mutator(current.map((target) => ({ ...target })));
    const clean = [];

    for (const target of (Array.isArray(next) ? next : current).slice(0, MAX_TARGETS)) {
      try {
        clean.push(normalizeTarget(target));
      } catch {}
    }

    if (JSON.stringify(clean) !== JSON.stringify(current)) {
      await chrome.storage.local.set({ [TARGETS_KEY]: clean });
    }
    return clean;
  });

  targetMutationQueue = run.catch(() => {});
  return run;
}

async function setTargetStatus(targetId, message, kind = '') {
  const nextMessage = String(message || '').slice(0, 300);
  await mutateTargets((targets) => {
    const target = targets.find((item) => item.id === targetId);
    if (
      target &&
      (target.lastStatus !== nextMessage || target.lastStatusKind !== kind)
    ) {
      target.lastStatus = nextMessage;
      target.lastStatusKind = kind;
      target.lastRunAt = Date.now();
    }
    return targets;
  });
}

async function addTarget(rawUrl, title = '') {
  const url = normalizeConversationUrl(rawUrl);
  let existed = false;
  let targetId = null;

  const targets = await mutateTargets((items) => {
    const existing = items.find((item) => item.url === url);
    if (existing) {
      existed = true;
      targetId = existing.id;
      if (title) existing.title = String(title).trim().slice(0, 160);
      return items;
    }

    if (items.length >= MAX_TARGETS) {
      throw new Error(`RepoTunnel Continuation supports up to ${MAX_TARGETS} saved chats.`);
    }

    const target = normalizeTarget({ url, title });
    targetId = target.id;
    items.push(target);
    return items;
  });

  await refreshAllTargets();
  return { targets, targetId, existed };
}

async function removeTarget(targetId) {
  bridgePollState.delete(String(targetId || ''));
  return mutateTargets((targets) => targets.filter((item) => item.id !== targetId));
}

async function getExactOpenTab(url) {
  const wanted = normalizeConversationUrl(url);
  const tabs = await chrome.tabs.query({ url: ['https://chatgpt.com/*'] });
  const matches = tabs.filter((tab) => {
    if (!tab.url) return false;
    try {
      return normalizeConversationUrl(tab.url) === wanted;
    } catch {
      return false;
    }
  });

  if (matches.length === 0) {
    throw new Error('The saved ChatGPT conversation is not open.');
  }
  if (matches.length > 1) {
    throw new Error('Duplicate tabs have the same saved ChatGPT conversation; RepoTunnel will not guess.');
  }
  return matches[0];
}

async function ensureContentScript(tabId) {
  try {
    const pong = await chrome.tabs.sendMessage(tabId, { type: 'REPOTUNNEL_BRIDGE_PING' });
    if (pong?.ok && pong?.version === EXTENSION_VERSION) return true;
  } catch {}

  try {
    await chrome.scripting.executeScript({
      target: { tabId },
      files: ['content.js']
    });
    const pong = await chrome.tabs.sendMessage(tabId, { type: 'REPOTUNNEL_BRIDGE_PING' });
    return Boolean(pong?.ok && pong?.version === EXTENSION_VERSION);
  } catch {
    return false;
  }
}

async function sendTabMessageWithRetry(tabId, message, timeoutMs = 10000) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;

  while (Date.now() < deadline) {
    try {
      return await chrome.tabs.sendMessage(tabId, message);
    } catch (error) {
      lastError = error;
      await ensureContentScript(tabId);
      await sleep(250);
    }
  }

  throw lastError || new Error('Could not communicate with the saved ChatGPT tab.');
}

async function bridgeFetch(path, body = null, timeoutMs = 2500) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);

  try {
    const response = await fetch(`${BRIDGE_BASE}${path}`, {
      method: body === null ? 'GET' : 'POST',
      headers: body === null ? undefined : { 'Content-Type': 'application/json' },
      body: body === null ? undefined : JSON.stringify(body),
      cache: 'no-store',
      signal: controller.signal
    });

    if (!response.ok) {
      let detail = '';
      try {
        const payload = await response.json();
        detail = payload?.error || '';
      } catch {}
      throw new Error(detail || `RepoTunnel bridge returned HTTP ${response.status}.`);
    }

    const payload = await response.json();
    bridgeLastOkAt = Date.now();
    await chrome.storage.session.set({
      [BRIDGE_SESSION_KEY]: { lastOkAt: bridgeLastOkAt }
    }).catch(() => {});
    return payload;
  } finally {
    clearTimeout(timer);
  }
}

async function abortPreparedMessage(tabId, target, job) {
  try {
    await sendTabMessageWithRetry(tabId, {
      type: 'REPOTUNNEL_ABORT_PREPARED',
      expectedUrl: target.url,
      jobId: job.jobId,
      revision: job.revision,
      message: job.message
    }, 3000);
  } catch {}
}

async function acknowledgeJob(targetId, job, status, retryable, error = null) {
  return bridgeFetch('/ack', {
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    jobId: job.jobId,
    targetId,
    claimToken: job.claimToken,
    revision: job.revision,
    status,
    retryable: Boolean(retryable),
    error
  });
}

function retryDelayMs(failures) {
  const exponent = Math.min(Math.max(0, failures - 1), 5);
  const base = Math.min(BRIDGE_RETRY_MAX_MS, 1000 * (2 ** exponent));
  return base + Math.floor(Math.random() * 350);
}

async function pollBridge(tabId, rawUrl, title = '', readyForDelivery = false) {
  if (!Number.isInteger(tabId) || !isConversationUrl(rawUrl)) return { ignored: true };

  const wanted = normalizeConversationUrl(rawUrl);
  const targets = await loadTargets();
  const target = targets.find((item) => item.url === wanted);
  if (!target) return { ignored: true };

  let exactTab;
  try {
    exactTab = await getExactOpenTab(target.url);
  } catch (error) {
    await setTargetStatus(target.id, error?.message || String(error), 'waiting');
    return { ignored: true };
  }
  if (exactTab.id !== tabId) return { ignored: true };

  const now = Date.now();
  const previous = bridgePollState.get(target.id) || {
    at: 0,
    running: false,
    failures: 0,
    nextRetryAt: 0
  };
  if (
    previous.running ||
    now < Number(previous.nextRetryAt || 0) ||
    now - Number(previous.at || 0) < BRIDGE_POLL_MS
  ) {
    return { ignored: true };
  }

  bridgePollState.set(target.id, { ...previous, at: now, running: true });

  try {
    const registration = await bridgeFetch('/register', {
      protocolVersion: BRIDGE_PROTOCOL_VERSION,
      targetId: target.id,
      url: target.url,
      title: title || target.title || exactTab.title || ''
    });

    if (registration?.protocolVersion !== BRIDGE_PROTOCOL_VERSION) {
      throw new Error(`RepoTunnel bridge protocol mismatch: expected ${BRIDGE_PROTOCOL_VERSION}.`);
    }

    bridgePollState.set(target.id, {
      at: Date.now(),
      running: true,
      failures: 0,
      nextRetryAt: 0
    });

    if (!readyForDelivery) {
      await setTargetStatus(target.id, 'Connected to RepoTunnel.', 'ok');
      return { ok: true, waitingForIdle: true };
    }

    const claim = await bridgeFetch('/claim', {
      protocolVersion: BRIDGE_PROTOCOL_VERSION,
      targetId: target.id
    });
    const job = claim?.job || null;
    if (!job) {
      await setTargetStatus(target.id, 'Connected to RepoTunnel.', 'ok');
      return { ok: true, pending: false };
    }

    let prepared;
    try {
      prepared = await sendTabMessageWithRetry(tabId, {
        type: 'REPOTUNNEL_PREPARE_MESSAGE',
        expectedUrl: target.url,
        jobId: job.jobId,
        workId: job.workId,
        revision: job.revision,
        message: job.message
      });
    } catch (error) {
      prepared = { ok: false, retryable: true, error: error?.message || String(error) };
    }

    if (!prepared?.ok) {
      const error = prepared?.error || 'ChatGPT message preparation failed.';
      await abortPreparedMessage(tabId, target, job);
      await acknowledgeJob(target.id, job, 'failed', prepared?.retryable !== false, error);
      await setTargetStatus(target.id, error, prepared?.retryable === false ? 'err' : 'waiting');
      return { ok: false, delivered: false, error };
    }

    let beginSendAuthorized = false;
    let beginSendError = null;
    for (let attempt = 0; attempt < 2; attempt += 1) {
      try {
        await bridgeFetch('/begin-send', {
          protocolVersion: BRIDGE_PROTOCOL_VERSION,
          jobId: job.jobId,
          targetId: target.id,
          claimToken: job.claimToken,
          revision: job.revision
        });
        beginSendAuthorized = true;
        break;
      } catch (error) {
        beginSendError = error;
        if (attempt === 0) await sleep(200);
      }
    }

    if (!beginSendAuthorized) {
      await abortPreparedMessage(tabId, target, job);
      const error = beginSendError?.message || String(beginSendError || 'begin-send failed');
      await setTargetStatus(target.id, `Send cancelled before dispatch: ${error}`, 'waiting');
      return { ok: false, cancelledBeforeSend: true, error };
    }

    let delivery;
    try {
      delivery = await chrome.tabs.sendMessage(tabId, {
        type: 'REPOTUNNEL_COMMIT_MESSAGE',
        expectedUrl: target.url,
        jobId: job.jobId,
        workId: job.workId,
        revision: job.revision,
        message: job.message
      });
    } catch (error) {
      delivery = { ok: false, uncertain: true, error: error?.message || String(error) };
    }

    const delivered = Boolean(delivery?.ok && delivery?.accepted);
    const definitelyNotSent = Boolean(delivery?.notSent);
    const ackStatus = delivered ? 'delivered' : definitelyNotSent ? 'not_sent' : 'uncertain';

    await acknowledgeJob(
      target.id,
      job,
      ackStatus,
      definitelyNotSent && delivery?.retryable !== false,
      delivered ? null : (delivery?.error || 'ChatGPT acceptance could not be proven.')
    );

    if (delivered) {
      await setTargetStatus(target.id, 'Continuation delivered and verified.', 'ok');
      return { ok: true, delivered: true };
    }

    if (definitelyNotSent) {
      const error = delivery?.error || 'Continuation was not sent; RepoTunnel may retry.';
      await setTargetStatus(target.id, error, delivery?.retryable === false ? 'err' : 'waiting');
      return { ok: false, notSent: true, error };
    }

    const error = delivery?.error || 'Delivery is uncertain; automatic resend is disabled.';
    await setTargetStatus(target.id, error, 'err');
    return { ok: false, uncertain: true, error };
  } catch (error) {
    const message = error?.message || String(error);
    const current = bridgePollState.get(target.id) || {};
    const failures = Number(current.failures || 0) + 1;
    bridgePollState.set(target.id, {
      ...current,
      at: Date.now(),
      running: true,
      failures,
      nextRetryAt: Date.now() + retryDelayMs(failures)
    });
    await setTargetStatus(target.id, `RepoTunnel bridge unavailable: ${message}`, 'err');
    return { ok: false, bridgeUnavailable: true, error: message };
  } finally {
    const current = bridgePollState.get(target.id) || {};
    bridgePollState.set(target.id, { ...current, at: Date.now(), running: false });
  }
}

async function getPopupState() {
  const targets = await loadTargets();
  const session = await chrome.storage.session.get(BRIDGE_SESSION_KEY).catch(() => ({}));
  const runtime = session?.[BRIDGE_SESSION_KEY] || {};
  const lastOkAt = Math.max(bridgeLastOkAt, Number(runtime.lastOkAt || 0));

  return {
    version: EXTENSION_VERSION,
    connected: targets.length > 0 && Date.now() - lastOkAt < 180000,
    targets
  };
}

async function ensureHeartbeatAlarm() {
  const alarm = await chrome.alarms.get(HEARTBEAT_ALARM);
  if (!alarm) {
    await chrome.alarms.create(HEARTBEAT_ALARM, {
      when: Date.now() + 5000,
      periodInMinutes: 1
    });
  }
}

async function refreshAllTargets() {
  const targets = await loadTargets();
  for (const target of targets) {
    try {
      const tab = await getExactOpenTab(target.url);
      if (!Number.isInteger(tab?.id) || !tab.url) continue;
      await ensureContentScript(tab.id);
      await pollBridge(tab.id, tab.url, tab.title || target.title || '', false);
    } catch {}
  }
}

async function recoverLifecycle() {
  await loadTargets();
  await ensureHeartbeatAlarm();
  await refreshAllTargets();
}

chrome.runtime.onInstalled.addListener(() => {
  recoverLifecycle().catch(() => {});
});

chrome.runtime.onStartup.addListener(() => {
  recoverLifecycle().catch(() => {});
});

recoverLifecycle().catch(() => {});

async function refreshTab(tabId, tabHint = null) {
  try {
    const tab = tabHint?.id === tabId ? tabHint : await chrome.tabs.get(tabId);
    if (!tab?.url || !isConversationUrl(tab.url)) return;
    const targets = await loadTargets();
    if (!targets.some((target) => target.url === normalizeConversationUrl(tab.url))) return;
    await ensureContentScript(tabId);
    await pollBridge(tabId, tab.url, tab.title || '', false);
  } catch {}
}

chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  if (changeInfo.status === 'complete' || typeof changeInfo.url === 'string') {
    refreshTab(tabId, tab).catch(() => {});
  }
});

chrome.tabs.onReplaced.addListener((addedTabId) => {
  refreshTab(addedTabId).catch(() => {});
});

chrome.tabs.onActivated.addListener(({ tabId }) => {
  refreshTab(tabId).catch(() => {});
});

chrome.tabs.onRemoved.addListener(() => {
  refreshAllTargets().catch(() => {});
});

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === HEARTBEAT_ALARM) refreshAllTargets().catch(() => {});
});

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  (async () => {
    if (message?.type === 'GET_STATE') {
      sendResponse({ ok: true, state: await getPopupState() });
      return;
    }

    if (message?.type === 'ADD_TARGET') {
      sendResponse({ ok: true, ...(await addTarget(message.url, message.title)) });
      return;
    }

    if (message?.type === 'REMOVE_TARGET') {
      sendResponse({ ok: true, targets: await removeTarget(String(message.targetId || '')) });
      return;
    }

    if (message?.type === 'REFRESH_BRIDGE') {
      await refreshAllTargets();
      sendResponse({ ok: true, state: await getPopupState() });
      return;
    }

    if (message?.type === 'REPOTUNNEL_BRIDGE_POLL') {
      if (!sender.tab?.id || !sender.tab.url) {
        sendResponse({ ok: false, error: 'Chat tab information is unavailable.' });
        return;
      }
      const result = await pollBridge(
        sender.tab.id,
        sender.tab.url,
        String(message.title || sender.tab.title || ''),
        Boolean(message.readyForDelivery)
      );
      sendResponse({ ok: true, ...result });
      return;
    }

    sendResponse({ ok: false, error: 'Unknown RepoTunnel Continuation request.' });
  })().catch((error) => {
    sendResponse({ ok: false, error: error?.message || String(error) });
  });

  return true;
});
