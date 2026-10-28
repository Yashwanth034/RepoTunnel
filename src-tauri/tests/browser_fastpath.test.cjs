"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const {
  reusableCdpClient,
  captureSemanticTrees,
} = require("../resources/browser_fastpath.cjs");

// 1 is WebSocket.OPEN. A dead/closing or unauthenticated socket must be
// rediscovered through RepoTunnel's normal Chrome target lookup.
test("reuses only live CDP targets", () => {
  assert.equal(reusableCdpClient(null), false);
  assert.equal(reusableCdpClient({ closed: false, socket: { readyState: 1 } }), true);
  assert.equal(reusableCdpClient({ closed: true, socket: { readyState: 1 } }), false);
  assert.equal(reusableCdpClient({ closed: false, socket: { readyState: 2 } }), false);
  assert.equal(reusableCdpClient({ closed: false, socket: { readyState: 3 } }), false);
});

test("enables domains and reads independent semantic trees concurrently", async () => {
  const pending = new Map();
  const sent = [];
  const client = {
    send(method, params = {}) {
      sent.push({ method, params });
      return new Promise((resolve) => { pending.set(method, resolve); });
    },
  };
  const captured = captureSemanticTrees(client);
  await new Promise(setImmediate);
  assert.deepEqual(sent.map((v) => v.method), ["Accessibility.enable", "DOM.enable"]);
  pending.get("DOM.enable")({});
  pending.get("Accessibility.enable")({});
  await new Promise(setImmediate);
  assert.deepEqual(sent.slice(2).map((v) => v.method), [
    "Accessibility.getFullAXTree", "DOM.getDocument", "Page.getFrameTree",
  ]);
  assert.deepEqual(sent[3].params, { depth: -1, pierce: true });
  const axTree = { nodes: [{ nodeId: "ax-1" }] };
  const domTree = { root: { backendNodeId: 2 } };
  const frameTree = { frameTree: { frame: { id: "page-1", loaderId: "doc-1" } } };
  pending.get("Page.getFrameTree")(frameTree);
  pending.get("DOM.getDocument")(domTree);
  pending.get("Accessibility.getFullAXTree")(axTree);
  assert.deepEqual(await captured, { axTree, domTree, frameTree });
});

test("snapshot fails closed if Chrome rejects any evidence domain", async () => {
  await assert.rejects(
    captureSemanticTrees({
      async send(method) {
        if (method === "DOM.getDocument") throw new Error("DOM inspection denied");
        return {};
      },
    }),
    /DOM inspection denied/,
  );
});
