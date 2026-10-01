import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";
import { register } from "node:module";
import type { PreviewEvent } from "../lib/api";

register(`data:text/javascript,${encodeURIComponent(`
  export async function load(url, context, nextLoad) {
    return url.endsWith(".css") ? { format: "module", source: "", shortCircuit: true } : nextLoad(url, context);
  }
`)}`, import.meta.url);

const dom = new JSDOM("<div id='root'></div>", { url: "http://localhost", pretendToBeVisual: true });
class ResizeObserver {
  constructor(private callback: (entries: unknown[]) => void) {}
  observe(target: HTMLElement) {
    queueMicrotask(() => this.callback([{ target, borderBoxSize: [{ blockSize: target.offsetHeight, inlineSize: 800 }] }]));
  }
  unobserve() {}
  disconnect() {}
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
  return { x: 0, y: 0, top: 0, left: 0, bottom: this.offsetHeight, right: 800, width: 800, height: this.offsetHeight, toJSON() {} };
};
dom.window.HTMLElement.prototype.scrollTo = function (options?: ScrollToOptions | number, _y?: number) {
  this.scrollTop = typeof options === "number" ? options : options?.top ?? this.scrollTop;
  queueMicrotask(() => this.dispatchEvent(new dom.window.Event("scroll")));
};
const React = await import("react");
const { act } = React;
const { createRoot } = await import("react-dom/client");
const { api } = await import("../lib/api");
const { PreviewDialog } = await import("./PreviewDialog");

test("loaded long previews and timelines mount only nearby rows without indexing raw JSON until search", async (t) => {
  let serializations = 0;
  const events: PreviewEvent[] = Array.from({ length: 800 }, (_, index) => {
    const role = index % 2 === 0 ? "user" : "assistant";
    return { index, role, timestamp: "", kind: role, text_summary: `fixture ${index}`,
      raw: { type: role, uuid: `fixture-${index}`, message: { role, content: `fixture ${index}` },
        toJSON() { serializations++; return { text: `fixture ${index}` }; } } };
  });
  t.mock.method(api, "previewPage", async (...[, , offset, limit]: Parameters<typeof api.previewPage>) => ({ events: events.slice(offset, offset + limit), capability: null }));
  t.mock.method(api, "previewUserPrompts", async () => ({ total_events: events.length, prompts: events.filter(e => e.role === "user").map(e => ({ index: e.index, offset: e.index, timestamp: "", text: e.text_summary, response: { text: "reply", timestamp: "" } })) }));
  const root = createRoot(document.getElementById("root")!);
  try {
    await act(async () => {
      root.render(<PreviewDialog open onOpenChange={() => {}} session={null} customRolloutPath="synthetic" initialJump={{ eventIndex: 798, eventOffset: 798, query: "" }} />);
    });
    await act(async () => { await new Promise(resolve => setTimeout(resolve, 60)); });
    const count = document.querySelectorAll("[data-event-index]").length;
    assert.ok(count > 0 && count < 50, `mounted ${count} messages`);
    assert.ok(document.querySelector('[data-event-index="798"]'), "tail target must be mounted before scrolling to it");
    assert.ok(document.querySelectorAll("[data-marker-index]").length < 100, "timeline rail must also be bounded");
    assert.equal(serializations, 0, "empty search must not stringify raw records");
  } finally {
    await act(async () => root.unmount());
  }
});

