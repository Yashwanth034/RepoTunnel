#!/usr/bin/env node
"use strict";

const fs = require("node:fs");
const path = require("node:path");
const readline = require("node:readline");

const [, , portArg, operation, ...args] = process.argv;
const port = Number(portArg);

function fail(message) {
  process.stderr.write(`${message}\n`);
  process.exit(1);
}

if (!Number.isInteger(port) || port < 1 || port > 65535) fail("Invalid Chrome DevTools port.");
if (!operation) fail("Missing browser operation.");
if (typeof fetch !== "function" || typeof WebSocket !== "function") {
  fail("RepoTunnel browser automation requires Node.js with fetch and WebSocket support (Node 20+).")
}

const baseUrl = `http://127.0.0.1:${port}`;

function out(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

async function jsonRequest(path, method = "GET") {
  const response = await fetch(`${baseUrl}${path}`, { method });
  const text = await response.text();
  if (!response.ok) throw new Error(`Chrome DevTools request failed (${response.status}): ${text.slice(0, 400)}`);
  if (!text.trim()) return null;
  try {
    return JSON.parse(text);
  } catch {
    throw new Error(`Chrome DevTools returned invalid JSON: ${text.slice(0, 400)}`);
  }
}

async function rawRequest(path, method = "GET") {
  const response = await fetch(`${baseUrl}${path}`, { method });
  const text = await response.text();
  if (!response.ok) throw new Error(`Chrome DevTools request failed (${response.status}): ${text.slice(0, 400)}`);
  return text;
}

async function listTabs() {
  const entries = await jsonRequest("/json/list");
  return (Array.isArray(entries) ? entries : [])
    .filter((entry) => entry && entry.type === "page" && entry.id && entry.webSocketDebuggerUrl)
    .map((entry) => ({
      id: String(entry.id),
      title: String(entry.title || "Untitled"),
      url: String(entry.url || "about:blank"),
      type: String(entry.type || "page"),
      webSocketDebuggerUrl: String(entry.webSocketDebuggerUrl),
    }));
}

async function findTab(tabId) {
  const tabs = await listTabs();
  const tab = tabs.find((entry) => entry.id === tabId);
  if (!tab) throw new Error("The selected browser tab is no longer available.");
  return tab;
}

function cdpSocket(url) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(url);
    const timer = setTimeout(() => {
      try { socket.close(); } catch {}
      reject(new Error("Timed out connecting to the Chrome DevTools target."));
    }, 5000);
    socket.addEventListener("open", () => {
      clearTimeout(timer);
      resolve(socket);
    }, { once: true });
    socket.addEventListener("error", () => {
      clearTimeout(timer);
      reject(new Error("Could not connect to the Chrome DevTools target."));
    }, { once: true });
  });
}

const persistentCdpClients = new Map();
const persistentCdpConnects = new Map();
const activeSemanticSequences = new Map();
let downloadTracker = null;

function closeStandaloneCdpClient(client) {
  if (!client || client.closed) return;
  client.closed = true;
  try { client.socket.close(); } catch {}
  for (const pending of client.pending.values()) {
    clearTimeout(pending.timer);
    pending.reject(new Error("Persistent Chrome DevTools connection closed."));
  }
  client.pending.clear();
}

function closePersistentCdpClient(tabId) {
  const client = persistentCdpClients.get(tabId);
  if (!client) return;
  persistentCdpClients.delete(tabId);
  closeStandaloneCdpClient(client);
}

async function createPersistentCdpClient(tab) {
  const socket = await cdpSocket(tab.webSocketDebuggerUrl);
  const client = {
    url: tab.webSocketDebuggerUrl,
    socket,
    pending: new Map(),
    eventListeners: new Set(),
    nextId: 1,
    closed: false,
  };

  const failPending = (message) => {
    if (client.closed) return;
    client.closed = true;
    for (const pending of client.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(new Error(message));
    }
    client.pending.clear();
  };

  socket.addEventListener("message", (event) => {
    let message;
    try { message = JSON.parse(String(event.data)); } catch { return; }
    if (!Object.prototype.hasOwnProperty.call(message, "id")) {
      for (const listener of Array.from(client.eventListeners)) {
        try { listener(message); } catch {}
      }
      return;
    }
    const pending = client.pending.get(message.id);
    if (!pending) return;
    client.pending.delete(message.id);
    clearTimeout(pending.timer);
    if (message.error) {
      pending.reject(new Error(message.error.message || "Chrome DevTools command failed."));
    } else {
      pending.resolve(message.result || {});
    }
  });
  socket.addEventListener("close", () => failPending("Persistent Chrome DevTools connection closed."));
  socket.addEventListener("error", () => failPending("Persistent Chrome DevTools connection failed."));

  client.onEvent = (listener) => {
    client.eventListeners.add(listener);
    return () => client.eventListeners.delete(listener);
  };

  client.send = (method, params = {}) => {
    if (client.closed || socket.readyState !== WebSocket.OPEN) {
      return Promise.reject(new Error("Persistent Chrome DevTools connection is not open."));
    }
    const id = client.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        client.pending.delete(id);
        reject(new Error(`Chrome DevTools command timed out: ${method}`));
      }, 12000);
      client.pending.set(id, { resolve, reject, timer });
      try {
        socket.send(JSON.stringify({ id, method, params }));
      } catch (error) {
        client.pending.delete(id);
        clearTimeout(timer);
        try { socket.close(); } catch {}
        failPending("Persistent Chrome DevTools connection failed while sending.");
        reject(error);
      }
    });
  };

  return client;
}

async function getPersistentCdpClient(tabId) {
  const tab = await findTab(tabId);
  let client = persistentCdpClients.get(tabId);
  if (!client || client.closed || client.url !== tab.webSocketDebuggerUrl) {
    if (client) closePersistentCdpClient(tabId);
    let connecting = persistentCdpConnects.get(tabId);
    if (!connecting) {
      connecting = createPersistentCdpClient(tab)
        .then((created) => {
          persistentCdpClients.set(tabId, created);
          return created;
        })
        .finally(() => persistentCdpConnects.delete(tabId));
      persistentCdpConnects.set(tabId, connecting);
    }
    client = await connecting;
  }
  return client;
}

async function persistentCdpCommand(tabId, method, params = {}) {
  const client = await getPersistentCdpClient(tabId);
  return await client.send(method, params);
}

async function closeAllPersistentCdpClients() {
  for (const tabId of Array.from(persistentCdpClients.keys())) {
    closePersistentCdpClient(tabId);
  }
  if (downloadTracker) {
    try { downloadTracker.unsubscribe(); } catch {}
    closeStandaloneCdpClient(downloadTracker.client);
    downloadTracker = null;
  }
}

async function minimizeWindowForTarget(tabId) {
  if (!tabId) return;
  const version = await jsonRequest("/json/version");
  const browserSocket = String(version?.webSocketDebuggerUrl || "");
  if (!browserSocket) return;
  const client = await createPersistentCdpClient({ webSocketDebuggerUrl: browserSocket });
  try {
    const info = await client.send("Browser.getWindowForTarget", { targetId: tabId });
    const windowId = Number(info?.windowId);
    if (!Number.isFinite(windowId)) return;
    await client.send("Browser.setWindowBounds", {
      windowId,
      bounds: { windowState: "minimized" },
    });
  } catch {
    // Some Chromium variants/window managers may reject minimize while a
    // window is being created. The launch flag and the next operation retry it.
  } finally {
    closeStandaloneCdpClient(client);
  }
}

