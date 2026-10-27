import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const appName = 'CC Sessions.app';

export function macGuiZipTarget(platform, version) {
  const targets = {
    'macos-arm64': { nodeArch: 'arm64', binaryArch: 'arm64', assetArch: 'aarch64', targetTriple: 'aarch64-apple-darwin' },
    'macos-intel': { nodeArch: 'x64', binaryArch: 'x86_64', assetArch: 'x64', targetTriple: 'x86_64-apple-darwin' },
  };
  const target = targets[platform];
  if (!target) throw new Error(`Unsupported macOS release platform: ${platform}`);
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error(`Invalid release version: ${version}`);
  }
  return { ...target, assetName: `CC.Sessions_${version}_${target.assetArch}.app.zip` };
}

export function assertMatchingManifests(source, extracted) {
  const paths = new Set([...Object.keys(source), ...Object.keys(extracted)]);
  for (const name of [...paths].sort()) {
    assert.deepEqual(extracted[name], source[name], `Extracted app differs at ${name}`);
  }
}

export function assertMacArchitecture(target, runtimePlatform, runtimeArch, binaryArches) {
  if (runtimePlatform !== 'darwin' || runtimeArch !== target.nodeArch) {
    throw new Error(`macOS GUI ZIP requires a native ${target.nodeArch} macOS runner`);
  }
  if (binaryArches && (binaryArches.length !== 1 || binaryArches[0] !== target.binaryArch)) {
    throw new Error(`App executable architecture ${binaryArches.join(',')} does not match ${target.binaryArch}`);
  }
}

function command(name, args) {
  return execFileSync(name, args, { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 }).trim();
}

function xattrs(filePath, symlink) {
  const option = symlink ? ['-s'] : [];
  const names = command('xattr', [...option, filePath]).split('\n').filter(Boolean).sort();
  return Object.fromEntries(names.map((name) => [
    name, command('xattr', [...option, '-px', name, filePath]).replace(/\s/g, '').toLowerCase(),
  ]));
}

async function manifest(root) {
  const result = {};
  async function walk(filePath, relative) {
    const info = await fs.lstat(filePath);
    const type = info.isDirectory() ? 'directory' : info.isFile() ? 'file' : info.isSymbolicLink() ? 'symlink' : null;
    if (!type) throw new Error(`Unsupported app entry: ${relative}`);
    const entry = { type, mode: info.mode & 0o7777, xattrs: xattrs(filePath, type === 'symlink') };
    if (type === 'file') {
      entry.size = info.size;
      entry.sha256 = createHash('sha256').update(await fs.readFile(filePath)).digest('hex');
    } else if (type === 'symlink') {
      entry.target = await fs.readlink(filePath);
    }
    result[relative] = entry;
    if (type === 'directory') {
      for (const child of (await fs.readdir(filePath)).sort()) {
        await walk(path.join(filePath, child), `${relative}/${child}`);
      }
    }
  }
  await walk(root, appName);
  return result;
}

export async function packageMacGuiZip(platform, version, {
  bundleDir,
  outputDir = path.join(repoRoot, 'release'),
} = {}) {
  const target = macGuiZipTarget(platform, version);
  assertMacArchitecture(target, process.platform, os.arch());
  if (!bundleDir) {
    const targetRoot = process.env.CARGO_TARGET_DIR
      ? path.resolve(repoRoot, process.env.CARGO_TARGET_DIR)
      : path.join(repoRoot, 'src-tauri', 'target');
    const candidates = [
      path.join(targetRoot, target.targetTriple, 'release', 'bundle', 'macos'),
      path.join(targetRoot, 'release', 'bundle', 'macos'),
    ];
    const found = [];
    for (const candidate of candidates) {
      try {
        if ((await fs.lstat(path.join(candidate, appName))).isDirectory()) found.push(candidate);
      } catch (error) {
        if (error.code !== 'ENOENT') throw error;
      }
    }
    if (found.length !== 1) throw new Error(`Expected one built app bundle, found ${found.length} in ${candidates.join(', ')}`);
    [bundleDir] = found;
  }
  const appPath = path.join(bundleDir, appName);
  if (!(await fs.lstat(appPath)).isDirectory()) throw new Error(`Expected app bundle: ${appPath}`);
  for (const executable of ['cc-session-manager', 'cc-sessions']) {
    const binary = path.join(appPath, 'Contents', 'MacOS', executable);
    const arches = command('lipo', ['-archs', binary]).split(/\s+/);
    assertMacArchitecture(target, process.platform, os.arch(), arches);
  }
  await fs.mkdir(outputDir, { recursive: true });
  const archive = path.join(outputDir, target.assetName);
  try {
    await fs.access(archive);
    throw new Error(`Refusing to replace existing release asset: ${archive}`);
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
  }
  const scratch = await fs.mkdtemp(path.join(os.tmpdir(), 'cc-sessions-gui-zip-'));
  try {
    command('ditto', ['-c', '-k', '--sequesterRsrc', '--keepParent', appPath, archive]);
    command('ditto', ['-x', '-k', archive, scratch]);
    const extractedRoot = path.join(scratch, appName);
    assert.deepEqual(await fs.readdir(scratch), [appName], 'ZIP must extract to one top-level .app');
    assertMatchingManifests(await manifest(appPath), await manifest(extractedRoot));
    const size = (await fs.stat(archive)).size;
    if (size <= 0) throw new Error(`Empty GUI ZIP: ${archive}`);
    console.log(`${archive} (${size} bytes; extracted files, modes, symlinks and xattrs verified)`);
    return archive;
  } catch (error) {
    await fs.rm(archive, { force: true });
    throw error;
  } finally {
    await fs.rm(scratch, { recursive: true, force: true });
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  if (process.argv.length !== 4 || process.argv[2] !== '--platform') {
    throw new Error('Usage: node scripts/package-macos-gui-zip.mjs --platform macos-arm64|macos-intel');
  }
  const packageJson = JSON.parse(await fs.readFile(path.join(repoRoot, 'package.json'), 'utf8'));
  await packageMacGuiZip(process.argv[3], packageJson.version);
}
