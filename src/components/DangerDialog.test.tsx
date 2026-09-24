import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";
const dom = new JSDOM("<div id='root'></div>", { url: "http://localhost", pretendToBeVisual: true });
Object.assign(globalThis, {
  window: dom.window, document: dom.window.document, HTMLElement: dom.window.HTMLElement,
  Node: dom.window.Node, NodeFilter: dom.window.NodeFilter, MutationObserver: dom.window.MutationObserver,
  CustomEvent: dom.window.CustomEvent, getComputedStyle: dom.window.getComputedStyle,
  HTMLInputElement: dom.window.HTMLInputElement, IS_REACT_ACT_ENVIRONMENT: true,
});
const React = await import("react");
const { act } = React;
const { createRoot } = await import("react-dom/client");
const { DangerDialog } = await import("./DangerDialog");

test("confirmation click prevents duplicate writes, retains failure and allows an explicit retry", async () => {
  let resolve!: () => void;
  let reject!: (reason: Error) => void;
  const pending = new Promise<void>((ok, fail) => { resolve = ok; reject = fail; });
  let calls = 0;
  const closed: boolean[] = [];
  const root = createRoot(document.getElementById("root")!);
  try {
    await act(async () => { root.render(<DangerDialog open onOpenChange={(v) => closed.push(v)} title="删除消息" confirmText="确认删除" onConfirm={() => { calls++; return calls === 1 ? pending : Promise.resolve(); }}>只删除已选择的逻辑消息</DangerDialog>); });
    const button = () => [...document.querySelectorAll("button")].find((b) => b.textContent === "确认删除")!;
    await act(async () => { button().click(); });
    assert.equal(button().disabled, true);
    await act(async () => { button().click(); });
    assert.equal(calls, 1);
    await act(async () => { reject(new Error("EDIT_CONFLICT")); });
    assert.ok(document.body.textContent?.includes("EDIT_CONFLICT"));
    assert.deepEqual(closed, []);
    await act(async () => { button().click(); });
    assert.equal(calls, 2);
    assert.deepEqual(closed, [false]);
  } finally { await act(async () => root.unmount()); dom.window.close(); }
});
