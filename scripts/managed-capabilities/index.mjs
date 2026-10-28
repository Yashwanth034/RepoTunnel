import { createHash } from 'node:crypto';
import { lstat, open, readdir, readFile } from 'node:fs/promises';
import path from 'node:path';

export const INDEX_SCHEMA = 1;
export const MAX_INDEX_LIFETIME_SECONDS = 72 * 60 * 60;
export const DEFAULT_INDEX_LIFETIME_SECONDS = 48 * 60 * 60;
export const MAX_RELEASES = 128;
const MAX_TREE_FILES = 50_000;
const MAX_TREE_BYTES = 512 * 1024 * 1024;
const TREE_DOMAIN = Buffer.from('RepoTunnelManagedContentV1\0', 'utf8');
const HEX_64 = /^[0-9a-f]{64}$/;
const VERSION = /^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$/;
const SAFE_ASSET = /^[A-Za-z0-9._+-]+$/;

function u64le(value) {
  const buffer = Buffer.alloc(8);
  buffer.writeBigUInt64LE(BigInt(value));
  return buffer;
}

export function parseVersion(version) {
  if (typeof version !== 'string' || !VERSION.test(version)) {
    throw new Error(`invalid semantic version: ${JSON.stringify(version)}`);
  }
  const core = version.split(/[-+]/, 1)[0];
  const [major, minor, patch] = core.split('.').map(Number);
  return { major, minor, patch, raw: version };
}

export function compareVersions(left, right) {
  const a = parseVersion(left);
  const b = parseVersion(right);
  for (const key of ['major', 'minor', 'patch']) {
    if (a[key] !== b[key]) return a[key] < b[key] ? -1 : 1;
  }
  if (a.raw === b.raw) return 0;
  return a.raw < b.raw ? -1 : 1;
}

export async function sha256File(filePath) {
  const stat = await lstat(filePath);
  if (!stat.isFile() || stat.isSymbolicLink() || stat.size <= 0) {
    throw new Error(`artifact must be a non-empty regular file: ${filePath}`);
  }
  const hash = createHash('sha256');
  const handle = await open(filePath, 'r');
  try {
    const buffer = Buffer.alloc(64 * 1024);
    let offset = 0;
    while (offset < stat.size) {
      const { bytesRead } = await handle.read(buffer, 0, buffer.length, offset);
      if (bytesRead === 0) break;
      hash.update(buffer.subarray(0, bytesRead));
      offset += bytesRead;
    }
    if (offset !== stat.size) throw new Error(`artifact changed while hashing: ${filePath}`);
  } finally {
    await handle.close();
  }
  return hash.digest('hex');
}

async function collectTree(root) {
  const rootStat = await lstat(root);
  if (!rootStat.isDirectory() || rootStat.isSymbolicLink()) {
    throw new Error('managed capability content root must be a real directory');
  }
  const stack = [root];
  const files = [];
  let total = 0;
  while (stack.length) {
    const directory = stack.pop();
    for (const entry of await readdir(directory, { withFileTypes: true })) {
      const absolute = path.join(directory, entry.name);
      const stat = await lstat(absolute);
      if (stat.isSymbolicLink()) throw new Error(`managed capability tree contains symlink: ${absolute}`);
      if (stat.isDirectory()) {
        stack.push(absolute);
        continue;
      }
      if (!stat.isFile()) throw new Error(`managed capability tree contains special file: ${absolute}`);
      const relative = path.relative(root, absolute).split(path.sep).join('/');
      if (!relative || Buffer.byteLength(relative) > 4096) throw new Error('managed capability path is outside the safety limit');
      total += stat.size;
      if (total > MAX_TREE_BYTES) throw new Error('managed capability content exceeds the safety limit');
      files.push({ relative, absolute, size: stat.size });
      if (files.length > MAX_TREE_FILES) throw new Error('managed capability content file count exceeds the safety limit');
    }
  }
  if (!files.length) throw new Error('managed capability content tree is empty');
  files.sort((a, b) => Buffer.from(a.relative).compare(Buffer.from(b.relative)));
  return files;
}

export async function installedTreeSha256(root) {
  const files = await collectTree(root);
  const hash = createHash('sha256');
  hash.update(TREE_DOMAIN);
  for (const file of files) {
    const relative = Buffer.from(file.relative, 'utf8');
    hash.update(u64le(relative.length));
    hash.update(relative);
    hash.update(u64le(file.size));
    const handle = await open(file.absolute, 'r');
    try {
      const buffer = Buffer.alloc(64 * 1024);
      let offset = 0;
      while (offset < file.size) {
        const { bytesRead } = await handle.read(buffer, 0, buffer.length, offset);
        if (bytesRead === 0) break;
        hash.update(buffer.subarray(0, bytesRead));
        offset += bytesRead;
      }
      if (offset !== file.size) throw new Error(`managed capability file changed while hashing: ${file.relative}`);
    } finally {
      await handle.close();
    }
  }
  return hash.digest('hex');
}

