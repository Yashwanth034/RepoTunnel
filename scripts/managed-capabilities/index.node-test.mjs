import assert from 'node:assert/strict';
import { mkdtemp, mkdir, symlink, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

import {
  MAX_INDEX_LIFETIME_SECONDS,
  buildIndex,
  installedTreeSha256,
} from './index.mjs';

const A = 'a'.repeat(64);
const B = 'b'.repeat(64);
const C = 'c'.repeat(64);

function release(overrides = {}) {
  const capability = overrides.capability ?? 'ast-grep';
  const osName = overrides.os ?? 'linux';
  const version = overrides.version ?? '0.46.0';
  const entrypoint = capability === 'rea'
    ? 'package/package.json'
    : capability === 'rea-node'
      ? (osName === 'windows' ? 'node.exe' : 'bin/node')
      : (osName === 'windows' ? 'ast-grep.exe' : 'ast-grep');
  const artifactType = capability === 'rea' ? 'npm-bundle-zip' : capability === 'rea-node' ? 'node-binary-zip' : 'zip';
  return {
    capability,
    version,
    repoTunnel: '>=0.4.0, <0.5.0',
    os: osName,
    arch: overrides.arch ?? 'x86_64',
    artifactType,
    url: `https://github.com/Yashwanth034/RepoTunnel/releases/download/managed-capabilities/${capability}-${version}-${osName}-${overrides.arch ?? 'x86_64'}.zip`,
    size: 12345,
    sha256: A,
    entrypoint,
    entrypointSha256: B,
    contentSha256: capability === 'rea' ? C : B,
    ...overrides,
  };
}

function previousIndex(releases) {
  return {
    schema: 1,
    generation: 7,
    issuedAt: 900,
    expiresAt: 2_000,
    releases,
  };
}

test('publisher derives release identity from the exact built artifact and payload tree', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'repotunnel-managed-release-'));
  const contentRoot = path.join(root, 'content');
  await mkdir(path.join(contentRoot, 'package/scripts'), { recursive: true });
  await writeFile(path.join(contentRoot, 'package/package.json'), '{"name":"rea-agents","version":"6.3.0"}');
  await writeFile(path.join(contentRoot, 'package/scripts/rea.mjs'), "console.log('ok');\n");
  const artifact = path.join(root, 'rea-6.3.0-linux-x86_64.zip');
  await writeFile(artifact, 'fixture archive bytes');

  const { buildReleaseFromArtifact, sha256File } = await import('./index.mjs');
  const candidate = await buildReleaseFromArtifact({
    capability: 'rea',
    version: '6.3.0',
    repoTunnel: '>=0.4.0, <0.5.0',
    os: 'linux',
    arch: 'x86_64',
    artifactPath: artifact,
    contentRoot,
    entrypoint: 'package/package.json',
  });

  assert.equal(candidate.size, Buffer.byteLength('fixture archive bytes'));
  assert.equal(candidate.sha256, await sha256File(artifact));
  assert.equal(candidate.entrypointSha256, await sha256File(path.join(contentRoot, 'package/package.json')));
  assert.equal(candidate.contentSha256, await installedTreeSha256(contentRoot));
  assert.equal(candidate.artifactType, 'npm-bundle-zip');
  assert.equal(
    candidate.url,
    'https://github.com/Yashwanth034/RepoTunnel/releases/download/managed-capabilities/rea-6.3.0-linux-x86_64.zip',
  );
});

test('publisher tree hash exactly matches RepoTunnel Rust V1 content identity', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'repotunnel-managed-tree-'));
  await mkdir(path.join(root, 'package/scripts'), { recursive: true });
  await writeFile(path.join(root, 'package/package.json'), '{"name":"rea-agents"}');
  await writeFile(path.join(root, 'package/scripts/rea.mjs'), "console.log('ok');\n");
  assert.equal(
    await installedTreeSha256(root),
    '96f639c2b6ed2cf12378aa6c19581e2f94d3d528c7047b86da0ece6caac7e90f',
  );
  await writeFile(path.join(root, 'package/scripts/rea.mjs'), "console.log('tampered');\n");
  assert.notEqual(
    await installedTreeSha256(root),
    '96f639c2b6ed2cf12378aa6c19581e2f94d3d528c7047b86da0ece6caac7e90f',
  );
});

if (process.platform !== 'win32') {
  test('publisher tree hash rejects symlinks', async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), 'repotunnel-managed-symlink-'));
    await writeFile(path.join(root, 'real.txt'), 'ok');
    await symlink(path.join(root, 'real.txt'), path.join(root, 'linked.txt'));
    await assert.rejects(() => installedTreeSha256(root), /symlink/i);
  });
}