test("timeline tail stays selected when the viewport cannot align it with the scroll-spy threshold", async (t) => {
  const events: PreviewEvent[] = Array.from({ length: 6 }, (_, index) => {
    const role = index % 2 === 0 ? "user" : "assistant";
    return { index, role, timestamp: "", kind: role, text_summary: `tail fixture ${index}`,
      raw: { type: role, uuid: `tail-${index}`, message: { role, content: `tail fixture ${index}` } } };
  });
  t.mock.method(api, "previewPage", async (...[, , offset, limit]: Parameters<typeof api.previewPage>) => ({ events: events.slice(offset, offset + limit), capability: null }));
  t.mock.method(api, "previewUserPrompts", async () => ({ total_events: events.length, prompts: events.filter(e => e.role === "user").map(e => ({ index: e.index, offset: e.index, timestamp: "", text: e.text_summary, response: { text: "reply", timestamp: "" } })) }));
  const root = createRoot(document.getElementById("root")!);
  try {
    await act(async () => root.render(<PreviewDialog open onOpenChange={() => {}} session={null} customRolloutPath="synthetic-tail" />));
    const viewport = document.querySelector<HTMLElement>("[data-radix-scroll-area-viewport]")!;
    assert.ok(viewport);
    Object.defineProperty(viewport, "scrollHeight", { configurable: true, get: () => 1100 });
    const positions = [0, 100, 280, 380, 900, 1000];
    t.mock.method(dom.window.HTMLElement.prototype, "getBoundingClientRect", function (this: HTMLElement) {
      const index = this.dataset.eventIndex;
      const top = index === undefined ? 0 : positions[Number(index)] - viewport.scrollTop;
      return { x: 0, y: top, top, left: 0, bottom: top + this.offsetHeight, right: 800, width: 800, height: this.offsetHeight, toJSON() {} };
    });
    t.mock.method(viewport, "scrollTo", (options?: ScrollToOptions | number) => {
      viewport.scrollTop = Math.max(0, Math.min(500, typeof options === "number" ? options : options?.top ?? viewport.scrollTop));
      queueMicrotask(() => viewport.dispatchEvent(new dom.window.Event("scroll")));
    });
    const settle = () => act(async () => { await new Promise(resolve => setTimeout(resolve, 60)); });
    const active = () => document.querySelector("[data-marker-index][aria-current='true']")?.getAttribute("aria-label");
    const clickPrompt = async (ordinal: number) => {
      await act(async () => document.querySelector<HTMLButtonElement>(`[aria-label="第 ${ordinal} 条用户提问"]`)!.click());
      await settle();
    };
    await clickPrompt(3);
    assert.equal(viewport.scrollTop, 500, "browser clamps the final prompt jump at the bottom");
    assert.equal(active(), "第 3 条用户提问", "scroll tracking must not replace the clicked tail with its predecessor");
    await clickPrompt(3);
    assert.equal(active(), "第 3 条用户提问", "repeated tail clicks stay selected");
    await act(async () => {
      viewport.dispatchEvent(new dom.window.WheelEvent("wheel", { deltaY: -400, bubbles: true }));
      viewport.scrollTo({ top: 100 });
    });
    await settle();
    assert.equal(active(), "第 2 条用户提问", "manual scrolling still updates the active prompt");
    await clickPrompt(1);
    assert.equal(active(), "第 1 条用户提问");
    await clickPrompt(3);
    assert.equal(active(), "第 3 条用户提问");
  } finally {
    await act(async () => root.unmount());
  }
});

