import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";
import type { FamilyOverlay, SessionSummary } from "../lib/api";

const dom = new JSDOM("<div id='root'></div>", { url: "http://localhost", pretendToBeVisual: true });
Object.assign(globalThis, {
  window: dom.window, document: dom.window.document, HTMLElement: dom.window.HTMLElement,
  Node: dom.window.Node, MutationObserver: dom.window.MutationObserver,
  CustomEvent: dom.window.CustomEvent, getComputedStyle: dom.window.getComputedStyle,
  IS_REACT_ACT_ENVIRONMENT: true,
});
const React = await import("react");
const { act } = React;
const { createRoot } = await import("react-dom/client");
const { SessionCard } = await import("./SessionCard");
const { TooltipProvider } = await import("./ui/tooltip");

const sourceId = "01a13911-1f71-7773-aec8-6d962d4d1069";
const session: SessionSummary = {
  provider: "codex", id: "01a13912-c64c-75d1-bb2b-0a77a008160a", rollout_path: "fixture.jsonl",
  cwd: "/workspace", cwd_display: "workspace", title: "同名会话", first_user_message: "",
  model: null, reasoning_effort: null, source: "cli", agent_nickname: null, agent_role: null,
  conversion_origin: null, tokens_used: 0, created_at: 1, updated_at: 1, archived: false,
  git_branch: null, rollout_bytes: 0, logs_count: 0, has_backup: false, resume_command: "",
};
const overlay: FamilyOverlay = {
  session_id: session.id, provider: "openai", family_id: null, branch_count: 0,
  is_active_branch: false, clone_state: "matches",
};
test.after(() => dom.window.close());

test("a Fork shows its source and opens the exact source ID without toggling selection", async () => {
  const opened: string[] = [];
  let toggles = 0;
  const root = createRoot(document.getElementById("root")!);
  try {
    await act(async () => root.render(<TooltipProvider><SessionCard
      s={session} selected={false} onToggleSelect={() => { toggles++; }}
      onPreview={() => {}} onCopyResume={() => {}} onRevealCwd={() => {}}
      onOpenForkSource={(id: string) => opened.push(id)}
      overlay={{ ...overlay, forked_from_id: sourceId, fork_count: 1 }}
    /></TooltipProvider>));
    const button = document.querySelector<HTMLButtonElement>(`button[aria-label="查看源会话 ${sourceId}"]`);
    assert.ok(button, "Fork source should be visible and keyboard accessible");
    assert.match(button.textContent ?? "", /Fork.*01a13911/);
    assert.match(document.body.textContent ?? "", /已派生 1 个 Fork/);
    await act(async () => button.click());
    assert.deepEqual(opened, [sourceId]);
    assert.equal(toggles, 0);
  } finally { await act(async () => root.unmount()); }
});

test("ordinary sessions and subagents are not labelled as Forks", async () => {
  const root = createRoot(document.getElementById("root")!);
  try {
    for (const source of ["cli", '{"subagent":{"thread_spawn":{"parent_thread_id":"parent"}}}']) {
      await act(async () => root.render(<TooltipProvider><SessionCard
        s={{ ...session, source }} selected={false} onToggleSelect={() => {}}
        onPreview={() => {}} onCopyResume={() => {}} onRevealCwd={() => {}}
        overlay={overlay}
      /></TooltipProvider>));
      assert.ok(!document.querySelector('button[aria-label^="查看源会话"]'));
      assert.ok(!document.body.textContent?.includes("已派生"));
    }
  } finally { await act(async () => root.unmount()); }
});