async function cdpCommand(tabId, method, params = {}) {
  if (operation === "serve") {
    return await persistentCdpCommand(tabId, method, params);
  }
  const tab = await findTab(tabId);
  const socket = await cdpSocket(tab.webSocketDebuggerUrl);
  const id = Math.floor(Math.random() * 1_000_000_000) + 1;
  return await new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      try { socket.close(); } catch {}
      reject(new Error(`Chrome DevTools command timed out: ${method}`));
    }, 12000);
    socket.addEventListener("message", (event) => {
      let message;
      try { message = JSON.parse(String(event.data)); } catch { return; }
      if (message.id !== id) return;
      clearTimeout(timer);
      try { socket.close(); } catch {}
      if (message.error) {
        reject(new Error(message.error.message || `Chrome DevTools command failed: ${method}`));
      } else {
        resolve(message.result || {});
      }
    });
    socket.addEventListener("error", () => {
      clearTimeout(timer);
      reject(new Error(`Chrome DevTools connection failed during ${method}.`));
    }, { once: true });
    socket.send(JSON.stringify({ id, method, params }));
  });
}

async function evaluate(tabId, expression, awaitPromise = true, returnByValue = true) {
  const result = await cdpCommand(tabId, "Runtime.evaluate", {
    expression,
    awaitPromise,
    returnByValue,
    userGesture: true,
  });
  if (result.exceptionDetails) {
    const description = result.exceptionDetails.exception?.description || result.exceptionDetails.text || "Page script failed.";
    throw new Error(description);
  }
  return result.result?.value;
}

async function waitForDocument(tabId, timeoutMs = 10000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const state = await evaluate(tabId, "document.readyState");
      if (state === "interactive" || state === "complete") return state;
    } catch {}
    await new Promise((resolve) => setTimeout(resolve, 120));
  }
  return null;
}

async function applyContext(tabId, headers = {}, userAgent = "") {
  await cdpCommand(tabId, "Network.enable");
  await cdpCommand(tabId, "Network.setExtraHTTPHeaders", { headers });
  if (userAgent) {
    await cdpCommand(tabId, "Network.setUserAgentOverride", { userAgent });
  }
  return { ok: true };
}

function cookieFingerprint(cookie) {
  return [
    String(cookie?.name || ""),
    String(cookie?.domain || ""),
    String(cookie?.path || ""),
    String(cookie?.value || ""),
  ].join("\u0000");
}

async function cookieSnapshot(client) {
  try {
    const result = await client.send("Network.getCookies");
    return Array.isArray(result?.cookies) ? result.cookies : [];
  } catch {
    return [];
  }
}

async function mutationState(tabId) {
  const tab = await findTab(tabId).catch(() => null);
  let generation = null;
  let frameUrl = "";
  try {
    const tree = await cdpCommand(tabId, "Page.getFrameTree");
    const frame = tree?.frameTree?.frame || {};
    generation = frame?.loaderId ? String(frame.loaderId) : null;
    frameUrl = String(frame?.url || "");
  } catch {}
  return {
    url: String(tab?.url || frameUrl || ""),
    documentGeneration: generation,
  };
}

async function navigateObserve(tabId, url, timeoutMs = 12000) {
  const startedAt = Date.now();
  const client = await getPersistentCdpClient(tabId);
  await client.send("Page.enable");
  await client.send("Runtime.enable");
  await client.send("Network.enable");

  const beforeCookies = await cookieSnapshot(client);
  const requests = new Map();
  const redirects = [];
  const networkErrors = [];
  const documentResponses = [];
  let requestCount = 0;
  let loadEventSeen = false;
  let resolveLoad;
  const loadPromise = new Promise((resolve) => { resolveLoad = resolve; });

  const unsubscribe = client.onEvent((message) => {
    const params = message?.params || {};
    if (message.method === "Page.loadEventFired") {
      loadEventSeen = true;
      resolveLoad(true);
      return;
    }
    if (message.method === "Network.requestWillBeSent") {
      const requestId = String(params.requestId || "");
      const request = params.request || {};
      const previous = requests.get(requestId);
      requestCount += 1;
      if (params.redirectResponse) {
        redirects.push({
          fromUrl: String(params.redirectResponse.url || previous?.url || ""),
          toUrl: String(request.url || ""),
          status: Number.isFinite(Number(params.redirectResponse.status))
            ? Number(params.redirectResponse.status)
            : null,
        });
      }
      requests.set(requestId, {
        url: String(request.url || ""),
        method: String(request.method || ""),
        resourceType: params.type ? String(params.type) : null,
      });
      return;
    }
    if (message.method === "Network.responseReceived") {
      const response = params.response || {};
      if (String(params.type || "") === "Document") {
        documentResponses.push({
          frameId: String(params.frameId || ""),
          url: String(response.url || ""),
          status: Number.isFinite(Number(response.status)) ? Number(response.status) : null,
        });
      }
      return;
    }
    if (message.method === "Network.loadingFailed" && networkErrors.length < 20) {
      const request = requests.get(String(params.requestId || "")) || {};
      networkErrors.push({
        url: request.url || null,
        method: request.method || null,
        errorText: String(params.errorText || "Network request failed").slice(0, 1000),
        resourceType: params.type ? String(params.type) : (request.resourceType || null),
      });
    }
  });

  let navigationResult = {};
  let navigationError = null;
  try {
    navigationResult = await client.send("Page.navigate", { url });
    if (navigationResult?.errorText) navigationError = String(navigationResult.errorText);
    const boundedTimeout = Math.min(Math.max(Number(timeoutMs) || 12000, 1000), 30000);
    await Promise.race([
      loadPromise,
      new Promise((resolve) => setTimeout(() => resolve(false), boundedTimeout)),
    ]);
  } catch (error) {
    navigationError = error instanceof Error ? error.message : String(error);
  } finally {
    unsubscribe();
  }

  let frameTree = {};
  try { frameTree = await client.send("Page.getFrameTree"); } catch {}
  const frame = frameTree?.frameTree?.frame || {};
  const navigationGeneration = navigationResult?.loaderId ? String(navigationResult.loaderId) : null;
  const documentGeneration = frame?.loaderId ? String(frame.loaderId) : null;
  const documentMatchesNavigation =
    !navigationGeneration || !documentGeneration || navigationGeneration === documentGeneration;

  let readyState = "";
  let title = "";
  let documentText = "";
  let documentHtml = "";
  if (documentMatchesNavigation) {
    try {
      const state = await client.send("Runtime.evaluate", {
        expression: "({readyState:document.readyState,title:document.title,url:location.href,text:(document.body?.innerText||'').slice(0,12000),html:(document.documentElement?.outerHTML||'').slice(0,12000)})",
        returnByValue: true,
      });
      const value = state?.result?.value || {};
      readyState = String(value.readyState || "");
      title = String(value.title || "");
      documentText = String(value.text || "");
      documentHtml = String(value.html || "");
    } catch {}
  }

  const finalTab = await findTab(tabId).catch(() => null);
  const finalUrl = String(finalTab?.url || frame?.url || url);
  if (!title && documentMatchesNavigation) title = String(finalTab?.title || "");

  const mainFrameId = String(navigationResult?.frameId || frame?.id || "");
  const matchingResponse = [...documentResponses]
    .reverse()
    .find((entry) => !mainFrameId || entry.frameId === mainFrameId)
    || documentResponses.at(-1)
    || null;

  const afterCookies = await cookieSnapshot(client);
  const before = new Map(beforeCookies.map((cookie) => [
    [String(cookie?.name || ""), String(cookie?.domain || ""), String(cookie?.path || "")].join("\u0000"),
    cookieFingerprint(cookie),
  ]));
  const changedCookieNames = new Set();
  for (const cookie of afterCookies) {
    const key = [String(cookie?.name || ""), String(cookie?.domain || ""), String(cookie?.path || "")].join("\u0000");
    if (before.get(key) !== cookieFingerprint(cookie)) {
      const name = String(cookie?.name || "");
      if (name) changedCookieNames.add(name);
    }
  }

  const ready = readyState === "interactive" || readyState === "complete";
  const timedOut = !navigationError && !loadEventSeen && !ready;
  let errorCode = null;
  if (navigationError) errorCode = "NAVIGATION_FAILED";
  else if (!documentMatchesNavigation) errorCode = "DOCUMENT_GENERATION_MISMATCH";
  else if (timedOut) errorCode = "PAGE_LOAD_TIMEOUT";

  return {
    requestedUrl: url,
    finalUrl,
    title,
    readyState,
    httpStatus: matchingResponse?.status ?? null,
    timedOut,
    loadState: navigationError
      ? "navigation_error"
      : (timedOut ? "timed_out" : (readyState || "loaded")),
    navigationGeneration,
    documentGeneration,
    documentMatchesNavigation,
    documentText,
    documentHtml,
    redirects: redirects.slice(0, 20),
    networkErrors,
    requestCount,
    cookiesChanged: Array.from(changedCookieNames).sort().slice(0, 50),
    durationMs: Math.max(0, Date.now() - startedAt),
    errorCode,
    errorText: navigationError,
  };
}

