import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";
import { register } from "node:module";
register(`data:text/javascript,${encodeURIComponent(`
  export async function load(url, context, nextLoad) {
    return url.endsWith(".css") ? { format: "module", source: "", shortCircuit: true } : nextLoad(url, context);
  }
`)}`, import.meta.url);
const dom = new JSDOM("<div id='root'></div>", { url: "http://localhost", pretendToBeVisual: true });
class ResizeObserver {
    private static observers = new Set<ResizeObserver>();
    private targets = new Set<HTMLElement>();
    constructor(private callback: (entries: unknown[]) => void) { ResizeObserver.observers.add(this); }
    observe(target: HTMLElement) {
        this.targets.add(target);
        queueMicrotask(() => this.notify(target));
    }
    unobserve(target: HTMLElement) { this.targets.delete(target); }
    disconnect() { this.targets.clear(); ResizeObserver.observers.delete(this); }
    private notify(target: HTMLElement) {
        if (this.targets.has(target))
            this.callback([{ target, borderBoxSize: [{ blockSize: target.offsetHeight, inlineSize: 800 }] }]);
    }
    static resize(target: HTMLElement) {
        for (const observer of this.observers)
            observer.notify(target);
    }
}
Object.assign(globalThis, {
    window: dom.window, document: dom.window.document, HTMLElement: dom.window.HTMLElement,
    Element: dom.window.Element, Node: dom.window.Node, NodeFilter: dom.window.NodeFilter,
    DocumentFragment: dom.window.DocumentFragment, MutationObserver: dom.window.MutationObserver,
    CustomEvent: dom.window.CustomEvent, getComputedStyle: dom.window.getComputedStyle,
    HTMLInputElement: dom.window.HTMLInputElement, ResizeObserver,
    requestAnimationFrame: dom.window.requestAnimationFrame.bind(dom.window),
    cancelAnimationFrame: dom.window.cancelAnimationFrame.bind(dom.window),
    IS_REACT_ACT_ENVIRONMENT: true,
});
Object.assign(dom.window, { ResizeObserver });
Object.defineProperties(dom.window.HTMLElement.prototype, {
    offsetHeight: { get() { return (this as HTMLElement).hasAttribute("data-index") ? 100 : 600; } },
    offsetWidth: { get() { return 800; } },
    clientHeight: { get() { return 600; } },
    scrollHeight: { get() {
            return Math.max(600, ...[...(this as HTMLElement).querySelectorAll<HTMLElement>("[style]")].map(node => Number.parseFloat(node.style.height) || 0));
        } },
});
dom.window.HTMLElement.prototype.getBoundingClientRect = function () {
    return { x: 0, y: 0, top: 0, left: 0, bottom: this.offsetHeight, right: 800, width: 800, height: this.offsetHeight, toJSON() { } };
};
dom.window.HTMLElement.prototype.scrollTo = function (options?: ScrollToOptions | number, _y?: number) {
    this.scrollTop = typeof options === "number" ? options : options?.top ?? this.scrollTop;
    queueMicrotask(() => this.dispatchEvent(new dom.window.Event("scroll")));
};
const React = await import("react");
const { act } = React;
const { createRoot } = await import("react-dom/client");
const { api } = await import("../lib/api");
const { default: SessionsRoute } = await import("./sessions");
const { useSettings } = await import("../stores/settings");
const { MemoryRouter } = await import("react-router-dom");
const { TooltipProvider } = await import("../components/ui/tooltip");
const { toast } = await import("sonner");
for (const provider of ["codex", "claude", "opencode"] as const)
    test(`${provider}: direct copy, repeat prevention, busy error and retry`, async (t) => {
        useSettings.setState({ settings: { codex_dir: "fixture", claude_dir: "fixture", opencode_dir: "fixture", backup_dir: "" } as any });
        const session = { provider, id: "fixture-id", rollout_path: "synthetic.jsonl", cwd: "/fixture", cwd_display: "fixture", title: "Synthetic session", first_user_message: "Synthetic prompt", model: null, reasoning_effort: null, source: null, agent_nickname: null, agent_role: null, conversion_origin: null, tokens_used: 0, created_at: Date.now(), updated_at: Date.now(), archived: false, git_branch: null, rollout_bytes: 100, logs_count: 4, has_backup: false, resume_command: "" };
        t.mock.method(api, "listSessions", async () => [session]);
        t.mock.method(api, "getSessionFamilyOverlay", async () => []);
        t.mock.method(api, "getArchiveLedger", async () => []);
        t.mock.method(api, "getProviderInfo", async () => ({ current: "openai" }));
        t.mock.method(api, "getProviderSyncPlan", async () => []);
        const notices: string[] = [];
        t.mock.method(toast, "error", (msg: string) => { notices.push(msg); return 1; });
        t.mock.method(toast, "success", () => 1);
        let calls = 0;
        let release: () => void = () => { };
        const method = provider === "codex" ? "duplicateSession" : provider === "claude" ? "duplicateClaudeSession" : "copyOpenCodeSession";
        t.mock.method(api, method, async () => { calls++; await new Promise<void>(r => release = r); if (calls === 1)
            throw new Error("会话正在写入，请稍后重试"); return { new_id: "new-fixture", total_lines: 4, message_count: 2, part_count: 2 }; });
        const root = createRoot(document.getElementById("root")!);
        try {
            await act(async () => root.render(<MemoryRouter initialEntries={[`/${provider}/sessions`]}><TooltipProvider><SessionsRoute provider={provider}/></TooltipProvider></MemoryRouter>));
            await act(async () => { await new Promise(r => setTimeout(r, 250)); });
            const getButton = () => [...document.querySelectorAll("button")].find(b => b.textContent?.trim() === (provider === "codex" ? "完整 Fork" : "复制会话"))!;
            assert.ok(getButton(), document.body.textContent || "");
            await act(async () => { const b = getButton(); b.click(); b.click(); });
            assert.equal(calls, 1);
            assert.equal(document.querySelector('[role="alertdialog"]'), null);
            assert.equal(document.querySelector('[role="dialog"]'), null);
            assert.ok([...document.querySelectorAll("button")].find(b => b.textContent?.includes(provider === "codex" ? "Fork 中" : "复制中"))?.disabled);
            await act(async () => release());
            assert.equal(notices.length, 1);
            assert.equal(document.querySelector('[role="alertdialog"]'), null);
            assert.ok(!getButton().disabled);
            await act(async () => getButton().click());
            assert.equal(calls, 2);
            await act(async () => release());
            assert.ok(!getButton().disabled);
        }
        finally {
            await act(async () => root.unmount());
        }
    });
test.after(() => dom.window.close());
