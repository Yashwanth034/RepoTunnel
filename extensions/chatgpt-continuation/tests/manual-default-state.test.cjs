const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const background = fs.readFileSync(path.join(__dirname, '..', 'background.js'), 'utf8');

function extractFunction(name) {
  const match = background.match(new RegExp(`function ${name}\\([^\\n]*\\) \\{[\\s\\S]*?\\n\\}`));
  assert.ok(match, `Could not find ${name} in background.js`);
  return match[0];
}

const sandbox = { URL };
vm.runInNewContext([
  extractFunction('conversationIdFromUrl'),
  extractFunction('normalizeConversationUrl'),
  extractFunction('makeManualTargetId'),
  extractFunction('normalizeManualTarget'),
  extractFunction('validUserCount'),
  extractFunction('sameManualDefaultIncident'),
  extractFunction('manualActionMatchesCurrentTarget'),
  'globalThis.helpers = { normalizeManualTarget, sameManualDefaultIncident, manualActionMatchesCurrentTarget };'
].join('\n\n'), sandbox);

const { normalizeManualTarget, sameManualDefaultIncident, manualActionMatchesCurrentTarget } = sandbox.helpers;

test('legacy default-message entries gain safe active defaults', () => {
  const target = normalizeManualTarget({
    url: 'https://chatgpt.com/c/example-chat',
    message: 'continue'
  });

  assert.equal(target.paused, false);
  assert.equal(target.controlRevision, 0);
  assert.equal(target.message, 'continue');
});

test('pause state and control revision survive normalization', () => {
  const target = normalizeManualTarget({
    url: 'https://chatgpt.com/c/example-chat',
    message: 'continue',
    paused: true,
    controlRevision: 7
  });

  assert.equal(target.paused, true);
  assert.equal(target.controlRevision, 7);
});

test('same event remains deduplicated across repeated polling', () => {
  assert.equal(sameManualDefaultIncident({
    url: 'https://chatgpt.com/c/example-chat',
    eventKey: 'turn-12',
    userCount: 4
  }, 'https://chatgpt.com/c/example-chat', 'turn-12', 4), true);
});

test('a new user turn is not blocked by a reused assistant turn key', () => {
  assert.equal(sameManualDefaultIncident({
    url: 'https://chatgpt.com/c/example-chat',
    eventKey: 'conversation-turn-12',
    userCount: 4,
    status: 'delivered'
  }, 'https://chatgpt.com/c/example-chat', 'conversation-turn-12', 5), false);
});

test('a new assistant turn is recognized when virtualization keeps the count steady', () => {
  assert.equal(sameManualDefaultIncident({
    url: 'https://chatgpt.com/c/example-chat',
    eventKey: 'turn-12',
    userCount: 4,
    status: 'delivered'
  }, 'https://chatgpt.com/c/example-chat', 'turn-13', 4), false);
});

test('count decreases from history virtualization does not replay the same turn', () => {
  assert.equal(sameManualDefaultIncident({
    url: 'https://chatgpt.com/c/example-chat',
    eventKey: 'turn-12',
    userCount: 8,
    status: 'delivered'
  }, 'https://chatgpt.com/c/example-chat', 'turn-12', 4), true);
});

test('delivery state is not reused across different conversations', () => {
  assert.equal(sameManualDefaultIncident({
    url: 'https://chatgpt.com/c/old-chat',
    eventKey: 'turn-12',
    userCount: 4
  }, 'https://chatgpt.com/c/new-chat', 'turn-12', 4), false);
});

test('pause or edit invalidates an in-flight send authorization', () => {
  const record = {
    status: 'recovering',
    eventKey: 'turn-12',
    action: 'normal',
    tabId: 17,
    url: 'https://chatgpt.com/c/example-chat',
    message: 'continue',
    controlRevision: 3
  };
  const target = {
    url: record.url,
    message: record.message,
    controlRevision: 4
  };

  assert.equal(manualActionMatchesCurrentTarget(record, target, 17, 'turn-12', 'normal'), false);
  assert.equal(manualActionMatchesCurrentTarget(
    { ...record, controlRevision: 4 }, target, 17, 'turn-12', 'normal'
  ), true);
});
