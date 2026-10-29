#!/usr/bin/env node

import { spawn } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import readline from 'node:readline';

function parseArgs(argv) {
  const args = new Map();
  for (let index = 0; index < argv.length; index += 2) {
    const key = argv[index];
    const value = argv[index + 1];
    if (!key?.startsWith('--') || value === undefined || value.startsWith('--')) {
      throw new Error(`invalid argument near ${JSON.stringify(key)}`);
    }
    args.set(key, value);
  }
  return args;
}

function required(args, key) {
  const value = args.get(key);
  if (!value) throw new Error(`missing ${key}`);
  return value;
}

const args = parseArgs(process.argv.slice(2));
const nodePath = path.resolve(required(args, '--node'));
const packageRoot = path.resolve(required(args, '--package-root'));
const expectedVersion = required(args, '--version');
const manifest = JSON.parse(await readFile(path.join(packageRoot, 'package.json'), 'utf8'));
if (manifest.name !== 'rea-agents' || manifest.version !== expectedVersion) {
  throw new Error(`unexpected REA package identity ${manifest.name}@${manifest.version}`);
}
const scriptRelative = manifest.bin?.rea ?? manifest.bin?.['rea-agents'];
if (typeof scriptRelative !== 'string' || !scriptRelative) {
  throw new Error('REA package has no expected CLI entrypoint');
}
const scriptPath = path.join(packageRoot, scriptRelative);

const child = spawn(nodePath, [scriptPath, 'mcp'], {
  cwd: packageRoot,
  stdio: ['pipe', 'pipe', 'pipe'],
  env: {
    PATH: process.env.PATH ?? '',
    HOME: process.env.HOME ?? process.cwd(),
    TMPDIR: process.env.TMPDIR ?? '',
    TEMP: process.env.TEMP ?? '',
    TMP: process.env.TMP ?? '',
    LANG: process.env.LANG ?? 'C.UTF-8',
    NO_COLOR: '1',
  },
});

let stderr = '';
child.stderr.setEncoding('utf8');
child.stderr.on('data', (chunk) => {
  stderr += chunk;
  if (stderr.length > 64 * 1024) stderr = stderr.slice(-64 * 1024);
});

let settled = false;
const timer = setTimeout(() => finish(new Error(`REA MCP smoke timed out. stderr: ${stderr}`)), 30_000);

function send(message) {
  child.stdin.write(`${JSON.stringify(message)}\n`);
}

function finish(error) {
  if (settled) return;
  settled = true;
  clearTimeout(timer);
  child.stdin.end();
  child.kill();
  if (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}

child.once('error', (error) => finish(error));
child.once('exit', (code, signal) => {
  if (!settled) finish(new Error(`REA MCP exited before tools/list (code=${code}, signal=${signal}). stderr: ${stderr}`));
});

const lines = readline.createInterface({ input: child.stdout });
lines.on('line', (line) => {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    return;
  }
  if (message.id === 1) {
    if (message.error || !message.result?.protocolVersion) {
      finish(new Error(`REA MCP initialize failed: ${line}`));
      return;
    }
    send({ jsonrpc: '2.0', method: 'notifications/initialized' });
    send({ jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} });
    return;
  }
  if (message.id === 2) {
    const tools = message.result?.tools;
    if (message.error || !Array.isArray(tools) || tools.length === 0) {
      finish(new Error(`REA MCP tools/list failed: ${line}`));
      return;
    }
    console.log(JSON.stringify({
      packageVersion: expectedVersion,
      protocolVersion: message.result?.protocolVersion ?? null,
      toolCount: tools.length,
      sampleTools: tools.slice(0, 8).map((tool) => tool.name),
    }));
    finish();
  }
});

send({
  jsonrpc: '2.0',
  id: 1,
  method: 'initialize',
  params: {
    protocolVersion: '2025-06-18',
    capabilities: {},
    clientInfo: { name: 'repotunnel-managed-capability-ci', version: '1.0.0' },
  },
});
