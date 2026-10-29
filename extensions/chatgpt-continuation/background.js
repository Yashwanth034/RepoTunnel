const MAX_TARGETS = 5;
const BRIDGE_BASE = 'http://127.0.0.1:43185/v1';
const BRIDGE_PROTOCOL_VERSION = 2;
const EXTENSION_VERSION = chrome.runtime.getManifest().version;
const BRIDGE_POLL_MS = 1200;
const BRIDGE_RETRY_MAX_MS = 30000;
const BRIDGE_SESSION_KEY = 'repoTunnelContinuationRuntimeV1';
const HEARTBEAT_ALARM = 'repotunnel-continuation-heartbeat';
const TARGETS_KEY = 'repoTunnelContinuationTargetsV1';
const MANUAL_TARGETS_KEY = 'repoTunnelManualDefaultTargetsV1';
const bridgePollState = new Map();
let bridgeLastOkAt = 0;
let targetMutationQueue = Promise.resolve();
let manualTargetMutationQueue = Promise.resolve();

const CONNECTION_RECOVERY_PREFIX = 'repoTunnelConnectionRecovery:';
const MANUAL_DEFAULT_ACTION_PREFIX = 'repoTunnelManualDefaultAction:';
const manualDefaultActionsInFlight = new Set();
const MESSAGE_TIMEOUT_RECOVERY_PREFIX = 'repoTunnelMessageTimeoutRecovery:';
const INTERRUPTED_REFRESH_PREFIX = 'repoTunnelInterruptedRefresh:';
// One initial attempt and up to three known-not-sent retries per incident.
// Backoff avoids repeated refresh/send cycles when ChatGPT remains unhealthy.
// "uncertain" and "delivered" NEVER become eligible for automatic retry.
const MAX_KNOWN_UNSENT_ATTEMPTS = 4;
const KNOWN_UNSENT_RETRY_DELAYS_MS = [5000, 15000, 45000, 120000];

function sameRecoveryIncident(record, url, eventKey, userCount = null) {
  return record?.url === url && (record.eventKey === eventKey ||
    (Number.isSafeInteger(userCount) && Number.isSafeInteger(record.userCount) &&
      record.userCount === userCount));
}

function sameManualDefaultIncident(record, url, eventKey, userCount = null) {
  if (record?.url !== url || record.eventKey !== eventKey) return false;
  const currentCount = validUserCount(userCount);
  const previousCount = validUserCount(record.userCount);

  // Prefer assistant-turn identity. If ChatGPT reuses that key, an advanced user
  // count marks a new cycle. Counts alone are not identity because history virtualizes.
  return !(currentCount !== null && previousCount !== null && currentCount > previousCount);
}

function manualActionMatchesCurrentTarget(record, target, tabId, eventKey, action) {
  return Boolean(
    record?.status === 'recovering' &&
    record.eventKey === eventKey &&
    record.action === action &&
    record.tabId === tabId &&
    record.url === target.url &&
    record.message === target.message &&
    record.controlRevision === target.controlRevision
  );
}
function validUserCount(value) {
  return Number.isSafeInteger(value) && value >= 0 ? value : null;
}
function knownUnsentRetryDecision(record, now = Date.now()) {
  if (!record || record.status !== 'not_sent') return { allowed: false };
  const attempts = Math.max(1, Number(record.attempts) || 1);
  if (attempts >= MAX_KNOWN_UNSENT_ATTEMPTS) return { allowed: false, exhausted: true };
  return { allowed: now >= Number(record.nextRetryAt || 0), exhausted: false };
}
function markKnownUnsent(record) {
  record.status = 'not_sent';
  const attempts = Math.max(1, Number(record.attempts) || 1);
  record.nextRetryAt = Date.now() +
    KNOWN_UNSENT_RETRY_DELAYS_MS[Math.min(attempts - 1, KNOWN_UNSENT_RETRY_DELAYS_MS.length - 1)];
}
function knownUnsentStatusSuffix(record) {
  return Math.max(1, Number(record?.attempts) || 1) >= MAX_KNOWN_UNSENT_ATTEMPTS
    ? ' Automatic retries paused after four known-unsent attempts. This incident needs manual attention; a genuinely new turn can start fresh recovery.'
    : ' Message definitely not sent; another safe attempt is scheduled after a cooldown.';
}
async function markOrphanedSendUncertain(record, key) {
  // A service worker could be suspended after dispatching Send but before
  // persisting the result. Treat abandoned "recovering" as UNCERTAIN, never
  // as known-not-sent or eligible for an automatic retry.
  if (record?.status === 'recovering' && Date.now() - record.startedAt >= 60000) {
    record.status = 'uncertain';
    await chrome.storage.session.set({ [key]: record });
  }
  return record;
}
function recoveryAttemptCount(previous, url, eventKey, userCount = null) {
  return sameRecoveryIncident(previous, url, eventKey, userCount)
    ? Math.max(1, Number(previous.attempts) || 1) + 1 : 1;
}
const connectionRecoveriesInFlight = new Set();
const messageTimeoutRecoveriesInFlight = new Set();
const backgroundTicksInFlight = new Set();
const BACKGROUND_TAB_PROTECTION_KEY = 'repoTunnelBackgroundTabProtectionV1';
const BACKGROUND_TAB_SESSION_KEY = 'repoTunnelBackgroundTabSessionV1';
let backgroundTabProtectionQueue = Promise.resolve();
let backgroundSessionResetting = false;

async function connectionRecoveryIsRunning(targetId) {
  if (connectionRecoveriesInFlight.has(targetId)) return true;
  const key = CONNECTION_RECOVERY_PREFIX + targetId;
  const state = await chrome.storage.session.get(key);
  const recovery = state[key];
  return connectionRecoveriesInFlight.has(targetId) || Boolean(
    recovery?.status === 'recovering' && Date.now() - recovery.startedAt < 60000
  );
}

