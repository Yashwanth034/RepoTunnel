import fs from "node:fs";
import path from "node:path";
import { spawn } from "node:child_process";
import { pathToFileURL } from "node:url";

const configPath = process.argv[2];
if (!configPath) {
  throw new Error("Video HTML capture config path is required.");
}
const config = JSON.parse(fs.readFileSync(configPath, "utf8"));
for (const key of ["chrome", "htmlPath", "framesDir", "qaReportPath", "width", "height", "fps", "frameCount"]) {
  if (config[key] === undefined || config[key] === null || config[key] === "") {
    throw new Error(`Missing capture config field: ${key}`);
  }
}

fs.mkdirSync(config.framesDir, { recursive: true });
if (config.sampleDir) fs.mkdirSync(config.sampleDir, { recursive: true });
const profileDir = fs.mkdtempSync(path.join(config.framesDir, ".chrome-profile-"));

const chrome = spawn(
  config.chrome,
  [
    "--headless=new",
    "--remote-debugging-pipe",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-renderer-backgrounding",
    "--disable-features=Translate,BackForwardCache",
    "--hide-scrollbars",
    "--force-device-scale-factor=1",
    `--user-data-dir=${profileDir}`,
    "about:blank",
  ],
  { stdio: ["ignore", "ignore", "pipe", "pipe", "pipe"] },
);

const writePipe = chrome.stdio[3];
const readPipe = chrome.stdio[4];
if (!writePipe || !readPipe) {
  throw new Error("Chrome remote debugging pipe is unavailable.");
}

let nextId = 1;
let buffered = Buffer.alloc(0);
const pending = new Map();
const listeners = new Map();

function onEvent(method, handler) {
  const list = listeners.get(method) || [];
  list.push(handler);
  listeners.set(method, list);
  return () => listeners.set(method, (listeners.get(method) || []).filter((item) => item !== handler));
}

function handleMessage(message) {
  if (message.id && pending.has(message.id)) {
    const { resolve, reject } = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) reject(new Error(message.error.message || JSON.stringify(message.error)));
    else resolve(message.result || {});
    return;
  }
  if (message.method) {
    for (const handler of listeners.get(message.method) || []) handler(message);
  }
}

readPipe.on("data", (chunk) => {
  buffered = Buffer.concat([buffered, chunk]);
  for (;;) {
    const marker = buffered.indexOf(0);
    if (marker < 0) break;
    const raw = buffered.subarray(0, marker).toString("utf8").trim();
    buffered = buffered.subarray(marker + 1);
    if (!raw) continue;
    try {
      handleMessage(JSON.parse(raw));
    } catch (error) {
      process.stderr.write(`Invalid Chrome DevTools message: ${error}\n`);
    }
  }
});

function send(method, params = {}, sessionId) {
  const id = nextId++;
  const payload = { id, method, params };
  if (sessionId) payload.sessionId = sessionId;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    writePipe.write(Buffer.from(JSON.stringify(payload) + "\0"));
    setTimeout(() => {
      if (pending.delete(id)) reject(new Error(`Chrome DevTools command timed out: ${method}`));
    }, 15000);
  });
}

async function evaluate(sessionId, expression, awaitPromise = true) {
  const result = await send(
    "Runtime.evaluate",
    { expression, awaitPromise, returnByValue: true, userGesture: false },
    sessionId,
  );
  if (result.exceptionDetails) {
    throw new Error(result.exceptionDetails.text || "HTML scene evaluation failed.");
  }
  return result.result?.value;
}

async function waitForLoad(sessionId) {
  return new Promise((resolve, reject) => {
    const cleanup = onEvent("Page.loadEventFired", (message) => {
      if (message.sessionId !== sessionId) return;
      cleanup();
      clearTimeout(timer);
      resolve();
    });
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error("HTML scene did not finish loading."));
    }, 15000);
  });
}

function frameName(index) {
  return `frame-${String(index).padStart(6, "0")}.png`;
}

function sampleIndices(frameCount) {
  if (frameCount <= 1) return [0];
  return [...new Set([0.1, 0.3, 0.5, 0.7, 0.9].map((p) => Math.min(frameCount - 1, Math.round((frameCount - 1) * p))))];
}

