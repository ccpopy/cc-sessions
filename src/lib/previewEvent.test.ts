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
