import assert from "node:assert/strict";
import test from "node:test";
import type { SessionSummary } from "./api";
import {
  buildSubagentChildrenIndex,
  buildSubagentParentGroups,
  collectRelatedSubagents,
  MISSING_PARENT_GROUP_KEY,
} from "./sessionSource.ts";

let uuidSeq = 0;

function uuid(seed: string): string {
  uuidSeq += 1;
  const suffix = String(uuidSeq).padStart(4, "0");
  const hex = seed.padEnd(8, "0").slice(0, 8) + suffix;
  return [`0000${hex.slice(0, 4)}`, hex.slice(4, 8), "7000", "8000", `${hex.slice(8)}00000000`]
    .join("-")
    .toLowerCase();
}

function rootSession(id: string, updatedAt: number): SessionSummary {
  return {
    provider: "codex",
    id,
    rollout_path: `/codex/${id}.jsonl`,
    cwd: "/work",
    cwd_display: "work",
    title: `root-${id.slice(0, 8)}`,
    first_user_message: "hello",
    model: null,
    reasoning_effort: null,
    source: null,
    agent_role: null,
    agent_nickname: null,
    conversion_origin: null,
    tokens_used: 0,
    created_at: updatedAt - 100,
    updated_at: updatedAt,
    archived: false,
    git_branch: null,
    rollout_bytes: 0,
    logs_count: 0,
    has_backup: false,
    resume_command: `codex resume ${id}`,
  };
}

function subagentSession(
  id: string,
  parentThreadId: string,
  depth: number,
  updatedAt: number,
): SessionSummary {
  const base = rootSession(id, updatedAt);
  base.created_at = updatedAt - 50;
  base.source = JSON.stringify({
    subagent: {
      thread_spawn: {
        parent_thread_id: parentThreadId,
        depth,
        agent_path: "/general-purpose",
      },
    },
  });
  return base;
}

test("one flat level groups direct subagents under their parent", () => {
  const parent = rootSession(uuid("a1b2c3d4"), 500);
  const first = subagentSession(uuid("b2c3d4e5"), parent.id, 1, 400);
  const second = subagentSession(uuid("c3d4e5f6"), parent.id, 1, 450);

  const groups = buildSubagentParentGroups([first, second], [parent, first, second]);

  assert.equal(groups.length, 1);
  assert.equal(groups[0]?.key, parent.id);
  assert.equal(groups[0]?.parent?.id, parent.id);
  assert.deepEqual(
    groups[0]?.descendants.map((item) => item.session.id),
    [first.id, second.id],
  );
  assert.deepEqual(
    groups[0]?.descendants.map((item) => item.relativeDepth),
    [1, 1],
  );
});

test("nested subagents keep depth-first order and relative depth", () => {
  const parent = rootSession(uuid("d4e5f6a7"), 900);
  const child = subagentSession(uuid("e5f6a7b8"), parent.id, 1, 500);
  const grandchild = subagentSession(uuid("f6a7b8c9"), child.id, 2, 700);
  const sibling = subagentSession(uuid("a7b8c9d0"), parent.id, 1, 600);
  const allSessions = [parent, child, grandchild, sibling];

  const related = collectRelatedSubagents(parent.id, allSessions);
  assert.deepEqual(
    related.map((item) => [item.id, item.relativeDepth]),
    [
      [child.id, 1],
      [grandchild.id, 2],
      [sibling.id, 1],
    ],
  );

  // 可见集合乱序传入，分组仍保持创建序的深度优先排列。
  const groups = buildSubagentParentGroups(
    [grandchild, sibling, child],
    allSessions,
  );
  assert.equal(groups.length, 1);
  assert.deepEqual(
    groups[0]?.descendants.map((item) => [item.session.id, item.relativeDepth]),
    [
      [child.id, 1],
      [grandchild.id, 2],
      [sibling.id, 1],
    ],
  );
});