async function messageTimeoutRecoveryIsRunning(targetId) {
  if (messageTimeoutRecoveriesInFlight.has(targetId)) return true;
  const key = MESSAGE_TIMEOUT_RECOVERY_PREFIX + targetId;
  const state = await chrome.storage.session.get(key);
  const recovery = state[key];
  return messageTimeoutRecoveriesInFlight.has(targetId) || Boolean(
    recovery?.status === 'recovering' && Date.now() - recovery.startedAt < 60000
  );
}

async function anyRecoveryIsRunning(targetId) {
  return await connectionRecoveryIsRunning(targetId) || await messageTimeoutRecoveryIsRunning(targetId);
}

async function recoveryTargetForTab(tabId, rawUrl) {
  if (!Number.isInteger(tabId) || !isConversationUrl(rawUrl)) return null;
  const url = normalizeConversationUrl(rawUrl);
  const targets = await loadTargets();
  const target = targets.find(item => item.url === url);
  if (!target) return null;
  try {
    const tab = await getExactOpenTab(url);
    return tab.id === tabId ? target : null;
  } catch {
    return null;
  }
}

async function recoverInterruptedChat(tabId, rawUrl, eventKey, userCount = null) {
  if (typeof eventKey !== 'string' || !eventKey || eventKey.length > 400) return { ignored: true };
  const target = await recoveryTargetForTab(tabId, rawUrl);
  if (!target || bridgePollState.get(target.id)?.running || connectionRecoveriesInFlight.has(target.id)) {
    return { ignored: true };
  }
  connectionRecoveriesInFlight.add(target.id);
  const key = CONNECTION_RECOVERY_PREFIX + target.id;
  let recovery = null;
  let previous = null;
  let dispatched = false;
  try {
    const saved = await chrome.storage.session.get(key);
    previous = await markOrphanedSendUncertain(saved[key], key);
    if (previous?.url === target.url &&
        (sameRecoveryIncident(previous, target.url, eventKey, userCount) || previous.status === 'recovering')) {
      if (previous.status !== 'not_sent') return { handled: true, ignored: true };
      const decision = knownUnsentRetryDecision(previous);
      if (!decision.allowed) return { handled: true, waiting: !decision.exhausted, retryExhausted: Boolean(decision.exhausted) };
    }
    recovery = {
      eventKey, tabId, url: target.url, startedAt: Date.now(),
      status: 'recovering', userCount, attempts: recoveryAttemptCount(previous, target.url, eventKey, userCount)
    };
    // Save before dispatch: a worker restart or lost response must not cause
    // another Stop/Send attempt for the same interrupted assistant turn.
    await chrome.storage.session.set({ [key]: recovery });
    const current = await recoveryTargetForTab(tabId, rawUrl);
    if (!current || current.id !== target.id || bridgePollState.get(target.id)?.running) {
      return { handled: true, notSent: true };
    }
    let result;
    try {
      // Every Send attempt is single-shot. Only a confirmed not_sent result
      // permits another reserved attempt after the retry cooldown.
      dispatched = true;
      result = await chrome.tabs.sendMessage(tabId, {
        type: 'RECOVER_INTERRUPTED_RESPONSE', expectedUrl: target.url, eventKey
      });
    } catch (error) {
      result = { uncertain: true, error: error?.message || String(error) };
    }
    const delivered = Boolean(result?.ok && result.accepted);
    recovery.status = delivered ? 'delivered' : result?.notSent ? 'not_sent' : 'uncertain';
    if (recovery.status === 'not_sent') markKnownUnsent(recovery);
    await setTargetStatus(target.id, delivered
      ? 'Interrupted response stopped. “continue” delivered and verified.'
      : result?.notSent
        ? (result?.error || 'Interrupted response was not sent.') + knownUnsentStatusSuffix(recovery)
        : result?.error || 'Interrupted-response delivery is uncertain; automatic replay is disabled.',
      delivered ? 'ok' : 'err');
    return { handled: true, delivered, notSent: Boolean(result?.notSent), uncertain: !delivered && !result?.notSent };
  } finally {
    if (recovery) {
      if (!dispatched) {
        // No browser action was dispatched. Restore the prior ledger so a
        // temporary admission race can retry without replaying any action.
        if (previous) await chrome.storage.session.set({ [key]: previous }).catch(() => {});
        else await chrome.storage.session.remove(key).catch(() => {});
      } else {
        if (recovery.status === 'recovering') recovery.status = 'uncertain';
        await chrome.storage.session.set({ [key]: recovery }).catch(() => {});
      }
    }
    connectionRecoveriesInFlight.delete(target.id);
  }
}

async function authorizeConnectionRecovery(tabId, rawUrl, eventKey) {
  const target = await recoveryTargetForTab(tabId, rawUrl);
  if (!target || !connectionRecoveriesInFlight.has(target.id) || bridgePollState.get(target.id)?.running) return false;
  const key = CONNECTION_RECOVERY_PREFIX + target.id;
  const state = await chrome.storage.session.get(key);
  const recovery = state[key];
  return Boolean(recovery?.status === 'recovering' && recovery.eventKey === eventKey && recovery.tabId === tabId && recovery.url === target.url);
}

