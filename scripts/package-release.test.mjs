import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { inflateRawSync } from "node:zlib";
import test from "node:test";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const notices = ["LICENSE", "third_party_licenses/magpie-MIT.txt"];

// Read the actual ZIP central directory and payloads, rather than checking script text.
function readZip(buffer) {
  const end = buffer.length - 22;
  assert.equal(buffer.readUInt32LE(end), 0x06054b50);
  let offset = buffer.readUInt32LE(end + 16);
  const files = new Map();
  for (let i = 0; i < buffer.readUInt16LE(end + 10); i += 1) {
    assert.equal(buffer.readUInt32LE(offset), 0x02014b50);
    const method = buffer.readUInt16LE(offset + 10);
    const size = buffer.readUInt32LE(offset + 20);
    const nameLength = buffer.readUInt16LE(offset + 28);
    const extraLength = buffer.readUInt16LE(offset + 30);
    const commentLength = buffer.readUInt16LE(offset + 32);
    const local = buffer.readUInt32LE(offset + 42);
    const name = buffer.subarray(offset + 46, offset + 46 + nameLength).toString("utf8");
    assert.equal(buffer.readUInt32LE(local), 0x04034b50);
    const start = local + 30 + buffer.readUInt16LE(local + 26) + buffer.readUInt16LE(local + 28);
    const data = buffer.subarray(start, start + size);
    assert.ok(method === 0 || method === 8);
    files.set(name, method === 8 ? inflateRawSync(data) : data);
    offset += 46 + nameLength + extraLength + commentLength;
  }
  return files;
}

async function createFixture(root) {
  const files = [
    ".gitattributes", ".gitignore", "README.md", "app-icon.svg", "components.json",
    "index.html", "package-lock.json", "postcss.config.js", "tailwind.config.ts",
    "tsconfig.json", "tsconfig.node.json", "vite.config.ts", "src-tauri/build.rs",
    "src-tauri/Cargo.lock", "src-tauri/Cargo.toml", "dist/index.html",
  ];
  for (const directory of ["img", "src", "src-tauri/capabilities", "src-tauri/icons", "src-tauri/src", "public", "scripts"]) {
    files.push(`${directory}/fixture.txt`);
  }
  const suffix = process.platform === "win32" ? ".exe" : "";
  files.push(`target/release/cc-session-manager${suffix}`, `target/release/cc-sessions${suffix}`,
    "target/release/bundle/installer/fixture_9.9.9.bin");
  for (const file of files) {
    await fs.mkdir(path.dirname(path.join(root, file)), { recursive: true });
    await fs.writeFile(path.join(root, file), `fixture: ${file}\n`);
  }
  await fs.writeFile(path.join(root, "package.json"), '{"type":"module","version":"9.9.9"}');
  for (const file of ["scripts/package-release.mjs", "src-tauri/tauri.conf.json", ...notices]) {
    await fs.mkdir(path.dirname(path.join(root, file)), { recursive: true });
    await fs.copyFile(path.join(repoRoot, file), path.join(root, file));
  }
}

test("all release ZIPs include exact license notices and preserve their payloads", async (t) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "cc-sessions-packaging-"));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  await createFixture(root);
  const result = spawnSync(process.execPath, ["scripts/package-release.mjs", "--target", "all", "--cargo-target-dir", "target"], {
    cwd: root, encoding: "utf8",
  });
  assert.equal(result.status, 0, result.stderr || result.stdout);
  const archives = (await fs.readdir(path.join(root, "release"))).filter((name) => name.endsWith(".zip"));
  assert.equal(archives.length, 4);
  const suffix = process.platform === "win32" ? ".exe" : "";
  for (const archive of archives) {
    const files = readZip(await fs.readFile(path.join(root, "release", archive)));
    for (const notice of notices) {
      assert.deepEqual(files.get(notice), await fs.readFile(path.join(repoRoot, notice)), `${archive}: ${notice}`);
    }
    const payload = archive.includes("-source-") ? "src/fixture.txt"
      : archive.includes("-portable-") ? `cc-session-manager${suffix}`
      : archive.includes("-product-") ? "installer/fixture_9.9.9.bin"
      : "dist/index.html";
    assert.ok(files.has(payload), `${archive}: ${payload}`);
    if (archive.includes("-cli-")) assert.ok(files.has(`cc-sessions${suffix}`));
  }
  await assert.rejects(fs.access(path.join(root, "release", ".tmp")));
});

test("Tauri bundles map both notices to readable resource paths", async () => {
  const config = JSON.parse(await fs.readFile(path.join(repoRoot, "src-tauri/tauri.conf.json"), "utf8"));
  for (const notice of notices) {
    assert.equal(config.bundle.resources[`../${notice}`], notice);
    await fs.access(path.resolve(repoRoot, "src-tauri", `../${notice}`));
  }
});