async function main() {
  const target = await send("Target.createTarget", { url: "about:blank" });
  const attached = await send("Target.attachToTarget", { targetId: target.targetId, flatten: true });
  const sessionId = attached.sessionId;
  await send("Page.enable", {}, sessionId);
  await send("Runtime.enable", {}, sessionId);
  await send(
    "Emulation.setDeviceMetricsOverride",
    {
      width: Number(config.width),
      height: Number(config.height),
      deviceScaleFactor: 1,
      mobile: false,
      screenWidth: Number(config.width),
      screenHeight: Number(config.height),
    },
    sessionId,
  );

  const loaded = waitForLoad(sessionId);
  const url = pathToFileURL(path.resolve(config.htmlPath)).href;
  await send("Page.navigate", { url }, sessionId);
  await loaded;

  let ready = false;
  for (let attempt = 0; attempt < 80; attempt += 1) {
    ready = Boolean(await evaluate(sessionId, "Boolean(window.__repotunnelReady)", false));
    if (ready) break;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  if (!ready) throw new Error("HTML/GSAP scene did not become ready.");

  const duration = Number(await evaluate(sessionId, "window.__repotunnelDuration")) || Number(config.frameCount) / Number(config.fps);
  const qaTimes = [...new Set([
    Math.max(0.08, Math.min(duration * 0.25, duration - 0.62)),
    Math.max(0.08, Math.min(duration * 0.50, duration - 0.62)),
    Math.max(0.08, Math.min(duration * 0.75, duration - 0.62)),
  ].map((value) => Number(value.toFixed(4))))];

  async function runQaSweep() {
    const reports = [];
    for (const t of qaTimes) {
      await evaluate(sessionId, `window.__repotunnelSeek(${JSON.stringify(t)})`);
      reports.push({ atSeconds: t, ...(await evaluate(sessionId, "window.__repotunnelQa()")) });
    }
    const issues = reports.flatMap((report) =>
      (report.issues || []).map((issue) => ({ ...issue, atSeconds: report.atSeconds }))
    );
    return {
      passed: reports.every((report) => report.passed),
      coverageRatio: reports.length ? Math.min(...reports.map((report) => Number(report.coverageRatio) || 0)) : 0,
      sampleReports: reports,
      issues,
    };
  }

  let qa = await runQaSweep();
  let autoFixed = false;
  if (!qa?.passed) {
    autoFixed = true;
    await evaluate(sessionId, "window.__repotunnelAutoFix()");
    qa = await runQaSweep();
  }
  qa = { ...qa, autoFixed };
  fs.mkdirSync(path.dirname(config.qaReportPath), { recursive: true });
  fs.writeFileSync(config.qaReportPath, JSON.stringify(qa, null, 2));
  if (!qa.passed) {
    throw new Error(`HTML design QA failed after auto-fix: ${(qa.issues || []).map((x) => x.code).join(", ")}`);
  }

  const samples = new Set(sampleIndices(Number(config.frameCount)));
  for (let frame = 0; frame < Number(config.frameCount); frame += 1) {
    const t = frame / Number(config.fps);
    await evaluate(sessionId, `window.__repotunnelSeek(${JSON.stringify(t)})`);
    const shot = await send(
      "Page.captureScreenshot",
      { format: "png", fromSurface: true, captureBeyondViewport: false },
      sessionId,
    );
    const bytes = Buffer.from(shot.data, "base64");
    fs.writeFileSync(path.join(config.framesDir, frameName(frame)), bytes);
    if (config.sampleDir && samples.has(frame)) {
      fs.writeFileSync(path.join(config.sampleDir, `sample-${String(frame).padStart(6, "0")}.png`), bytes);
    }
  }

  await send("Target.closeTarget", { targetId: target.targetId }).catch(() => {});
}

let failure = null;
try {
  await main();
} catch (error) {
  failure = error;
} finally {
  try { writePipe.end(); } catch {}
  try { chrome.kill("SIGTERM"); } catch {}
  try { fs.rmSync(profileDir, { recursive: true, force: true }); } catch {}
}
if (failure) {
  process.stderr.write(String(failure?.stack || failure) + "\n");
  process.exit(1);
}
