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
const { SettingsSheet } = await import("./SettingsSheet");
const { TooltipProvider } = await import("./ui/tooltip");
const { api } = await import("../lib/api");
const { useSettings } = await import("../stores/settings");
import type { AppUpdateInfo, Settings } from "../lib/api";

const initial: Settings = {
  codex_dir: "codex", claude_dir: "claude", opencode_dir: "opencode", cursor_dir: "cursor",
  backup_dir: "backup", open_command: "", refresh_interval_ms: 1000,
  preview_only_messages: true, preview_collapse_process: true,
  update_network: { proxy_url: "", download_mirror: "" },
};
const available: AppUpdateInfo = {
  current_version: "0.6.12", latest_version: "9.0.0", html_url: "https://github.com/ccpopy/cc-sessions/releases",
  available: true, can_auto_install: true, install_mode: "portable", install_dir: "test-only",
  asset_name: "test.exe", message: null,
};

test.after(() => dom.window.close());

async function mount(t: TestContext, desktop = true, settings = initial) {
  if (desktop) window.__TAURI_INTERNALS__ = {};
  else delete window.__TAURI_INTERNALS__;
  useSettings.setState({ settings: structuredClone(settings), loading: false, error: null });
  t.mock.method(api, "appVersion", async () => "0.6.12");
  for (const name of ["validateCodexDir", "validateClaudeDir", "validateOpenCodeDir", "validateCursorDir"] as const) {
    t.mock.method(api, name, async () => ({ valid: true, has_state_db: true, has_sessions: true, threads_count: 0 }));
  }
  const root = createRoot(document.getElementById("root")!);
  await act(async () => {
    root.render(<TooltipProvider><SettingsSheet trigger={<button>打开设置</button>} /></TooltipProvider>);
  });
  await act(async () => button("打开设置").click());
  return root;
}

function button(label: string) {
  const element = [...document.querySelectorAll("button")].find((item) => item.textContent?.trim() === label);
  assert.ok(element, `missing button: ${label}`);
  return element;
}

function input(label: string) {
  const element = document.querySelector<HTMLInputElement>(`input[aria-label="${label}"]`);
  assert.ok(element, `missing input: ${label}`);
  return element;
}

async function fill(label: string, value: string) {
  await act(async () => {
    const element = input(label);
    Object.getOwnPropertyDescriptor(dom.window.HTMLInputElement.prototype, "value")!.set!.call(element, value);
    element.dispatchEvent(new dom.window.Event("input", { bubbles: true }));
  });
}

test("current update network is used for check and install without discarding unsaved directories", async (t) => {
  const checks: unknown[] = [];
  const installs: unknown[] = [];
  let completeCheck!: (info: AppUpdateInfo) => void;
  t.mock.method(api, "checkAppUpdate", async (network?: unknown) => {
    checks.push(network);
    return new Promise<AppUpdateInfo>((resolve) => { completeCheck = resolve; });
  });
  t.mock.method(api, "installAppUpdate", async (network?: unknown) => { installs.push(network); });
  const root = await mount(t);
  try {
    await fill("Codex 目录", "unsaved-codex");
    await fill("更新代理地址", " http://127.0.0.1:7890 ");
    await fill("下载加速地址", " https://mirror.example/gh/ ");
    await act(async () => button("检查更新").click());
    const network = { proxy_url: "http://127.0.0.1:7890", download_mirror: "https://mirror.example/gh/" };
    assert.deepEqual(checks, [network]);
    assert.equal(input("更新代理地址").disabled, true);
    assert.equal(input("下载加速地址").disabled, true);
    assert.equal(input("Codex 目录").value, "unsaved-codex");
    assert.deepEqual(useSettings.getState().settings, initial);
    await act(async () => completeCheck(available));
    await act(async () => button("下载并安装").click());
    assert.deepEqual(installs, [network]);
  } finally { await act(async () => root.unmount()); }
});

test("network preferences persist with settings and a failed check can be retried", async (t) => {
  let saved = structuredClone(initial);
  let attempts = 0;
  t.mock.method(api, "saveSettings", async (settings: Settings) => { saved = structuredClone(settings); });
  t.mock.method(api, "getSettings", async () => structuredClone(saved));
  t.mock.method(api, "checkAppUpdate", async () => {
    if (++attempts === 1) throw new Error("更新代理连接失败");
    return available;
  });
  const root = await mount(t);
  try {
    await fill("更新代理地址", "socks5h://127.0.0.1:1080");
    await fill("下载加速地址", "https://mirror.example/");
    await act(async () => button("保存设置").click());
    assert.deepEqual(saved.update_network, {
      proxy_url: "socks5h://127.0.0.1:1080", download_mirror: "https://mirror.example/",
    });
    await act(async () => button("检查更新").click());
    assert.ok(document.body.textContent?.includes("更新代理连接失败"));
    assert.equal(input("更新代理地址").disabled, false);
    await act(async () => button("检查更新").click());
    assert.ok(button("下载并安装"));
    await fill("下载加速地址", "");
    assert.ok(!document.body.textContent?.includes("有新版本"));
  } finally { await act(async () => root.unmount()); }
});

test("missing mirror defaults to ghproxy and saved custom or empty values survive reload", async (t) => {
  let saved = structuredClone(initial);
  Reflect.deleteProperty(saved, "update_network");
  t.mock.method(api, "saveSettings", async (settings: Settings) => { saved = structuredClone(settings); });
  t.mock.method(api, "getSettings", async () => structuredClone(saved));
  const root = await mount(t, true, saved);
  try {
    assert.equal(input("下载加速地址").value, "https://ghproxy.net/");
    for (const mirror of ["https://mirror.example/gh/", ""]) {
      await fill("下载加速地址", mirror);
      await act(async () => button("保存设置").click());
      await act(async () => useSettings.getState().load());
      assert.equal(saved.update_network.download_mirror, mirror);
      assert.equal(input("下载加速地址").value, mirror);
    }
  } finally { await act(async () => root.unmount()); }
});

test("Web UI does not offer desktop-only proxy settings", async (t) => {
  const root = await mount(t, false);
  try {
    assert.equal(document.querySelector('input[aria-label="更新代理地址"]'), null);
    assert.equal(document.querySelector('input[aria-label="下载加速地址"]'), null);
  } finally { await act(async () => root.unmount()); }
});
