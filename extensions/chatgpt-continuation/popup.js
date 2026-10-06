const addCurrentButton = document.getElementById('addCurrent');
const addUrlButton = document.getElementById('addUrl');
const refreshButton = document.getElementById('refreshBridge');
const urlInput = document.getElementById('targetUrl');
const targetList = document.getElementById('targetList');
const targetCount = document.getElementById('targetCount');
const bridgeState = document.getElementById('bridgeState');
const status = document.getElementById('status');
const version = document.getElementById('version');

let state = { targets: [], connected: false, version: '' };

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

function render() {
  targetList.replaceChildren();
  targetCount.textContent = `${state.targets.length}/5`;
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
