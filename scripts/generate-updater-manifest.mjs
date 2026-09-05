#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

function fail(message) {
  console.error(message);
  process.exit(1);
}

const [releaseDirectoryArg, tagArg, repositoryArg, notesFileArg] = process.argv.slice(2);
if (!releaseDirectoryArg || !tagArg || !repositoryArg) {
  fail("Usage: node scripts/generate-updater-manifest.mjs <release-dir> <tag> <owner/repo> [notes-file]");
}

const releaseDirectory = path.resolve(releaseDirectoryArg);
if (!fs.existsSync(releaseDirectory) || !fs.statSync(releaseDirectory).isDirectory()) {
  fail(`Release directory does not exist: ${releaseDirectory}`);
}

const files = fs.readdirSync(releaseDirectory).sort();
const signatures = new Map();
for (const file of files) {
  if (!file.endsWith(".sig")) continue;
  const artifact = file.slice(0, -4);
  signatures.set(artifact, fs.readFileSync(path.join(releaseDirectory, file), "utf8").trim());
}

function classify(file) {
  const lower = file.toLowerCase();
  if (lower.endsWith(".deb")) return ["linux-x86_64-deb"];
  if (lower.endsWith(".rpm")) return ["linux-x86_64-rpm"];
  if (lower.endsWith(".appimage")) return ["linux-x86_64-appimage", "linux-x86_64"];
  if (lower.endsWith(".msi")) return ["windows-x86_64-msi"];
  if (lower.endsWith(".exe")) return ["windows-x86_64-nsis", "windows-x86_64"];
  if (lower.endsWith(".app.tar.gz")) {
    if (/(?:_|-)aarch64(?:\.|_|-)/i.test(file) || /aarch64/i.test(file)) {
      return ["darwin-aarch64-app", "darwin-aarch64"];
    }
    if (/(?:_|-)(?:x64|x86_64)(?:\.|_|-)/i.test(file) || /x86_64|x64/i.test(file)) {
      return ["darwin-x86_64-app", "darwin-x86_64"];
    }
  }
  return [];
}

const platforms = {};
for (const [artifact, signature] of signatures) {
  if (!signature) fail(`Empty updater signature: ${artifact}.sig`);
  const artifactPath = path.join(releaseDirectory, artifact);
  if (!fs.existsSync(artifactPath) || !fs.statSync(artifactPath).isFile()) {
    fail(`Updater signature has no matching artifact: ${artifact}.sig`);
  }
  for (const target of classify(artifact)) {
    if (platforms[target]) {
      fail(`More than one updater artifact maps to ${target}: ${artifact}`);
    }
    platforms[target] = {
      url: `https://github.com/${repositoryArg}/releases/download/${encodeURIComponent(tagArg)}/${encodeURIComponent(artifact)}`,
      signature,
    };
  }
}

const requiredTargets = [
  "linux-x86_64-deb",
  "linux-x86_64-rpm",
  "linux-x86_64-appimage",
  "windows-x86_64-nsis",
  "windows-x86_64-msi",
  "darwin-aarch64-app",
  "darwin-x86_64-app",
];
const missing = requiredTargets.filter((target) => !platforms[target]);
if (missing.length) {
  fail(`Missing signed updater artifacts for: ${missing.join(", ")}`);
}

const packageJson = JSON.parse(fs.readFileSync(path.resolve("package.json"), "utf8"));
const version = String(packageJson.version ?? "").trim();
if (!version) fail("package.json has no version.");
if (tagArg !== `v${version}`) {
  fail(`Updater tag ${tagArg} does not match package version v${version}.`);
}

let notes = "";
if (notesFileArg) {
  const notesPath = path.resolve(notesFileArg);
  if (fs.existsSync(notesPath)) notes = fs.readFileSync(notesPath, "utf8").trim();
}

const manifest = {
  version,
  notes: notes || undefined,
  pub_date: new Date().toISOString(),
  platforms,
};

const outputPath = path.join(releaseDirectory, "latest.json");
fs.writeFileSync(outputPath, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`Wrote ${outputPath} with ${Object.keys(platforms).length} updater targets.`);
