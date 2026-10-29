const addCurrentButton = document.getElementById('addCurrent');
const addUrlButton = document.getElementById('addUrl');
const refreshButton = document.getElementById('refreshBridge');
const urlInput = document.getElementById('targetUrl');
const targetList = document.getElementById('targetList');
const targetCount = document.getElementById('targetCount');
const manualUrlInput = document.getElementById('manualUrl');
const manualMessageInput = document.getElementById('manualMessage');
const addManualTargetButton = document.getElementById('addManualTarget');
const cancelManualEditButton = document.getElementById('cancelManualEdit');
const manualTargetList = document.getElementById('manualTargetList');
const manualTargetCount = document.getElementById('manualTargetCount');
const bridgeState = document.getElementById('bridgeState');
const status = document.getElementById('status');
const version = document.getElementById('version');

let state = { targets: [], manualTargets: [], connected: false, version: '' };
let editingManualTargetId = null;

function setStatus(message, kind = '') {
  status.textContent = message;
  status.className = `status ${kind}`.trim();
}

function normalizeConversationUrl(raw) {
  const url = new URL(String(raw || '').trim());
  const segments = url.pathname.split('/').filter(Boolean);
  if (
    url.protocol !== 'https:' ||
    url.hostname !== 'chatgpt.com' ||
    segments.length !== 2 ||
    segments[0] !== 'c' ||
    !/^[A-Za-z0-9_-]{1,160}$/.test(segments[1])
  ) {
    throw new Error('Open or paste an exact ChatGPT conversation URL.');
  }
  return `https://chatgpt.com/c/${segments[1]}`;
}

function shortUrl(raw) {
  try {
    const id = new URL(raw).pathname.split('/').filter(Boolean).at(-1) || '';
    return `chatgpt.com/c/${id.slice(0, 14)}${id.length > 14 ? '…' : ''}`;
  } catch {
    return raw;
  }
}

function renderTarget(target, index) {
  const row = document.createElement('div');
  row.className = 'target';
  row.dataset.targetId = target.id;

  const info = document.createElement('div');
  info.className = 'target-info';

  const title = document.createElement('strong');
  title.textContent = target.title || `Chat ${index + 1}`;
  title.title = target.title || target.url;

  const url = document.createElement('span');
  url.textContent = shortUrl(target.url);
  url.title = target.url;

  info.append(title, url);

  if (target.lastStatus) {
    const message = document.createElement('small');
    message.className = `target-status ${target.lastStatusKind || ''}`.trim();
    message.textContent = target.lastStatus;
    message.title = target.lastStatus;
    info.append(message);
  }

  const remove = document.createElement('button');
  remove.type = 'button';
  remove.className = 'remove';
  remove.dataset.action = 'remove';
  remove.textContent = 'Remove';

  row.append(info, remove);
  return row;
}

function renderManualTarget(target) {
  const row = document.createElement('div');
  row.className = target.paused ? 'target paused' : 'target';
  row.dataset.targetId = target.id;

  const info = document.createElement('div');
  info.className = 'target-info';

  const url = document.createElement('strong');
  url.textContent = shortUrl(target.url);
  url.title = target.url;

  const message = document.createElement('small');
  message.className = 'manual-message';
  message.textContent = target.message;
  message.title = target.message;
  info.append(url, message);

  const lastStatus = document.createElement('small');
  lastStatus.className = `target-status ${target.paused ? 'waiting' : target.lastStatusKind || ''}`.trim();
  lastStatus.textContent = target.paused ? 'Paused — automatic messages disabled.' : target.lastStatus || '';
  lastStatus.title = lastStatus.textContent;
  if (lastStatus.textContent) info.append(lastStatus);

  const actions = document.createElement('div');
  actions.className = 'target-actions';

  const edit = document.createElement('button');
  edit.type = 'button';
  edit.className = 'edit';
  edit.dataset.action = 'manual-edit';
  edit.textContent = 'Edit';

  const pause = document.createElement('button');
  pause.type = 'button';
  pause.className = 'toggle-paused';
  pause.dataset.action = 'manual-toggle-pause';
  pause.textContent = target.paused ? 'Resume' : 'Pause';
  pause.setAttribute('aria-label', target.paused ? 'Resume automatic messages' : 'Pause automatic messages');

  const remove = document.createElement('button');
  remove.type = 'button';
  remove.className = 'remove';
  remove.dataset.action = 'manual-remove';
  remove.textContent = 'Remove';

  actions.append(edit, pause, remove);
  row.append(info, actions);
  return row;
}