async function recoverMessageDeliveryTimeout(tabId, rawUrl, eventKey, userCount = null) {
  if (typeof eventKey !== 'string' || !eventKey || eventKey.length > 400) return { ignored: true };
  const target = await recoveryTargetForTab(tabId, rawUrl);
  if (
    !target || bridgePollState.get(target.id)?.running ||
    connectionRecoveriesInFlight.has(target.id) || messageTimeoutRecoveriesInFlight.has(target.id)
  ) return { ignored: true };

  messageTimeoutRecoveriesInFlight.add(target.id);
  const key = MESSAGE_TIMEOUT_RECOVERY_PREFIX + target.id;
  let recovery = null;
  let previous = null;
  let dispatched = false;
  try {
    const saved = await chrome.storage.session.get(key);
    previous = await markOrphanedSendUncertain(saved[key], key);
    if (previous?.url === target.url &&
        (sameRecoveryIncident(previous, target.url, eventKey, userCount) || previous.status === 'recovering')) {
      if (previous.status !== 'not_sent') return { handled: true, ignored: true };
      const decision = knownUnsentRetryDecision(previous);
      if (!decision.allowed) return { handled: true, waiting: !decision.exhausted, retryExhausted: Boolean(decision.exhausted) };
    }

    recovery = {
      eventKey, tabId, url: target.url, startedAt: Date.now(),
      status: 'recovering', userCount, attempts: recoveryAttemptCount(previous, target.url, eventKey, userCount)
    };
    // Persist before dispatch so an uncertain browser response can never replay the same timeout turn.
    await chrome.storage.session.set({ [key]: recovery });
    const current = await recoveryTargetForTab(tabId, rawUrl);
    if (
      !current || current.id !== target.id || bridgePollState.get(target.id)?.running ||
      connectionRecoveriesInFlight.has(target.id)
    ) return { handled: true, notSent: true };

    let result;
    try {
      dispatched = true;
      result = await chrome.tabs.sendMessage(tabId, {
        type: 'RECOVER_MESSAGE_DELIVERY_TIMEOUT', expectedUrl: target.url, eventKey
      });
    } catch (error) {
      result = { uncertain: true, error: error?.message || String(error) };
    }

    const delivered = Boolean(result?.ok && result.accepted);
    recovery.status = delivered ? 'delivered' : result?.notSent ? 'not_sent' : 'uncertain';
    if (recovery.status === 'not_sent') markKnownUnsent(recovery);
    await setTargetStatus(
      target.id,
      delivered
        ? 'Message-delivery timeout recovered from RepoTunnel live state.'
        : result?.notSent
          ? (result?.error || 'Timeout message was not sent.') + knownUnsentStatusSuffix(recovery)
          : result?.error || 'Message-timeout delivery is uncertain; automatic replay is disabled.',
      delivered ? 'ok' : 'err'
    );
    return {
      handled: true,
      delivered,
      notSent: Boolean(result?.notSent),
      uncertain: !delivered && !result?.notSent
    };
  } finally {
    if (recovery) {
      if (!dispatched) {
        if (previous) await chrome.storage.session.set({ [key]: previous }).catch(() => {});
        else await chrome.storage.session.remove(key).catch(() => {});
      } else {
        if (recovery.status === 'recovering') recovery.status = 'uncertain';
        await chrome.storage.session.set({ [key]: recovery }).catch(() => {});
      }
    }
    messageTimeoutRecoveriesInFlight.delete(target.id);
  }
}

async function authorizeMessageTimeoutRecovery(tabId, rawUrl, eventKey) {
  const target = await recoveryTargetForTab(tabId, rawUrl);
  if (
    !target || !messageTimeoutRecoveriesInFlight.has(target.id) ||
    connectionRecoveriesInFlight.has(target.id) || bridgePollState.get(target.id)?.running
  ) return false;
  const key = MESSAGE_TIMEOUT_RECOVERY_PREFIX + target.id;
  const state = await chrome.storage.session.get(key);
  const recovery = state[key];
  return Boolean(
    recovery?.status === 'recovering' && recovery.eventKey === eventKey &&
    recovery.tabId === tabId && recovery.url === target.url
  );
}

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

function makeManualTargetId(url) {
  const conversationId = conversationIdFromUrl(new URL(normalizeConversationUrl(url)));
  if (conversationId.length <= 120) return `manual-${conversationId}`;
  return `manual-${conversationId.slice(0, 56)}-${conversationId.slice(-56)}`;
}