export async function buildReleaseFromArtifact({
  capability,
  version,
  repoTunnel,
  os,
  arch,
  artifactPath,
  contentRoot,
  entrypoint: declaredEntrypoint,
}) {
  const expectedEntrypoint = capability === 'ast-grep'
    ? (os === 'windows' ? 'ast-grep.exe' : 'ast-grep')
    : capability === 'rea'
      ? 'package/package.json'
      : capability === 'rea-node'
        ? (os === 'windows' ? 'node.exe' : 'bin/node')
        : null;
  if (!expectedEntrypoint) throw new Error(`unsupported capability: ${capability}`);
  if (declaredEntrypoint && declaredEntrypoint !== expectedEntrypoint) {
    throw new Error(`entrypoint does not match built-in adapter for ${capability}/${os}`);
  }

  const expectedAsset = `${capability}-${version}-${os}-${arch}.zip`;
  if (path.basename(artifactPath) !== expectedAsset) {
    throw new Error(`artifact filename must be ${expectedAsset}`);
  }
  const artifactStat = await lstat(artifactPath);
  if (!artifactStat.isFile() || artifactStat.isSymbolicLink() || artifactStat.size <= 0) {
    throw new Error('managed capability artifact must be a non-empty regular file');
  }

  const entrypointPath = path.join(contentRoot, ...expectedEntrypoint.split('/'));
  const entrypointSha256 = await sha256File(entrypointPath);
  const artifactType = capability === 'ast-grep'
    ? 'zip'
    : capability === 'rea'
      ? 'npm-bundle-zip'
      : 'node-binary-zip';
  const release = {
    capability,
    version,
    repoTunnel,
    os,
    arch,
    artifactType,
    url: `https://github.com/Yashwanth034/RepoTunnel/releases/download/managed-capabilities/${expectedAsset}`,
    size: artifactStat.size,
    sha256: await sha256File(artifactPath),
    entrypoint: expectedEntrypoint,
    entrypointSha256,
    contentSha256: capability === 'rea' ? await installedTreeSha256(contentRoot) : entrypointSha256,
  };
  return assertRelease(release);
}

function assertRelease(release) {
  const requiredStrings = ['capability', 'version', 'repoTunnel', 'os', 'arch', 'artifactType', 'url', 'sha256', 'entrypoint', 'entrypointSha256', 'contentSha256'];
  for (const key of requiredStrings) {
    if (typeof release[key] !== 'string' || !release[key]) throw new Error(`release.${key} is required`);
  }
  parseVersion(release.version);
  if (!['ast-grep', 'rea', 'rea-node'].includes(release.capability)) throw new Error(`unsupported capability: ${release.capability}`);
  if (!['linux', 'windows', 'macos'].includes(release.os)) throw new Error(`unsupported os: ${release.os}`);
  if (!['x86_64', 'aarch64'].includes(release.arch)) throw new Error(`unsupported arch: ${release.arch}`);
  if (!Number.isSafeInteger(release.size) || release.size <= 0 || release.size > 256 * 1024 * 1024) throw new Error('release.size is outside the safety limit');
  for (const key of ['sha256', 'entrypointSha256', 'contentSha256']) {
    if (!HEX_64.test(release[key])) throw new Error(`release.${key} must be lowercase SHA-256`);
  }
  const parsed = new URL(release.url);
  if (parsed.protocol !== 'https:' || parsed.hostname !== 'github.com' || parsed.username || parsed.password || parsed.port || parsed.search || parsed.hash) {
    throw new Error('release URL must use RepoTunnel fixed HTTPS GitHub release origin');
  }
  const prefix = '/Yashwanth034/RepoTunnel/releases/download/managed-capabilities/';
  if (!parsed.pathname.startsWith(prefix)) throw new Error('release URL must use RepoTunnel managed-capabilities release tag');
  const asset = parsed.pathname.slice(prefix.length);
  if (!SAFE_ASSET.test(asset) || asset.includes('..')) throw new Error('release asset name is unsafe');
  const expectedEntrypoint = release.capability === 'ast-grep'
    ? (release.os === 'windows' ? 'ast-grep.exe' : 'ast-grep')
    : release.capability === 'rea'
      ? 'package/package.json'
      : (release.os === 'windows' ? 'node.exe' : 'bin/node');
  if (release.entrypoint !== expectedEntrypoint) throw new Error(`unexpected entrypoint for ${release.capability}/${release.os}`);
  const expectedType = release.capability === 'ast-grep' ? 'zip' : release.capability === 'rea' ? 'npm-bundle-zip' : 'node-binary-zip';
  if (release.artifactType !== expectedType) throw new Error(`unexpected artifact type for ${release.capability}`);
  if (release.capability !== 'rea' && release.contentSha256 !== release.entrypointSha256) {
    throw new Error('single-file capabilities must bind content identity to the entrypoint hash');
  }
  const allowed = new Set([...requiredStrings, 'size']);
  for (const key of Object.keys(release)) {
    if (!allowed.has(key)) throw new Error(`unknown release field: ${key}`);
  }
  return release;
}

function keyOf(release) {
  return `${release.capability}\0${release.os}\0${release.arch}`;
}

