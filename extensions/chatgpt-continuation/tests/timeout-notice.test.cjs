const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '..', 'content.js'), 'utf8');

function extractFunction(name) {
  const pattern = new RegExp(`  function ${name}\\([^\\n]*\\) \\{[\\s\\S]*?^  \\}`, 'm');
  const match = source.match(pattern);
  assert.ok(match, `Could not find ${name} in content.js`);
  return match[0];
}

function makeElement(text, options = {}) {
  return {
    innerText: text,
    textContent: text,
    id: options.id || '',
    getAttribute(name) {
      if (name === 'data-testid') return options.testId || null;
      if (name === 'data-turn-key') return options.turnKey || null;
      if (name === 'data-message-id') return options.messageId || null;
      return null;
    },
    querySelector() { return null; },
    querySelectorAll() { return []; },
    compareDocumentPosition() { return 0; },
    closest(selector) {
      if (options.markdown && selector.includes('.markdown')) return {};
      if (options.inMessage && selector.includes('[data-message-author-role]')) return {};
      return null;
    }
  };
}

function mountDetector(elements, turn = makeElement('', { turnKey: 'assistant-turn-9' }), detectorName = 'findMessageDeliveryTimeout') {
  const sandbox = {
    document: { querySelectorAll: () => elements },
    isVisible: () => true,
    latestRecoveryAssistantTurn: () => turn,
    getUserMessages: () => [],
    hasExactTurnNotice: () => false,
    Node: { DOCUMENT_POSITION_FOLLOWING: 4 }
  };

  vm.runInNewContext([
    extractFunction('hasExactStandaloneNotice'),
    extractFunction('findMessageDeliveryTimeout'),
    extractFunction('findInterruptedResponse'),
    'globalThis.detectors = { findMessageDeliveryTimeout, findInterruptedResponse };'
  ].join('\n\n'), sandbox);

  return sandbox.detectors[detectorName]();
}

test('timeout detector recognizes ChatGPT error banner outside the assistant turn', () => {
  const banner = makeElement('Message delivery timed out. Please try again. Retry');
  const exactMessage = makeElement('Message delivery timed out. Please try again.');
  const result = mountDetector([banner, exactMessage]);

  assert.equal(result?.eventKey, 'assistant-turn-9');
});

test('timeout detector ignores the same phrase quoted inside message content', () => {
  const quoted = makeElement('Message delivery timed out. Please try again.', { markdown: true, inMessage: true });
  assert.equal(mountDetector([quoted]), null);
});

test('timeout detector requires the exact current error wording', () => {
  const unrelated = makeElement('Message delivery timed out. Please try again later.');
  assert.equal(mountDetector([unrelated]), null);
});

test('timeout detector ignores error-looking text inside an assistant message', () => {
  const assistantText = makeElement('Message delivery timed out. Please try again.', { inMessage: true });
  assert.equal(mountDetector([assistantText]), null);
});

test('interrupted recovery recognizes the exact banner outside the assistant turn', () => {
  const banner = makeElement('Connection interrupted. Waiting for the complete answer');
  const result = mountDetector([banner], undefined, 'findInterruptedResponse');
  assert.equal(result?.eventKey, 'assistant-turn-9');
});

test('interrupted recovery ignores quoted notices in message content', () => {
  const quoted = makeElement('Connection interrupted. Waiting for the complete answer', { markdown: true, inMessage: true });
  assert.equal(mountDetector([quoted], undefined, 'findInterruptedResponse'), null);
});
