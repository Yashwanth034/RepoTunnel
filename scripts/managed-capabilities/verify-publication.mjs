#!/usr/bin/env node

import path from 'node:path';
import process from 'node:process';

import { verifyPublicationBundle } from './index.mjs';

function parseArgs(argv) {
  const values = new Map();
  for (let index = 0; index < argv.length; index += 2) {
    const flag = argv[index];
    const value = argv[index + 1];
    if (!flag?.startsWith('--') || value === undefined) {
      throw new Error('usage: verify-publication.mjs --index <capability-index.json> --bundle-dir <directory>');
    }
    values.set(flag, value);
  }
  const indexPath = values.get('--index');
  const bundleDir = values.get('--bundle-dir');
  if (!indexPath || !bundleDir || values.size !== 2) {
    throw new Error('usage: verify-publication.mjs --index <capability-index.json> --bundle-dir <directory>');
  }
  return { indexPath: path.resolve(indexPath), bundleDir: path.resolve(bundleDir) };
}

try {
  const result = await verifyPublicationBundle(parseArgs(process.argv.slice(2)));
  process.stdout.write(`Verified publication generation ${result.generation} with ${result.archiveCount} exact archives.\n`);
} catch (error) {
  process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
  process.exitCode = 1;
}
