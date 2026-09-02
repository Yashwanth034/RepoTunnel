#!/usr/bin/env node
"use strict";

const fs = require("node:fs");
const path = require("node:path");

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

async function cdpCommand(tabId, method, params = {}) {
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

function decodeJsonArg(index, fallback) {
  if (args[index] === undefined) return fallback;
  try { return JSON.parse(args[index]); } catch { throw new Error("Invalid RepoTunnel browser argument."); }
}

function clip(value, max = 16000) {
  const text = value == null ? "" : String(value);
  return text.length > max ? `${text.slice(0, max)}\n…truncated` : text;
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
      url: message.params?.stackTrace?.callFrames?.[0]?.url || null,
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
      url: details.url || details.stackTrace?.callFrames?.[0]?.url || null,
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
      url: entry.url || null,
      timestamp: Date.now(),
    };
  }
  return null;
}

function normalizeNetworkEvent(tabId, message, requests) {
  if (message.method === "Network.requestWillBeSent") {
    const request = message.params?.request || {};
    requests.set(String(message.params?.requestId || ""), {
      url: request.url || null,
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
      url: request.url || null,
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
      url: response.url || request.url || null,
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

async function main() {
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
      const target = await jsonRequest(`/json/new?${encodeURIComponent(url)}`, "PUT");
      if (target?.id) await waitForDocument(String(target.id), 10000);
      out({ tab: target });
      return;
    }
    case "activate-tab": {
      const tabId = args[0];
      await rawRequest(`/json/activate/${encodeURIComponent(tabId)}`);
      out({ ok: true });
      return;
    }
    case "close-tab": {
      const tabId = args[0];
      await rawRequest(`/json/close/${encodeURIComponent(tabId)}`);
      out({ ok: true });
      return;
    }
    case "navigate": {
      const [tabId, url] = args;
      await cdpCommand(tabId, "Page.navigate", { url });
      await waitForDocument(tabId, 10000);
      out({ ok: true });
      return;
    }
    case "reload": {
      const [tabId] = args;
      await cdpCommand(tabId, "Page.reload", { ignoreCache: false });
      await waitForDocument(tabId, 10000);
      out({ ok: true });
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
      const clearFirst = decodeJsonArg(3, true) !== false;
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
    case "inspect": {
      const [tabId] = args;
      const selector = args[1] || "";
      const maxChars = Math.min(Math.max(Number(args[2] || 12000), 1000), 50000);
      const expression = selector
        ? `(() => { const el=document.querySelector(${JSON.stringify(selector)}); if(!el) return {found:false,title:document.title,url:location.href}; return {found:true,title:document.title,url:location.href,selector:${JSON.stringify(selector)},tag:el.tagName,text:(el.innerText||el.textContent||'').slice(0,${maxChars}),html:el.outerHTML.slice(0,${maxChars})}; })()`
        : `(() => ({found:true,title:document.title,url:location.href,selector:null,tag:'DOCUMENT',text:(document.body?.innerText||'').slice(0,${maxChars}),html:(document.documentElement?.outerHTML||'').slice(0,${maxChars})}))()`;
      out(await evaluate(tabId, expression));
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
      const fullPage = decodeJsonArg(1, false) === true;
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

main().catch((error) => fail(error instanceof Error ? error.message : String(error)));