function decodeJsonArg(values, index, fallback) {
  if (values[index] === undefined) return fallback;
  try { return JSON.parse(values[index]); } catch { throw new Error("Invalid RepoTunnel browser argument."); }
}

function clip(value, max = 16000) {
  const text = value == null ? "" : String(value);
  return text.length > max ? `${text.slice(0, max)}\n…truncated` : text;
}

function safeNetworkUrl(value) {
  const raw = value == null ? "" : String(value);
  try {
    const parsed = new URL(raw);
    const sensitive = /(^|[-_])(token|secret|password|passwd|api[-_]?key|access[-_]?key|credential|authorization|cookie|session|jwt|signature|sig|auth|auth[-_]?code|code)($|[-_])/i;
    for (const key of Array.from(parsed.searchParams.keys())) {
      if (sensitive.test(key)) parsed.searchParams.set(key, "[REDACTED]");
    }
    parsed.hash = "";
    return parsed.toString();
  } catch {
    return clip(raw, 8192);
  }
}

function axPrimitive(value) {
  if (value && typeof value === "object" && Object.prototype.hasOwnProperty.call(value, "value")) {
    return value.value;
  }
  return value ?? null;
}

function axText(value, max = 2000) {
  const primitive = axPrimitive(value);
  if (primitive == null) return "";
  if (typeof primitive === "object") {
    try { return clip(JSON.stringify(primitive), max); } catch { return ""; }
  }
  return clip(String(primitive), max);
}

function domAttributes(node) {
  const attrs = new Map();
  const values = Array.isArray(node?.attributes) ? node.attributes : [];
  for (let index = 0; index + 1 < values.length; index += 2) {
    attrs.set(String(values[index]).toLowerCase(), String(values[index + 1] ?? ""));
  }
  return attrs;
}

function collectDomNodes(node, output) {
  if (!node || typeof node !== "object") return;
  if (Number.isInteger(node.backendNodeId)) output.set(node.backendNodeId, node);
  for (const child of node.children || []) collectDomNodes(child, output);
  for (const shadow of node.shadowRoots || []) collectDomNodes(shadow, output);
  for (const pseudo of node.pseudoElements || []) collectDomNodes(pseudo, output);
  if (node.contentDocument) collectDomNodes(node.contentDocument, output);
  if (node.templateContent) collectDomNodes(node.templateContent, output);
}

function semanticBackendIdentity(node) {
  const ax = String(node?.nodeId || "");
  const dom = Number(node?.backendDOMNodeId || 0);
  return dom > 0 ? `dom:${dom};ax:${ax}` : `ax:${ax}`;
}

function semanticProperties(node) {
  const result = new Map();
  for (const property of node?.properties || []) {
    if (!property?.name) continue;
    result.set(String(property.name), axPrimitive(property.value));
  }
  return result;
}

function semanticStates(properties) {
  const states = [];
  const add = (value) => { if (!states.includes(value)) states.push(value); };
  const truthy = (name) => properties.get(name) === true || properties.get(name) === "true";

  if (truthy("disabled")) add("disabled"); else add("enabled");
  for (const state of ["focusable", "focused", "readonly", "required", "selected", "multiline"]) {
    if (truthy(state)) add(state);
  }
  const editable = properties.get("editable");
  if (editable === true || editable === "true" || editable === "plaintext" || editable === "richtext") {
    add("editable");
  }
  if (properties.has("expanded")) add(truthy("expanded") ? "expanded" : "collapsed");
  if (properties.has("checked")) {
    const value = properties.get("checked");
    add(value === "mixed" ? "mixed" : truthy("checked") ? "checked" : "unchecked");
  }
  if (properties.has("pressed")) {
    const value = properties.get("pressed");
    add(value === "mixed" ? "mixed" : truthy("pressed") ? "pressed" : "not-pressed");
  }
  return states;
}

function semanticActions(role, states) {
  const actions = [];
  const add = (value) => { if (!actions.includes(value)) actions.push(value); };
  const disabled = states.includes("disabled");
  const editable = states.includes("editable") || ["textbox", "searchbox", "combobox", "spinbutton"].includes(role);
  const clickable = new Set([
    "button", "link", "checkbox", "radio", "switch", "tab", "menuitem",
    "menuitemcheckbox", "menuitemradio", "option", "treeitem",
  ]);
  if (!disabled && clickable.has(role)) add("click");
  if (!disabled && editable && !states.includes("readonly")) add("type");
  if (!disabled && (states.includes("focusable") || editable || clickable.has(role))) add("focus");
  return actions;
}

function privateFieldHint(value) {
  const text = String(value || "").toLowerCase();
  return [
    "password", "passwd", "passcode", "passphrase", "pin", "secret",
    "credential", "token", "api key", "access key", "private key",
    "verification code", "one-time", "one time", "otp", "2fa", "mfa",
  ].some((hint) => text.includes(hint));
}

function privateField(domNode, role, name, description) {
  const attrs = domAttributes(domNode);
  const inputType = (attrs.get("type") || "").toLowerCase();
  const autocomplete = (attrs.get("autocomplete") || "").toLowerCase();
  if (inputType === "password") return true;
  if (autocomplete.includes("password") || autocomplete.includes("one-time-code")) return true;
  return [
    role, name, description, attrs.get("name"), attrs.get("id"),
    attrs.get("aria-label"), attrs.get("placeholder"),
  ].some(privateFieldHint);
}

function safePrivateFieldName(domNode, fallback) {
  const attrs = domAttributes(domNode);
  for (const candidate of [attrs.get("aria-label"), attrs.get("placeholder"), attrs.get("name"), fallback]) {
    if (candidate && privateFieldHint(candidate)) return clip(candidate, 1000);
  }
  return "Sensitive field";
}

async function semanticSnapshot(tabId, requestedMaxNodes) {
  const tab = await findTab(tabId);
  const maxNodes = Math.min(Math.max(Number(requestedMaxNodes || 800), 20), 2000);

  await cdpCommand(tabId, "Accessibility.enable");
  await cdpCommand(tabId, "DOM.enable");
  const axTree = await cdpCommand(tabId, "Accessibility.getFullAXTree");
  const domTree = await cdpCommand(tabId, "DOM.getDocument", { depth: -1, pierce: true });
  const frameTree = await cdpCommand(tabId, "Page.getFrameTree");

  const rawNodes = Array.isArray(axTree?.nodes) ? axTree.nodes : [];
  const domNodes = new Map();
  collectDomNodes(domTree?.root, domNodes);

  const usable = rawNodes.filter((node) => {
    const role = axText(node?.role, 200).toLowerCase();
    return !node?.ignored && role && role !== "none";
  });
  const selected = usable.slice(0, maxNodes);
  const selectedIds = new Set(selected.map((node) => String(node.nodeId || "")));
  const identityByAxId = new Map(
    rawNodes.map((node) => [String(node.nodeId || ""), semanticBackendIdentity(node)])
  );

  const nodes = selected.map((node) => {
    const role = axText(node.role, 200).toLowerCase() || "unknown";
    let name = axText(node.name, 1000);
    let description = axText(node.description, 1000);
    let value = axText(node.value, 2000) || null;
    let text = [
      "statictext", "inlinetextbox", "heading", "paragraph", "cell",
      "rowheader", "columnheader",
    ].includes(role) ? (name || null) : null;
    const properties = semanticProperties(node);
    const states = semanticStates(properties);
    const actions = semanticActions(role, states);
    const backendDomId = Number(node.backendDOMNodeId || 0);
    const domNode = backendDomId > 0 ? domNodes.get(backendDomId) : null;
    const sensitive = privateField(domNode, role, name, description);

    if (sensitive) {
      name = safePrivateFieldName(domNode, name);
      description = "";
      text = null;
      value = null;
    }

    return {
      backendId: semanticBackendIdentity(node),
      role,
      name,
      description,
      text,
      value,
      states,
      actions,
      bounds: null,
      sensitive,
      parentBackendId: node.parentId && selectedIds.has(String(node.parentId))
        ? identityByAxId.get(String(node.parentId)) || null
        : null,
      childBackendIds: (node.childIds || [])
        .filter((id) => selectedIds.has(String(id)))
        .map((id) => identityByAxId.get(String(id)))
        .filter(Boolean),
    };
  });

  const documentIdentity = semanticDocumentIdentity(frameTree, tab.id);

  return {
    title: tab.title || "",
    url: tab.url || "",
    documentIdentity,
    totalNodes: usable.length,
    truncated: usable.length > selected.length,
    nodes,
  };
}

