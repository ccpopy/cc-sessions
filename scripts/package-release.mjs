import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { deflateRawSync } from "node:zlib";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "..");
const crcTable = createCrcTable();

const args = parseArgs(process.argv.slice(2));
const target = args.target ?? "all";
const outputDir = args.outputDir ?? "release";
const buildProduct = args.buildProduct ?? false;
const version = args.version ?? readPackageVersion();
const platform = platformLabel();

if (!["source", "portable", "product", "cli", "all"].includes(target)) {
  throw new Error(`Unknown target: ${target}`);
}

const outputRoot = path.join(repoRoot, outputDir);
const tempRoot = path.join(outputRoot, ".tmp");
const cargoTargetRoot = args.cargoTargetDir
  ? path.resolve(repoRoot, args.cargoTargetDir)
  : process.env.CARGO_TARGET_DIR
  ? path.resolve(repoRoot, process.env.CARGO_TARGET_DIR)
  : path.join(repoRoot, "src-tauri", "target");

await ensureDir(outputRoot);
await cleanDir(tempRoot);

try {
  if (target === "source" || target === "all") {
    await createSourcePackage();
  }

  if (target === "portable" || target === "all") {
    await createPortablePackage();
  }

  if (target === "product" || target === "all") {
    await createProductPackage();
  }

  if (target === "cli" || target === "all") {
    await createCliPackage();
  }
} finally {
  await fs.rm(tempRoot, { recursive: true, force: true });
}

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--build-product" || arg === "--buildProduct") {
      out.buildProduct = true;
    } else if (arg.startsWith("--target=")) {
      out.target = arg.slice("--target=".length);
    } else if (arg === "--target") {
      out.target = argv[++i];
    } else if (arg.startsWith("--version=")) {
      out.version = arg.slice("--version=".length);
    } else if (arg === "--version") {
      out.version = argv[++i];
    } else if (arg.startsWith("--output-dir=")) {
      out.outputDir = arg.slice("--output-dir=".length);
    } else if (arg === "--output-dir" || arg === "--outputDir") {
      out.outputDir = argv[++i];
    } else if (arg.startsWith("--cargo-target-dir=")) {
      out.cargoTargetDir = arg.slice("--cargo-target-dir=".length);
    } else if (arg === "--cargo-target-dir" || arg === "--cargoTargetDir") {
      out.cargoTargetDir = argv[++i];
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }
  return out;
}

function readPackageVersion() {
  const packageJson = JSON.parse(readFileSync(path.join(repoRoot, "package.json"), "utf8"));
  return packageJson.version;
}

function platformLabel() {
  if (process.platform === "win32") return "windows";
  if (process.platform === "darwin") return `macos-${macosArchitectureLabel()}`;
  if (process.platform === "linux") return "linux";
  return process.platform;
}

function macosArchitectureLabel() {
  const arch = os.arch();
  if (arch === "arm64") return "arm64";
  if (arch === "x64") return "intel";
  throw new Error(`Unsupported macOS architecture: ${arch}`);
}

function executableName() {
  return process.platform === "win32" ? "cc-session-manager.exe" : "cc-session-manager";
}

function cliExecutableName() {
  return process.platform === "win32" ? "cc-sessions.exe" : "cc-sessions";
}

async function createSourcePackage() {
  const packageName = `cc-session-manager-source-v${version}`;
  const stage = path.join(tempRoot, packageName);
  const archivePath = path.join(outputRoot, `${packageName}.zip`);

  await cleanDir(stage);

  const directories = [
    "img",
    "src",
    "src-tauri/capabilities",
    "src-tauri/icons",
    "src-tauri/src",
    "public",
    "scripts",
  ];

  const files = [
    ".gitattributes",
    ".gitignore",
    "LICENSE",
    "README.md",
    "app-icon.svg",
    "components.json",
    "index.html",
    "package-lock.json",
    "package.json",
    "postcss.config.js",
    "tailwind.config.ts",
    "tsconfig.json",
    "tsconfig.node.json",
    "vite.config.ts",
    "src-tauri/build.rs",
    "src-tauri/Cargo.lock",
    "src-tauri/Cargo.toml",
    "src-tauri/tauri.conf.json",
  ];

  for (const item of directories) {
    await copyRepoItem(item, stage);
  }
  for (const item of files) {
    await copyRepoItem(item, stage);
  }

  await writeZipFromDirectory(stage, archivePath);
  console.log(`Source package: ${archivePath}`);
}

