import assert from "node:assert/strict";
import test from "node:test";
import type { PreviewEvent } from "./api";
import {
  buildConversationPreviewRows,
  isVisibleConversationEvent,
} from "./conversationDisplay.ts";

function event(
  index: number,
  role: "user" | "assistant",
  phase?: "commentary" | "final_answer",
): PreviewEvent {
  return {
    index,
    timestamp: "",
    role,
    kind: "message",
    text_summary: `${role}-${index}`,
    raw: {
      type: "response_item",
      payload: {
        type: "message",
        role,
        ...(phase ? { phase } : {}),
      },
    },
  };
}

test("Codex commentary is collapsed and the explicit final answer stays visible", () => {
  const rows = buildConversationPreviewRows([
    event(0, "user"),
    event(1, "assistant", "commentary"),
    event(2, "assistant", "commentary"),
    event(3, "assistant", "final_answer"),
  ]);

  assert.deepEqual(
    rows.map((row) =>
      row.type === "event"
        ? [row.type, row.event.index]
        : [row.type, row.events.map((item) => item.index), row.hasFinalResponse],
    ),
    [
      ["event", 0],
      ["collapsed", [1, 2], true],
      ["event", 3],
    ],
  );
});

test("a commentary-only interrupted turn is not presented as having a final answer", () => {
  const rows = buildConversationPreviewRows([
    event(0, "user"),
    event(1, "assistant", "commentary"),
    event(2, "user"),
  ]);

  assert.deepEqual(
    rows.map((row) =>
      row.type === "event"
        ? [row.type, row.event.index]
        : [row.type, row.events.map((item) => item.index), row.hasFinalResponse],
    ),
    [
      ["event", 0],
      ["collapsed", [1], false],
      ["event", 2],
    ],
  );
});

test("phase-less Claude messages keep the last assistant message as the reply", () => {
  const rows = buildConversationPreviewRows([
    event(0, "user"),
    event(1, "assistant"),
    event(2, "assistant"),
  ]);

  assert.deepEqual(
    rows.map((row) =>
      row.type === "event"
        ? [row.type, row.event.index]
        : [row.type, row.events.map((item) => item.index), row.hasFinalResponse],
    ),
    [
      ["event", 0],
      ["collapsed", [1], true],
      ["event", 2],
    ],
  );
});

test("conversation-only view hides user prompts missing from the active timeline", () => {
  const visiblePromptIndexes = new Set([2]);

  assert.equal(isVisibleConversationEvent(event(0, "user"), visiblePromptIndexes), false);
  assert.equal(isVisibleConversationEvent(event(1, "assistant"), visiblePromptIndexes), true);
  assert.equal(isVisibleConversationEvent(event(2, "user"), visiblePromptIndexes), true);
});

test("conversation-only view keeps user prompts while timeline data is unavailable", () => {
  assert.equal(isVisibleConversationEvent(event(0, "user"), null), true);
});

test("process collapse hides commentary rows while keeping the final answer", () => {
  const rows = buildConversationPreviewRows(
    [
      event(0, "user"),
      event(1, "assistant", "commentary"),
      event(2, "assistant", "commentary"),
      event(3, "assistant", "final_answer"),
    ],
    { hideProcessMessages: true },
  );

  assert.deepEqual(
    rows.map((row) =>
      row.type === "event" ? [row.type, row.event.index] : [row.type, row.key],
    ),
    [
      ["event", 0],
      ["event", 3],
    ],
  );
});

test("process collapse hides a commentary-only interrupted response", () => {
  const rows = buildConversationPreviewRows(
    [event(0, "user"), event(1, "assistant", "commentary"), event(2, "user")],
    { hideProcessMessages: true },
  );

  assert.deepEqual(
    rows.map((row) => (row.type === "event" ? row.event.index : row.key)),
    [0, 2],
  );
});