function parseBackendDomId(value) {
  const id = Number(value);
  if (!Number.isInteger(id) || id <= 0) throw new Error("Invalid semantic DOM node identity.");
  return id;
}

function quadCenter(quad) {
  if (!Array.isArray(quad) || quad.length < 8) throw new Error("Semantic element has no usable box.");
  return {
    x: (Number(quad[0]) + Number(quad[2]) + Number(quad[4]) + Number(quad[6])) / 4,
    y: (Number(quad[1]) + Number(quad[3]) + Number(quad[5]) + Number(quad[7])) / 4,
  };
}

function semanticDocumentIdentity(frameTree, fallbackTabId) {
  const frame = frameTree?.frameTree?.frame || {};
  return `${String(frame.id || fallbackTabId)}:${String(frame.loaderId || "unknown-loader")}`;
}

function semanticMissingNodeError(error) {
  const message = String(error?.message || error || "").toLowerCase();
  return message.includes("no node") ||
    message.includes("could not find node") ||
    message.includes("node with given id") ||
    message.includes("not found");
}

function semanticNodeAttributes(node) {
  const attrs = domAttributes(node);
  return {
    disabled: attrs.has("disabled") || (attrs.get("aria-disabled") || "").toLowerCase() === "true",
    readonly: attrs.has("readonly") || (attrs.get("aria-readonly") || "").toLowerCase() === "true",
    hidden: attrs.has("hidden") || (attrs.get("aria-hidden") || "").toLowerCase() === "true",
  };
}

async function semanticActionability(tabId, backendNodeId, expectedDocumentIdentity, action) {
  const frameTree = await cdpCommand(tabId, "Page.getFrameTree");
  const currentDocumentIdentity = semanticDocumentIdentity(frameTree, tabId);
  if (currentDocumentIdentity !== expectedDocumentIdentity) {
    return {
      ok: false,
      code: "STALE_REF",
      error: "Browser document changed after the semantic snapshot.",
    };
  }

  let described;
  try {
    described = await cdpCommand(tabId, "DOM.describeNode", {
      backendNodeId,
      depth: action === "click" ? -1 : 0,
      pierce: true,
    });
  } catch (error) {
    if (semanticMissingNodeError(error)) {
      return { ok: false, code: "STALE_REF", error: "Semantic target no longer exists." };
    }
    throw error;
  }

  const attrs = semanticNodeAttributes(described?.node);
  if (attrs.hidden) return { ok: false, code: "NOT_ACTIONABLE", error: "Semantic target is hidden." };
  if (attrs.disabled) return { ok: false, code: "NOT_ACTIONABLE", error: "Semantic target is disabled." };
  if (action === "type" && attrs.readonly) {
    return { ok: false, code: "NOT_ACTIONABLE", error: "Semantic target is read-only." };
  }
  if (action === "type" && privateField(described?.node, "", "", "")) {
    return {
      ok: false,
      code: "SENSITIVE_FIELD",
      error: "RepoTunnel blocks semantic typing into sensitive credential fields.",
    };
  }

  try {
    await cdpCommand(tabId, "DOM.scrollIntoViewIfNeeded", { backendNodeId });
  } catch (error) {
    if (semanticMissingNodeError(error)) {
      return { ok: false, code: "STALE_REF", error: "Semantic target no longer exists." };
    }
    return { ok: false, code: "NOT_ACTIONABLE", error: "Semantic target cannot be scrolled into view." };
  }

  let box;
  try {
    box = await cdpCommand(tabId, "DOM.getBoxModel", { backendNodeId });
  } catch (error) {
    if (semanticMissingNodeError(error)) {
      return { ok: false, code: "STALE_REF", error: "Semantic target no longer exists." };
    }
    return { ok: false, code: "NOT_ACTIONABLE", error: "Semantic target has no visible box." };
  }
  const quad = box?.model?.border || box?.model?.content;
  const point = quadCenter(quad);

  if (action === "click") {
    const subtree = new Map();
    collectDomNodes(described?.node, subtree);
    const hit = await cdpCommand(tabId, "DOM.getNodeForLocation", {
      x: Math.round(point.x),
      y: Math.round(point.y),
      includeUserAgentShadowDOM: true,
    });
    const hitBackendNodeId = Number(hit?.backendNodeId || 0);
    if (!hitBackendNodeId || !subtree.has(hitBackendNodeId)) {
      return {
        ok: false,
        code: "NOT_ACTIONABLE",
        error: "Semantic target is covered by another element.",
      };
    }
  }

  return { ok: true, point };
}

async function waitForSemanticActionability(
  tabId,
  backendNodeId,
  expectedDocumentIdentity,
  action,
  attempts = 3,
) {
  let last = null;
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    last = await semanticActionability(tabId, backendNodeId, expectedDocumentIdentity, action);
    if (last?.ok) return last;
    if (last?.code === "STALE_REF" || last?.code === "SENSITIVE_FIELD") break;
    if (attempt + 1 < attempts) {
      await new Promise((resolve) => setTimeout(resolve, 80 * (attempt + 1)));
    }
  }
  throw new Error(`${last?.code || "NOT_ACTIONABLE"}: ${last?.error || "Semantic target is not actionable."}`);
}

async function semanticClick(tabId, backendId, expectedDocumentIdentity) {
  const backendNodeId = parseBackendDomId(backendId);
  const actionable = await waitForSemanticActionability(
    tabId,
    backendNodeId,
    expectedDocumentIdentity,
    "click",
  );
  const point = actionable.point;
  await cdpCommand(tabId, "Input.dispatchMouseEvent", {
    type: "mouseMoved", x: point.x, y: point.y,
  });
  await cdpCommand(tabId, "Input.dispatchMouseEvent", {
    type: "mousePressed", x: point.x, y: point.y, button: "left", clickCount: 1,
  });
  await cdpCommand(tabId, "Input.dispatchMouseEvent", {
    type: "mouseReleased", x: point.x, y: point.y, button: "left", clickCount: 1,
  });
  return { ok: true, x: point.x, y: point.y };
}

