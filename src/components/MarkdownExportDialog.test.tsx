import assert from "node:assert/strict";
import test, { type TestContext } from "node:test";
import { JSDOM } from "jsdom";

const dom = new JSDOM("<div id='root'></div>", { url: "http://localhost", pretendToBeVisual: true });
Object.assign(globalThis, {
  window: dom.window, document: dom.window.document, HTMLElement: dom.window.HTMLElement,
  Element: dom.window.Element, Node: dom.window.Node, NodeFilter: dom.window.NodeFilter,
  DocumentFragment: dom.window.DocumentFragment,
  MutationObserver: dom.window.MutationObserver, CustomEvent: dom.window.CustomEvent,
  getComputedStyle: dom.window.getComputedStyle, HTMLInputElement: dom.window.HTMLInputElement,
  IS_REACT_ACT_ENVIRONMENT: true,
});
const React = await import("react");
const { act } = React;
const { createRoot } = await import("react-dom/client");
const { api } = await import("../lib/api");
const { MarkdownExportDialog } = await import("./MarkdownExportDialog");
import type { MarkdownExportOptions, SessionSummary } from "../lib/api";
import type { MarkdownPreviewPage } from "../lib/api";

async function mount(t: TestContext) {
  const exported: MarkdownExportOptions[] = [];
  t.mock.method(api, "previewSessionMarkdown", async () => ({ markdown: "preview", messages: [], has_more: false, next_offset: 0, truncated: false }));
  t.mock.method(api, "exportSessionMarkdown", async (params: { options: MarkdownExportOptions }) => {
    exported.push(params.options);
    return { ok: true, out_path: "fixture.md", markdown: "", message_count: 250, total_message_count: 250, bytes: 1 };
  });
  t.mock.method(window, "prompt", () => "fixture.md");
  const root = createRoot(document.getElementById("root")!);
  await act(async () => root.render(<MarkdownExportDialog open onOpenChange={() => {}} session={{ id: "fixture", title: "fixture", provider: "codex", rollout_path: "fixture.jsonl" } as SessionSummary} />));
  t.after(async () => { await act(async () => root.unmount()); });
  return { exported };
}

test("header export control writes the whole conversation without loading summaries", async (t) => {
  const { exported } = await mount(t);
  const button = [...document.querySelectorAll("button")].find((b) => b.textContent === "导出全部对话");
  assert.ok(button, "the header control must be an actionable button");
  await act(async () => button.click());
  assert.equal(exported.length, 1);
  assert.equal(exported[0].selected_indices, null);
});

test("date filtering exports all matches without enabling paged message selection", async (t) => {
  const { exported } = await mount(t);
  const start = document.getElementById("md-range-from-date");
  assert.ok(start, "time filtering must be independent of paged selection");
  await act(async () => start.click());
  const today = document.querySelector<HTMLButtonElement>('[data-today="true"] button');
  assert.ok(today);
  await act(async () => today.click());
  const button = [...document.querySelectorAll("button")].find((b) => b.textContent === "导出时间范围");
  assert.ok(button);
  await act(async () => button.click());
  assert.equal(exported.length, 1);
  assert.ok(exported[0].time_from);
  assert.equal(exported[0].selected_indices, null, "unloaded messages must not be excluded");
});

test("changing the date range reloads matching summaries and discards the old page", async (t) => {
  await mount(t);
  const requests: { params: { options: MarkdownExportOptions; offset: number }; resolve: (page: MarkdownPreviewPage) => void }[] = [];
  t.mock.method(api, "previewSessionMarkdown", (params: { options: MarkdownExportOptions; offset: number }) => new Promise<MarkdownPreviewPage>((resolve) => {
    requests.push({ params, resolve });
  }));
  await act(async () => document.getElementById("md-selection")!.click());
  const oldPage = requests.find((r) => !r.params.options.include_front_matter)!;
  assert.ok(oldPage);
  await act(async () => document.getElementById("md-range-from-date")!.click());
  await act(async () => document.querySelector<HTMLButtonElement>('[data-today="true"] button')!.click());
  const matching = requests.find((r) => !r.params.options.include_front_matter && r.params.options.time_from)!;
  assert.ok(matching);
  assert.equal(matching.params.offset, 0);
  const page = (index: number, text: string): MarkdownPreviewPage => ({ markdown: "", messages: [{ index, text, timestamp: "", role: "user" }], next_offset: 80, has_more: false, truncated: false });
  await act(async () => matching.resolve(page(200, "latest-summary")));
  await act(async () => oldPage.resolve(page(0, "stale-summary")));
  assert.ok(document.body.textContent?.includes("latest-summary"));
  assert.ok(!document.body.textContent?.includes("stale-summary"));
});

