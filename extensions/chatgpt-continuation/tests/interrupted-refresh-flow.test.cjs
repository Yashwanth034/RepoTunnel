const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '..', 'background.js'), 'utf8');

function extractFunction(name, asyncFunction = false) {
  const prefix = asyncFunction ? 'async function' : 'function';
  const pattern = new RegExp(`^${prefix} ${name}\\([^\\n]*\\) \\{[\\s\\S]*?^\\}`, 'm');
  const match = source.match(pattern);
  assert.ok(match, `Could not find ${name} in background.js`);
  return match[0];
}

function makeHarness() {
  const storage = {};
  const reloads = [];
  const target = {
    id: 'chat-example',
    url: 'https://chatgpt.com/c/example',
    message: 'continue',
    controlRevision: 0
  };
  const sandbox = {
    Date,
    Map,
    Number,
    String,
    Boolean,
    Promise,
    INTERRUPTED_REFRESH_PREFIX: 'repoTunnelInterruptedRefresh:',
    validUserCount(value) {
      return Number.isSafeInteger(value) && value >= 0 ? value : null;
    },
    bridgePollState: new Map(),
    isConversationUrl: (url) => String(url || '').startsWith('https://chatgpt.com/c/'),
    normalizeConversationUrl: (url) => String(url).split('?')[0],
    interruptedRefreshTargetForTab: async () => ({ mode: 'continuation', target }),
    chrome: {
      storage: {
        session: {
          get: async (key) => key === null ? { ...storage } : ({ [key]: storage[key] }),
          set: async (entries) => Object.assign(storage, entries),
          remove: async (key) => { delete storage[key]; }
        }
      },
      tabs: {
        reload: async (tabId) => { reloads.push(tabId); }
      }
    }
  };
  vm.runInNewContext([
    extractFunction('interruptedRefreshKey'),
    extractFunction('sameInterruptedRefreshIncident'),
    extractFunction('beginInterruptedRefreshRecovery', true),
    extractFunction('markInterruptedRefreshNavigation', true),
    'globalThis.api = { interruptedRefreshKey, sameInterruptedRefreshIncident, beginInterruptedRefreshRecovery, markInterruptedRefreshNavigation };'
  ].join('\n\n'), sandbox);
  return { api: sandbox.api, storage, reloads };
}

test('interrupted recovery reloads an exact tab only once per incident', async () => {
  const { api, storage, reloads } = makeHarness();
  const first = await api.beginInterruptedRefreshRecovery(27, 'https://chatgpt.com/c/example', 'turn-9', 5);
  const key = api.interruptedRefreshKey('continuation', 'chat-example');

  assert.equal(first.handled, true);
  assert.equal(first.refreshing, true);
  assert.deepEqual(reloads, [27]);
  assert.equal(storage[key].status, 'reload_requested');
  await api.markInterruptedRefreshNavigation(27, 'https://chatgpt.com/c/example', 'loading');
  assert.equal(storage[key].status, 'page_loading');
  await api.markInterruptedRefreshNavigation(27, 'https://chatgpt.com/c/example', 'complete');
  assert.equal(storage[key].status, 'refreshing');

  const duplicate = await api.beginInterruptedRefreshRecovery(27, 'https://chatgpt.com/c/example', 'turn-9', 5);
  assert.equal(duplicate.handled, true);
  assert.equal(duplicate.waiting, true);
  assert.deepEqual(reloads, [27]);
});

test('a distinct interrupted turn can start a new recovery after the previous one completes', async () => {
  const { api, storage, reloads } = makeHarness();
  const key = api.interruptedRefreshKey('continuation', 'chat-example');
  await api.beginInterruptedRefreshRecovery(27, 'https://chatgpt.com/c/example', 'turn-9', 5);
  storage[key].status = 'delivered';

  const next = await api.beginInterruptedRefreshRecovery(27, 'https://chatgpt.com/c/example', 'turn-10', 6);
  assert.equal(next.refreshing, true);
  assert.deepEqual(reloads, [27, 27]);
});

test('count increases distinguish a new incident even when the assistant turn key is reused', () => {
  const { api } = makeHarness();
  assert.equal(api.sameInterruptedRefreshIncident({
    url: 'https://chatgpt.com/c/example',
    eventKey: 'conversation-turn-9',
    userCount: 5
  }, 'https://chatgpt.com/c/example', 'conversation-turn-9', 6), false);
});
