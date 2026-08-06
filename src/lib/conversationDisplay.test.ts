import assert from "node:assert/strict";
import test from "node:test";
import type { PreviewEvent } from "./api";
import {
  buildConversationPreviewRows,
  isAssistantTextToolUseEvent,
  isProcessGroupExpanded,
  isVisibleConversationEvent,
  summarizeProcessGroupExpansion,
  toConversationDisplayEvent,
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

function claudeToolEvent(index: number, text: string | null): PreviewEvent {
  return {
    index,
    timestamp: "",
    role: "tool_call",
    kind: "assistant",
    text_summary: text ?? "[Tool: Bash]",
    raw: {
      type: "assistant",
      message: {
        role: "assistant",
        phase: "commentary",
        content: [
          ...(text === null ? [] : [{ type: "text", text }]),
          { type: "tool_use", id: `toolu_${index}`, name: "Bash", input: { command: "pwd" } },
        ],
      },
    },
  };
}

function openCodeEvent(
  index: number,
  role: "user" | "assistant" | "reasoning" | "tool_call",
  options: {
    messageId: string;
    parentId?: string;
    phase?: "commentary" | "final_answer";
    finish?: string;
    partType?: string;
  },
): PreviewEvent {
  const messageRole = role === "user" ? "user" : "assistant";
  const partType =
    options.partType ??
    (role === "tool_call" ? "tool" : role === "reasoning" ? "reasoning" : "text");
  return {
    index,
    timestamp: "",
    role,
    kind: messageRole,
    text_summary: `${role}-${index}`,
    raw: {
      type: messageRole,
      message: {
        role: messageRole,
        ...(options.phase ? { phase: options.phase } : {}),
        content: [],
      },
      opencode: {
        part_id: `part_${index}`,
        message_id: options.messageId,
        parent_id: options.parentId ?? null,
        finish: options.finish ?? null,
        part_type: partType,
        phase: options.phase ?? null,
      },
    },
  };
}

test("Claude text plus tool_use is projected as an assistant process message", () => {
  const mixed = claudeToolEvent(1, "正在检查代理配置");
  const projected = toConversationDisplayEvent(mixed);

  assert.equal(isAssistantTextToolUseEvent(mixed), true);
  assert.equal(projected.role, "assistant");
  assert.equal(projected.raw, mixed.raw);

  const rows = buildConversationPreviewRows([
    event(0, "user"),
    projected,
    event(2, "assistant", "final_answer"),
  ]);
  assert.deepEqual(
    rows.map((row) =>
      row.type === "event"
        ? [row.type, row.event.index]
        : [row.type, row.events.map((item) => item.index), row.hasFinalResponse],
    ),
    [
      ["event", 0],
      ["process", [1], true],
      ["event", 2],
    ],
  );
});

test("pure Claude tool_use stays a tool event", () => {
  const toolOnly = claudeToolEvent(1, null);

  assert.equal(isAssistantTextToolUseEvent(toolOnly), false);
  assert.equal(toConversationDisplayEvent(toolOnly), toolOnly);
});

test("Codex commentary is grouped and the explicit final answer stays visible", () => {
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
      ["process", [1, 2], true],
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
      ["process", [1], false],
      ["event", 2],
    ],
  );
});

test("OpenCode process groups keep reasoning but exclude tool calls", () => {
  const rows = buildConversationPreviewRows([
    openCodeEvent(0, "user", { messageId: "user_1" }),
    openCodeEvent(1, "reasoning", {
      messageId: "assistant_process",
      parentId: "user_1",
      phase: "commentary",
      finish: "tool-calls",
    }),
    openCodeEvent(2, "tool_call", {
      messageId: "assistant_process",
      parentId: "user_1",
      phase: "commentary",
      finish: "tool-calls",
    }),
    openCodeEvent(3, "assistant", {
      messageId: "assistant_final",
      parentId: "user_1",
      phase: "final_answer",
      finish: "stop",
    }),
  ]);

  assert.deepEqual(
    rows.map((row) =>
      row.type === "event"
        ? [row.type, row.event.index]
        : [row.type, row.events.map((item) => item.index), row.hasFinalResponse],
    ),
    [
      ["event", 0],
      ["process", [1], true],
      ["event", 3],
    ],
  );
});