async function semanticType(tabId, backendId, text, clearFirst, expectedDocumentIdentity) {
  const backendNodeId = parseBackendDomId(backendId);
  await waitForSemanticActionability(
    tabId,
    backendNodeId,
    expectedDocumentIdentity,
    "type",
  );
  await cdpCommand(tabId, "DOM.focus", { backendNodeId });

  if (clearFirst) {
    const selectModifier = process.platform === "darwin" ? 4 : 2;
    await cdpCommand(tabId, "Input.dispatchKeyEvent", {
      type: "keyDown", modifiers: selectModifier, key: "a", code: "KeyA",
      windowsVirtualKeyCode: 65, nativeVirtualKeyCode: 65,
    });
    await cdpCommand(tabId, "Input.dispatchKeyEvent", {
      type: "keyUp", modifiers: selectModifier, key: "a", code: "KeyA",
      windowsVirtualKeyCode: 65, nativeVirtualKeyCode: 65,
    });
    await cdpCommand(tabId, "Input.dispatchKeyEvent", {
      type: "keyDown", key: "Backspace", code: "Backspace",
      windowsVirtualKeyCode: 8, nativeVirtualKeyCode: 8,
    });
    await cdpCommand(tabId, "Input.dispatchKeyEvent", {
      type: "keyUp", key: "Backspace", code: "Backspace",
      windowsVirtualKeyCode: 8, nativeVirtualKeyCode: 8,
    });
  }

  await cdpCommand(tabId, "Input.insertText", { text: String(text ?? "") });
  return { ok: true };
}

function validateSemanticSequenceId(sequenceId) {
  const value = String(sequenceId || "").trim();
  if (!/^[A-Za-z0-9_-]{1,128}$/.test(value)) {
    throw new Error("Browser semantic sequence ID is invalid.");
  }
  return value;
}

function throwIfSemanticSequenceCancelled(token, index) {
  if (token.cancelled) {
    const suffix = Number.isInteger(index) ? ` before step ${index + 1}` : "";
    throw new Error(`SEQUENCE_CANCELLED: Browser semantic sequence was cancelled${suffix}.`);
  }
}

async function cancellableSemanticWait(waitMs, token, index) {
  let remaining = waitMs;
  while (remaining > 0) {
    throwIfSemanticSequenceCancelled(token, index);
    const chunk = Math.min(remaining, 40);
    await new Promise((resolve) => setTimeout(resolve, chunk));
    remaining -= chunk;
  }
  throwIfSemanticSequenceCancelled(token, index);
}

function cancelSemanticSequence(sequenceId) {
  const id = validateSemanticSequenceId(sequenceId);
  const token = activeSemanticSequences.get(id);
  if (!token) {
    return { sequenceId: id, cancelled: false, active: false };
  }
  token.cancelled = true;
  return { sequenceId: id, cancelled: true, active: true };
}

async function semanticSequence(tabId, expectedDocumentIdentity, rawSteps, sequenceId) {
  if (!Array.isArray(rawSteps) || rawSteps.length < 1 || rawSteps.length > 64) {
    throw new Error("Browser semantic sequence requires 1..64 steps.");
  }
  const id = validateSemanticSequenceId(sequenceId);
  if (activeSemanticSequences.has(id)) {
    throw new Error("A browser semantic sequence with this ID is already active.");
  }

  const token = { cancelled: false };
  activeSemanticSequences.set(id, token);
  const startedAt = Date.now();
  const deadline = startedAt + 25_000;
  let totalWaitMs = 0;
  let totalTextBytes = 0;
  let completedSteps = 0;

  try {
    for (let index = 0; index < rawSteps.length; index += 1) {
      throwIfSemanticSequenceCancelled(token, index);
      if (Date.now() > deadline) {
        throw new Error(`SEQUENCE_TIMEOUT: Browser semantic sequence exceeded 25000 ms before step ${index + 1}.`);
      }

      const step = rawSteps[index] || {};
      const operation = String(step.operation || "");
      try {
        if (operation === "wait") {
          const waitMs = Number(step.waitMs ?? 0);
          if (!Number.isInteger(waitMs) || waitMs < 0 || waitMs > 2000) {
            throw new Error("Wait must be an integer from 0..2000 ms.");
          }
          totalWaitMs += waitMs;
          if (totalWaitMs > 10_000) {
            throw new Error("Total sequence wait time exceeds 10000 ms.");
          }
          if (waitMs > 0) await cancellableSemanticWait(waitMs, token, index);
        } else if (operation === "click") {
          await semanticClick(tabId, step.backendId, expectedDocumentIdentity);
          throwIfSemanticSequenceCancelled(token, index);
        } else if (operation === "type") {
          const text = String(step.text ?? "");
          totalTextBytes += Buffer.byteLength(text, "utf8");
          if (totalTextBytes > 128 * 1024) {
            throw new Error("Total sequence typed text exceeds 131072 bytes.");
          }
          await semanticType(
            tabId,
            step.backendId,
            text,
            step.clearFirst === true,
            expectedDocumentIdentity,
          );
          throwIfSemanticSequenceCancelled(token, index);
        } else {
          throw new Error(`Unsupported semantic sequence operation: ${operation || "<empty>"}.`);
        }
        completedSteps += 1;
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        if (message.startsWith("SEQUENCE_CANCELLED:") || message.startsWith("SEQUENCE_TIMEOUT:")) {
          throw error;
        }
        throw new Error(`SEQUENCE_STEP_${index + 1}: ${message}`);
      }
    }

    return {
      ok: true,
      sequenceId: id,
      completedSteps,
      totalSteps: rawSteps.length,
      elapsedMs: Date.now() - startedAt,
    };
  } finally {
    if (activeSemanticSequences.get(id) === token) {
      activeSemanticSequences.delete(id);
    }
  }
}

function normalizeConsoleEvent(tabId, message) {
  if (message.method === "Runtime.consoleAPICalled") {
    const type = String(message.params?.type || "log");
    if (!["error", "warning", "assert"].includes(type)) return null;
    const text = (message.params?.args || []).map((arg) => arg.value ?? arg.description ?? "").join(" ");
    return {
      kind: "console",
      tabId,
      level: type === "warning" ? "warning" : "error",
      message: clip(text || type, 8000),
      url: message.params?.stackTrace?.callFrames?.[0]?.url
        ? safeNetworkUrl(message.params.stackTrace.callFrames[0].url)
        : null,
      timestamp: Date.now(),
    };
  }
  if (message.method === "Runtime.exceptionThrown") {
    const details = message.params?.exceptionDetails || {};
    return {
      kind: "console",
      tabId,
      level: "error",
      message: clip(details.exception?.description || details.text || "Uncaught exception", 8000),
      url: details.url || details.stackTrace?.callFrames?.[0]?.url
        ? safeNetworkUrl(details.url || details.stackTrace.callFrames[0].url)
        : null,
      timestamp: Date.now(),
    };
  }
  if (message.method === "Log.entryAdded") {
    const entry = message.params?.entry || {};
    if (!["error", "warning"].includes(entry.level)) return null;
    return {
      kind: "console",
      tabId,
      level: entry.level,
      message: clip(entry.text || "Browser log entry", 8000),
      url: entry.url ? safeNetworkUrl(entry.url) : null,
      timestamp: Date.now(),
    };
  }
  return null;
}

function normalizeNetworkHistoryEvent(tabId, message, requests) {
  const params = message.params || {};
  const requestId = String(params.requestId || "");
  const request = requests.get(requestId) || {};
  if (message.method === "Network.responseReceived") {
    const response = params.response || {};
    const status = Number(response.status || 0);
    return {
      kind: "network-entry",
      tabId,
      requestId,
      url: safeNetworkUrl(response.url || request.url || ""),
      method: request.method || null,
      status: Number.isFinite(status) && status > 0 ? status : null,
      statusText: response.statusText ? clip(response.statusText, 500) : null,
      resourceType: params.type || request.resourceType || null,
      mimeType: response.mimeType ? clip(response.mimeType, 200) : null,
      failed: status >= 400,
      errorText: status >= 400 ? clip(response.statusText || `HTTP ${status}`, 1000) : null,
      timestamp: Date.now(),
    };
  }
  if (message.method === "Network.loadingFailed") {
    return {
      kind: "network-entry",
      tabId,
      requestId,
      url: safeNetworkUrl(request.url || ""),
      method: request.method || null,
      status: null,
      statusText: null,
      resourceType: params.type || request.resourceType || null,
      mimeType: null,
      failed: true,
      errorText: clip(params.errorText || "Network request failed", 1000),
      timestamp: Date.now(),
    };
  }
  return null;
}

