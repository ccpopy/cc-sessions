import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';
import { parse } from 'yaml';
const read = (name) => parse(readFileSync(new URL(`../.github/workflows/${name}`, import.meta.url), 'utf8'));
test('release publishes only after same-revision checks and every platform completes', () => {
  const workflow = read('release.yml');
  assert.deepEqual(workflow.jobs.publish.needs, ['verify', 'build']);
  assert.equal(workflow.jobs.verify.uses, './.github/workflows/ci.yml');
  assert.equal(workflow.jobs.build.steps.find((s) => s.uses?.startsWith('tauri-apps/tauri-action')).with.releaseDraft, true);
  assert.equal(workflow.jobs.build.strategy['fail-fast'], false);
  assert.equal(workflow.jobs.publish['continue-on-error'], undefined);
});
test('default CI covers stores, hooks, UI, CLI and the pinned native contract', () => {
  const ci = read('ci.yml');
  assert.ok('workflow_call' in ci.on);
  assert.equal(ci.jobs.native.uses, './.github/workflows/native-contract.yml');
  for (const name of ['linux', 'windows']) assert.ok(ci.jobs[name].steps.some((s) => s.run === 'npm run cli:check'));
  const pkg = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));
  assert.match(pkg.scripts['test:frontend'], /src\/\*\*\/\*\.test\.tsx/);
  assert.match(pkg.scripts['test:frontend'], /scripts\/\*\.test\.mjs/);
});

import { platforms, validateRelease } from './release-gate.mjs';
const validGate = () => {
  const version = '0.6.9';
  return { needs: { verify: { result: 'success' }, build: { result: 'success' } }, sha: 'abc123', tagSha: 'abc123', version, draft: true,
    proofs: platforms.map((platform) => ({ platform, sha: 'abc123', version })),
    assets: [...platforms.map((p) => `cc-sessions-cli-v${version}-${p}.zip`),
      `cc-session-manager-portable-v${version}-windows.exe`, `cc-session-manager-portable-v${version}-windows.zip`,
      `CC.Sessions_${version}_x64-setup.exe`, `CC.Sessions_${version}_amd64.AppImage`,
      `CC.Sessions_${version}_aarch64.dmg`, `CC.Sessions_${version}_x64.dmg`].map((name) => ({ name, size: 100 })) };
};
test('injected required-check or platform failure cannot pass the actual publish gate', () => {
  validateRelease(validGate());
  for (const job of ['verify', 'build']) for (const result of ['failure', 'cancelled', 'skipped', undefined]) {
    const input = validGate(); input.needs[job].result = result;
    assert.throws(() => validateRelease(input), /Required job/);
  }
  for (const platform of platforms) {
    const input = validGate(); input.proofs = input.proofs.filter((p) => p.platform !== platform);
    assert.throws(() => validateRelease(input), /build proof/);
  }
});
test('changed tag, mismatched build SHA, missing assets and already-public releases are rejected', () => {
  for (const mutate of [
    (input) => { input.tagSha = 'changed'; },
    (input) => { input.proofs[0].sha = 'changed'; },
    (input) => { input.assets.pop(); },
    (input) => { input.draft = false; },
  ]) { const input = validGate(); mutate(input); assert.throws(() => validateRelease(input)); }
});
