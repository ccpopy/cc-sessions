import assert from "node:assert/strict";
import test from "node:test";
import { cleanSessionBusyErrors, sessionBusyMessages } from "./session-activity";

test("activity errors cover rejected commands and partial/background results without matching content", () => {
  assert.deepEqual(sessionBusyMessages(new Error("[SESSION_BUSY] 会话 a 正在写入")), ["会话 a 正在写入"]);
  assert.deepEqual(sessionBusyMessages({ reports: [
    { ok: false, error: "导入失败：[SESSION_BUSY] 会话 b 正在写入" },
    { ok: false, error: "[SESSION_BUSY] 会话 b 正在写入" },
    { content: "[SESSION_BUSY] 会话内容中的文本" },
  ] }), ["会话 b 正在写入"]);
  assert.deepEqual(sessionBusyMessages({ error: "普通错误" }), []);
  assert.deepEqual(cleanSessionBusyErrors({ reports: [{ error: "[SESSION_BUSY] 写入中", content: "[SESSION_BUSY] 原始内容" }] }), {
    reports: [{ error: "写入中", content: "[SESSION_BUSY] 原始内容" }],
  });
});
