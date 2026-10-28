"use strict";

// A live, owned WebSocket already binds all commands to one immutable Chrome
// target. Reusing it avoids a redundant HTTP /json/list on every CDP call.
// Closed and failed connections must still use the regular discovery path.
function reusableCdpClient(client) {
  return Boolean(client && !client.closed && client.socket?.readyState === 1);
}

// Keep the full, browser-provided Accessibility and piercing DOM trees.
// CDP executes independent commands in parallel and returns results in their
// original format; the existing signed semantic refs and privacy filtering
// continue to run in browser_bridge.cjs, unchanged.
async function captureSemanticTrees(client) {
  await Promise.all([
    client.send("Accessibility.enable"),
    client.send("DOM.enable"),
  ]);
  const [axTree, domTree, frameTree] = await Promise.all([
    client.send("Accessibility.getFullAXTree"),
    client.send("DOM.getDocument", { depth: -1, pierce: true }),
    client.send("Page.getFrameTree"),
  ]);
  return { axTree, domTree, frameTree };
}

module.exports = { reusableCdpClient, captureSemanticTrees };
