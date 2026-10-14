import assert from "node:assert/strict";
import test from "node:test";
import { sessionDisplayPreview } from "./sessionText.ts";

test("conversation and timeline summaries show the request or selected answer", () => {
  assert.equal(sessionDisplayPreview("# Files mentioned by the user:\n\n## guide.md: /tmp/guide.md\n\n## My request:\n请修复这处问题"), "请修复这处问题");
  assert.equal(sessionDisplayPreview('<send_user_message_question_reply>[{"question":"选择服务商？","answer":"OpenAI"},{"question":"需要备份吗？","answer":"需要"}]</send_user_message_question_reply>'), "OpenAI；需要");
  assert.equal(sessionDisplayPreview("# Files mentioned by the user:\n\n## guide.md: /tmp/guide.md\n\n## My request:\n"), "guide.md");
});

test("ordinary, incomplete and embedded transcript summaries keep their existing meaning", () => {
  for (const text of ["普通消息", "# Files mentioned by the user:\n\n## guide.md:", "<send_user_message_question_reply>invalid JSON</send_user_message_question_reply>"]) {
    assert.equal(sessionDisplayPreview(text), text);
  }
  assert.equal(sessionDisplayPreview("The following is the Codex agent history whose request action you are assessing.\nTRANSCRIPT START\n[1] user: 请检查权限\n[2] assistant: 正在检查\nTRANSCRIPT END"), "请检查权限");
});
