#!/usr/bin/env node

import { mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';

import { buildReleaseFromArtifact } from './index.mjs';

function parseArgs(argv) {
  const args = new Map();
  for (let index = 0; index < argv.length; index += 2) {
    const key = argv[index];
    const value = argv[index + 1];
    if (!key?.startsWith('--') || value === undefined || value.startsWith('--')) {
      throw new Error(`invalid argument near ${JSON.stringify(key)}`);
    }
    if (args.has(key)) throw new Error(`duplicate argument: ${key}`);
    args.set(key, value);
  }
  return args;
}

function required(args, key) {
  const value = args.get(key);
  if (!value) throw new Error(`missing required argument ${key}`);
  return value;
}

const args = parseArgs(process.argv.slice(2));
const capability = required(args, '--capability');
const version = required(args, '--version');
const os = required(args, '--os');
const arch = required(args, '--arch');
const artifactPath = required(args, '--artifact');
const contentRoot = required(args, '--content-root');
const output = required(args, '--output');
const repoTunnel = args.get('--repo-tunnel') ?? '>=0.4.0, <0.5.0';

const release = await buildReleaseFromArtifact({
  capability,
  version,
  repoTunnel,
  os,
  arch,
  artifactPath,
  contentRoot,
});

await mkdir(path.dirname(output), { recursive: true });
await writeFile(output, `${JSON.stringify(release, null, 2)}\n`, { flag: 'wx' });
console.log(output);
