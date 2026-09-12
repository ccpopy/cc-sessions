import assert from "node:assert/strict";
import test from "node:test";
import type { PreviewEvent } from "./api";
import {
  buildPreviewEventSearchText,
  canDeleteEvent,
  canEditEventText,
  editableText,
  extractPreviewEventText,
  isConversationMessage,
  isStableForkNode,
  openCodeForkPoint,
  parseDiffCommentPrompt,
} from "./previewEvent.ts";

function event(raw: unknown, role: PreviewEvent["role"] = "user"): PreviewEvent {
  return {
    index: 1,
    timestamp: "",
    role,
    kind: "message",
    text_summary: "fallback",
    raw,
  };
}

test("Claude copy boundaries require an identified main-chain message", () => {
  const raw = { type: "user", uuid: "message-uuid", message: { role: "user", content: "hello" } };
  assert.equal(isStableForkNode(event(raw), "claude"), true);
  assert.equal(isStableForkNode(event({ ...raw, isMeta: true }), "claude"), false);
  assert.equal(isStableForkNode(event({ ...raw, isSidechain: true }), "claude"), false);
  assert.equal(isStableForkNode(event({ ...raw, uuid: undefined }), "claude"), false);
  assert.equal(isStableForkNode(event({ ...raw, uuid: "" }), "claude"), false);
  assert.equal(isStableForkNode(event({ ...raw, type: "progress" }), "claude"), false);
  assert.equal(isStableForkNode(event({ ...raw, type: "custom-title" }), "claude"), false);
  assert.equal(isStableForkNode(event(null), "claude"), false);
  assert.equal(isStableForkNode(event(raw), "opencode"), false);
});

test("Claude tool and signed-thinking messages remain valid inclusive SDK copy boundaries", () => {
  for (const content of [
    [{ type: "tool_use", id: "tool-1", name: "Read", input: {} }],
    [{ type: "text", text: "Inspecting" }, { type: "tool_use", id: "tool-1", name: "Read", input: {} }],
    [{ type: "thinking", thinking: "reasoning", signature: "opaque" }],
  ]) {
    assert.equal(isStableForkNode(event({
      type: "assistant", uuid: "assistant-uuid", message: { role: "assistant", content },
    }, "tool_call"), "claude"), true);
  }
  assert.equal(isStableForkNode(event({
    type: "user", uuid: "result-uuid", message: { role: "user", content: [
      { type: "tool_result", tool_use_id: "tool-1", content: "result" },
    ] },
  }, "tool_result"), "claude"), true);
});

test("OpenCode boundaries retain both message and part identity for whole-message copies", () => {
  for (const part_type of ["text", "reasoning", "tool"]) {
    const raw = { message: { role: "assistant" }, opencode: { message_id: "msg_first", part_id: "prt_first", part_type } };
    const node = event(raw, "assistant");
    assert.equal(isStableForkNode(node, "opencode"), true);
    assert.deepEqual(openCodeForkPoint(node), { event_index: 1, message_id: "msg_first", part_id: "prt_first" });
    for (const broken of [
      { ...raw, message: undefined },
      { ...raw, opencode: { ...raw.opencode, message_id: "" } },
      { ...raw, opencode: { ...raw.opencode, part_id: undefined } },
      { ...raw, opencode: { ...raw.opencode, part_type: "step-finish" } },
    ]) assert.equal(isStableForkNode(event(broken), "opencode"), false);
    assert.equal(openCodeForkPoint({ ...node, index: -1 }), null);
  }
  assert.equal(isStableForkNode(event(null), "opencode"), false);
});

test("Codex retains its existing message and event fork boundaries", () => {
  assert.equal(isStableForkNode(event({ type: "response_item", payload: { type: "message" } })), true);
  assert.equal(isStableForkNode(event({ type: "event_msg", payload: { type: "user_message" } })), true);
  assert.equal(isStableForkNode(event({ type: "response_item", payload: { type: "function_call" } }, "tool_call")), false);
});

test("extracts editable Codex text and exposes matching edit/delete capabilities", () => {
  const message = event({
    type: "response_item",
    payload: {
      type: "message",
      content: [{ type: "input_text", text: "first" }, { type: "input_text", text: "second" }],
    },
  });

  assert.equal(extractPreviewEventText(message), "first\n\nsecond");
  assert.equal(editableText(message), "first\nsecond");
  assert.equal(canEditEventText("codex", message), true);
  assert.equal(canDeleteEvent("codex", message), true);
  assert.equal(isConversationMessage(message), true);
});

test("builds a normalized search index from summary, kind, and raw event data", () => {
  const searchable = event({ payload: { command: "NPM RUN BUILD" } });
  searchable.kind = "Tool_Call";
  searchable.text_summary = "Frontend Check";

  const text = buildPreviewEventSearchText(searchable);

  assert.equal(text.includes("frontend check"), true);
  assert.equal(text.includes("tool_call"), true);
  assert.equal(text.includes("npm run build"), true);
});

test("internal Codex context messages are hidden from the conversation view", () => {
  const context = event({
    type: "response_item",
    payload: {
      type: "message",
      message: "<environment_context>\nworkspace\n</environment_context>",
    },
  });

  assert.equal(isConversationMessage(context), false);
});

test("parses diff comments and the follow-up request", () => {
  const parsed = parseDiffCommentPrompt(`
    Diff comments:

    Comment 1:
    File: browser:src/app.ts Lines: 10-12
    Comment: Handle the empty state.

    My request for Codex:
    Apply the review feedback.
  `);

  assert.deepEqual(parsed, {
    comments: [{ number: 1, context: "src/app.ts", body: "Handle the empty state." }],
    request: "Apply the review feedback.",
  });
});
