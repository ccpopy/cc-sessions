import assert from "node:assert/strict";
import test, { type TestContext } from "node:test";
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
const { MemoryRouter } = await import("react-router-dom");
const { default: RepairRoute } = await import("./repair");
const { TooltipProvider } = await import("../components/ui/tooltip");
const { api } = await import("../lib/api");
const { useSettings } = await import("../stores/settings");
import type { Settings, DiagnosticReport, SessionProvider } from "../lib/api";

const diagnostic: DiagnosticReport = {
  rollout_count: 1, archived_rollout_count: 0, index_count: 1, threads_count: 1,
  threads_active_count: 1, threads_archived_count: 0, rollout_ids: [], index_ids: [], threads_ids: [],
  missing_in_index: [], missing_in_threads: [], orphan_in_index: [], orphan_in_threads: [],
  orphan_subagent_count: 1, orphan_subagent_ids: ["orphan"], current_provider: "new", provider_mismatched_families: 1,
};
const origins = { scanned: 1, fork_marked: 1, provider_sync_marked: 0, unknown_marked: 0, skipped_existing: 0 };
const pruneResult = { index_removed: 0, threads_removed: 0, subagents_removed: 1,
  family_branches_removed: 1, families_removed: 0, families_recovered: 0, families_normalized: 0,
  families_skipped: [], desktop_restart_required: false };

async function mount(t: TestContext, provider: SessionProvider = "codex") {
  useSettings.setState({ settings: { codex_dir: "codex", claude_dir: "claude" } as Settings });
  t.mock.method(api, "diagnoseCodexState", async () => diagnostic);
  t.mock.method(api, "getProviderInfo", async () => ({ current: "new", is_explicit: true }));
  t.mock.method(api, "diagnoseClaudeHistoryOrphans", async () => ({ orphan_rows: 1, orphan_session_ids: ["orphan"] }));
  t.mock.method(api, "diagnoseClaudeGuiVisibility", async () => ({ issues: [{ session_id: "a", proposed_title: "Title", project_dir: "project" }] }));
  const root = createRoot(document.getElementById("root")!);
  await act(async () => root.render(<MemoryRouter><TooltipProvider><RepairRoute provider={provider} /></TooltipProvider></MemoryRouter>));
  t.after(async () => { await act(async () => root.unmount()); });
}
function button(label: string) {
  const result = [...document.querySelectorAll("button")].find((item) => item.textContent?.trim().startsWith(label));
  assert.ok(result, `Missing button: ${label}`);
  return result;
}
async function click(label: string) { await act(async () => button(label).click()); }
function noDialog() { assert.equal(document.querySelector('[role="alertdialog"]'), null); }
test.after(() => dom.window.close());

test("provider migration and archive metadata repair run directly", async (t) => {
  const migration = t.mock.method(api, "runProviderSync", async (_input: Parameters<typeof api.runProviderSync>[0]) => []);
  const backfill = t.mock.method(api, "backfillArchiveOrigins", async () => origins);
  await mount(t);
  await act(async () => {
    const migrate = button("批量迁移到当前服务商");
    migrate.click();
    migrate.click();
  });
  noDialog();
  assert.equal(migration.mock.callCount(), 1);
  assert.equal(migration.mock.calls[0].arguments[0].dry_run, false);
  await click("扫描归档来源");
  await click("补全归档来源标记");
  noDialog();
  assert.deepEqual(backfill.mock.calls.map((call) => call.arguments), [["codex", true], ["codex", false]]);
});

test("family metadata repair runs directly while session deletion can be cancelled and confirmed once", async (t) => {
  const prune = t.mock.method(api, "pruneOrphanEntries", async (_input: Parameters<typeof api.pruneOrphanEntries>[0]) => pruneResult);
  t.mock.method(api, "verifyFamilyIntegrity", async () => ({ items: [], all_ok: true }));
  await mount(t);
  await click("执行深度校验");
  await click("清理 family 残留");
  noDialog();
  assert.equal(prune.mock.calls[1].arguments[0].prune_family, true);
  assert.equal(prune.mock.calls[1].arguments[0].dry_run, false);
  await click("清理孤儿子代理");
  assert.ok(document.querySelector('[role="alertdialog"]'));
  await click("取消");
  assert.equal(prune.mock.callCount(), 2);
  await click("清理孤儿子代理");
  await click("确认清理");
  noDialog();
  assert.equal(prune.mock.callCount(), 3);
  assert.equal(prune.mock.calls[2].arguments[0].prune_subagents, true);
});

