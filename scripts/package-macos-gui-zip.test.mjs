import assert from 'node:assert/strict';
import test from 'node:test';
import { assertMacArchitecture, assertMatchingManifests, macGuiZipTarget } from './package-macos-gui-zip.mjs';

test('GUI ZIP names and architecture checks cover both macOS runners', () => {
  assert.deepEqual(macGuiZipTarget('macos-arm64', '0.7.3'), {
    nodeArch: 'arm64', binaryArch: 'arm64', assetArch: 'aarch64', targetTriple: 'aarch64-apple-darwin',
    assetName: 'CC.Sessions_0.7.3_aarch64.app.zip',
  });
  assert.deepEqual(macGuiZipTarget('macos-intel', '0.7.3'), {
    nodeArch: 'x64', binaryArch: 'x86_64', assetArch: 'x64', targetTriple: 'x86_64-apple-darwin',
    assetName: 'CC.Sessions_0.7.3_x64.app.zip',
  });
  assert.throws(() => macGuiZipTarget('windows', '0.7.3'), /Unsupported/);
  assert.throws(() => macGuiZipTarget('macos-arm64', '../bad'), /Invalid release version/);
  assert.equal(macGuiZipTarget('macos-arm64', '0.7.3-rc.1+build.2').assetName,
    'CC.Sessions_0.7.3-rc.1+build.2_aarch64.app.zip');
});

test('both macOS jobs reject the wrong runner or binary architecture', () => {
  for (const platform of ['macos-arm64', 'macos-intel']) {
    const target = macGuiZipTarget(platform, '0.7.3');
    assertMacArchitecture(target, 'darwin', target.nodeArch, [target.binaryArch]);
    assert.throws(() => assertMacArchitecture(target, 'win32', target.nodeArch), /native/);
    assert.throws(() => assertMacArchitecture(target, 'darwin', 'wrong'), /native/);
    assert.throws(() => assertMacArchitecture(target, 'darwin', target.nodeArch, ['wrong']), /architecture/);
    assert.throws(() => assertMacArchitecture(target, 'darwin', target.nodeArch, [target.binaryArch, 'extra']), /architecture/);
  }
});

test('extracted app verification catches missing, modified and metadata-changed entries', () => {
  const source = {
    'CC Sessions.app': { type: 'directory', mode: 0o755, xattrs: {} },
    'CC Sessions.app/Contents/MacOS/app': {
      type: 'file', mode: 0o755, size: 12, sha256: 'abc', xattrs: { 'com.apple.FinderInfo': 'beef' },
    },
    'CC Sessions.app/Contents/Resources/link': {
      type: 'symlink', mode: 0o777, target: '../MacOS/app', xattrs: {},
    },
  };
  assertMatchingManifests(source, structuredClone(source));
  for (const change of [
    (copy) => { delete copy['CC Sessions.app/Contents/MacOS/app']; },
    (copy) => { copy['CC Sessions.app/Contents/MacOS/app'].sha256 = 'changed'; },
    (copy) => { copy['CC Sessions.app/Contents/MacOS/app'].mode = 0o644; },
    (copy) => { copy['CC Sessions.app/Contents/MacOS/app'].xattrs = {}; },
    (copy) => { copy['CC Sessions.app/Contents/Resources/link'].target = 'elsewhere'; },
  ]) {
    const copy = structuredClone(source);
    change(copy);
    assert.throws(() => assertMatchingManifests(source, copy), /Extracted app differs/);
  }
});
