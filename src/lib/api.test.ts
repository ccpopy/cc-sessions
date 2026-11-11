import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";
import { api } from "./api";

const dom = new JSDOM("", { url: "http://localhost" });
Object.assign(globalThis, { window: dom.window, CustomEvent: dom.window.CustomEvent });
window.__CC_SESSIONS_WEBUI__ = { apiToken: "synthetic-test-token" };
test.after(() => dom.window.close());

test("busy rejection reaches the invoking view once, without a global blocking event", async (t) => {
  const events: string[] = [];
  t.mock.method(window, "dispatchEvent", (event: Event) => { events.push(event.type); return true; });
  t.mock.method(globalThis, "fetch", async () => new Response(JSON.stringify({ error: "[SESSION_BUSY] 会话正在写入，请稍后重试" }), { status: 409 }));
  await assert.rejects(api.listSessions("codex", "synthetic"), /会话正在写入，请稍后重试/);
  assert.deepEqual(events, []);
});

test("partial failures preserve item-level busy details and success results without a global event", async (t) => {
  const events: string[] = [];
  t.mock.method(window, "dispatchEvent", (event: Event) => { events.push(event.type); return true; });
  t.mock.method(globalThis, "fetch", async () => new Response(JSON.stringify([
    { id: "first", ok: true },
    { id: "second", ok: false, error: "[SESSION_BUSY] 第二条会话正在写入" },
  ])));
  const result = await api.deleteSessions("codex", "synthetic", ["first", "second"]);
  assert.equal(result[0].ok, true);
  assert.equal(result[1].ok, false);
  assert.equal(result[1].error, "第二条会话正在写入");
  assert.deepEqual(events, []);
});
