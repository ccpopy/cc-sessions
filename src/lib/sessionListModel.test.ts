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
  const now = Date.now() / 1000;
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

test("metadata-only changes cannot move an old activity into today's bucket", () => {
  const now = Date.now();
  const old = {
    ...session("opened", "old-project", now / 1000),
    activity_at_ms: now - 60 * 86400 * 1000,
  };
  const rows = buildTimeListRows([old], {
    today: false, yesterday: false, week: false, month: false, earlier: false,
  });

  assert.equal(rows[0]?.key, "bucket:earlier");
  assert.equal(rows[1]?.type, "session");
});

test("project order and latest time follow activity while update timestamps stay intact", () => {
  const old = { ...session("opened", "old-project", 3000), activity_at_ms: 1_000_789 };
  const recent = { ...session("recent", "recent-project", 2000), activity_at_ms: 2_000_123 };
  const groups = buildProjectListGroups([old, recent]);

  assert.deepEqual(groups.map((group) => group.cwd), ["recent-project", "old-project"]);
  assert.deepEqual(groups.map((group) => group.latest), [2000.123, 1000.789]);
  assert.equal(groups[1]?.items[0]?.updated_at, 3000);
});
