const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const { JSDOM } = require('jsdom');

const extensionDir = path.join(__dirname, '..');
const html = fs.readFileSync(path.join(extensionDir, 'popup.html'), 'utf8');
const script = fs.readFileSync(path.join(extensionDir, 'popup.js'), 'utf8');

async function mountPopup() {
  const dom = new JSDOM(html, { runScripts: 'outside-only', url: 'chrome-extension://repotunnel/popup.html' });
  const calls = [];
  const state = {
    targets: [],
    manualTargets: [{
      id: 'manual-chat-1',
      url: 'https://chatgpt.com/c/chat-1',
      message: 'continue',
      paused: false,
      controlRevision: 0,
      lastStatus: 'Connected',
      lastStatusKind: 'ok'
    }],
    connected: false,
    version: '0.4.3'
  };

  dom.window.chrome = {
    runtime: {
      sendMessage: async (message) => {
        calls.push({ ...message });
        if (message.type === 'GET_STATE') return { ok: true, state };
        if (message.type === 'EDIT_MANUAL_TARGET') {
          const target = state.manualTargets.find((item) => item.id === message.targetId);
          if (!target) return { ok: false, error: 'missing target' };
          target.url = message.url;
          target.message = message.message;
          target.controlRevision += 1;
          target.id = `manual-${message.url.split('/').at(-1)}`;
          return { ok: true };
        }
        if (message.type === 'SET_MANUAL_TARGET_PAUSED') {
          const target = state.manualTargets.find((item) => item.id === message.targetId);
          if (!target) return { ok: false, error: 'missing target' };
          target.paused = message.paused;
          target.controlRevision += 1;
          return { ok: true };
        }
        return { ok: true };
      }
    },
    tabs: { query: async () => [] }
  };

  dom.window.eval(script);
  await new Promise((resolve) => setImmediate(resolve));
  return { dom, calls, state };
}

function click(dom, selector) {
  const element = dom.window.document.querySelector(selector);
  assert.ok(element, `Missing element: ${selector}`);
  element.dispatchEvent(new dom.window.MouseEvent('click', { bubbles: true }));
}

test('manual chat can be edited without sending a message immediately', async () => {
  const { dom, calls, state } = await mountPopup();
  try {
    click(dom, '[data-action="manual-edit"]');
    const url = dom.window.document.getElementById('manualUrl');
    const message = dom.window.document.getElementById('manualMessage');
    assert.equal(url.value, 'https://chatgpt.com/c/chat-1');
    assert.equal(message.value, 'continue');

    url.value = 'https://chatgpt.com/c/chat-2';
    message.value = 'updated prompt';
    click(dom, '#addManualTarget');
    await new Promise((resolve) => setImmediate(resolve));

    assert.ok(calls.some((call) => call.type === 'EDIT_MANUAL_TARGET' &&
      call.url === 'https://chatgpt.com/c/chat-2' && call.message === 'updated prompt'));
    assert.equal(state.manualTargets[0].message, 'updated prompt');
    assert.equal(calls.some((call) => call.type === 'REPOTUNNEL_MANUAL_DEFAULT_ACTION'), false);
    assert.equal(dom.window.document.getElementById('manualUrl').value, '');
  } finally {
    dom.window.close();
  }
});

test('manual chat pause and resume controls persist the requested state', async () => {
  const { dom, calls, state } = await mountPopup();
  try {
    click(dom, '[data-action="manual-toggle-pause"]');
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(state.manualTargets[0].paused, true);
    assert.ok(calls.some((call) => call.type === 'SET_MANUAL_TARGET_PAUSED' && call.paused === true));
    assert.equal(dom.window.document.querySelector('[data-action="manual-toggle-pause"]').textContent, 'Resume');

    click(dom, '[data-action="manual-toggle-pause"]');
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(state.manualTargets[0].paused, false);
    assert.ok(calls.some((call) => call.type === 'SET_MANUAL_TARGET_PAUSED' && call.paused === false));
    assert.equal(dom.window.document.querySelector('[data-action="manual-toggle-pause"]').textContent, 'Pause');
  } finally {
    dom.window.close();
  }
});
