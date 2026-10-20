import assert from "node:assert/strict";
import test from "node:test";
import type { SessionSummary } from "./api";
import {
  compareSessionActivityDesc,
  sessionActivityMs,
  sessionActivitySeconds,
} from "./sessionActivity.ts";

function session(id: string, updatedAt: number, activityAtMs?: number | null): SessionSummary {
  return {
    id,
    provider: "codex",
    created_at: 1,
    updated_at: updatedAt,
    activity_at_ms: activityAtMs,
  } as SessionSummary;
}

function orderedIds(sessions: SessionSummary[]): string[] {
  return [...sessions].sort(compareSessionActivityDesc).map((item) => item.id);
}

test("metadata updates preserve the list order until native activity advances", () => {
  const recent = session("recent", 2000, 2_000_123);
  const older = session("older", 1000, 1_000_789);
  const opened = { ...older, updated_at: 3000 };

  assert.deepEqual(orderedIds([older, recent]), ["recent", "older"]);
  assert.deepEqual(orderedIds([opened, recent]), ["recent", "older"]);
  assert.equal(sessionActivitySeconds(opened), 1000.789);
  assert.equal(opened.updated_at, 3000);
  assert.deepEqual(
    orderedIds([{ ...opened, activity_at_ms: 4_000_567 }, recent]),
    ["older", "recent"],
  );
});

test("native ordering keeps milliseconds and resolves equal activity by ID descending", () => {
  const latest = session("a-newest", 10, 7_002);
  const tieB = { ...session("b-tie", 100, 7_001), created_at: 999 };
  const tieZ = { ...session("z-tie", 50, 7_001), created_at: 1 };

  assert.deepEqual(orderedIds([tieB, tieZ, latest]), ["a-newest", "z-tie", "b-tie"]);
});

test("older backend responses and unset activity fall back to update seconds", () => {
  for (const activity of [undefined, null, 0, -1, Number.NaN]) {
    const legacy = session("legacy", 50, activity);
    assert.equal(sessionActivityMs(legacy), 50_000);
    assert.equal(sessionActivitySeconds(legacy), 50);
  }
});

test("other providers retain update, creation and ID tie ordering", () => {
  const a = { ...session("a", 50), provider: "claude", created_at: 10 } as SessionSummary;
  const b = { ...a, id: "b" };
  const newer = { ...a, id: "newer", created_at: 20 };
  assert.deepEqual(orderedIds([b, a, newer]), ["newer", "a", "b"]);
});

test("mixed-provider activity ties sort consistently for every input order", () => {
  const a = session("a", 50, 1_000);
  const z = session("z", 50, 1_000);
  const claude = { ...a, id: "other", provider: "claude" } as SessionSummary;
  for (const input of [[a, z, claude], [z, claude, a], [claude, a, z]]) {
    assert.deepEqual(orderedIds(input), ["other", "z", "a"]);
  }
});