test('index generation is monotonic and expiry stays inside client safety window', () => {
  const next = buildIndex({
    previousIndex: previousIndex([release({ version: '0.45.3' })]),
    candidates: [release({ version: '0.46.0', sha256: C })],
    now: 1_000,
  });
  assert.equal(next.generation, 8);
  assert.equal(next.issuedAt, 1_000);
  assert.ok(next.expiresAt > next.issuedAt);
  assert.ok(next.expiresAt - next.issuedAt <= MAX_INDEX_LIFETIME_SECONDS);
  assert.equal(next.releases[0].version, '0.46.0');
});

test('publisher rejects downgrade and same-version equivocation', () => {
  const prior = previousIndex([release({ version: '0.46.0' })]);
  assert.throws(
    () => buildIndex({ previousIndex: prior, candidates: [release({ version: '0.45.3' })], now: 1_000 }),
    /downgrade/i,
  );
  assert.throws(
    () => buildIndex({ previousIndex: prior, candidates: [release({ version: '0.46.0', sha256: C })], now: 1_000 }),
    /equivocation/i,
  );
});

test('major REA and managed Node changes require explicit approval', () => {
  for (const capability of ['rea', 'rea-node']) {
    const prior = previousIndex([release({ capability, version: '6.4.0' })]);
    const candidate = release({ capability, version: '7.0.0', sha256: C, entrypointSha256: capability === 'rea' ? B : C, contentSha256: C });
    assert.throws(
      () => buildIndex({ previousIndex: prior, candidates: [candidate], now: 1_000 }),
      /explicit approval/i,
    );
    assert.equal(
      buildIndex({ previousIndex: prior, candidates: [candidate], now: 1_000, allowBreaking: true }).releases[0].version,
      '7.0.0',
    );
  }
});

test('normal ast-grep compatible bump is accepted after contract tests', () => {
  const prior = previousIndex([release({ version: '0.45.3' })]);
  const next = buildIndex({
    previousIndex: prior,
    candidates: [release({ version: '0.46.0', sha256: C })],
    now: 1_000,
  });
  assert.equal(next.releases[0].version, '0.46.0');
});

test('publisher rejects untrusted URLs, unknown fields and arbitrary capability types', () => {
  assert.throws(
    () => buildIndex({ candidates: [release({ url: 'https://example.com/helper.zip' })], now: 1_000 }),
    /fixed HTTPS GitHub release origin/i,
  );
  assert.throws(
    () => buildIndex({ candidates: [{ ...release(), execute: 'anything' }], now: 1_000 }),
    /unknown release field/i,
  );
  assert.throws(
    () => buildIndex({ candidates: [release({ capability: 'shell' })], now: 1_000 }),
    /unsupported capability/i,
  );
});

test('publisher refuses an expiry longer than the client accepts', () => {
  assert.throws(
    () => buildIndex({ candidates: [release()], now: 1_000, lifetimeSeconds: MAX_INDEX_LIFETIME_SECONDS + 1 }),
    /lifetime/i,
  );
});

test('publication verifier binds the exact archive bytes named by the capability index', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'repotunnel-managed-publication-'));
  const artifactName = 'ast-grep-0.46.0-linux-x86_64.zip';
  const artifactPath = path.join(root, artifactName);
  const bytes = Buffer.from('validated candidate archive');
  await writeFile(artifactPath, bytes);

  const publisher = await import('./index.mjs');
  assert.equal(typeof publisher.verifyPublicationBundle, 'function');
  const index = {
    schema: 1,
    generation: 8,
    issuedAt: 1_000,
    expiresAt: 2_000,
    releases: [release({
      size: bytes.length,
      sha256: await publisher.sha256File(artifactPath),
    })],
  };
  const indexPath = path.join(root, 'capability-index.json');
  await writeFile(indexPath, JSON.stringify(index));

  await publisher.verifyPublicationBundle({ indexPath, bundleDir: root });
  await writeFile(artifactPath, 'tampered');
  await assert.rejects(
    () => publisher.verifyPublicationBundle({ indexPath, bundleDir: root }),
    /size|sha-?256|hash/i,
  );
});

test('publication verifier rejects archive bytes that are not referenced by the index', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'repotunnel-managed-publication-extra-'));
  const artifactName = 'ast-grep-0.46.0-linux-x86_64.zip';
  const artifactPath = path.join(root, artifactName);
  const bytes = Buffer.from('validated candidate archive');
  await writeFile(artifactPath, bytes);

  const publisher = await import('./index.mjs');
  assert.equal(typeof publisher.verifyPublicationBundle, 'function');
  const indexPath = path.join(root, 'capability-index.json');
  await writeFile(indexPath, JSON.stringify({
    schema: 1,
    generation: 8,
    issuedAt: 1_000,
    expiresAt: 2_000,
    releases: [release({
      size: bytes.length,
      sha256: await publisher.sha256File(artifactPath),
    })],
  }));
  await writeFile(path.join(root, 'unexpected.zip'), 'not signed');

  await assert.rejects(
    () => publisher.verifyPublicationBundle({ indexPath, bundleDir: root }),
    /unexpected|unreferenced|extra/i,
  );
});