test("loading subsequent pages retains existing rows, selection and preview, including a retry", async (t) => {
  const { exported } = await mount(t);
  const requests: { offset: number; resolve: (page: MarkdownPreviewPage) => void; reject: (error: Error) => void }[] = [];
  t.mock.method(api, "previewSessionMarkdown", (params: { options: MarkdownExportOptions; offset: number }) => {
    if (params.options.include_front_matter) return Promise.resolve({ markdown: "retained-preview", messages: [], next_offset: 0, has_more: false, truncated: false });
    return new Promise<MarkdownPreviewPage>((resolve, reject) => requests.push({ offset: params.offset, resolve, reject }));
  });
  const page = (offset: number, count = 80): MarkdownPreviewPage => ({
    markdown: "", messages: Array.from({ length: count }, (_, i) => ({ index: offset + i, text: `summary-${offset + i}`, timestamp: "", role: "user" })),
    next_offset: offset + count, has_more: offset + count < 165, truncated: false,
  });
  const rows = () => [...document.querySelectorAll<HTMLElement>('div[role="button"]')];
  const loadButton = () => [...document.querySelectorAll("button")].find((b) => /^(加载下一页摘要|重试加载)$/.test(b.textContent ?? ""))!;
  const settlePreview = () => act(async () => { await new Promise((resolve) => setTimeout(resolve, 300)); });
  await act(async () => document.getElementById("md-selection")!.click());
  assert.equal(requests[0].offset, 0);
  await act(async () => requests[0].resolve(page(0)));
  const firstRows = rows();
  assert.equal(firstRows.length, 80);
  await act(async () => firstRows[79].click()); // Preserve a deliberate deselection.
  await settlePreview();
  assert.equal(document.querySelector("pre")?.textContent, "retained-preview");
  await act(async () => loadButton().click());
  assert.equal(requests[1].offset, 80);
  assert.ok(firstRows.every((row) => row.isConnected), "loading must not collapse the existing scroll content");
  assert.equal(document.querySelector("pre")?.textContent, "retained-preview", "the preview must not collapse while loading more summaries");
  await act(async () => requests[1].reject(new Error("fixture read failure")));
  assert.deepEqual(rows(), firstRows, "failed pagination retains the same rows for retry");
  const failure = document.querySelector('[role="alert"]')!;
  await act(async () => loadButton().click());
  assert.ok(failure.isConnected, "keep the error area until retry completes so the scroll height cannot shrink");
  assert.equal(requests[2].offset, 80);
  await act(async () => requests[2].resolve(page(80)));
  assert.equal(rows().length, 160);
  assert.deepEqual(rows().slice(0, 80), firstRows, "append reuses existing row elements");
  assert.equal(firstRows[79].querySelector('[role="checkbox"]')?.getAttribute("data-state"), "unchecked");
  await act(async () => loadButton().click());
  assert.equal(requests[3].offset, 160);
  assert.equal(rows().length, 160);
  await act(async () => requests[3].resolve(page(160, 5)));
  assert.equal(rows().length, 165);
  assert.equal(loadButton(), undefined);
  assert.ok(document.body.textContent?.includes("已到会话末尾"));
  await act(async () => [...document.querySelectorAll("button")].find((b) => b.textContent === "导出已选消息")!.click());
  assert.deepEqual(exported[0].selected_indices, Array.from({ length: 165 }, (_, i) => i).filter((i) => i !== 79));
});