function normalizeNetworkEvent(tabId, message, requests) {
  if (message.method === "Network.requestWillBeSent") {
    const request = message.params?.request || {};
    requests.set(String(message.params?.requestId || ""), {
      url: request.url ? safeNetworkUrl(request.url) : null,
      method: request.method || null,
      resourceType: message.params?.type || null,
    });
    return null;
  }
  if (message.method === "Network.loadingFinished") {
    requests.delete(String(message.params?.requestId || ""));
    return null;
  }
  if (message.method === "Network.loadingFailed") {
    const params = message.params || {};
    const request = requests.get(String(params.requestId || "")) || {};
    requests.delete(String(params.requestId || ""));
    return {
      kind: "network",
      tabId,
      url: request.url ? safeNetworkUrl(request.url) : null,
      method: request.method || null,
      status: null,
      errorText: clip(params.errorText || "Network request failed", 4000),
      resourceType: params.type || request.resourceType || null,
      timestamp: Date.now(),
    };
  }
  if (message.method === "Network.responseReceived") {
    const response = message.params?.response || {};
    const status = Number(response.status || 0);
    if (status < 400) return null;
    const request = requests.get(String(message.params?.requestId || "")) || {};
    return {
      kind: "network",
      tabId,
      url: response.url || request.url ? safeNetworkUrl(response.url || request.url) : null,
      method: request.method || null,
      status,
      errorText: clip(response.statusText || `HTTP ${status}`, 4000),
      resourceType: message.params?.type || null,
      timestamp: Date.now(),
    };
  }
  return null;
}

function monitorEmitter(eventPath) {
  fs.mkdirSync(path.dirname(eventPath), { recursive: true });
  if (!fs.existsSync(eventPath)) fs.writeFileSync(eventPath, "");
  let approximateBytes = fs.existsSync(eventPath) ? fs.statSync(eventPath).size : 0;
  return (value) => {
    const line = `${JSON.stringify(value)}\n`;
    fs.appendFileSync(eventPath, line);
    approximateBytes += Buffer.byteLength(line);
    if (approximateBytes > 2_000_000) {
      try {
        const data = fs.readFileSync(eventPath);
        const tail = data.subarray(Math.max(0, data.length - 1_000_000));
        const firstNewline = tail.indexOf(10);
        const trimmed = firstNewline >= 0 ? tail.subarray(firstNewline + 1) : tail;
        fs.writeFileSync(eventPath, trimmed);
        approximateBytes = trimmed.length;
      } catch {}
    }
  };
}

async function configureDownloads(workspaceId, tabId, downloadPath, eventPath, relativeDirectory) {
  if (!path.isAbsolute(downloadPath)) throw new Error("Download path must be absolute.");
  fs.mkdirSync(downloadPath, { recursive: true });
  const stats = fs.statSync(downloadPath);
  if (!stats.isDirectory()) throw new Error("Download path is not a directory.");

  if (downloadTracker) {
    try { downloadTracker.unsubscribe(); } catch {}
    closeStandaloneCdpClient(downloadTracker.client);
    downloadTracker = null;
  }
  const version = await jsonRequest("/json/version");
  const browserSocket = String(version?.webSocketDebuggerUrl || "");
  if (!browserSocket) throw new Error("Chrome did not expose its browser-level DevTools endpoint.");
  const client = await createPersistentCdpClient({ webSocketDebuggerUrl: browserSocket });

  const emit = monitorEmitter(eventPath);
  const unsubscribe = client.onEvent((message) => {
    const params = message.params || {};
    if (message.method === "Browser.downloadWillBegin") {
      emit({
        kind: "download-start",
        workspaceId,
        tabId,
        guid: String(params.guid || ""),
        url: params.url ? safeNetworkUrl(params.url) : "",
        suggestedFilename: clip(params.suggestedFilename || "download", 1000),
        relativeDirectory,
        timestamp: Date.now(),
      });
    } else if (message.method === "Browser.downloadProgress") {
      emit({
        kind: "download-progress",
        workspaceId,
        tabId,
        guid: String(params.guid || ""),
        totalBytes: Number.isFinite(Number(params.totalBytes)) ? Number(params.totalBytes) : null,
        receivedBytes: Number.isFinite(Number(params.receivedBytes)) ? Number(params.receivedBytes) : 0,
        state: String(params.state || "inProgress"),
        relativeDirectory,
        timestamp: Date.now(),
      });
    }
  });

  try {
    await client.send("Browser.setDownloadBehavior", {
      behavior: "allowAndName",
      downloadPath,
      eventsEnabled: true,
    });
  } catch (error) {
    try { unsubscribe(); } catch {}
    throw error;
  }

  downloadTracker = { unsubscribe, client, workspaceId, downloadPath, eventPath, relativeDirectory };
  return {
    ok: true,
    tabId,
    relativeDirectory,
    exactPathsUseDownloadGuid: true,
    resumable: false,
  };
}

async function resetDownloads() {
  if (!downloadTracker) return { ok: true };
  const tracker = downloadTracker;
  downloadTracker = null;
  try {
    await tracker.client.send("Browser.setDownloadBehavior", {
      behavior: "default",
      eventsEnabled: false,
    });
  } finally {
    try { tracker.unsubscribe(); } catch {}
    closeStandaloneCdpClient(tracker.client);
  }
  return { ok: true };
}

async function cancelDownload(tabId, guid) {
  if (!guid || !/^[A-Za-z0-9_-]{1,160}$/.test(guid)) {
    throw new Error("Download GUID is invalid.");
  }
  if (!downloadTracker || downloadTracker.client.closed) {
    throw new Error("Browser download tracking is not configured.");
  }
  await downloadTracker.client.send("Browser.cancelDownload", { guid });
  return { ok: true, guid, tabId };
}

async function uploadFile(tabId, selector, filePath) {
  if (!selector || selector.length > 4096) throw new Error("Upload selector is invalid.");
  if (!path.isAbsolute(filePath)) throw new Error("Upload path must be absolute.");
  const stats = fs.statSync(filePath);
  if (!stats.isFile()) throw new Error("Upload path is not a regular file.");

  await cdpCommand(tabId, "DOM.enable");
  const document = await cdpCommand(tabId, "DOM.getDocument", { depth: 1, pierce: true });
  const rootNodeId = document?.root?.nodeId;
  if (!Number.isInteger(rootNodeId)) throw new Error("Chrome did not return a DOM root node.");
  const query = await cdpCommand(tabId, "DOM.querySelector", {
    nodeId: rootNodeId,
    selector,
  });
  const nodeId = query?.nodeId;
  if (!Number.isInteger(nodeId) || nodeId <= 0) {
    throw new Error("No upload element matches the selector.");
  }
  const attributes = await cdpCommand(tabId, "DOM.getAttributes", { nodeId });
  const flat = Array.isArray(attributes?.attributes) ? attributes.attributes : [];
  const attr = {};
  for (let i = 0; i + 1 < flat.length; i += 2) {
    attr[String(flat[i]).toLowerCase()] = String(flat[i + 1]);
  }
  if (String(attr.type || "").toLowerCase() !== "file") {
    throw new Error("The selected element is not an <input type=file> control.");
  }
  await cdpCommand(tabId, "DOM.setFileInputFiles", {
    files: [filePath],
    nodeId,
  });
  return {
    ok: true,
    tabId,
    selector,
    fileName: path.basename(filePath),
    sizeBytes: stats.size,
  };
}