test("short prompt jumps survive layout corrections until the user scrolls", async (t) => {
  const events: PreviewEvent[] = Array.from({ length: 12 }, (_, index) => {
    const role = index % 2 === 0 ? "user" : "assistant";
    return { index, role, timestamp: "", kind: role, text_summary: `short fixture ${index}`,
      raw: { type: role, uuid: `short-${index}`, message: { role, content: `short fixture ${index}` } } };
  });
  t.mock.method(api, "previewPage", async (...[, , offset, limit]: Parameters<typeof api.previewPage>) => ({ events: events.slice(offset, offset + limit), capability: null }));
  t.mock.method(api, "previewUserPrompts", async () => ({ total_events: events.length, prompts: events.filter(e => e.role === "user").map(e => ({ index: e.index, offset: e.index, timestamp: "", text: e.text_summary, response: { text: "reply", timestamp: "" } })) }));
  const root = createRoot(document.getElementById("root")!);
  try {
    await act(async () => root.render(<PreviewDialog open onOpenChange={() => {}} session={null} customRolloutPath="synthetic-short" />));
    const viewport = document.querySelector<HTMLElement>("[data-radix-scroll-area-viewport]")!;
    assert.ok(viewport);
    Object.defineProperty(viewport, "scrollHeight", { configurable: true, get: () => 1200 });
    t.mock.method(dom.window.HTMLElement.prototype, "getBoundingClientRect", function (this: HTMLElement) {
      const index = this.dataset.eventIndex;
      const top = index === undefined ? 0 : 16 + Number(index) * 86 - viewport.scrollTop;
      return { x: 0, y: top, top, left: 0, bottom: top + 70, right: 800, width: 800, height: 70, toJSON() {} };
    });
    t.mock.method(viewport, "scrollTo", (options?: ScrollToOptions | number) => {
      viewport.scrollTop = Math.max(0, Math.min(600, typeof options === "number" ? options : options?.top ?? viewport.scrollTop));
      queueMicrotask(() => viewport.dispatchEvent(new dom.window.Event("scroll")));
    });
    const settle = () => act(async () => { await new Promise(resolve => setTimeout(resolve, 60)); });
    const active = () => document.querySelector("[data-marker-index][aria-current='true']")?.getAttribute("aria-label");
    const clickPrompt = async (ordinal: number) => {
      await act(async () => document.querySelector<HTMLButtonElement>(`[aria-label="第 ${ordinal} 条用户提问"]`)!.click());
      await settle();
    };
    await clickPrompt(1);
    assert.equal(viewport.scrollTop, 0);
    assert.equal(active(), "第 1 条用户提问", "the next short prompt must not replace the clicked first prompt");
    await clickPrompt(3);
    assert.equal(viewport.scrollTop, 344);
    assert.equal(active(), "第 3 条用户提问", "middle jumps must also keep their clicked prompt");
    await act(async () => viewport.scrollTo({ top: 360 }));
    await settle();
    assert.equal(active(), "第 3 条用户提问", "virtualizer layout corrections must not replace the clicked prompt");
    await clickPrompt(3);
    await act(async () => viewport.dispatchEvent(new dom.window.Event("scroll")));
    await settle();
    assert.equal(active(), "第 3 条用户提问", "repeated clicks and settling scroll events must not move the selection");
    await act(async () => {
      viewport.dispatchEvent(new dom.window.WheelEvent("wheel", { deltaY: 86, bubbles: true }));
      viewport.scrollTo({ top: 430 });
    });
    await settle();
    assert.equal(active(), "第 4 条用户提问", "wheel scrolling resumes normal scroll tracking");
    await clickPrompt(3);
    await act(async () => {
      viewport.parentElement!.dispatchEvent(new dom.window.MouseEvent("pointerdown", { bubbles: true }));
      viewport.scrollTo({ top: 430 });
    });
    await settle();
    assert.equal(active(), "第 4 条用户提问", "dragging the scrollbar resumes normal scroll tracking");
    await clickPrompt(3);
    await act(async () => viewport.dispatchEvent(new dom.window.KeyboardEvent("keydown", { key: "Home", bubbles: true, cancelable: true })));
    await settle();
    assert.equal(viewport.scrollTop, 0);
    assert.equal(active(), "第 2 条用户提问", "keyboard scrolling resumes the existing viewport-based tracking");
    await clickPrompt(6);
    assert.equal(active(), "第 6 条用户提问", "tail jumps remain correct");
    await clickPrompt(1);
    assert.equal(active(), "第 1 条用户提问", "jumping back from the tail must replace the previous selection");
    const search = document.querySelector<HTMLInputElement>('[aria-label="搜索已加载的会话内容"]')!;
    const setSearch = async (text: string) => {
      await act(async () => {
        Object.getOwnPropertyDescriptor(dom.window.HTMLInputElement.prototype, "value")!.set!.call(search, text);
        search.dispatchEvent(new dom.window.Event("input", { bubbles: true }));
      });
      await settle();
      assert.equal(document.querySelectorAll("[data-event-index]").length, 1, "search narrows the viewport to one message");
    };
    await setSearch("short fixture 0");
    await clickPrompt(1);
    assert.equal(search.value, "", "timeline jumps clear the search filter");
    assert.equal(active(), "第 1 条用户提问", "clearing a matching search must not replace the clicked first prompt");
    await setSearch("short fixture 4");
    await clickPrompt(3);
    assert.equal(viewport.scrollTop, 344, "matching search jumps align after the full conversation returns");
    assert.equal(active(), "第 3 条用户提问");
    await setSearch("short fixture 0");
    await clickPrompt(3);
    assert.equal(viewport.scrollTop, 344, "hidden search targets align after clearing the filter");
    assert.equal(active(), "第 3 条用户提问");
  } finally {
    await act(async () => root.unmount());
  }
});

test.after(() => dom.window.close());
