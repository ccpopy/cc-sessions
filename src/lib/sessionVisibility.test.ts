import assert from "node:assert/strict";
import test from "node:test";
import type { FamilyOverlay, SessionSummary } from "./api";
import { selectNormalFamilySessions, selectSessionsForView } from "./sessionVisibility.ts";

function session(id: string, updatedAt: number): SessionSummary {
  return {
    provider: "codex",
    id,
    rollout_path: `${id}.jsonl`,
    cwd: "F:\\work",
    cwd_display: "work",
    title: id,
    first_user_message: id,
    model: null,
    reasoning_effort: null,
    source: null,
    agent_nickname: null,
    agent_role: null,
    conversion_origin: null,
    tokens_used: 0,
    created_at: updatedAt - 10,
    updated_at: updatedAt,
    archived: false,
    git_branch: null,
    rollout_bytes: 0,
    logs_count: 0,
    has_backup: false,
    resume_command: `codex resume ${id}`,
  };
}

function familyOverlay(
  sessionId: string,
  familyId: string,
  provider: string,
  active: boolean,
): FamilyOverlay {
  return {
    session_id: sessionId,
    provider,
    family_id: familyId,
    branch_count: 2,
    is_active_branch: active,
    archive_origin: null,
    clone_state: provider === "openai" ? "matches" : "has_clone",
  };
}

test("current provider branch represents a family even when the stored active branch is stale", () => {
  const staleActive = session("custom-active", 100);
  const currentBranch = session("openai-current", 500);
  const overlay = new Map<string, FamilyOverlay>([
    [staleActive.id, familyOverlay(staleActive.id, "family-1", "custom", true)],
    [currentBranch.id, familyOverlay(currentBranch.id, "family-1", "openai", false)],
  ]);

  assert.deepEqual(
    selectNormalFamilySessions([staleActive, currentBranch], overlay, "openai").map(
      (item) => item.id,
    ),
    ["openai-current"],
  );
});

test("current provider branch wins before activity time so one family has one usable card", () => {
  const newerOtherProvider = session("custom-active", 500);
  const olderCurrentProvider = session("openai-current", 100);
  const overlay = new Map<string, FamilyOverlay>([
    [
      newerOtherProvider.id,
      familyOverlay(newerOtherProvider.id, "family-1", "custom", true),
    ],
    [
      olderCurrentProvider.id,
      familyOverlay(olderCurrentProvider.id, "family-1", "openai", false),
    ],
  ]);

  assert.deepEqual(
    selectNormalFamilySessions(
      [newerOtherProvider, olderCurrentProvider],
      overlay,
      "openai",
    ).map((item) => item.id),
    ["openai-current"],
  );
});

test("stored active branch remains visible when the current provider has no family branch", () => {
  const active = session("custom-active", 100);
  const historical = session("legacy-history", 500);
  const overlay = new Map<string, FamilyOverlay>([
    [active.id, familyOverlay(active.id, "family-1", "custom", true)],
    [historical.id, familyOverlay(historical.id, "family-1", "legacy", false)],
  ]);

  assert.deepEqual(
    selectNormalFamilySessions([historical, active], overlay, "openai").map(
      (item) => item.id,
    ),
    ["custom-active"],
  );
});

test("duplicate current-provider branches use the most recently updated branch", () => {
  const older = session("openai-old", 100);
  const newer = session("openai-new", 500);
  const overlay = new Map<string, FamilyOverlay>([
    [older.id, familyOverlay(older.id, "family-1", "openai", true)],
    [newer.id, familyOverlay(newer.id, "family-1", "openai", false)],
  ]);

  assert.deepEqual(
    selectNormalFamilySessions([older, newer], overlay, "openai").map((item) => item.id),
    ["openai-new"],
  );
});

test("selected family representatives and standalone sessions are sorted by last activity", () => {
  const familyOld = session("family-old", 100);
  const standaloneNewest = session("standalone-newest", 900);
  const standaloneMiddle = session("standalone-middle", 400);
  const overlay = new Map<string, FamilyOverlay>([
    [familyOld.id, familyOverlay(familyOld.id, "family-1", "openai", true)],
  ]);

  assert.deepEqual(
    selectNormalFamilySessions(
      [familyOld, standaloneMiddle, standaloneNewest],
      overlay,
      "openai",
    ).map((item) => item.id),
    ["standalone-newest", "standalone-middle", "family-old"],
  );
});

test("content search scope uses the same normal-family representative as the page", () => {
  const staleActive = session("custom-active", 500);
  const currentBranch = session("openai-current", 100);
  const overlay = new Map<string, FamilyOverlay>([
    [staleActive.id, familyOverlay(staleActive.id, "family-1", "custom", true)],
    [currentBranch.id, familyOverlay(currentBranch.id, "family-1", "openai", false)],
  ]);

  assert.deepEqual(
    selectSessionsForView([staleActive, currentBranch], overlay, "openai", {
      provider: "codex",
      showSubagentSessions: false,
      showArchivedSessions: false,
    }).map((item) => item.id),
    ["openai-current"],
  );
});

test("archived search scope hides family branches from other providers", () => {
  const currentBranch = { ...session("openai-current", 100), archived: true };
  const otherBranch = { ...session("custom-history", 500), archived: true };
  const overlay = new Map<string, FamilyOverlay>([
    [currentBranch.id, familyOverlay(currentBranch.id, "family-1", "openai", true)],
    [otherBranch.id, familyOverlay(otherBranch.id, "family-1", "custom", false)],
  ]);

  assert.deepEqual(
    selectSessionsForView([otherBranch, currentBranch], overlay, "openai", {
      provider: "codex",
      showSubagentSessions: false,
      showArchivedSessions: true,
    }).map((item) => item.id),
    ["openai-current"],
  );
});

test("search scope honors overlay-only subagent classification", () => {
  const main = session("main", 500);
  const subagent = session("overlay-subagent", 100);
  const overlay = new Map<string, FamilyOverlay>([
    [
      subagent.id,
      {
        session_id: subagent.id,
        provider: "openai",
        family_id: null,
        branch_count: 1,
        is_active_branch: false,
        clone_state: "subagent",
      },
    ],
  ]);

  assert.deepEqual(
    selectSessionsForView([subagent, main], overlay, "openai", {
      provider: "codex",
      showSubagentSessions: false,
      showArchivedSessions: false,
    }).map((item) => item.id),
    ["main"],
  );
  assert.deepEqual(
    selectSessionsForView([subagent, main], overlay, "openai", {
      provider: "codex",
      showSubagentSessions: true,
      showArchivedSessions: false,
    }).map((item) => item.id),
    ["overlay-subagent"],
  );
});