async function monitorTarget(tab, emit) {
  let socket;
  try {
    socket = await cdpSocket(tab.webSocketDebuggerUrl);
  } catch {
    return;
  }
  let nextId = 1;
  const requests = new Map();
  const send = (method, params = {}) => {
    try { socket.send(JSON.stringify({ id: nextId++, method, params })); } catch {}
  };
  send("Runtime.enable");
  send("Log.enable");
  send("Network.enable", { maxTotalBufferSize: 1_000_000, maxResourceBufferSize: 256_000 });
  socket.addEventListener("message", (event) => {
    let message;
    try { message = JSON.parse(String(event.data)); } catch { return; }
    const consoleEvent = normalizeConsoleEvent(tab.id, message);
    if (consoleEvent) emit(consoleEvent);
    const networkHistoryEvent = normalizeNetworkHistoryEvent(tab.id, message, requests);
    if (networkHistoryEvent) emit(networkHistoryEvent);
    const networkEvent = normalizeNetworkEvent(tab.id, message, requests);
    if (networkEvent) emit(networkEvent);
  });
  await new Promise((resolve) => {
    socket.addEventListener("close", resolve, { once: true });
    socket.addEventListener("error", resolve, { once: true });
  });
}

async function monitor(eventPath) {
  const emit = monitorEmitter(eventPath);
  const active = new Set();
  while (true) {
    let tabs = [];
    try { tabs = await listTabs(); } catch {}
    for (const tab of tabs) {
      if (active.has(tab.id)) continue;
      active.add(tab.id);
      monitorTarget(tab, emit)
        .catch(() => undefined)
        .finally(() => active.delete(tab.id));
    }
    await new Promise((resolve) => setTimeout(resolve, 800));
  }
}

