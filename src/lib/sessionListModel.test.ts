import assert from "node:assert/strict";
import test from "node:test";
import type { SessionSummary } from "./api.ts";
import {
  buildProjectListGroups,
  buildProjectListRows,
  buildTimeListRows,
  type SessionBucketKey,
} from "./sessionListModel.ts";

function session(id: string, cwd: string, updatedAt: number): SessionSummary {
  return {
    id,
    cwd,
    cwd_display: cwd,
    updated_at: updatedAt,
    rollout_path: `${cwd}/${id}.jsonl`,
    provider: "codex",
  } as SessionSummary;
}

test("collapsed time buckets keep their header but remove all session rows", () => {
  const now = Date.now();
  const collapsed = {
    today: true,
    yesterday: false,
    week: false,
    month: false,
    earlier: false,
  } satisfies Record<SessionBucketKey, boolean>;

  const rows = buildTimeListRows([session("a", "one", now)], collapsed);

  assert.deepEqual(rows.map((row) => row.type), ["bucket"]);
  assert.equal(rows[0]?.key, "bucket:today");
});

test("empty session collections produce no virtual rows", () => {
  const collapsed = {
    today: false,
    yesterday: false,
    week: false,
    month: false,
    earlier: false,
  } satisfies Record<SessionBucketKey, boolean>;

  assert.deepEqual(buildTimeListRows([], collapsed), []);
  assert.deepEqual(buildProjectListGroups([]), []);
  assert.deepEqual(buildProjectListRows([], {}), []);
});

test("project rows mount cards only for expanded groups and keep newest groups first", () => {
  const groups = buildProjectListGroups([
    session("old", "old-project", 10),
    session("new", "new-project", 20),
  ]);
  const rows = buildProjectListRows(groups, { "new-project": true });

  assert.deepEqual(groups.map((group) => group.cwd), ["new-project", "old-project"]);
  assert.deepEqual(rows.map((row) => row.type), ["project", "session", "project"]);
  assert.equal(rows[1]?.key.includes("new-project"), true);
});
