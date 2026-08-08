import assert from "node:assert/strict";
import test from "node:test";
import type { SessionSummary } from "./api";
import { compareSessionSize, oppositeSortDirection } from "./sessionSort.ts";

function session(
  id: string,
  tokensUsed: number,
  rolloutBytes: number,
  updatedAt: number,
): SessionSummary {
  return {
    id,
    tokens_used: tokensUsed,
    rollout_bytes: rolloutBytes,
    updated_at: updatedAt,
  } as SessionSummary;
}

test("会话大小支持升序和降序", () => {
  const sessions = [
    session("small", 10, 100, 1),
    session("large", 30, 100, 1),
    session("medium", 20, 100, 1),
  ];

  assert.deepEqual(
    [...sessions].sort((a, b) => compareSessionSize(a, b, "desc")).map((item) => item.id),
    ["large", "medium", "small"],
  );
  assert.deepEqual(
    [...sessions].sort((a, b) => compareSessionSize(a, b, "asc")).map((item) => item.id),
    ["small", "medium", "large"],
  );
});

test("token 相同时按文件大小排序，并稳定保留更新时间兜底", () => {
  const sessions = [
    session("older-large-file", 10, 300, 1),
    session("newer-small-file", 10, 100, 3),
    session("older-small-file", 10, 100, 2),
  ];

  assert.deepEqual(
    [...sessions].sort((a, b) => compareSessionSize(a, b, "asc")).map((item) => item.id),
    ["newer-small-file", "older-small-file", "older-large-file"],
  );
});

test("排序方向可以原位切换", () => {
  assert.equal(oppositeSortDirection("desc"), "asc");
  assert.equal(oppositeSortDirection("asc"), "desc");
});