async function createPortablePackage() {
  if (buildProduct) {
    run("npx", ["tauri", "build", "--no-bundle", "--ci"], tauriBuildOptions());
  }

  const exePath = path.join(cargoTargetRoot, "release", executableName());
  if (!(await exists(exePath))) {
    throw new Error(`Release executable was not found: ${exePath}. Run with --build-product first.`);
  }

  const packageName = `cc-session-manager-portable-v${version}-${platform}`;
  const stage = path.join(tempRoot, packageName);
  const archivePath = path.join(outputRoot, `${packageName}.zip`);
  const executableOutputPath =
    process.platform === "win32"
      ? path.join(outputRoot, `cc-session-manager-portable-v${version}-windows.exe`)
      : null;

  await cleanDir(stage);
  await fs.copyFile(exePath, path.join(stage, executableName()));
  await fs.writeFile(path.join(stage, "cc-session-manager.portable"), "portable\n", "utf8");
  const readme = [
    "CC Sessions Portable",
    "",
    "Run:",
    `1. Unzip this package on ${platform}.`,
    `2. ${process.platform === "win32" ? "Double-click cc-session-manager.exe." : "Run ./cc-session-manager."}`,
    "",
    "Requirement:",
    process.platform === "darwin"
      ? "- macOS. Gatekeeper may require approving the app in System Settings if it is unsigned."
      : process.platform === "linux"
        ? "- Linux desktop environment with WebKitGTK/WebView dependencies installed."
        : "- Windows 10/11 with Microsoft Edge WebView2 Runtime.",
    "",
    "This is a portable package. It does not install shortcuts or an uninstaller.",
    "The cc-session-manager.portable marker keeps in-app updates in this extracted folder.",
    "When an update is installed, the app closes, replaces the executable in place, and restarts automatically.",
    "",
  ].join("\n");
  await fs.writeFile(path.join(stage, "README.txt"), readme, "utf8");

  await writeZipFromDirectory(stage, archivePath);
  if (executableOutputPath) {
    let writtenPath = executableOutputPath;
    try {
      await fs.copyFile(exePath, writtenPath);
    } catch (error) {
      if (error?.code !== "EBUSY" && error?.code !== "EPERM") throw error;
      // Windows 正在运行的便携版无法被覆盖；仍然产出 ZIP，并给新 EXE 一个可验证的备用名称。
      writtenPath = path.join(outputRoot, `cc-session-manager-portable-v${version}-windows-refresh.exe`);
      await fs.copyFile(exePath, writtenPath);
      console.warn(`Portable executable is running and could not be replaced: ${executableOutputPath}`);
    }
    console.log(`Portable executable: ${writtenPath}`);
  }
  console.log(`Portable package: ${archivePath}`);
}

async function createProductPackage() {
  const bundleDir = path.join(cargoTargetRoot, "release", "bundle");
  if (buildProduct) {
    await fs.rm(bundleDir, { recursive: true, force: true });
    run("npx", ["tauri", "build", "--ci"], tauriBuildOptions());
  }

  if (!(await exists(bundleDir))) {
    throw new Error(`Tauri bundle directory was not found: ${bundleDir}. Run with --build-product first.`);
  }

  const bundleFiles = await listFiles(bundleDir);
  if (bundleFiles.length === 0) {
    throw new Error(`Tauri bundle directory has no files: ${bundleDir}`);
  }
  const packageFiles = platform === "windows"
    ? bundleFiles.filter((filePath) => path.basename(filePath).includes(version))
    : bundleFiles;
  if (packageFiles.length === 0) {
    throw new Error(`Tauri bundle directory has no files for version ${version}: ${bundleDir}`);
  }

  const packageName = `cc-session-manager-product-v${version}-${platform}`;
  const stage = path.join(tempRoot, packageName);
  const archivePath = path.join(outputRoot, `${packageName}.zip`);

  await cleanDir(stage);
  for (const filePath of packageFiles) {
    const relativePath = path.relative(bundleDir, filePath);
    const destination = path.join(stage, relativePath);
    await ensureDir(path.dirname(destination));
    await fs.copyFile(filePath, destination);
  }

  const readme = [
    "CC Sessions",
    "",
    "This package contains the end-user build output generated by Tauri.",
    "",
    "Install:",
    "1. Unzip this package.",
    "2. Run the installer or application bundle for this platform.",
    "",
    "Do not use this package for source changes. Use the source package instead.",
    "",
  ].join("\n");
  await fs.writeFile(path.join(stage, "README.txt"), readme, "utf8");

  await writeZipFromDirectory(stage, archivePath);
  console.log(`Product package: ${archivePath}`);
}