function render() {
  targetList.replaceChildren();
  manualTargetList.replaceChildren();
  targetCount.textContent = `${state.targets.length}/5`;
  manualTargetCount.textContent = String(state.manualTargets.length);
  version.textContent = state.version ? `v${state.version}` : '';

  if (state.targets.length === 0) {
    bridgeState.textContent = 'Not set';
    bridgeState.className = 'badge';
  } else if (state.connected) {
    bridgeState.textContent = 'Connected';
    bridgeState.className = 'badge connected';
  } else {
    bridgeState.textContent = 'Waiting';
    bridgeState.className = 'badge';
  }

  if (state.targets.length === 0) {
    const empty = document.createElement('p');
    empty.className = 'empty';
    empty.textContent = 'No ChatGPT conversations saved.';
    targetList.append(empty);
  } else {
    state.targets.forEach((target, index) => targetList.append(renderTarget(target, index)));
  }

  if (state.manualTargets.length === 0) {
    const empty = document.createElement('p');
    empty.className = 'empty';
    empty.textContent = 'No default-message chats saved.';
    manualTargetList.append(empty);
  } else {
    state.manualTargets.forEach((target) => manualTargetList.append(renderManualTarget(target)));
  }
}

async function refreshState() {
  const response = await chrome.runtime.sendMessage({ type: 'GET_STATE' });
  if (!response?.ok) throw new Error(response?.error || 'Could not read extension state.');
  state = response.state;
  render();
}

async function addTarget(url, title = '') {
  const response = await chrome.runtime.sendMessage({
    type: 'ADD_TARGET',
    url: normalizeConversationUrl(url),
    title
  });
  if (!response?.ok) throw new Error(response?.error || 'Could not save this chat.');
  urlInput.value = '';
  await refreshState();
  setStatus(response.existed ? 'This chat was already saved.' : 'Chat saved and registered.', 'ok');
}

function resetManualEdit() {
  editingManualTargetId = null;
  manualUrlInput.value = '';
  manualMessageInput.value = '';
  addManualTargetButton.textContent = 'Save default-message chat';
  cancelManualEditButton.hidden = true;
}

async function addManualTarget(url, message) {
  const response = await chrome.runtime.sendMessage({
    type: 'ADD_MANUAL_TARGET',
    url: normalizeConversationUrl(url),
    message: String(message || '').trim()
  });
  if (!response?.ok) throw new Error(response?.error || 'Could not save this default-message chat.');
  resetManualEdit();
  await refreshState();
  setStatus(response.existed ? 'Default message updated.' : 'Default-message chat saved.', 'ok');
}

async function saveManualTarget(url, message) {
  if (!editingManualTargetId) {
    await addManualTarget(url, message);
    return;
  }

  const response = await chrome.runtime.sendMessage({
    type: 'EDIT_MANUAL_TARGET',
    targetId: editingManualTargetId,
    url: normalizeConversationUrl(url),
    message: String(message || '').trim()
  });
  if (!response?.ok) throw new Error(response?.error || 'Could not update this default-message chat.');
  resetManualEdit();
  await refreshState();
  setStatus('Default-message chat updated. The new settings apply to future safe deliveries.', 'ok');
}

async function currentTab() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  return tab;
}

addCurrentButton.addEventListener('click', async () => {
  addCurrentButton.disabled = true;
  try {
    const tab = await currentTab();
    if (!tab?.url) throw new Error('Could not read the current browser tab.');
    await addTarget(tab.url, tab.title || '');
  } catch (error) {
    setStatus(error?.message || String(error), 'err');
  } finally {
    addCurrentButton.disabled = false;
  }
});

