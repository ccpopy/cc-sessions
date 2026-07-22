import assert from "node:assert/strict";
import test from "node:test";
import { compareVersions, normalizeVersion } from "./version.ts";

test("normalizes a GitHub release tag", () => {
  assert.equal(normalizeVersion(" v1.2.3 "), "1.2.3");
});

test("compares numeric version segments", () => {
  assert.equal(compareVersions("0.10.0", "0.9.9"), 1);
  assert.equal(compareVersions("1.2.0", "1.2"), 0);
  assert.equal(compareVersions("1.1.9", "1.2.0"), -1);
});