async function createCliPackage() {
  if (buildProduct) {
    run("npm", ["run", "build"]);
    const buildArgs = [
      "build",
      "--manifest-path",
      path.join("src-tauri", "Cargo.toml"),
      "--release",
      "--no-default-features",
      "--bin",
      "cc-sessions",
    ];
    if (args.cargoTargetDir) {
      buildArgs.push("--target-dir", args.cargoTargetDir);
    }
    run("cargo", buildArgs);
  }

  const exePath = path.join(cargoTargetRoot, "release", cliExecutableName());
  if (!(await exists(exePath))) {
    throw new Error(`CLI executable was not found: ${exePath}. Run with --build-product first.`);
  }

  const packageName = `cc-sessions-cli-v${version}-${platform}`;
  const stage = path.join(tempRoot, packageName);
  const archivePath = path.join(outputRoot, `${packageName}.zip`);

  await cleanDir(stage);
  await fs.copyFile(exePath, path.join(stage, cliExecutableName()));
  await fs.writeFile(path.join(stage, "cc-sessions.portable"), "portable\n", "utf8");
  const distPath = path.join(repoRoot, "dist");
  if (!(await exists(distPath))) {
    throw new Error(`Web UI dist directory was not found: ${distPath}. Run npm run build first.`);
  }
  await fs.cp(distPath, path.join(stage, "dist"), { recursive: true });

  const readme = [
    "CC Sessions CLI",
    "",
    "Run:",
    `1. Unzip this package on ${platform}.`,
    `2. Run ${process.platform === "win32" ? ".\\cc-sessions.exe" : "./cc-sessions"}.`,
    "",
    "This CLI build is compiled without the Tauri desktop feature. It does not require a desktop session or WebView runtime.",
    "",
    "Examples:",
    "./cc-sessions list --limit 20",
    "./cc-sessions webui --host 127.0.0.1 --port 17888",
    "./cc-sessions --provider claude webui --host 127.0.0.1 --port 17888",
    "./cc-sessions --provider codex convert /path/to/rollout.jsonl --mode native",
    "./cc-sessions --provider claude convert /path/to/session.jsonl --mode simple",
    "./cc-sessions repair diagnose --json",
    "",
    "This package includes cc-sessions.portable, so Web UI settings are stored beside the executable as cc-sessions-webui-settings.json.",
    "Installed or custom builds without that marker use the OS user config directory.",
    "Set CC_SESSIONS_WEBUI_SETTINGS to use an explicit settings file path.",
    "",
  ].join("\n");
  await fs.writeFile(path.join(stage, "README.txt"), readme, "utf8");

  await writeZipFromDirectory(stage, archivePath);
  console.log(`CLI package: ${archivePath}`);
}

