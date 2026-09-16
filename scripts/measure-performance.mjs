// Node 22.13+ (built-in SQLite). Uses generated, isolated data only.
import { createHash } from "node:crypto";
import { spawn, execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { setTimeout as delay } from "node:timers/promises";

const args = process.argv.slice(2);
const option = (name, fallback) => args.includes(name) ? args[args.indexOf(name) + 1] : fallback;
const binary = resolve(option("--binary", "src-tauri/target/release/cc-sessions.exe"));
const mode = option("--mode", "optimized");
if (!["baseline", "optimized"].includes(mode)) throw new Error("--mode must be baseline or optimized");
const output = resolve(option("--output", "output/performance"));
const samples = Number(option("--samples", "15"));
const port = Number(option("--port", "18769"));
if (!Number.isInteger(samples) || samples < 2 || !Number.isInteger(port) || port < 1 || port > 65535) {
  throw new Error("Invalid --samples or --port");
}
mkdirSync(output, { recursive: true });
const root = mkdtempSync(join(output, "fixture-"));
const dirs = Object.fromEntries(["codex", "claude", "opencode", "cursor", "backups"].map((kind) => {
  const path = join(root, kind);
  mkdirSync(path, { recursive: true });
  return [kind, path];
}));
const time = "2026-09-01T00:00:00Z";
const text = "Performance fixture. ".repeat(50);
const count = 6000;
const rows = Array.from({ length: count }, (_, index) => ({
  type: index % 2 ? "assistant" : "user", sessionId: "long-session", cwd: root, timestamp: time,
  message: { role: index % 2 ? "assistant" : "user", content: `${index}: ${text}` },
}));
const project = join(dirs.claude, "projects", "fixture");
mkdirSync(project, { recursive: true });
const claudePath = join(project, "long-session.jsonl");
writeFileSync(claudePath, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
for (let index = 0; index < 60; index += 1) {
  writeFileSync(join(project, `other-${index}.jsonl`), rows.slice(0, 100).map((row) => JSON.stringify({ ...row, sessionId: `other-${index}` })).join("\n") + "\n");
}
const codexPath = join(dirs.codex, "long-session.jsonl");
writeFileSync(codexPath, rows.map((row) => JSON.stringify({
  timestamp: time, type: "response_item", payload: { type: "message", role: row.message.role,
    content: [{ type: row.type === "user" ? "input_text" : "output_text", text: row.message.content }] },
})).join("\n") + "\n");

const opencodePath = join(dirs.opencode, "opencode.db");
const db = new DatabaseSync(opencodePath);
db.exec(`CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, directory TEXT, title TEXT, version TEXT, time_created INTEGER, time_updated INTEGER, time_archived INTEGER);
  CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
  CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
  CREATE INDEX message_session_time_created_id_idx ON message(session_id,time_created,id);
  CREATE INDEX part_session_idx ON part(session_id);
  CREATE INDEX part_message_id_id_idx ON part(message_id,id);
  INSERT INTO session VALUES ('long-session','fixture',NULL,'/fixture','Performance fixture','1.18.30',1000,9000,NULL);
  BEGIN;`);
const message = db.prepare("INSERT INTO message VALUES (?, 'long-session', ?, ?, ?)");
const part = db.prepare("INSERT INTO part VALUES (?, ?, 'long-session', ?, ?, ?)");
for (let index = 0; index < count; index += 1) {
  const id = `m-${String(index).padStart(6, "0")}`;
  message.run(id, index + 1000, index + 1000, JSON.stringify({ role: rows[index].type, modelID: "fixture", tokens: { total: 10 } }));
  part.run(`p-${id}`, id, index + 1000, index + 1000, JSON.stringify({ type: "text", text: rows[index].message.content }));
}
db.exec("COMMIT");
const queryPlan = db.prepare("EXPLAIN QUERY PLAN SELECT id,time_created FROM part WHERE session_id = ? ORDER BY time_created,id LIMIT 80").all("long-session");
db.close();

const cursorAgentDir = join(dirs.cursor, "agent-fixture");
mkdirSync(cursorAgentDir);
const store = new DatabaseSync(join(cursorAgentDir, "store.db"));
store.exec("CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB); CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT); BEGIN");
const blob = store.prepare("INSERT INTO blobs VALUES (?, ?)");
const ids = [];
for (let index = 0; index < count; index += 1) {
  const id = createHash("sha256").update(`fixture-${index}`).digest();
  ids.push(Buffer.concat([Buffer.from([0x0a, 32]), id]));
  blob.run(id.toString("hex"), Buffer.from(JSON.stringify(rows[index].message)));
}
const rootId = createHash("sha256").update("fixture-root").digest("hex");
blob.run(rootId, Buffer.concat(ids));
store.prepare("INSERT INTO meta VALUES ('0', ?)").run(Buffer.from(JSON.stringify({ agentId: "long-session", latestRootBlobId: rootId })).toString("hex"));
store.exec("COMMIT");
store.close();
const locator = (prefix, data) => `${prefix}:${Buffer.from(JSON.stringify(data)).toString("base64url")}`;
const sources = [
  ["codex", codexPath], ["claude", claudePath],
  ["opencode", locator("opencode", { db: opencodePath, session: "long-session" })],
  ["cursor", locator("cursor", { path: cursorAgentDir, session: "long-session", store: "agent" })],
];
const settingsPath = join(root, "settings.json");
writeFileSync(settingsPath, JSON.stringify({ codex_dir: dirs.codex, claude_dir: dirs.claude,
  opencode_dir: dirs.opencode, cursor_dir: dirs.cursor, backup_dir: dirs.backups, refresh_interval_ms: 3600000 }));
const child = spawn(binary, ["webui", "--port", String(port), "--provider", "claude",
  "--codex-dir", dirs.codex, "--claude-dir", dirs.claude, "--opencode-dir", dirs.opencode, "--cursor-dir", dirs.cursor], {
  windowsHide: true, stdio: ["ignore", "pipe", "pipe"],
  env: { ...process.env, CC_SESSIONS_WEBUI_SETTINGS: settingsPath,
    CC_SESSIONS_WEBUI_DIST: resolve("dist"), CC_SESSIONS_PROFILE: args.includes("--profile") ? join(root, "profile.jsonl") : "" },
});
let log = "";
child.stdout.on("data", (chunk) => { log += chunk; });
child.stderr.on("data", (chunk) => { log += chunk; });
process.on("SIGINT", () => { child.kill(); process.exit(0); });
process.on("SIGTERM", () => { child.kill(); process.exit(0); });
const url = `http://127.0.0.1:${port}`;
let token;
try {
  for (let retry = 0; retry < 100; retry += 1) {
    try {
      const html = await (await fetch(url)).text();
      token = JSON.parse(html.match(/window\.__CC_SESSIONS_WEBUI__ = (.*?);<\/script>/)?.[1] ?? "{}").apiToken;
      if (token) break;
    } catch { /* Startup only; do not include this in operation timing. */ }
    if (child.exitCode !== null) throw new Error(log);
    await delay(100);
  }
  if (!token) throw new Error(`Server did not start: ${log}`);
  writeFileSync(join(root, "fixture.json"), JSON.stringify({ sources, dirs, count, url, pid: child.pid }, null, 2));
  console.log(JSON.stringify({ fixture: root, url, pid: child.pid }));
  if (args.includes("--serve")) await new Promise((resolveExit) => child.on("exit", resolveExit));
  else {
    const common = { codexDir: dirs.codex, claudeDir: dirs.claude, opencodeDir: dirs.opencode, cursorDir: dirs.cursor };
    let responseBytes = 0;
    const invoke = async (command, body) => {
      const response = await fetch(`${url}/api/invoke/${command}`, {
        method: "POST", headers: { "Content-Type": "application/json", "X-CC-Sessions-Webui-Token": token }, body: JSON.stringify(body),
      });
      const raw = await response.text();
      responseBytes += Buffer.byteLength(raw);
      if (!response.ok) throw new Error(`${command}: ${raw}`);
      return JSON.parse(raw);
    };
    const results = [];
    const measure = async (scenario, run) => {
      const times = [];
      const bytes = [];
      for (let index = 0; index <= samples; index += 1) {
        responseBytes = 0;
        const start = performance.now();
        await run();
        times.push(performance.now() - start);
        bytes.push(responseBytes);
      }
      const warm = times.slice(1).sort((a, b) => a - b);
      results.push({ scenario, first_ms: times[0], warm_n: samples,
        median_ms: warm.length % 2 ? warm[(warm.length - 1) / 2] : (warm[warm.length / 2 - 1] + warm[warm.length / 2]) / 2,
        p95_ms: warm[Math.ceil(warm.length * 0.95) - 1], response_bytes: bytes[1],
        warm_ms: times.slice(1), warm_response_bytes: bytes.slice(1) });
      console.log(JSON.stringify(results.at(-1)));
    };
    await measure("claude_list", () => invoke("list_sessions", { ...common, provider: "claude" }));
    await measure("claude_exact_rename", () => invoke("rename_session", { ...common, provider: "claude", id: "long-session", rolloutPath: claudePath, title: "Renamed fixture" }));
    await measure("opencode_list", () => invoke("list_sessions", { ...common, provider: "opencode" }));
    const opencodeLocator = sources[2][1];
    for (const offset of [0, 3000, 5900]) {
      await measure(`opencode_preview_${offset}`, () => invoke("preview_session_range", { provider: "opencode", rolloutPath: opencodeLocator, offset, limit: 80 }));
    }
    for (const [provider, rolloutPath] of sources) {
      const header = { title: "Performance fixture", session_id: "long-session", provider };
      const options = { include_front_matter: true, include_reasoning: false, include_tools: false, ai_handoff_preamble: false };
      await measure(`${provider}_open_and_save`, async () => {
        if (mode === "baseline") {
          await invoke("preview_session_range", { provider, rolloutPath, offset: 0, limit: 100000 });
          await invoke("export_session_markdown", { provider, rolloutPath, header, options, outPath: null });
        } else {
          await invoke("preview_session_markdown", { provider, rolloutPath, header, options, offset: 0 });
        }
        const result = await invoke("export_session_markdown", { provider, rolloutPath, header, options, outPath: join(root, `${provider}.md`) });
        if (result.message_count !== count) throw new Error(`Incomplete export: ${provider} ${result.message_count}`);
      });
    }
    const peak = process.platform === "win32" ? Number(execFileSync("powershell.exe", ["-NoProfile", "-Command", `(Get-Process -Id ${child.pid}).PeakWorkingSet64`], { encoding: "utf8", windowsHide: true }).trim()) : null;
    writeFileSync(join(root, "results.json"), JSON.stringify({ mode, binary_sha256: createHash("sha256").update(readFileSync(binary)).digest("hex"),
      source_revision: option("--revision", "unspecified"), profile_enabled: args.includes("--profile"), platform: process.platform,
      count, claude_files: 61, query_plan: queryPlan, process_lifetime_peak_working_set_bytes: peak, results }, null, 2));
  }
} finally {
  child.kill();
  writeFileSync(join(root, "server.log"), log);
}
