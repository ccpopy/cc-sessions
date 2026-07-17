import assert from "node:assert/strict";
import test from "node:test";
import { ProviderSyncRegistry } from "./providerSyncRegistry.ts";

test("rejects a duplicate sync for the same session until the first one finishes", () => {
  const registry = new ProviderSyncRegistry();

  assert.equal(registry.tryBeginSession("session-1"), true);
  assert.equal(registry.tryBeginSession("session-1"), false);
  assert.deepEqual([...registry.snapshot().sessionIds], ["session-1"]);

  registry.finishSession("session-1");
  assert.equal(registry.tryBeginSession("session-1"), true);
});

test("batch sync cannot start while an individual sync is running", () => {
  const registry = new ProviderSyncRegistry();

  assert.equal(registry.tryBeginSession("session-1"), true);
  assert.equal(registry.tryBeginBatch(), false);

  registry.finishSession("session-1");
  assert.equal(registry.tryBeginBatch(), true);
});

test("finishing one session does not clear another queued session state", () => {
  const registry = new ProviderSyncRegistry();

  assert.equal(registry.tryBeginSession("session-1"), true);
  assert.equal(registry.tryBeginSession("session-2"), true);

  registry.finishSession("session-1");
  assert.deepEqual([...registry.snapshot().sessionIds], ["session-2"]);
  assert.equal(registry.tryBeginBatch(), false);
});

test("individual and duplicate batch syncs are rejected while a batch is running", () => {
  const registry = new ProviderSyncRegistry();

  assert.equal(registry.tryBeginBatch(), true);
  assert.equal(registry.tryBeginBatch(), false);
  assert.equal(registry.tryBeginSession("session-1"), false);

  registry.finishBatch();
  assert.equal(registry.tryBeginSession("session-1"), true);
});