addUrlButton.addEventListener('click', async () => {
  addUrlButton.disabled = true;
  try {
    if (!urlInput.value.trim()) throw new Error('Paste an exact ChatGPT conversation URL.');
    await addTarget(urlInput.value);
  } catch (error) {
    setStatus(error?.message || String(error), 'err');
  } finally {
    addUrlButton.disabled = false;
  }
});

urlInput.addEventListener('keydown', (event) => {
  if (event.key === 'Enter') addUrlButton.click();
});

addManualTargetButton.addEventListener('click', async () => {
  addManualTargetButton.disabled = true;
  try {
    if (!manualUrlInput.value.trim()) throw new Error('Paste an exact ChatGPT conversation URL.');
    if (!manualMessageInput.value.trim()) throw new Error('Enter the default message for this chat.');
    await saveManualTarget(manualUrlInput.value, manualMessageInput.value);
  } catch (error) {
    setStatus(error?.message || String(error), 'err');
  } finally {
    addManualTargetButton.disabled = false;
  }
});

cancelManualEditButton.addEventListener('click', () => {
  resetManualEdit();
  setStatus('Edit cancelled.');
});

targetList.addEventListener('click', async (event) => {
  const button = event.target.closest('button[data-action="remove"]');
  if (!button) return;
  const row = button.closest('.target');
  if (!row?.dataset.targetId) return;

  button.disabled = true;
  try {
    const response = await chrome.runtime.sendMessage({
      type: 'REMOVE_TARGET',
      targetId: row.dataset.targetId
    });
    if (!response?.ok) throw new Error(response?.error || 'Could not remove this chat.');
    await refreshState();
    setStatus('Chat removed.', 'ok');
  } catch (error) {
    setStatus(error?.message || String(error), 'err');
  } finally {
    button.disabled = false;
  }
});

manualTargetList.addEventListener('click', async (event) => {
  const button = event.target.closest('button[data-action]');
  if (!button) return;
  const row = button.closest('.target');
  const targetId = row?.dataset.targetId;
  const target = state.manualTargets.find((item) => item.id === targetId);
  if (!targetId || !target) return;

  if (button.dataset.action === 'manual-edit') {
    editingManualTargetId = target.id;
    manualUrlInput.value = target.url;
    manualMessageInput.value = target.message;
    addManualTargetButton.textContent = 'Save changes';
    cancelManualEditButton.hidden = false;
    manualUrlInput.focus();
    setStatus('Editing saved chat. Changes will not send a message by themselves.');
    return;
  }

  button.disabled = true;
  try {
    if (button.dataset.action === 'manual-toggle-pause') {
      const paused = !target.paused;
      const response = await chrome.runtime.sendMessage({
        type: 'SET_MANUAL_TARGET_PAUSED',
        targetId,
        paused
      });
      if (!response?.ok) throw new Error(response?.error || 'Could not update this chat.');
      await refreshState();
      setStatus(paused ? 'Automatic messages paused for this chat.' : 'Automatic messages resumed for this chat.', 'ok');
      return;
    }

    if (button.dataset.action === 'manual-remove') {
      const response = await chrome.runtime.sendMessage({
        type: 'REMOVE_MANUAL_TARGET',
        targetId
      });
      if (!response?.ok) throw new Error(response?.error || 'Could not remove this default-message chat.');
      if (editingManualTargetId === targetId) resetManualEdit();
      await refreshState();
      setStatus('Default-message chat removed.', 'ok');
    }
  } catch (error) {
    setStatus(error?.message || String(error), 'err');
  } finally {
    button.disabled = false;
  }
});


refreshButton.addEventListener('click', async () => {
  refreshButton.disabled = true;
  try {
    const response = await chrome.runtime.sendMessage({ type: 'REFRESH_BRIDGE' });
    if (!response?.ok) throw new Error(response?.error || 'Could not refresh RepoTunnel bridge.');
    state = response.state;
    render();
    setStatus(state.connected ? 'RepoTunnel bridge is connected.' : 'RepoTunnel is not connected yet.', state.connected ? 'ok' : '');
  } catch (error) {
    setStatus(error?.message || String(error), 'err');
  } finally {
    refreshButton.disabled = false;
  }
});

refreshState().catch((error) => setStatus(error?.message || String(error), 'err'));
