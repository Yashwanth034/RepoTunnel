#!/usr/bin/env node

import { mkdir, readdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';

import { buildIndex } from './index.mjs';

function parseArgs(argv) {
  const values = new Map();
  const flags = new Set();
  for (let index = 0; index < argv.length; index += 1) {
    const key = argv[index];
    if (key === '--allow-breaking') {
      flags.add(key);
      continue;
    }
    const value = argv[index + 1];
    if (!key?.startsWith('--') || value === undefined || value.startsWith('--')) {
      throw new Error(`invalid argument near ${JSON.stringify(key)}`);
    }
    if (values.has(key)) throw new Error(`duplicate argument: ${key}`);
    values.set(key, value);
    index += 1;
  }
  return { values, flags };
}

function required(values, key) {
  const value = values.get(key);
  if (!value) throw new Error(`missing required argument ${key}`);
  return value;
}

async function readJson(filePath) {
  return JSON.parse(await readFile(filePath, 'utf8'));
}

const { values, flags } = parseArgs(process.argv.slice(2));
const candidateDir = required(values, '--candidate-dir');
const output = required(values, '--output');
const previousPath = values.get('--previous');
const now = values.has('--now') ? Number(values.get('--now')) : Math.floor(Date.now() / 1000);
if (!Number.isSafeInteger(now) || now <= 0) throw new Error('--now must be positive Unix seconds');

const files = (await readdir(candidateDir, { withFileTypes: true }))
  .filter((entry) => entry.isFile() && entry.name.endsWith('.json'))
  .map((entry) => path.join(candidateDir, entry.name))
  .sort();
const candidates = [];
for (const file of files) {
  const value = await readJson(file);
  if (typeof value?.capability !== 'string') continue;
  candidates.push(value);
}
if (candidates.length === 0) throw new Error('candidate directory contains no release metadata JSON');

let previousIndex = null;
if (previousPath) {
  try {
    previousIndex = await readJson(previousPath);
  } catch (error) {
    if (error?.code !== 'ENOENT') throw error;
  }
}

const index = buildIndex({
  previousIndex,
  candidates,
  now,
  allowBreaking: flags.has('--allow-breaking'),
});
await mkdir(path.dirname(output), { recursive: true });
await writeFile(output, `${JSON.stringify(index, null, 2)}\n`, { flag: 'wx' });
console.log(output);
