import assert from "node:assert/strict";
import test, { type TestContext } from "node:test";
import { JSDOM } from "jsdom";
import type { BranchSyncState, FamilyStore } from "../lib/api";

const dom = new JSDOM("<div id='root'></div>", { url: "http://localhost", pretendToBeVisual: true });
Object.assign(globalThis, {
  window: dom.window, document: dom.window.document, HTMLElement: dom.window.HTMLElement,
  Node: dom.window.Node, NodeFilter: dom.window.NodeFilter, MutationObserver: dom.window.MutationObserver,
  CustomEvent: dom.window.CustomEvent, getComputedStyle: dom.window.getComputedStyle,
  HTMLInputElement: dom.window.HTMLInputElement,
  IS_REACT_ACT_ENVIRONMENT: true,
});
const React = await import("react");
const { act } = React;
const { createRoot } = await import("react-dom/client");
const { FamilyHistorySheet } = await import("./FamilyHistorySheet");
const { api } = await import("../lib/api");
const { toast } = await import("sonner");
const store: FamilyStore = {
  version: 1, index: { parent: "parent", child: "parent" },
  families: { parent: {
    family_id: "parent", root_id: "parent", active_id: "child", title: "Fixture", updated_at: "2026-10-14",
    chain: [
      { id: "parent", provider: "custom", status: "archived", rollout_relpath: "sessions/original.jsonl", created_at: "2026-10-14", sha256: null, line_count: null, note: null },
      { id: "child", provider: "openai", status: "active", rollout_relpath: "sessions/child.jsonl", created_at: "2026-10-14", sha256: null, line_count: null, note: null },
    ],
  } },
};
test.after(() => dom.window.close());

async function mount(t: TestContext, relation: BranchSyncState["relation"], appendable = 0) {
  t.mock.method(api, "getFamilyStore", async () => structuredClone(store));
  t.mock.method(api, "getArchiveLedger", async () => []);
  t.mock.method(api, "getFamilyBranchSyncStates", async () => [{
    branch_id: "parent", relation, provider: "openai", rollout_relpath: "sessions/current.jsonl",
    active_lines: 5, branch_lines: 3, appendable_lines_to_active: 0,
    appendable_lines_to_branch: appendable, error: null,
  }]);
  const root = createRoot(document.getElementById("root")!);
  await act(async () => root.render(<FamilyHistorySheet open onOpenChange={() => {}} sessionId="parent" codexDir="fixture" currentProvider="openai" />));
  return root;
}

function button(label: string) {
  const element = [...document.querySelectorAll("button")].find((b) => b.textContent?.trim() === label);
  assert.ok(element, `missing button: ${label}`);
  return element;
}

test("branch provider labels and switch confirmation use verified rollout metadata", async (t) => {
  const notices: string[] = [];
  t.mock.method(toast, "success", (message: string) => { notices.push(message); return 1; });
  t.mock.method(api, "rollbackFamilyActive", async () => {});
  const root = await mount(t, "same");
  try {
    assert.ok(!document.body.textContent?.includes("custom"));
    assert.ok(document.body.textContent?.includes("sessions/current.jsonl"));
    await act(async () => button("设为当前").click());
    await act(async () => button("确认切换").click());
    assert.deepEqual(notices, ["已切换到 openai 分支"]);
  } finally { await act(async () => root.unmount()); }
});

test("paginated ahead history can switch without promising or invoking line synchronization", async (t) => {
  const calls: string[] = [];
  t.mock.method(api, "rollbackFamilyActive", async () => { calls.push("switch"); });
  t.mock.method(api, "syncActiveIntoBranch", async () => { calls.push("append"); throw new Error("must not append"); });
  const root = await mount(t, "active_ahead");
  try {
    assert.ok(!document.body.textContent?.includes("同步到此分支"));
    await act(async () => button("设为当前").click());
    assert.ok(document.body.textContent?.includes("不会自动合并"));
    await act(async () => button("确认切换").click());
    assert.deepEqual(calls, ["switch"]);
  } finally { await act(async () => root.unmount()); }
});

test("legacy ahead history keeps sync-and-switch behavior", async (t) => {
  const calls: string[] = [];
  t.mock.method(api, "rollbackFamilyActive", async () => { calls.push("switch"); });
  t.mock.method(api, "syncActiveIntoBranch", async () => {
    calls.push("append"); return { source_id: "child", target_id: "parent", appended_lines: 2, total_lines: 5 };
  });
  const root = await mount(t, "active_ahead", 2);
  try {
    assert.ok(button("同步到此分支"));
    await act(async () => button("设为当前").click());
    await act(async () => button("同步并切换").click());
    assert.deepEqual(calls, ["append", "switch"]);
  } finally { await act(async () => root.unmount()); }
});
