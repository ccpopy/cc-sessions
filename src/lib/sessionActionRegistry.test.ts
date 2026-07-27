import assert from "node:assert/strict";
import test from "node:test";
import { SessionActionRegistry } from "./sessionActionRegistry.ts";

test("reserves a session synchronously until the action finishes", () => {
  const registry = new SessionActionRegistry();

  assert.equal(registry.tryBegin("session-1"), true);
  assert.equal(registry.tryBegin("session-1"), false);
  assert.deepEqual([...registry.snapshot()], ["session-1"]);

  registry.finish("session-1");
  assert.equal(registry.tryBegin("session-1"), true);
});

test("finishing one action does not clear another session", () => {
  const registry = new SessionActionRegistry();

  assert.equal(registry.tryBegin("session-1"), true);
  assert.equal(registry.tryBegin("session-2"), true);
  registry.finish("session-1");

  assert.deepEqual([...registry.snapshot()], ["session-2"]);
});
