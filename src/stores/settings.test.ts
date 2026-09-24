import assert from "node:assert/strict";
import test from "node:test";
import { api, type Settings } from "../lib/api";
import { useSettings } from "./settings";

const initial: Settings = {
  codex_dir: "codex", claude_dir: "claude", opencode_dir: "opencode", cursor_dir: "cursor",
  backup_dir: "backup", open_command: "", refresh_interval_ms: 1000,
  preview_only_messages: true, preview_collapse_process: true,
};
const deferred = () => {
  let resolve!: () => void;
  const promise = new Promise<void>((r) => { resolve = r; });
  return { promise, resolve };
};

test("concurrent settings patches serialize against the last successful state", async (t) => {
  useSettings.setState({ settings: { ...initial } });
  let persisted = { ...initial };
  const gate = deferred();
  let calls = 0;
  t.mock.method(api, "saveSettings", async (settings: Settings) => {
    if (++calls === 1) await gate.promise;
    persisted = settings;
  });
  const a = useSettings.getState().save({ preview_only_messages: false });
  const b = useSettings.getState().save({ preview_collapse_process: false });
  gate.resolve();
  await Promise.all([a, b]);
  assert.equal(calls, 2);
  assert.equal(persisted.preview_only_messages, false);
  assert.equal(persisted.preview_collapse_process, false);
  assert.deepEqual(useSettings.getState().settings, persisted);
});

test("a failed save does not roll back successful patches or poison later saves", async (t) => {
  useSettings.setState({ settings: { ...initial } });
  let persisted = { ...initial };
  t.mock.method(api, "saveSettings", async (settings: Settings) => {
    if (settings.backup_dir === "bad") throw new Error("disk failure");
    persisted = settings;
  });
  const first = useSettings.getState().save({ preview_only_messages: false });
  const failure = assert.rejects(useSettings.getState().save({ backup_dir: "bad" }), /disk failure/);
  const later = useSettings.getState().save({ preview_collapse_process: false });
  await Promise.all([first, failure, later]);
  assert.deepEqual(persisted, { ...initial, preview_only_messages: false, preview_collapse_process: false });
  assert.deepEqual(useSettings.getState().settings, persisted);
});

test("an in-flight load cannot overwrite a later save after a page change", async (t) => {
  useSettings.setState({ settings: { ...initial } });
  const gate = deferred();
  let persisted = { ...initial };
  t.mock.method(api, "getSettings", async () => { await gate.promise; return { ...initial }; });
  t.mock.method(api, "saveSettings", async (settings: Settings) => { persisted = settings; });
  const load = useSettings.getState().load();
  const save = useSettings.getState().save({ preview_only_messages: false });
  gate.resolve();
  await Promise.all([load, save]);
  assert.equal(useSettings.getState().settings?.preview_only_messages, false);
  assert.deepEqual(useSettings.getState().settings, persisted);
});