test("OpenCode tool-only activity does not create a process group", () => {
  const rows = buildConversationPreviewRows([
    openCodeEvent(0, "user", { messageId: "user_1" }),
    openCodeEvent(1, "tool_call", {
      messageId: "assistant_process",
      parentId: "user_1",
      phase: "commentary",
      finish: "tool-calls",
    }),
    openCodeEvent(2, "assistant", {
      messageId: "assistant_final",
      parentId: "user_1",
      phase: "final_answer",
      finish: "stop",
    }),
  ]);

  assert.deepEqual(
    rows.map((row) =>
      row.type === "event"
        ? [row.type, row.event.index]
        : [row.type, row.events.map((item) => item.index), row.hasFinalResponse],
    ),
    [
      ["event", 0],
      ["event", 2],
    ],
  );
});

test("OpenCode interrupted process chain does not invent a final answer", () => {
  const rows = buildConversationPreviewRows([
    openCodeEvent(0, "user", { messageId: "user_1" }),
    openCodeEvent(1, "reasoning", {
      messageId: "assistant_process",
      parentId: "user_1",
      phase: "commentary",
      finish: "tool-calls",
    }),
    openCodeEvent(2, "user", { messageId: "user_2" }),
  ]);

  assert.deepEqual(
    rows.map((row) =>
      row.type === "event"
        ? [row.type, row.event.index]
        : [row.type, row.events.map((item) => item.index), row.hasFinalResponse],
    ),
    [
      ["event", 0],
      ["process", [1], false],
      ["event", 2],
    ],
  );
});

test("phase-less OpenCode text falls back to the final response after real process events", () => {
  const rows = buildConversationPreviewRows([
    openCodeEvent(0, "user", { messageId: "user_1" }),
    openCodeEvent(1, "reasoning", {
      messageId: "assistant_process",
      parentId: "user_1",
      phase: "commentary",
      finish: "tool-calls",
    }),
    openCodeEvent(2, "assistant", {
      messageId: "assistant_legacy_final",
      parentId: "user_1",
    }),
  ]);

  assert.deepEqual(
    rows.map((row) =>
      row.type === "event"
        ? [row.type, row.event.index]
        : [row.type, row.events.map((item) => item.index), row.hasFinalResponse],
    ),
    [
      ["event", 0],
      ["process", [1], true],
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
      ["process", [1], true],
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

test("search jump keeps its target visible even when the timeline hides that turn", () => {
  const visiblePromptIndexes = new Set([2]);

  assert.equal(
    isVisibleConversationEvent(event(0, "user"), visiblePromptIndexes, 0),
    true,
  );
});

test("process groups follow the global default and per-turn overrides", () => {
  const overrides = { 10: true, 20: false };

  assert.equal(isProcessGroupExpanded(10, true, overrides), true);
  assert.equal(isProcessGroupExpanded(20, false, overrides), false);
  assert.equal(isProcessGroupExpanded(30, true, overrides), false);
  assert.equal(isProcessGroupExpanded(30, false, overrides), true);
});

test("process group expansion summary reports uniform and mixed states", () => {
  assert.equal(summarizeProcessGroupExpansion([], true, {}), "collapsed");
  assert.equal(summarizeProcessGroupExpansion([], false, {}), "expanded");
  assert.equal(summarizeProcessGroupExpansion([10, 20], true, {}), "collapsed");
  assert.equal(summarizeProcessGroupExpansion([10, 20], false, {}), "expanded");
  assert.equal(
    summarizeProcessGroupExpansion([10, 20], true, { 10: true }),
    "mixed",
  );
});
