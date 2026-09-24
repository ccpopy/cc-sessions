import assert from "node:assert/strict";
import test, { type TestContext } from "node:test";
import { JSDOM } from "jsdom";
import React, { act } from "react";
import { createRoot } from "react-dom/client";
import { api, type SessionProvider, type SessionSummary, type Settings } from "../lib/api";
import { useSettings } from "../stores/settings";
import { useSessions } from "./useSessions";

const deferred = <T,>() => {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((ok, fail) => { resolve = ok; reject = fail; });
  return { promise, resolve, reject };
};
const row = (id: string) => ({ id, title: id, cwd: "fixture", first_user_message: id, archived: false } as SessionSummary);
async function fixture(t: TestContext) {
  const dom = new JSDOM("<div id='root'></div>", { url: "http://localhost" });
  const previous = { window: globalThis.window, document: globalThis.document };
  Object.assign(globalThis, { window: dom.window, document: dom.window.document, IS_REACT_ACT_ENVIRONMENT: true });
  useSettings.setState({ settings: { codex_dir: "fixture", claude_dir: "claude" } as Settings });
  const requests: ReturnType<typeof deferred<SessionSummary[]>>[] = [];
  t.mock.method(api, "listSessions", () => {
    const request = deferred<SessionSummary[]>(); requests.push(request); return request.promise;
  });
  let controller!: ReturnType<typeof useSessions>;
  function View({ provider }: { provider: SessionProvider }) {
    controller = useSessions(provider, "");
    return <><button onClick={() => void controller.refresh({ afterMutation: true })}>保存后刷新</button>
      <output>{controller.allSessions.map((s) => s.id).join(",")}</output><span>{controller.loading ? "loading" : controller.error}</span></>;
  }
  const root = createRoot(document.getElementById("root")!);
  const render = async (provider: SessionProvider) => { await act(async () => { root.render(<View provider={provider} />); }); };
  t.after(async () => { await act(async () => root.unmount()); dom.window.close(); Object.assign(globalThis, previous); });
  await render("codex");
  // Explicit initial refresh avoids timer-based request races in the test itself.
  await act(async () => { void controller.refresh(); await Promise.resolve(); });
  return { requests, render, current: () => controller, click: async () => { await act(async () => { document.querySelector("button")!.click(); }); } };
}

test("a real click after mutation discards the old list and awaits a new request", async (t) => {
  const f = await fixture(t);
  assert.equal(f.requests.length, 1);
  await f.click();
  await act(async () => { f.requests[0].resolve([row("before")]); });
  assert.equal(document.querySelector("output")!.textContent, "");
  assert.equal(f.requests.length, 2);
  await act(async () => { f.requests[1].resolve([row("after")]); });
  assert.equal(document.querySelector("output")!.textContent, "after");
  assert.equal(f.current().loading, false);
});

test("ordinary refreshes coalesce; mutations during the follow-up require another read", async (t) => {
  const f = await fixture(t);
  await act(async () => { void f.current().refresh(); void f.current().refresh(); });
  assert.equal(f.requests.length, 1);
  await f.click();
  await act(async () => { f.requests[0].reject(new Error("stale request failure")); });
  assert.equal(f.current().error, null);
  assert.equal(f.requests.length, 2);
  await f.click();
  await act(async () => { f.requests[1].resolve([row("intermediate")]); });
  assert.equal(f.requests.length, 3);
  await act(async () => { f.requests[2].resolve([row("latest")]); });
  assert.equal(document.querySelector("output")!.textContent, "latest");
});

test("a late result cannot contaminate a new provider or directory scope", async (t) => {
  const f = await fixture(t);
  await f.click();
  const staleRefresh = f.current().refresh;
  await f.render("claude");
  await act(async () => { void f.current().refresh(); });
  assert.equal(f.requests.length, 2);
  await act(async () => { await staleRefresh({ afterMutation: true }); });
  await act(async () => { f.requests[1].resolve([row("claude")]); f.requests[0].resolve([row("old-codex")]); });
  assert.equal(document.querySelector("output")!.textContent, "claude");
  assert.equal(f.requests.length, 2);
});