function normalizeManualTarget(target) {
  const url = normalizeConversationUrl(target?.url || '');
  const message = String(target?.message || '').trim();
  if (!message) throw new Error('Enter a default message for this chat.');
  return {
    id: makeManualTargetId(url),
    url,
    message,
    paused: target?.paused === true,
    controlRevision: Number.isSafeInteger(target?.controlRevision) && target.controlRevision >= 0
      ? target.controlRevision
      : 0,
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

async function loadManualTargets() {
  const state = await chrome.storage.local.get(MANUAL_TARGETS_KEY);
  const source = state[MANUAL_TARGETS_KEY];
  const clean = [];
  for (const target of (Array.isArray(source) ? source : [])) {
    try {
      clean.push(normalizeManualTarget(target));
    } catch {}
  }
  if (!Array.isArray(source) || JSON.stringify(source) !== JSON.stringify(clean)) {
    await chrome.storage.local.set({ [MANUAL_TARGETS_KEY]: clean });
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

function mutateManualTargets(mutator) {
  const run = manualTargetMutationQueue.then(async () => {
    const current = await loadManualTargets();
    const next = await mutator(current.map((target) => ({ ...target })));
    const clean = [];
    for (const target of (Array.isArray(next) ? next : current)) {
      try {
        clean.push(normalizeManualTarget(target));
      } catch {}
    }
    if (JSON.stringify(clean) !== JSON.stringify(current)) {
      await chrome.storage.local.set({ [MANUAL_TARGETS_KEY]: clean });
    }
    return clean;
  });
  manualTargetMutationQueue = run.catch(() => {});
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

async function setManualTargetStatus(targetId, message, kind = '') {
  const nextMessage = String(message || '').slice(0, 300);
  await mutateManualTargets((targets) => {
    const target = targets.find((item) => item.id === targetId);
    if (target && (target.lastStatus !== nextMessage || target.lastStatusKind !== kind)) {
      target.lastStatus = nextMessage;
      target.lastStatusKind = kind;
      target.lastRunAt = Date.now();
    }
    return targets;
  });
}

async function addTarget(rawUrl, title = '') {
  const url = normalizeConversationUrl(rawUrl);
  const manualTargets = await loadManualTargets();
  if (manualTargets.some((item) => item.url === url)) {
    throw new Error('This chat is already configured in Default message chats. Remove it there before adding it to RepoTunnel continuation.');
  }
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
  const targets = await mutateTargets((targets) => targets.filter((item) => item.id !== targetId));
  await reconcileBackgroundTabProtection();
  return targets;
}

async function addManualTarget(rawUrl, rawMessage) {
  const url = normalizeConversationUrl(rawUrl);
  const message = String(rawMessage || '').trim();
  if (!message) throw new Error('Enter a default message for this chat.');
  const continuationTargets = await loadTargets();
  if (continuationTargets.some((item) => item.url === url)) {
    throw new Error('This chat is already saved in RepoTunnel continuation. Remove it there before adding a default message.');
  }

  let existed = false;
  let targetId = null;
  const manualTargets = await mutateManualTargets((items) => {
    const existing = items.find((item) => item.url === url);
    if (existing) {
      existed = true;
      existing.message = message;
      targetId = existing.id;
      return items;
    }
    const target = normalizeManualTarget({ url, message });
    targetId = target.id;
    items.push(target);
    return items;
  });
  await chrome.storage.session.remove(MANUAL_DEFAULT_ACTION_PREFIX + targetId).catch(() => {});
  await refreshAllTargets();
  return { manualTargets, targetId, existed };
}

async function editManualTarget(targetId, rawUrl, rawMessage) {
  const id = String(targetId || '');
  const url = normalizeConversationUrl(rawUrl);
  const message = String(rawMessage || '').trim();
  if (!message) throw new Error('Enter a default message for this chat.');

  const continuationTargets = await loadTargets();
  if (continuationTargets.some((item) => item.url === url)) {
    throw new Error('This chat is already saved in RepoTunnel continuation.');
  }

  let previousTarget = null;
  const manualTargets = await mutateManualTargets((items) => {
    const index = items.findIndex((item) => item.id === id);
    if (index < 0) throw new Error('This default-message chat no longer exists.');
    if (items.some((item, itemIndex) => itemIndex !== index && item.url === url)) {
      throw new Error('Another default-message chat already uses this conversation URL.');
    }

    previousTarget = { ...items[index] };
    items[index] = normalizeManualTarget({
      ...items[index],
      url,
      message,
      controlRevision: (Number(items[index].controlRevision) || 0) + 1,
      lastStatus: 'Updated. Waiting for a completed reply.',
      lastStatusKind: 'waiting',
      lastRunAt: Date.now()
    });
    return items;
  });

  const updatedTarget = manualTargets.find((item) => item.url === url);
  await chrome.storage.session.remove(MANUAL_DEFAULT_ACTION_PREFIX + id).catch(() => {});
  if (updatedTarget) {
    await chrome.storage.session.remove(MANUAL_DEFAULT_ACTION_PREFIX + updatedTarget.id).catch(() => {});
  }
  await refreshAllTargets();
  return { manualTargets, targetId: updatedTarget?.id || id, previousTargetId: previousTarget?.id || id };
}

async function setManualTargetPaused(targetId, paused) {
  const id = String(targetId || '');
  const nextPaused = Boolean(paused);
  const manualTargets = await mutateManualTargets((items) => {
    const target = items.find((item) => item.id === id);
    if (!target) throw new Error('This default-message chat no longer exists.');
    target.paused = nextPaused;
    target.controlRevision = (Number(target.controlRevision) || 0) + 1;
    target.lastStatus = nextPaused
      ? 'Paused. Automatic messages are disabled.'
      : 'Resumed. Waiting for a completed reply.';
    target.lastStatusKind = 'waiting';
    target.lastRunAt = Date.now();
    return items;
  });
  await refreshAllTargets();
  return { manualTargets, paused: nextPaused };
}

async function removeManualTarget(targetId) {
  const id = String(targetId || '');
  manualDefaultActionsInFlight.delete(id);
  await chrome.storage.session.remove(MANUAL_DEFAULT_ACTION_PREFIX + id).catch(() => {});
  const manualTargets = await mutateManualTargets((targets) => targets.filter((item) => item.id !== id));
  await reconcileBackgroundTabProtection();
  return manualTargets;
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

async function manualTargetForTab(tabId, rawUrl) {
  if (!Number.isInteger(tabId) || !isConversationUrl(rawUrl)) return null;
  const url = normalizeConversationUrl(rawUrl);
  const [targets, continuationTargets] = await Promise.all([loadManualTargets(), loadTargets()]);
  // Fail closed if stale storage somehow contains the same exact chat in both
  // modes. RepoTunnel continuation remains authoritative for that conversation.
  if (continuationTargets.some((item) => item.url === url)) return null;
  const target = targets.find((item) => item.url === url && !item.paused);
  if (!target) return null;
  try {
    const tab = await getExactOpenTab(url);
    return tab.id === tabId ? target : null;
  } catch {
    return null;
  }
}

function interruptedRefreshKey(mode, targetId) {
  return INTERRUPTED_REFRESH_PREFIX + mode + ':' + targetId;
}

function sameInterruptedRefreshIncident(record, url, eventKey, userCount = null) {
  if (record?.url !== url || record.eventKey !== eventKey) return false;
  const currentCount = validUserCount(userCount);
  const previousCount = validUserCount(record.userCount);
  return !(currentCount !== null && previousCount !== null && currentCount > previousCount);
}

async function interruptedRefreshTargetForTab(tabId, rawUrl) {
  if (!Number.isInteger(tabId) || !isConversationUrl(rawUrl)) return null;
  const url = normalizeConversationUrl(rawUrl);
  const [continuationTargets, manualTargets] = await Promise.all([loadTargets(), loadManualTargets()]);
  const continuation = continuationTargets.find((item) => item.url === url);
  const manual = manualTargets.find((item) => item.url === url && !item.paused);
  const mode = continuation ? 'continuation' : manual ? 'manual' : '';
  const target = continuation || manual;
  if (!target || (continuation && manual)) {
    if (continuation && manual) return null;
    if (!target) return null;
  }
  try {
    const tab = await getExactOpenTab(url);
    if (tab.id !== tabId) return null;
  } catch {
    return null;
  }
  return { mode, target };
}

async function beginInterruptedRefreshRecovery(tabId, rawUrl, eventKey, userCount = null) {
  if (typeof eventKey !== 'string' || !eventKey || eventKey.length > 400) return { ignored: true };
  const match = await interruptedRefreshTargetForTab(tabId, rawUrl);
  if (!match) return { ignored: true };
  const { mode, target } = match;
  if (mode === 'continuation' && bridgePollState.get(target.id)?.running) {
    return { handled: true, waiting: true };
  }
  const key = interruptedRefreshKey(mode, target.id);
  const saved = await chrome.storage.session.get(key);
  const previous = saved[key];
  const sameIncident = sameInterruptedRefreshIncident(previous, target.url, eventKey, userCount);

  if (sameIncident) {
    if (['reload_requested', 'page_loading', 'refreshing', 'claimed', 'sending', 'delivered', 'uncertain'].includes(previous.status)) {
      return {
        handled: true,
        waiting: ['reload_requested', 'page_loading', 'refreshing', 'claimed', 'sending'].includes(previous.status)
      };
    }
    if (previous.status === 'not_sent') {
      return { handled: true, waiting: Boolean(previous.retryable) };
    }
  }
  if (previous && ['reload_requested', 'page_loading', 'refreshing', 'claimed', 'sending'].includes(previous.status) &&
      Date.now() - Number(previous.updatedAt || previous.startedAt || 0) < 120000) {
    return { handled: true, waiting: true };
  }

  const message = mode === 'manual' ? target.message : 'continue';
  if (!String(message || '').trim()) return { handled: true, error: 'The saved recovery message is empty.' };

  const record = {
    mode,
    targetId: target.id,
    tabId,
    url: target.url,
    eventKey,
    userCount,
    message,
    controlRevision: Number(target.controlRevision) || 0,
    status: 'reload_requested',
    retryable: false,
    refreshCount: 1,
    startedAt: Date.now(),
    updatedAt: Date.now()
  };
  await chrome.storage.session.set({ [key]: record });
  try {
    await chrome.tabs.reload(tabId);
    return { handled: true, refreshing: true };
  } catch (error) {
    await chrome.storage.session.remove(key).catch(() => {});
    return { ignored: true, error: error?.message || String(error) };
  }
}

async function claimInterruptedRefreshRecovery(tabId, rawUrl) {
  const match = await interruptedRefreshTargetForTab(tabId, rawUrl);
  if (!match) return { ignored: true };
  const { mode, target } = match;
  const key = interruptedRefreshKey(mode, target.id);
  const saved = await chrome.storage.session.get(key);
  const record = saved[key];
  if (!record || record.tabId !== tabId || record.url !== target.url) return { ignored: true };

  const mayRetry = record.status === 'not_sent' && record.retryable &&
    Date.now() >= Number(record.nextRetryAt || 0);
  if (record.status !== 'refreshing' && !mayRetry) {
    if (record.status === 'claimed' && Date.now() - Number(record.updatedAt || 0) > 120000) {
      record.status = 'uncertain';
      record.retryable = false;
      record.updatedAt = Date.now();
      await chrome.storage.session.set({ [key]: record });
    }
    return { ignored: true };
  }

  record.status = 'claimed';
  record.updatedAt = Date.now();
  await chrome.storage.session.set({ [key]: record });
  return {
    ok: true,
    recovery: {
      mode: record.mode,
      targetId: record.targetId,
      tabId: record.tabId,
      url: record.url,
      eventKey: record.eventKey,
      message: record.message,
      controlRevision: record.controlRevision,
      refreshCount: record.refreshCount
    }
  };
}

async function interruptedRefreshAuthorization(tabId, rawUrl, recovery, transitionToSending = false) {
  if (!recovery || typeof recovery.targetId !== 'string' || typeof recovery.eventKey !== 'string') return false;
  const match = await interruptedRefreshTargetForTab(tabId, rawUrl);
  if (!match || match.mode !== recovery.mode || match.target.id !== recovery.targetId) return false;
  const { target } = match;
  const key = interruptedRefreshKey(recovery.mode, recovery.targetId);
  const saved = await chrome.storage.session.get(key);
  const record = saved[key];
  if (
    !record || record.status !== 'claimed' || record.tabId !== tabId ||
    record.url !== target.url || record.eventKey !== recovery.eventKey ||
    record.message !== recovery.message ||
    record.controlRevision !== (Number(target.controlRevision) || 0) ||
    (recovery.mode === 'manual' && (target.paused || target.message !== record.message))
  ) return false;

  if (transitionToSending) {
    record.status = 'sending';
    record.retryable = false;
    record.updatedAt = Date.now();
    await chrome.storage.session.set({ [key]: record });
  }
  return true;
}

async function completeInterruptedRefreshRecovery(tabId, rawUrl, recovery, outcome, retryable = false, error = '') {
  if (!recovery || !['delivered', 'not_sent', 'uncertain'].includes(outcome)) return { ok: false };
  const key = interruptedRefreshKey(String(recovery.mode || ''), String(recovery.targetId || ''));
  const saved = await chrome.storage.session.get(key);
  const record = saved[key];
  if (!record || record.tabId !== tabId || record.url !== normalizeConversationUrl(rawUrl) ||
      record.eventKey !== recovery.eventKey || !['claimed', 'sending'].includes(record.status)) {
    return { ok: false, stale: true };
  }

  record.status = record.status === 'sending' && outcome === 'not_sent' ? 'uncertain' : outcome;
  record.retryable = record.status === 'not_sent' && Boolean(retryable);
  record.updatedAt = Date.now();
  record.error = String(error || '').slice(0, 240);
  if (record.retryable) record.nextRetryAt = Date.now() + 15000;
  await chrome.storage.session.set({ [key]: record });

  if (record.mode === 'manual') {
    await setManualTargetStatus(record.targetId,
      record.status === 'delivered' ? 'Interrupted chat recovered after refresh; default message verified.' :
      record.status === 'not_sent' ? 'Refresh recovery has not sent yet; waiting for ChatGPT to settle.' :
      'Refresh recovery send is uncertain; automatic replay is disabled.',
      record.status === 'delivered' ? 'ok' : record.status === 'not_sent' ? 'waiting' : 'err');
  } else {
    await setTargetStatus(record.targetId,
      record.status === 'delivered' ? 'Interrupted chat recovered after refresh; continuation verified.' :
      record.status === 'not_sent' ? 'Refresh recovery has not sent yet; waiting for ChatGPT to settle.' :
      'Refresh recovery send is uncertain; automatic replay is disabled.',
      record.status === 'delivered' ? 'ok' : record.status === 'not_sent' ? 'waiting' : 'err');
  }
  return { ok: true, status: record.status };
}

async function markInterruptedRefreshNavigation(tabId, rawUrl, loadStatus) {
  if (!Number.isInteger(tabId) || !isConversationUrl(rawUrl) ||
      !['loading', 'complete'].includes(loadStatus)) return;
  const url = normalizeConversationUrl(rawUrl);
  const records = await chrome.storage.session.get(null);
  for (const [key, record] of Object.entries(records || {})) {
    if (!key.startsWith(INTERRUPTED_REFRESH_PREFIX) || !record ||
        record.tabId !== tabId || record.url !== url) continue;

    let nextStatus = record.status;
    if (loadStatus === 'loading' && record.status === 'reload_requested') {
      nextStatus = 'page_loading';
    } else if (loadStatus === 'complete' &&
        ['reload_requested', 'page_loading'].includes(record.status)) {
      nextStatus = 'refreshing';
    }
    if (nextStatus !== record.status) {
      record.status = nextStatus;
      record.updatedAt = Date.now();
      await chrome.storage.session.set({ [key]: record });
    }
  }
}

async function runManualDefaultAction(tabId, rawUrl, eventKey, action, userCount = null) {
  if (!['normal', 'interrupted', 'timeout'].includes(action)) return { ignored: true };
  if (typeof eventKey !== 'string' || !eventKey || eventKey.length > 400) return { ignored: true };
  const target = await manualTargetForTab(tabId, rawUrl);
  if (!target) return { ignored: true };
  if (manualDefaultActionsInFlight.has(target.id)) return { handled: true, ignored: true };

  manualDefaultActionsInFlight.add(target.id);
  const key = MANUAL_DEFAULT_ACTION_PREFIX + target.id;
  let record = null;
  let previous = null;
  let dispatched = false;
  try {
    const saved = await chrome.storage.session.get(key);
    previous = await markOrphanedSendUncertain(saved[key], key);
    const sameIncident = sameManualDefaultIncident(previous, target.url, eventKey, userCount);
    if (sameIncident) {
      if (previous.status !== 'not_sent') return { handled: true, ignored: true };
      const decision = knownUnsentRetryDecision(previous);
      if (!decision.allowed) return { handled: true, waiting: !decision.exhausted, retryExhausted: Boolean(decision.exhausted) };
    }

    record = {
      tabId,
      url: target.url,
      eventKey,
      action,
      message: target.message,
      controlRevision: target.controlRevision,
      startedAt: Date.now(),
      status: 'recovering',
      userCount,
      attempts: sameIncident ? Math.max(1, Number(previous?.attempts) || 1) + 1 : 1
    };
    await chrome.storage.session.set({ [key]: record });

    const current = await manualTargetForTab(tabId, rawUrl);
    if (!current || current.id !== target.id ||
        current.message !== record.message ||
        current.controlRevision !== record.controlRevision) return { handled: true, notSent: true };

    let result;
    try {
      dispatched = true;
      result = await chrome.tabs.sendMessage(tabId, {
        type: 'REPOTUNNEL_MANUAL_DEFAULT_ACTION',
        expectedUrl: target.url,
        eventKey,
        action,
        message: target.message
      });
    } catch (error) {
      result = { uncertain: true, error: error?.message || String(error) };
    }

    const delivered = Boolean(result?.ok && result.accepted);
    const notSent = Boolean(result?.notSent);
    record.status = delivered ? 'delivered' : notSent ? 'not_sent' : 'uncertain';
    if (record.status === 'not_sent') markKnownUnsent(record);
    await setManualTargetStatus(
      target.id,
      delivered
        ? 'Default message delivered and verified.'
        : notSent
        ? (result?.error || 'Default message was not sent.') + knownUnsentStatusSuffix(record)
        : result?.error || 'Default-message delivery is uncertain; automatic replay is disabled.',
      delivered ? 'ok' : notSent && record.attempts < MAX_KNOWN_UNSENT_ATTEMPTS ? 'waiting' : 'err'
    );
    return { handled: true, delivered, notSent, uncertain: !delivered && !notSent };
  } finally {
    if (record) {
      if (!dispatched) {
        if (previous) await chrome.storage.session.set({ [key]: previous }).catch(() => {});
        else await chrome.storage.session.remove(key).catch(() => {});
      } else {
        if (record.status === 'recovering') record.status = 'uncertain';
        await chrome.storage.session.set({ [key]: record }).catch(() => {});
      }
    }
    manualDefaultActionsInFlight.delete(target.id);
  }
}

async function authorizeManualDefaultAction(tabId, rawUrl, eventKey, action) {
  const target = await manualTargetForTab(tabId, rawUrl);
  if (!target || !manualDefaultActionsInFlight.has(target.id)) return false;
  const key = MANUAL_DEFAULT_ACTION_PREFIX + target.id;
  const state = await chrome.storage.session.get(key);
  const record = state[key];
  return manualActionMatchesCurrentTarget(record, target, tabId, eventKey, action);
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
  if (!target || await anyRecoveryIsRunning(target.id)) return { ignored: true };

  let exactTab;
  try {
    exactTab = await getExactOpenTab(target.url);
  } catch (error) {
    await setTargetStatus(target.id, error?.message || String(error), 'waiting');
    return { ignored: true };
  }
  if (exactTab.id !== tabId || await anyRecoveryIsRunning(target.id)) return { ignored: true };

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

  // Pair synchronous admission with the interruption-recovery reservation.
  if (connectionRecoveriesInFlight.has(target.id)) return { ignored: true };
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

    if (await anyRecoveryIsRunning(target.id)) return { ignored: true };

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

async function loadObservedTargets() {
  const [targets, manualTargets] = await Promise.all([loadTargets(), loadManualTargets()]);
  const continuationUrls = new Set(targets.map((target) => target.url));
  return [
    ...targets,
    ...manualTargets.filter((target) => !target.paused && !continuationUrls.has(target.url))
  ];
}

async function getPopupState() {
  const [targets, manualTargets] = await Promise.all([loadTargets(), loadManualTargets()]);
  const session = await chrome.storage.session.get(BRIDGE_SESSION_KEY).catch(() => ({}));
  const runtime = session?.[BRIDGE_SESSION_KEY] || {};
  const lastOkAt = Math.max(bridgeLastOkAt, Number(runtime.lastOkAt || 0));

  return {
    version: EXTENSION_VERSION,
    connected: targets.length > 0 && Date.now() - lastOkAt < 180000,
    targets,
    manualTargets
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

function initializeBackgroundTabSession(newBrowserSession = false) {
  if (newBrowserSession) backgroundSessionResetting = true;
  const operation = backgroundTabProtectionQueue.catch(() => {}).then(async () => {
    if (newBrowserSession) {
      // Tab IDs belong to one browser session. Never restore stale IDs after
      // browser restart, even if a new tab happens to have the same ID/URL.
      await chrome.storage.local.remove(BACKGROUND_TAB_PROTECTION_KEY);
    } else {
      if (backgroundSessionResetting) return;
      const marker = await chrome.storage.session.get(BACKGROUND_TAB_SESSION_KEY);
      if (!marker[BACKGROUND_TAB_SESSION_KEY]) {
        // Reload/update clears session storage but leaves open-tab settings.
        // Adopt durable ownership only when the live URL still agrees.
        const state = await chrome.storage.local.get(BACKGROUND_TAB_PROTECTION_KEY);
        const records = state[BACKGROUND_TAB_PROTECTION_KEY] || {};
        for (const id of Object.keys(records)) {
          try {
            const tab = await chrome.tabs.get(Number(id));
            if (normalizeConversationUrl(tab.url) !== records[id].lastKnownUrl) delete records[id];
          } catch { delete records[id]; }
        }
        if (backgroundSessionResetting) return;
        await chrome.storage.local.set({ [BACKGROUND_TAB_PROTECTION_KEY]: records });
      }
    }
    await chrome.storage.session.set({ [BACKGROUND_TAB_SESSION_KEY]: true });
    if (newBrowserSession) backgroundSessionResetting = false;
  });
  backgroundTabProtectionQueue = operation;
  return operation;
}

function reconcileBackgroundTabProtection() {
  const operation = backgroundTabProtectionQueue.catch(() => {}).then(async () => {
    if (backgroundSessionResetting) return;
    const marker = await chrome.storage.session.get(BACKGROUND_TAB_SESSION_KEY);
    // On startup/reload, wait for the explicit lifecycle event to distinguish
    // a new browser session from surviving open tabs before changing settings.
    if (!marker[BACKGROUND_TAB_SESSION_KEY] || backgroundSessionResetting) return;
    const targets = await loadObservedTargets();
    const tabs = await chrome.tabs.query({ url: ['https://chatgpt.com/*'] });
    const state = await chrome.storage.local.get(BACKGROUND_TAB_PROTECTION_KEY);
    const protectedTabs = state[BACKGROUND_TAB_PROTECTION_KEY] || {};
    const wanted = new Map();
    for (const target of targets) {
      const matches = tabs.filter(tab => {
        try { return normalizeConversationUrl(tab.url) === target.url; } catch { return false; }
      });
      if (matches.length === 1 && Number.isInteger(matches[0].id)) wanted.set(String(matches[0].id), matches[0]);
    }
    for (const id of Object.keys(protectedTabs)) {
      if (wanted.has(id)) continue;
      let tab;
      try { tab = await chrome.tabs.get(Number(id)); } catch { delete protectedTabs[id]; continue; }
      try {
        if (backgroundSessionResetting) return;
        if (tab.autoDiscardable === false) await chrome.tabs.update(Number(id), { autoDiscardable: protectedTabs[id].originalAutoDiscardable });
        delete protectedTabs[id];
      } catch { /* Retain the original setting for a later restoration retry. */ }
    }
    for (const [id, tab] of wanted) {
      if (tab.autoDiscardable !== true) continue;
      // Persist the original setting before changing it, so worker restarts
      // can restore it when the chat is removed or the tab navigates away.
      protectedTabs[id] = { originalAutoDiscardable: true, lastKnownUrl: normalizeConversationUrl(tab.url) };
      if (backgroundSessionResetting) return;
      await chrome.storage.local.set({ [BACKGROUND_TAB_PROTECTION_KEY]: protectedTabs });
      if (backgroundSessionResetting) return;
      try { await chrome.tabs.update(Number(id), { autoDiscardable: false }); } catch {}
    }
    if (!backgroundSessionResetting) await chrome.storage.local.set({ [BACKGROUND_TAB_PROTECTION_KEY]: protectedTabs });
  });
  backgroundTabProtectionQueue = operation;
  return operation;
}

async function runSavedChatBackgroundTick(target) {
  if (backgroundTicksInFlight.has(target.id)) return;
  backgroundTicksInFlight.add(target.id);
  let timeout;
  try {
    await Promise.race([
      (async () => {
        const tab = await getExactOpenTab(target.url);
        if (!Number.isInteger(tab?.id) || !tab.url || tab.discarded || tab.frozen) return;
        if (!await ensureContentScript(tab.id)) return;
        await chrome.tabs.sendMessage(tab.id, {
          type: 'REPOTUNNEL_BACKGROUND_TICK',
          expectedUrl: target.url
        });
      })(),
      new Promise((_, reject) => { timeout = setTimeout(() => reject(new Error('Background chat check timed out.')), 30000); })
    ]);
  } catch {
    // Closing, navigating, or temporarily suspending a tab is normal.
    // Other saved chats have independent ticks and continue to be checked.
  } finally {
    clearTimeout(timeout);
    backgroundTicksInFlight.delete(target.id);
  }
}

async function refreshAllTargets() {
  await reconcileBackgroundTabProtection();
  const targets = await loadObservedTargets();
  await Promise.allSettled(targets.map(runSavedChatBackgroundTick));
}

async function recoverLifecycle() {
  await Promise.all([loadTargets(), loadManualTargets()]);
  await ensureHeartbeatAlarm();
  await refreshAllTargets();
}

chrome.runtime.onInstalled.addListener(() => {
  return initializeBackgroundTabSession().then(recoverLifecycle).catch(() => {});
});

chrome.runtime.onStartup.addListener(() => {
  return initializeBackgroundTabSession(true).then(recoverLifecycle).catch(() => {});
});

recoverLifecycle().catch(() => {});

async function refreshTab(tabId, tabHint = null) {
  try {
    const tab = tabHint?.id === tabId ? tabHint : await chrome.tabs.get(tabId);
    if (!tab?.url || !isConversationUrl(tab.url)) return;
    const targets = await loadObservedTargets();
    const target = targets.find((target) => target.url === normalizeConversationUrl(tab.url));
    if (!target) return;
    await reconcileBackgroundTabProtection();
    await runSavedChatBackgroundTick(target);
  } catch {}
}

chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  const updated = async () => {
    if (changeInfo.status === 'loading' || changeInfo.status === 'complete') {
      await markInterruptedRefreshNavigation(tabId, tab?.url, changeInfo.status);
    }
    if (typeof changeInfo.url === 'string') reconcileBackgroundTabProtection().catch(() => {});
    if (changeInfo.status === 'complete' || typeof changeInfo.url === 'string') {
      await refreshTab(tabId, tab);
    }
  };
  updated().catch(() => {});
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
  if (alarm.name === HEARTBEAT_ALARM) return refreshAllTargets().catch(() => {});
});

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  (async () => {
    if (message?.type === 'REPOTUNNEL_BACKGROUND_DELAY') {
      const ms = Number(message.ms);
      if (!Number.isInteger(sender.tab?.id) || !isConversationUrl(sender.tab?.url) || !Number.isInteger(ms) || ms < 0 || ms > 1000) {
        sendResponse({ ok: false, error: 'Invalid background wait.' });
        return;
      }
      await sleep(ms);
      sendResponse({ ok: true });
      return;
    }
    if (message?.type === 'CHAT_INTERRUPTED_REFRESH_BEGIN') {
      sendResponse({
        ok: true,
        ...(await beginInterruptedRefreshRecovery(
          sender.tab?.id,
          sender.tab?.url,
          String(message.eventKey || ''),
          validUserCount(message.userCount)
        ))
      });
      return;
    }
    if (message?.type === 'CHAT_INTERRUPTED_REFRESH_CLAIM') {
      sendResponse({
        ok: true,
        ...(await claimInterruptedRefreshRecovery(sender.tab?.id, sender.tab?.url))
      });
      return;
    }
    if (message?.type === 'CHAT_INTERRUPTED_REFRESH_AUTHORIZE' ||
        message?.type === 'CHAT_INTERRUPTED_REFRESH_BEGIN_SEND') {
      sendResponse({
        ok: true,
        authorized: await interruptedRefreshAuthorization(
          sender.tab?.id,
          sender.tab?.url,
          message.recovery,
          message.type === 'CHAT_INTERRUPTED_REFRESH_BEGIN_SEND'
        )
      });
      return;
    }
    if (message?.type === 'CHAT_INTERRUPTED_REFRESH_COMPLETE') {
      sendResponse({
        ok: true,
        ...(await completeInterruptedRefreshRecovery(
          sender.tab?.id,
          sender.tab?.url,
          message.recovery,
          String(message.outcome || ''),
          Boolean(message.retryable),
          String(message.error || '')
        ))
      });
      return;
    }
    if (message?.type === 'CHAT_CONNECTION_INTERRUPTED') {
      sendResponse({ ok: true, ...(await recoverInterruptedChat(sender.tab?.id, sender.tab?.url, message.eventKey, validUserCount(message.userCount))) });
      return;
    }
    if (message?.type === 'CONNECTION_RECOVERY_READY') {
      sendResponse({ ok: true, authorized: await authorizeConnectionRecovery(sender.tab?.id, sender.tab?.url, message.eventKey) });
      return;
    }
    if (message?.type === 'CHAT_MESSAGE_DELIVERY_TIMED_OUT') {
      sendResponse({ ok: true, ...(await recoverMessageDeliveryTimeout(sender.tab?.id, sender.tab?.url, message.eventKey, validUserCount(message.userCount))) });
      return;
    }
    if (message?.type === 'MESSAGE_TIMEOUT_RECOVERY_READY') {
      sendResponse({ ok: true, authorized: await authorizeMessageTimeoutRecovery(sender.tab?.id, sender.tab?.url, message.eventKey) });
      return;
    }
    if (message?.type === 'CHAT_MANUAL_DEFAULT_ACTION') {
      sendResponse({
        ok: true,
        ...(await runManualDefaultAction(
          sender.tab?.id,
          sender.tab?.url,
          String(message.eventKey || ''),
          String(message.action || ''),
          validUserCount(message.userCount)
        ))
      });
      return;
    }
    if (message?.type === 'MANUAL_DEFAULT_ACTION_READY') {
      sendResponse({
        ok: true,
        authorized: await authorizeManualDefaultAction(
          sender.tab?.id,
          sender.tab?.url,
          String(message.eventKey || ''),
          String(message.action || '')
        )
      });
      return;
    }
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

    if (message?.type === 'ADD_MANUAL_TARGET') {
      sendResponse({ ok: true, ...(await addManualTarget(message.url, message.message)) });
      return;
    }

    if (message?.type === 'EDIT_MANUAL_TARGET') {
      sendResponse({
        ok: true,
        ...(await editManualTarget(String(message.targetId || ''), message.url, message.message))
      });
      return;
    }

    if (message?.type === 'SET_MANUAL_TARGET_PAUSED') {
      sendResponse({
        ok: true,
        ...(await setManualTargetPaused(String(message.targetId || ''), message.paused))
      });
      return;
    }

    if (message?.type === 'REMOVE_MANUAL_TARGET') {
      sendResponse({ ok: true, manualTargets: await removeManualTarget(String(message.targetId || '')) });
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