export function breakingPromotion(previous, candidate) {
  if (!previous) return false;
  const a = parseVersion(previous.version);
  const b = parseVersion(candidate.version);
  return (candidate.capability === 'rea' || candidate.capability === 'rea-node') && a.major !== b.major;
}

export function buildIndex({ previousIndex = null, candidates, now = Math.floor(Date.now() / 1000), allowBreaking = false, lifetimeSeconds = DEFAULT_INDEX_LIFETIME_SECONDS }) {
  if (!Array.isArray(candidates) || candidates.length === 0) throw new Error('at least one candidate release is required');
  if (!Number.isSafeInteger(now) || now <= 0) throw new Error('now must be positive Unix seconds');
  if (!Number.isSafeInteger(lifetimeSeconds) || lifetimeSeconds <= 0 || lifetimeSeconds > MAX_INDEX_LIFETIME_SECONDS) throw new Error('index lifetime is outside the client safety window');

  const previousReleases = previousIndex?.releases ?? [];
  if (previousIndex) {
    if (previousIndex.schema !== INDEX_SCHEMA || !Number.isSafeInteger(previousIndex.generation) || previousIndex.generation <= 0 || !Array.isArray(previousReleases)) {
      throw new Error('previous index is invalid');
    }
    for (const release of previousReleases) assertRelease(release);
  }
  for (const release of candidates) assertRelease(release);

  const latest = new Map();
  for (const release of previousReleases) {
    const key = keyOf(release);
    const current = latest.get(key);
    if (!current || compareVersions(current.version, release.version) < 0) latest.set(key, release);
  }

  for (const candidate of candidates) {
    const key = keyOf(candidate);
    const previous = latest.get(key);
    if (previous) {
      const order = compareVersions(candidate.version, previous.version);
      if (order < 0) throw new Error(`refusing managed capability downgrade ${previous.version} -> ${candidate.version} for ${key}`);
      if (order === 0) {
        const identityFields = ['sha256', 'entrypointSha256', 'contentSha256', 'url', 'size'];
        if (identityFields.some((field) => candidate[field] !== previous[field])) {
          throw new Error(`refusing same-version managed capability equivocation for ${key} ${candidate.version}`);
        }
        continue;
      }
      if (!allowBreaking && breakingPromotion(previous, candidate)) {
        throw new Error(`breaking managed capability major update requires explicit approval: ${previous.version} -> ${candidate.version} for ${key}`);
      }
    }
    latest.set(key, candidate);
  }

  const releases = [...latest.values()].sort((a, b) => {
    const keyCompare = keyOf(a).localeCompare(keyOf(b));
    return keyCompare || compareVersions(b.version, a.version);
  });
  if (releases.length > MAX_RELEASES) throw new Error('managed capability index release count exceeds client safety limit');

  return {
    schema: INDEX_SCHEMA,
    generation: (previousIndex?.generation ?? 0) + 1,
    issuedAt: now,
    expiresAt: now + lifetimeSeconds,
    releases,
  };
}

export async function readJson(filePath) {
  return JSON.parse(await readFile(filePath, 'utf8'));
}

export async function verifyPublicationBundle({ indexPath, bundleDir }) {
  const index = await readJson(indexPath);
  if (index?.schema !== INDEX_SCHEMA || !Number.isSafeInteger(index.generation) || index.generation <= 0) {
    throw new Error('publication capability index is invalid');
  }
  if (!Array.isArray(index.releases) || index.releases.length === 0 || index.releases.length > MAX_RELEASES) {
    throw new Error('publication capability index release count is invalid');
  }

  const expectedArchives = new Set();
  for (const release of index.releases) {
    assertRelease(release);
    const asset = path.basename(new URL(release.url).pathname);
    const expectedAsset = `${release.capability}-${release.version}-${release.os}-${release.arch}.zip`;
    if (asset !== expectedAsset) {
      throw new Error(`publication release URL asset must be ${expectedAsset}`);
    }
    if (expectedArchives.has(asset)) {
      throw new Error(`publication index references duplicate archive ${asset}`);
    }
    expectedArchives.add(asset);

    const artifactPath = path.join(bundleDir, asset);
    const stat = await lstat(artifactPath).catch(() => null);
    if (!stat || !stat.isFile() || stat.isSymbolicLink()) {
      throw new Error(`publication archive is missing or unsafe: ${asset}`);
    }
    if (stat.size !== release.size) {
      throw new Error(`publication archive size does not match signed index: ${asset}`);
    }
    const digest = await sha256File(artifactPath);
    if (digest !== release.sha256) {
      throw new Error(`publication archive SHA-256 does not match signed index: ${asset}`);
    }
  }

  for (const entry of await readdir(bundleDir, { withFileTypes: true })) {
    if (!entry.name.endsWith('.zip')) continue;
    if (!entry.isFile() || entry.isSymbolicLink?.()) {
      throw new Error(`publication bundle contains an unsafe archive entry: ${entry.name}`);
    }
    if (!expectedArchives.has(entry.name)) {
      throw new Error(`publication bundle contains an unreferenced extra archive: ${entry.name}`);
    }
  }

  return { generation: index.generation, archiveCount: expectedArchives.size };
}
