const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '..', 'content.js'), 'utf8');
const poll = source.match(/^  async function pollBridge\(\) \{[\s\S]*?^  \}/m);
assert.ok(poll, 'content.js must contain the pollBridge routing implementation');

async function runInterruptedPoll({ generating, handled = {} }) {
  const sent = [];
  const sandbox = {
    bridgePollRunning: false,
    interruptedRecoveryRunning: false,
    messageTimeoutRecoveryRunning: false,
    manualDefaultActionRunning: false,
    interruptedRefreshRecoveryRunning: false,
    preparedDelivery: null,
    findInterruptedResponse: () => ({ eventKey: 'assistant-turn-9' }),
    findEditor: () => ({}),
    editorText: () => '',
    composerIsHumanIdle: () => true,
    getUserMessages: () => [{}, {}],
    getStopButton: () => generating ? {} : null,
    chrome: {
      runtime: {
        async sendMessage(message) {
          sent.push(message);
          if (message.type === 'CHAT_INTERRUPTED_REFRESH_CLAIM') return {};
          return { handled: Boolean(handled[message.type]) };
        }
      }
    }
  };
  vm.runInNewContext(poll[0] + '\nglobalThis.runPoll = pollBridge;', sandbox);
  await sandbox.runPoll();
  return sent.filter(m => m.type !== 'CHAT_INTERRUPTED_REFRESH_CLAIM');
}

test('actively generating interruption tries in-place continuation without refreshing', async () => {
  const messages = await runInterruptedPoll({
    generating: true,
    handled: { CHAT_CONNECTION_INTERRUPTED: true }
  });
  assert.deepEqual(messages.map(m => m.type), ['CHAT_CONNECTION_INTERRUPTED']);
  assert.equal(messages[0].eventKey, 'assistant-turn-9');
  assert.equal(messages[0].userCount, 2);
});

test('stopped interruption requests exactly one refresh first', async () => {
  const messages = await runInterruptedPoll({
    generating: false,
    handled: { CHAT_INTERRUPTED_REFRESH_BEGIN: true }
  });
  assert.deepEqual(messages.map(m => m.type), ['CHAT_INTERRUPTED_REFRESH_BEGIN']);
});

test('stopped interruption falls back to continuation if refresh is not handled', async () => {
  const messages = await runInterruptedPoll({
    generating: false,
    handled: { CHAT_CONNECTION_INTERRUPTED: true }
  });
  assert.deepEqual(messages.map(m => m.type), [
    'CHAT_INTERRUPTED_REFRESH_BEGIN', 'CHAT_CONNECTION_INTERRUPTED'
  ]);
});

test('actively generating interruption never sends refresh even when continuation is unavailable', async () => {
  const messages = await runInterruptedPoll({
    generating: true,
    handled: { CHAT_MANUAL_DEFAULT_ACTION: true }
  });
  assert.deepEqual(messages.map(m => m.type), [
    'CHAT_CONNECTION_INTERRUPTED', 'CHAT_MANUAL_DEFAULT_ACTION'
  ]);
});