test("dry-run deletion bypasses confirmation and never requests a write", async (t) => {
  const prune = t.mock.method(api, "pruneOrphanEntries", async (_input: Parameters<typeof api.pruneOrphanEntries>[0]) => pruneResult);
  await mount(t);
  await act(async () => document.getElementById("dry-run")!.click());
  await click("清理孤儿子代理");
  noDialog();
  assert.equal(prune.mock.calls[0].arguments[0].dry_run, true);
});

test("Claude title repair runs directly and blocks repeated clicks while pending", async (t) => {
  let finish!: (result: unknown) => void;
  const repair = t.mock.method(api, "repairClaudeGuiVisibility", () => new Promise((resolve) => { finish = resolve; }));
  await mount(t, "claude");
  await act(async () => {
    const repair = button("修复会话标题");
    repair.click();
    repair.click();
  });
  noDialog();
  assert.equal(button("修复会话标题").disabled, true);
  await click("修复会话标题");
  assert.equal(repair.mock.callCount(), 1);
  assert.deepEqual(repair.mock.calls[0].arguments, ["claude", false]);
  await act(async () => finish({ fixed: 1, errors: [] }));
  assert.equal(button("修复会话标题").disabled, false);
});

for (const provider of ["codex", "claude"] as const) {
  test(`${provider} repair guard releases on scope changes without unlocking a newer operation`, async (t) => {
    const finishes: Array<(value: any) => void> = [];
    const operation = provider === "codex"
      ? t.mock.method(api, "runProviderSync", () => new Promise((resolve) => finishes.push(resolve)))
      : t.mock.method(api, "repairClaudeGuiVisibility", () => new Promise((resolve) => finishes.push(resolve)));
    const label = provider === "codex" ? "批量迁移到当前服务商" : "修复会话标题";
    const result = provider === "codex" ? [] : { fixed: 1, errors: [] };
    await mount(t, provider);
    const guiDiagnosis = t.mock.method(api, "diagnoseClaudeGuiVisibility", async (directory: string) => ({
      issues: [{ session_id: directory, proposed_title: directory, project_dir: directory }],
    }));
    await click(label);
    await act(async () => useSettings.setState({ settings: {
      ...useSettings.getState().settings!,
      [provider === "codex" ? "codex_dir" : "claude_dir"]: "another-directory",
    } }));
    assert.equal(button(label).disabled, false);
    await click(label);
    assert.equal(operation.mock.callCount(), 2);
    await act(async () => finishes[0](result));
    assert.equal(button(label).disabled, true, "old completion must not release the new operation");
    if (provider === "claude") {
      assert.deepEqual(guiDiagnosis.mock.calls.map((call) => call.arguments[0]), ["another-directory"]);
      assert.ok(document.body.textContent?.includes("another-directory"), "new diagnostics must remain visible");
    }
    await click(label);
    assert.equal(operation.mock.callCount(), 2);
    await act(async () => finishes[1](result));
    assert.equal(button(label).disabled, false);
  });
}

test("Claude diagnostic responses from a previous directory cannot replace current results", async (t) => {
  await mount(t, "claude");
  let finishOld!: (result: any) => void;
  t.mock.method(api, "diagnoseClaudeGuiVisibility", async (directory: string) => {
    if (directory === "slow-directory") return new Promise((resolve) => { finishOld = resolve; });
    return { issues: [{ session_id: directory, proposed_title: "Current diagnostic title", project_dir: directory }] };
  });
  const changeDirectory = async (directory: string) => {
    await act(async () => useSettings.setState({ settings: {
      ...useSettings.getState().settings!, claude_dir: directory,
    } }));
  };
  await changeDirectory("slow-directory");
  await changeDirectory("current-directory");
  await act(async () => finishOld({ issues: [{ session_id: "old", proposed_title: "Outdated diagnostic title", project_dir: "old" }] }));
  assert.ok(document.body.textContent?.includes("Current diagnostic title"));
  assert.equal(document.body.textContent?.includes("Outdated diagnostic title"), false);
});