async function executeOperation(currentOperation, currentArgs, emit = out) {
  const operation = currentOperation;
  const args = currentArgs;
  const out = emit;
  switch (operation) {
    case "ping": {
      const version = await jsonRequest("/json/version");
      out({ ok: true, browser: version?.Browser || null, protocolVersion: version?.["Protocol-Version"] || null });
      return;
    }
    case "list-tabs": {
      out({ tabs: await listTabs() });
      return;
    }
    case "new-tab": {
      const url = args[0] || "about:blank";
      const headers = decodeJsonArg(args, 1, {});
      const userAgent = args[2] || "";
      const target = await jsonRequest(`/json/new?${encodeURIComponent("about:blank")}`, "PUT");
      if (target?.id) {
        const tabId = String(target.id);
        await applyContext(tabId, headers, userAgent);
        if (url !== "about:blank") {
          await cdpCommand(tabId, "Page.navigate", { url });
          await waitForDocument(tabId, 10000);
        }
        await minimizeWindowForTarget(tabId);
      }
      out({ tab: target });
      return;
    }
    case "new-window": {
      const url = args[0] || "about:blank";
      const headers = decodeJsonArg(args, 1, {});
      const userAgent = args[2] || "";
      const version = await jsonRequest("/json/version");
      const browserSocket = String(version?.webSocketDebuggerUrl || "");
      if (!browserSocket) throw new Error("Chrome did not expose its browser-level DevTools endpoint.");
      const client = await createPersistentCdpClient({ webSocketDebuggerUrl: browserSocket });
      let tabId = "";
      try {
        const created = await client.send("Target.createTarget", { url: "about:blank", newWindow: true });
        tabId = String(created?.targetId || "");
      } finally {
        closeStandaloneCdpClient(client);
      }
      if (!tabId) throw new Error("Chrome did not return the new window tab ID.");
      let target = null;
      for (let attempt = 0; attempt < 40; attempt += 1) {
        try {
          target = await findTab(tabId);
          break;
        } catch {}
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
      if (!target) throw new Error("Chrome created a new window, but its tab did not become ready.");
      await applyContext(tabId, headers, userAgent);
      if (url !== "about:blank") {
        await cdpCommand(tabId, "Page.navigate", { url });
        await waitForDocument(tabId, 10000);
      }
      await minimizeWindowForTarget(tabId);
      out({ tab: target });
      return;
    }
    case "activate-tab": {
      const tabId = args[0];
      await rawRequest(`/json/activate/${encodeURIComponent(tabId)}`);
      await minimizeWindowForTarget(tabId);
      out({ ok: true });
      return;
    }
    case "minimize-window": {
      const tabId = args[0];
      await minimizeWindowForTarget(tabId);
      out({ ok: true });
      return;
    }
    case "close-tab": {
      const tabId = args[0];
      await rawRequest(`/json/close/${encodeURIComponent(tabId)}`);
      closePersistentCdpClient(tabId);
      out({ ok: true });
      return;
    }
    case "apply-context": {
      const [tabId] = args;
      const headers = decodeJsonArg(args, 1, {});
      const userAgent = args[2] || "";
      out(await applyContext(tabId, headers, userAgent));
      return;
    }
    case "navigate": {
      const [tabId, url] = args;
      await cdpCommand(tabId, "Page.navigate", { url });
      await waitForDocument(tabId, 10000);
      out({ ok: true });
      return;
    }
    case "navigate-observe": {
      const [tabId, url] = args;
      const timeoutMs = Number(args[2] || 12000);
      out(await navigateObserve(tabId, url, timeoutMs));
      return;
    }
    case "mutation-state": {
      const [tabId] = args;
      out(await mutationState(tabId));
      return;
    }
    case "reload": {
      const [tabId] = args;
      await cdpCommand(tabId, "Page.reload", { ignoreCache: false });
      await waitForDocument(tabId, 10000);
      out({ ok: true });
      return;
    }
    case "configure-downloads": {
      const [workspaceId, tabId, downloadPath, eventPath, relativeDirectory] = args;
      out(await configureDownloads(workspaceId, tabId, downloadPath, eventPath, relativeDirectory));
      return;
    }
    case "reset-downloads": {
      out(await resetDownloads());
      return;
    }
    case "cancel-download": {
      const [tabId, guid] = args;
      out(await cancelDownload(tabId, guid));
      return;
    }
    case "upload-file": {
      const [tabId, selector, filePath] = args;
      out(await uploadFile(tabId, selector, filePath));
      return;
    }
    case "click": {
      const [tabId, selector] = args;
      const expression = `(() => { const el = document.querySelector(${JSON.stringify(selector)}); if (!el) return {ok:false,error:'No element matches the selector.'}; el.scrollIntoView({block:'center',inline:'center'}); el.click(); return {ok:true,tag:el.tagName,text:(el.innerText||el.getAttribute('aria-label')||el.getAttribute('title')||'').slice(0,500)}; })()`;
      const result = await evaluate(tabId, expression);
      if (!result?.ok) throw new Error(result?.error || "Could not click the selected element.");
      await new Promise((resolve) => setTimeout(resolve, 180));
      out({ ok: true, detail: result });
      return;
    }
    case "type": {
      const [tabId, selector, text] = args;
      const clearFirst = decodeJsonArg(args, 3, true) !== false;
      const focusExpression = `(() => { const el = document.querySelector(${JSON.stringify(selector)}); if (!el) return {ok:false,error:'No element matches the selector.'}; el.scrollIntoView({block:'center',inline:'center'}); el.focus(); if (${clearFirst ? "true" : "false"}) { if ('value' in el) { const proto = Object.getPrototypeOf(el); const descriptor = Object.getOwnPropertyDescriptor(proto, 'value'); if (descriptor?.set) descriptor.set.call(el, ''); else el.value=''; el.dispatchEvent(new Event('input',{bubbles:true})); } else if (el.isContentEditable) { el.textContent=''; el.dispatchEvent(new Event('input',{bubbles:true})); } } return {ok:true}; })()`;
      const focus = await evaluate(tabId, focusExpression);
      if (!focus?.ok) throw new Error(focus?.error || "Could not focus the selected element.");
      await cdpCommand(tabId, "Input.insertText", { text });
      out({ ok: true });
      return;
    }
    case "scroll": {
      const [tabId] = args;
      const x = Number(args[1] || 0);
      const y = Number(args[2] || 0);
      if (!Number.isFinite(x) || !Number.isFinite(y)) throw new Error("Scroll distances must be numbers.");
      await evaluate(tabId, `(() => { window.scrollBy(${x}, ${y}); return {x:window.scrollX,y:window.scrollY}; })()`);
      out({ ok: true });
      return;
    }
    case "semantic-snapshot": {
      const [tabId] = args;
      out(await semanticSnapshot(tabId, args[1]));
      return;
    }
    case "semantic-click": {
      const [tabId, backendId, expectedDocumentIdentity] = args;
      out(await semanticClick(tabId, backendId, expectedDocumentIdentity));
      return;
    }
    case "semantic-type": {
      const [tabId, backendId, text] = args;
      const clearFirst = decodeJsonArg(args, 3, false) === true;
      const expectedDocumentIdentity = args[4] || "";
      out(await semanticType(tabId, backendId, text, clearFirst, expectedDocumentIdentity));
      return;
    }
    case "semantic-sequence": {
      const [tabId, expectedDocumentIdentity, sequenceId] = args;
      const steps = decodeJsonArg(args, 3, []);
      out(await semanticSequence(tabId, expectedDocumentIdentity, steps, sequenceId));
      return;
    }
    case "semantic-sequence-cancel": {
      const [sequenceId] = args;
      out(cancelSemanticSequence(sequenceId));
      return;
    }
    case "inspect": {
      const [tabId] = args;
      const selector = args[1] || "";
      const maxChars = Math.min(Math.max(Number(args[2] || 12000), 1000), 50000);
      const expression = selector
        ? `(() => { const el=document.querySelector(${JSON.stringify(selector)}); if(!el) return {found:false,title:document.title,url:location.href}; return {found:true,title:document.title,url:location.href,selector:${JSON.stringify(selector)},tag:el.tagName,text:(el.innerText||el.textContent||'').slice(0,${maxChars}),html:el.outerHTML.slice(0,${maxChars})}; })()`
        : `(() => ({found:true,title:document.title,url:location.href,selector:null,tag:'DOCUMENT',text:(document.body?.innerText||'').slice(0,${maxChars}),html:(document.documentElement?.outerHTML||'').slice(0,${maxChars})}))()`;
      const inspection = await evaluate(tabId, expression);
      let generation = null;
      try {
        const tree = await cdpCommand(tabId, "Page.getFrameTree");
        generation = tree?.frameTree?.frame?.loaderId || null;
      } catch {}
      out({ ...(inspection || {}), documentGeneration: generation });
      return;
    }
    case "pick-element": {
      const [tabId] = args;
      const xRatio = Math.min(Math.max(Number(args[1] || 0), 0), 1);
      const yRatio = Math.min(Math.max(Number(args[2] || 0), 0), 1);
      const expression = `(() => {
        const x = Math.max(0, Math.min(window.innerWidth - 1, ${xRatio} * window.innerWidth));
        const y = Math.max(0, Math.min(window.innerHeight - 1, ${yRatio} * window.innerHeight));
        const el = document.elementFromPoint(x, y);
        if (!el) return {found:false};
        const esc = (value) => (window.CSS && CSS.escape) ? CSS.escape(value) : String(value).replace(/[^a-zA-Z0-9_-]/g, '\\\\$&');
        const unique = (selector) => { try { return document.querySelectorAll(selector).length === 1; } catch { return false; } };
        let selector = '';
        if (el.id) {
          const candidate = '#' + esc(el.id);
          if (unique(candidate)) selector = candidate;
        }
        if (!selector) {
          for (const attr of ['data-testid','data-test','data-cy','name','aria-label']) {
            const value = el.getAttribute(attr);
            if (!value) continue;
            const candidate = el.tagName.toLowerCase() + '[' + attr + '=' + JSON.stringify(value) + ']';
            if (unique(candidate)) { selector = candidate; break; }
          }
        }
        if (!selector) {
          const parts = [];
          let node = el;
          while (node && node.nodeType === 1 && node !== document.documentElement) {
            let part = node.tagName.toLowerCase();
            const useful = Array.from(node.classList || []).filter((name) => /^[a-zA-Z_][a-zA-Z0-9_-]*$/.test(name)).slice(0, 2);
            if (useful.length) part += '.' + useful.map(esc).join('.');
            const parent = node.parentElement;
            if (parent) {
              const same = Array.from(parent.children).filter((child) => child.tagName === node.tagName);
              if (same.length > 1) part += ':nth-of-type(' + (same.indexOf(node) + 1) + ')';
            }
            parts.unshift(part);
            const candidate = parts.join(' > ');
            if (unique(candidate)) { selector = candidate; break; }
            node = parent;
          }
          if (!selector) selector = parts.join(' > ') || el.tagName.toLowerCase();
        }
        return {
          found:true,
          url:location.href,
          selector,
          tag:el.tagName,
          text:(el.innerText || el.textContent || el.getAttribute('aria-label') || '').trim().slice(0,2000),
          html:el.outerHTML.slice(0,12000)
        };
      })()`;
      const result = await evaluate(tabId, expression);
      if (!result?.found) throw new Error("No page element was found at that preview position.");
      out(result);
      return;
    }
    case "screenshot": {
      const [tabId] = args;
      const fullPage = decodeJsonArg(args, 1, false) === true;
      const outputPath = args[2] || "";
      await cdpCommand(tabId, "Page.enable");
      const result = await cdpCommand(tabId, "Page.captureScreenshot", {
        format: "png",
        fromSurface: true,
        captureBeyondViewport: fullPage,
      });
      if (outputPath && result.data) { fs.mkdirSync(path.dirname(outputPath), { recursive: true }); fs.writeFileSync(outputPath, Buffer.from(result.data, "base64")); }
      out({ data: result.data || "", mimeType: "image/png", fullPage });
      return;
    }
    case "monitor": {
      const eventPath = args[0];
      if (!eventPath) throw new Error("Missing browser monitor event path.");
      await monitor(eventPath);
      return;
    }
    default:
      throw new Error(`Unsupported RepoTunnel browser operation: ${operation}`);
  }
}

async function serve() {
  const rl = readline.createInterface({
    input: process.stdin,
    crlfDelay: Infinity,
  });
  const inFlight = new Set();

  const handleLine = async (line) => {
    if (!line.trim()) return;
    if (Buffer.byteLength(line, "utf8") > 512 * 1024) {
      out({ id: null, ok: false, error: "Persistent browser helper request is too large." });
      return;
    }

    let request;
    try {
      request = JSON.parse(line);
    } catch {
      out({ id: null, ok: false, error: "Persistent browser helper received invalid JSON." });
      return;
    }

    const id = request?.id;
    if (!Number.isSafeInteger(id) || id < 1) {
      out({ id: null, ok: false, error: "Persistent browser helper request ID is invalid." });
      return;
    }

    const requestedOperation = String(request?.operation || "");
    if (!requestedOperation || requestedOperation === "serve" || requestedOperation === "monitor") {
      out({ id, ok: false, error: "Persistent browser helper operation is not allowed." });
      return;
    }
    const requestedArgs = Array.isArray(request?.args)
      ? request.args.map((value) => String(value ?? ""))
      : [];

    let emitted = false;
    let result = {};
    try {
      await executeOperation(requestedOperation, requestedArgs, (value) => {
        emitted = true;
        result = value ?? {};
      });
      out({ id, ok: true, result: emitted ? result : {} });
    } catch (error) {
      out({
        id,
        ok: false,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  };

  try {
    for await (const line of rl) {
      if (inFlight.size >= 32) {
        await Promise.race(inFlight);
      }
      let task;
      task = handleLine(line)
        .catch((error) => {
          out({
            id: null,
            ok: false,
            error: error instanceof Error ? error.message : String(error),
          });
        })
        .finally(() => inFlight.delete(task));
      inFlight.add(task);
    }
    await Promise.allSettled(Array.from(inFlight));
  } finally {
    await closeAllPersistentCdpClients();
  }
}

async function main() {
  if (operation === "serve") {
    await serve();
    return;
  }
  await executeOperation(operation, args, out);
}

main().catch((error) => fail(error instanceof Error ? error.message : String(error)));