function tauriBuildOptions() {
  return {
    env: {
      ...process.env,
      CARGO_TARGET_DIR: cargoTargetRoot,
    },
  };
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: repoRoot,
    shell: process.platform === "win32",
    stdio: "inherit",
    ...options,
  });
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed with exit code ${result.status}.`);
  }
}

async function copyRepoItem(relativePath, destinationRoot) {
  const source = path.join(repoRoot, relativePath);
  if (!(await exists(source))) {
    throw new Error(`Required source package item was not found: ${source}`);
  }
  const destination = path.join(destinationRoot, relativePath);
  await ensureDir(path.dirname(destination));
  await fs.cp(source, destination, { recursive: true });
}

async function ensureDir(dir) {
  await fs.mkdir(dir, { recursive: true });
}

async function cleanDir(dir) {
  await fs.rm(dir, { recursive: true, force: true });
  await ensureDir(dir);
}

async function exists(targetPath) {
  try {
    await fs.access(targetPath);
    return true;
  } catch {
    return false;
  }
}

async function listFiles(root) {
  const out = [];
  async function walk(current) {
    const entries = await fs.readdir(current, { withFileTypes: true });
    for (const entry of entries) {
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) {
        await walk(full);
      } else if (entry.isFile()) {
        out.push(full);
      }
    }
  }
  await walk(root);
  return out;
}

async function writeZipFromDirectory(root, archivePath) {
  const files = await listFiles(root);
  const records = [];
  const chunks = [];
  let offset = 0;

  for (const filePath of files) {
    const data = await fs.readFile(filePath);
    const stat = await fs.stat(filePath);
    const compressed = deflateRawSync(data, { level: 9 });
    const body = compressed.length < data.length ? compressed : data;
    const method = body === compressed ? 8 : 0;
    const name = path.relative(root, filePath).replace(/\\/g, "/");
    const nameBuffer = Buffer.from(name, "utf8");
    const crc = crc32(data);
    const { dosTime, dosDate } = toDosDateTime(stat.mtime);
    const localHeader = Buffer.alloc(30);

    localHeader.writeUInt32LE(0x04034b50, 0);
    localHeader.writeUInt16LE(20, 4);
    localHeader.writeUInt16LE(0x0800, 6);
    localHeader.writeUInt16LE(method, 8);
    localHeader.writeUInt16LE(dosTime, 10);
    localHeader.writeUInt16LE(dosDate, 12);
    localHeader.writeUInt32LE(crc, 14);
    localHeader.writeUInt32LE(body.length, 18);
    localHeader.writeUInt32LE(data.length, 22);
    localHeader.writeUInt16LE(nameBuffer.length, 26);
    localHeader.writeUInt16LE(0, 28);

    chunks.push(localHeader, nameBuffer, body);
    records.push({
      nameBuffer,
      crc,
      compressedSize: body.length,
      size: data.length,
      method,
      mode: stat.mode,
      dosTime,
      dosDate,
      offset,
    });
    offset += localHeader.length + nameBuffer.length + body.length;
  }

  const centralChunks = [];
  let centralSize = 0;
  for (const record of records) {
    const centralHeader = Buffer.alloc(46);
    centralHeader.writeUInt32LE(0x02014b50, 0);
    centralHeader.writeUInt16LE(process.platform === "win32" ? 20 : 0x0314, 4);
    centralHeader.writeUInt16LE(20, 6);
    centralHeader.writeUInt16LE(0x0800, 8);
    centralHeader.writeUInt16LE(record.method, 10);
    centralHeader.writeUInt16LE(record.dosTime, 12);
    centralHeader.writeUInt16LE(record.dosDate, 14);
    centralHeader.writeUInt32LE(record.crc, 16);
    centralHeader.writeUInt32LE(record.compressedSize, 20);
    centralHeader.writeUInt32LE(record.size, 24);
    centralHeader.writeUInt16LE(record.nameBuffer.length, 28);
    centralHeader.writeUInt16LE(0, 30);
    centralHeader.writeUInt16LE(0, 32);
    centralHeader.writeUInt16LE(0, 34);
    centralHeader.writeUInt16LE(0, 36);
    centralHeader.writeUInt32LE(zipExternalAttributes(record.mode), 38);
    centralHeader.writeUInt32LE(record.offset, 42);
    centralChunks.push(centralHeader, record.nameBuffer);
    centralSize += centralHeader.length + record.nameBuffer.length;
  }

  const endRecord = Buffer.alloc(22);
  endRecord.writeUInt32LE(0x06054b50, 0);
  endRecord.writeUInt16LE(0, 4);
  endRecord.writeUInt16LE(0, 6);
  endRecord.writeUInt16LE(records.length, 8);
  endRecord.writeUInt16LE(records.length, 10);
  endRecord.writeUInt32LE(centralSize, 12);
  endRecord.writeUInt32LE(offset, 16);
  endRecord.writeUInt16LE(0, 20);

  await fs.rm(archivePath, { force: true });
  await fs.writeFile(archivePath, Buffer.concat([...chunks, ...centralChunks, endRecord]));
}

function zipExternalAttributes(mode) {
  if (process.platform === "win32") return 0;
  return ((mode & 0xffff) << 16) >>> 0;
}

function toDosDateTime(date) {
  const year = Math.max(date.getFullYear(), 1980);
  const dosTime =
    (date.getHours() << 11) |
    (date.getMinutes() << 5) |
    Math.floor(date.getSeconds() / 2);
  const dosDate =
    ((year - 1980) << 9) |
    ((date.getMonth() + 1) << 5) |
    date.getDate();
  return { dosTime, dosDate };
}

function createCrcTable() {
  const table = new Uint32Array(256);
  for (let i = 0; i < 256; i += 1) {
    let c = i;
    for (let k = 0; k < 8; k += 1) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[i] = c >>> 0;
  }
  return table;
}

function crc32(buffer) {
  let crc = 0xffffffff;
  for (const byte of buffer) {
    crc = crcTable[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  }
  return (crc ^ 0xffffffff) >>> 0;
}