test("parent cycles fall into the missing-parent group instead of looping", () => {
  const left = subagentSession(uuid("b8c9d0e1"), "0", 1, 200);
  const right = subagentSession(uuid("c9d0e1f2"), left.id, 2, 100);
  // 两个会话互相把对方标成父，形成环；父 id 都在数据集内。
  left.source = JSON.stringify({
    subagent: {
      thread_spawn: {
        parent_thread_id: right.id,
        depth: 1,
        agent_path: "/general-purpose",
      },
    },
  });

  const groups = buildSubagentParentGroups([left, right], [left, right]);

  assert.equal(groups.length, 1);
  assert.equal(groups[0]?.key, MISSING_PARENT_GROUP_KEY);
  assert.equal(groups[0]?.parent, null);
  assert.equal(groups[0]?.descendants.length, 2);
});

test("subagents with a deleted parent land in the trailing missing-parent group", () => {
  const presentParent = rootSession(uuid("d0e1f2a3"), 300);
  const presentChild = subagentSession(uuid("e1f2a3b4"), presentParent.id, 1, 200);
  const deletedParentId = "11111111-2222-7000-8000-333333333333";
  const orphan = subagentSession(uuid("f2a3b4c5"), deletedParentId, 1, 100);

  const groups = buildSubagentParentGroups(
    [orphan, presentChild],
    [presentParent, presentChild, orphan],
  );

  assert.equal(groups.length, 2);
  assert.equal(groups[0]?.key, presentParent.id);
  assert.equal(groups[1]?.key, MISSING_PARENT_GROUP_KEY);
  assert.deepEqual(
    groups[1]?.descendants.map((item) => item.session.id),
    [orphan.id],
  );
});

test("groups sort by latest member activity descending", () => {
  const quietParent = rootSession(uuid("a3b4c5d6"), 1000);
  const quietChild = subagentSession(uuid("b4c5d6e7"), quietParent.id, 1, 100);
  const activeParent = rootSession(uuid("c5d6e7f8"), 200);
  const activeChild = subagentSession(uuid("d6e7f8a9"), activeParent.id, 1, 900);

  const groups = buildSubagentParentGroups(
    [quietChild, activeChild],
    [quietParent, quietChild, activeParent, activeChild],
  );

  assert.deepEqual(
    groups.map((group) => group.key),
    [quietParent.id, activeParent.id],
  );
  assert.equal(groups[0]?.latest, 1000);
  assert.equal(groups[1]?.latest, 900);
});

test("search-visible subagent still groups under its parent from allSessions", () => {
  const parent = rootSession(uuid("e7f8a9b0"), 800);
  const child = subagentSession(uuid("f8a9b0c1"), parent.id, 1, 700);
  // 搜索只命中子代理：可见集合里没有父，但全量数据里有。
  const groups = buildSubagentParentGroups([child], [parent, child]);

  assert.equal(groups.length, 1);
  assert.equal(groups[0]?.key, parent.id);
  assert.equal(groups[0]?.parent?.id, parent.id);
  assert.deepEqual(
    groups[0]?.descendants.map((item) => item.session.id),
    [child.id],
  );
});

test("children index sorts by creation time and ignores non-codex sources", () => {
  const parent = rootSession(uuid("a9b0c1d2"), 500);
  const later = subagentSession(uuid("b0c1d2e3"), parent.id, 1, 300);
  const earlier = subagentSession(uuid("c1d2e3f4"), parent.id, 1, 200);
  const claudeChild = {
    ...subagentSession(uuid("d2e3f4a5"), parent.id, 1, 100),
    provider: "claude" as const,
  };

  const index = buildSubagentChildrenIndex([parent, later, earlier, claudeChild]);

  assert.deepEqual(
    index.get(parent.id)?.map((entry) => entry.session.id),
    [earlier.id, later.id],
  );
});

test("subagent source without agent_path still parses and groups", () => {
  // 新版 Codex 的 thread_spawn 不再写 agent_path（实测为 null），
  // 分组能力只依赖 parent_thread_id 与 depth。
  const parent = rootSession(uuid("b1c2d3e4"), 600);
  const child = subagentSession(uuid("c2d3e4f5"), parent.id, 1, 500);
  const parsed = JSON.parse(child.source ?? "{}");
  parsed.subagent.thread_spawn.agent_path = null;
  child.source = JSON.stringify(parsed);

  const groups = buildSubagentParentGroups([child], [parent, child]);

  assert.equal(groups.length, 1);
  assert.equal(groups[0]?.key, parent.id);
  assert.deepEqual(
    groups[0]?.descendants.map((item) => item.session.id),
    [child.id],
  );
});
