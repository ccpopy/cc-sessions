# CC Sessions

![Version](https://img.shields.io/github/v/release/ccpopy/cc-sessions?label=version&sort=semver)
![License](https://img.shields.io/badge/license-MIT-green)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey)
![Tauri](https://img.shields.io/badge/Tauri-2-ff9900)

CC Sessions 是一款本地桌面应用，用于浏览、检索、备份、导入导出及修复 Codex 与 Claude Code 的会话记录。应用基于 Tauri、React、TypeScript 和 Rust 构建，默认读取本机的 `.codex` 和 `.claude` 目录。

![CC Sessions 模拟数据截图](img/readme-screenshot.png)

## 功能

- 按 Codex / Claude Code 来源查看会话列表。
- 支持按 ID、标题、首条消息及工作目录进行搜索。
- 预览 JSONL 会话内容，区分用户消息、助手消息、推理过程、工具调用与工具返回。
- 会话消息级编辑：在预览中原地改写用户/助手消息文本、删除单条上下文事件（自动级联配对的工具调用/返回、Codex 镜像行与关联推理，Claude 自动重连 parentUuid 链）；每次写入前自动保存原始快照并记录编辑日志，支持逐步撤销与一键还原，编辑后可直接 `resume` 续聊同一会话。思考/推理内容（Codex 加密、Claude 带签名）只可删除、不可改写。
- 归档管理与官方 Codex app 对齐：归档会把 rollout 移入 `archived_sessions/` 并同步 threads 表与 `session_index.jsonl`，取消归档按文件名日期移回 `sessions/YYYY/MM/DD/`；官方 app 里已归档的会话也会纳入列表（含 threads 记录缺失的孤儿归档文件），可直接取消归档或删除。
- 备份、恢复、导入、导出会话包。
- 将会话导出为人眼可读的 Markdown，用于知识归档或多 AI 之间的上下文传递。
- 修复 Codex 本地索引、重建 `threads` 表、清理 orphan 记录。
- Codex 会话支持 provider 分支管理，并可从稳定对话节点创建回溯分支。
- 设置页面支持手动检查 GitHub Release 更新，并跳转至最新 Release 下载页面。

## 快捷键

| 场景 | 快捷键 | 作用 |
| --- | --- | --- |
| 全局 | <kbd>Ctrl</kbd> / <kbd>Cmd</kbd> + <kbd>K</kbd> | 聚焦搜索框 |
| 全局 | <kbd>Ctrl</kbd> / <kbd>Cmd</kbd> + <kbd>B</kbd> | 展开或收起侧边栏 |
| 全局 | <kbd>Ctrl</kbd> / <kbd>Cmd</kbd> + <kbd>Shift</kbd> + <kbd>L</kbd> | 切换明暗主题 |
| 会话列表 | <kbd>Delete</kbd> / <kbd>Backspace</kbd> | 删除已选会话 |
| 会话预览 | <kbd>Home</kbd> | 滚动到已加载内容顶部 |
| 会话预览 | <kbd>End</kbd> | 滚动到已加载内容底部，并继续加载后续内容 |
| 会话预览 | <kbd>Page Up</kbd> | 向上翻页 |
| 会话预览 | <kbd>Page Down</kbd> | 向下翻页，并在接近底部时继续加载后续内容 |

全局快捷键不会在输入框、文本框或弹窗内触发；会话预览的滚动快捷键只在预览弹窗内生效，并会避开过滤输入框。

## 开发环境

前置依赖：

- Node.js 20 及以上版本
- npm
- Rust stable 工具链
- 目标平台对应的 Tauri 2 构建依赖

安装依赖：

```bash
npm ci
```

启动开发环境：

```bash
npm run tauri:dev
```

前端构建：

```bash
npm run build
```

Tauri 构建：

```bash
npm run tauri:build
```

## CLI / WSL 无桌面环境

仓库同时提供无桌面 CLI 二进制 `cc-sessions`。CLI 构建关闭 Tauri `desktop` feature，不启动窗口，也不依赖 WebView / WebKitGTK，适合 WSL、服务器或只有 SSH 的环境。

检查 CLI 构建：

```bash
npm run cli:check
```

构建 release 版 CLI：

```bash
npm run cli:build
```

构建后的二进制位于：

```bash
src-tauri/target/release/cc-sessions
```

Windows 下文件名为 `cc-sessions.exe`。

推荐优先使用交互式菜单。菜单会用序号逐层引导会话列表、搜索、项目分组、预览、备份、导入导出、修复诊断等常用操作，比直接记子命令更适合日常使用。直接运行 CLI 不带子命令时会进入菜单模式：

```bash
npm run cli:run
```

Windows release 版构建后可直接运行：

```powershell
.\src-tauri\target\release\cc-sessions.exe
```

进入菜单后输入序号即可逐层选择功能；列表页支持 `n` 下一页、`p` 上一页、`b` 返回上一层、`m` 返回主菜单、`0` 退出。列表页还支持 `s` 选择多个当前页序号、`u` 取消选择、`c` 清空选择、`d` 删除已选会话；选择序号可以用空格或逗号分隔，也支持 `1-3` 这种范围。删除、覆盖恢复、清理和分支切换等写入操作需要输入 `yes` 才会执行。

菜单中的 Codex / Claude 会话入口默认显示主会话；查看列表、搜索、按项目查看和按大小查看时，可以选择“只查看子代理会话”，这样子代理也能继续使用同一套分页、项目分组和排序能力。

交互菜单里的“预览会话内容”默认只显示 Codex / Claude Code 应用中可见的用户消息和助手消息，不显示工具调用、工具返回、元数据，也会过滤 Codex 注入的 AGENTS 指令和环境上下文。如需排查完整 JSONL 事件流，可在预览模式中选择“全部事件”。

需要脚本化或机器可读输出时，再使用子命令：

```bash
cargo run --manifest-path src-tauri/Cargo.toml --no-default-features --bin cc-sessions -- list --limit 20 --sort size
cargo run --manifest-path src-tauri/Cargo.toml --no-default-features --bin cc-sessions -- --json repair diagnose
```

推荐入口：

```bash
cc-sessions
cc-sessions menu
```

常用脚本化命令：

```bash
cc-sessions list --limit 20 --sort size
cc-sessions list --subagent --sort time
cc-sessions --provider claude search "关键词"
cc-sessions --provider claude projects --subagent
cc-sessions --codex-dir "\\wsl.localhost\Ubuntu\home\me\.codex" list
cc-sessions projects --archived
cc-sessions preview ~/.codex/sessions/.../rollout-xxx.jsonl --all
cc-sessions preview ~/.codex/sessions/.../rollout-xxx.jsonl --limit 40
cc-sessions preview ~/.codex/sessions/.../rollout-xxx.jsonl --mode all --limit 40
cc-sessions webui --host 127.0.0.1 --port 17888
cc-sessions --provider claude webui --host 127.0.0.1 --port 17888
cc-sessions backup create --backup-dir ./backups --id <session-id> --name first-backup
cc-sessions repair diagnose --json
cc-sessions repair index --dry-run
cc-sessions bundle export --out-dir ./bundles --id <session-id>
```

默认路径与桌面端一致：Codex 读取 `~/.codex`，Claude Code 读取 `~/.claude`。可通过 `--codex-dir`、`--claude-dir` 覆盖；Windows 下读取 WSL 内 Codex 数据时，`--codex-dir` 使用 `\\wsl.localhost\<发行版>\home\<用户>\.codex` 这类 UNC 路径。`list`、`search` 和 `projects` 默认只显示主会话，加入 `--subagent` 后只显示子代理会话，并保留按时间、项目和大小的排序 / 分组能力。`list` 和 `search` 支持 `--sort size` 按 token 从小到大排序，便于找出问候测试等低消耗无效会话。`preview` 默认是 `--mode conversation`，输出完整正文；`--summary` 可切回一行摘要，`--raw` 输出原始 JSONL，`--all` 或 `--limit 0` 会一直读取到文件末尾。如需查看工具调用、工具返回和原始元数据，请使用 `--mode all`。需要机器可读输出时加 `--json`。

### CLI Web UI

无桌面环境也可以启动一个内置 Web UI：

```bash
cc-sessions webui --host 127.0.0.1 --port 17888
```

默认绑定 `127.0.0.1`，只监听本机回环地址，适合 WSL、服务器和 SSH 环境。启动时会生成一次性 API token，并注入到本次返回的 Web 页面；浏览器后续调用本地 API 时必须带上该 token，请求没有 token 或 token 不匹配会被拒绝。

Web UI 的设置会持久化。官方 CLI 便携包内包含 `cc-sessions.portable` 标记文件，因此配置会写到 `cc-sessions` 可执行文件同目录下的 `cc-sessions-webui-settings.json`。安装版或没有该标记的自定义构建会写到系统用户配置目录下的 `cc-sessions/cc-sessions-webui-settings.json`，避免安装目录不可写。需要指定确切位置时，可以设置环境变量 `CC_SESSIONS_WEBUI_SETTINGS` 指向目标 JSON 文件。首次启动会创建配置文件；之后页面里保存的路径会继续沿用。只有在启动命令显式传入 `--codex-dir` 或 `--claude-dir` 时，才会用命令行路径覆盖并写回配置。若配置文件所在目录不可写，保存会直接报错。

`--provider codex|claude` 会决定打开根路径 `/` 时默认进入哪一组页面，例如：

```bash
cc-sessions --provider claude webui --host 127.0.0.1 --port 17888
```

WSL2 中启动后，Windows 宿主机通常可以直接打开 `http://localhost:17888` 访问；如果本机 localhost 转发不可用，可以在 WSL 内查看 IP：

```bash
hostname -I
```

然后显式绑定所有网卡：

```bash
cc-sessions webui --host 0.0.0.0 --port 17888
```

绑定 `0.0.0.0` 可能让局域网内其他设备访问此服务。该 Web UI 没有账号登录，只有在你明确需要宿主机或其他设备通过 WSL IP 访问时才应使用。

浏览器版 Web UI 复用桌面端页面。涉及选择目录、选择 zip 或保存 zip 的入口，在桌面端仍会打开系统对话框；在普通浏览器里会要求手动输入路径。这个路径以运行 `cc-sessions webui` 的环境为准，例如在 WSL 内启动时应填写 WSL 内可访问的路径。

### CLI 修复项说明

CLI 和桌面端的修复功能只处理 Codex 本地索引和可见性问题，不会修改会话正文语义，也不会凭空恢复已经删除的 JSONL 会话文件。

- `修复 session_index.jsonl`：扫描 `~/.codex/sessions/` 下仍存在的 active rollout 文件，重建 Codex 的 `session_index.jsonl`。它用于修复“会话文件还在，但索引缺失导致列表看不到”的问题，不是修复 JSONL 内容。
- `重建 threads 表`：从 rollout 元数据重新写入或更新 `~/.codex/state_5.sqlite` 中的 `threads` 表。它用于修复 Codex 本地列表、搜索、标题、工作目录等数据库记录缺失或漂移的问题。
- `清理 orphan 记录`：删除 `session_index.jsonl` 或 `threads` 表里指向已不存在 rollout 文件的残留记录。它不会删除仍存在的有效会话文件。
- `克隆会话到 provider` / `批量克隆到当前 provider`：用于处理 Codex `model_provider` 切换后，历史会话 provider 与当前配置不一致导致的可见性或续聊问题。
- `从事件创建回溯分支`：从某个稳定事件位置复制出新分支，并归档原 active 分支。该操作会写入本地 Codex 会话文件和索引，执行前会要求确认。
- `Claude GUI 会话列表修复`（`repair claude-gui [--fix] [--dry-run]`）：Claude Code 的 GUI（如 VS Code 插件）只读取会话文件头尾各 64KB 推导标题，推导失败的会话会从历史列表中消失（CLI `claude --resume` 不受影响，常见于走中转 provider 时 AI 标题生成失败、长会话 compact 后续聊等场景）。修复方式与官方"重命名会话"一致：在 jsonl 末尾补写一条 `custom-title` 记录，不改动既有内容。

## 发布

项目中以下文件的版本号需保持一致：

- `package.json`
- `src-tauri/Cargo.toml`
- `src-tauri/tauri.conf.json`

推送形如 `v0.2.6` 的 tag 将触发 GitHub Actions 打包并创建 Release：

```bash
git tag -a v0.2.6 -m "v0.2.6"
git push origin main
git push origin v0.2.6
```

工作流会在 Windows、macOS Apple Silicon、macOS Intel 和 Linux 上分别构建 Tauri 安装产物。macOS 打包要求 `src-tauri/icons/icon.icns` 存在，本仓库已提交 Tauri 生成的跨平台图标文件。

Windows Release 会额外上传 `cc-session-manager-portable-v版本号-windows.exe`，这是无需安装即可直接运行的便携版可执行文件。

Release 也会在 Windows、macOS Apple Silicon、macOS Intel 和 Linux job 中分别上传 `cc-sessions-cli-v版本号-平台.zip`，这是不依赖桌面环境的 CLI 版本；macOS CLI 包名会区分 `macos-arm64` 和 `macos-intel`。远程仓库推送版本 tag 触发发布时，CLI 包会和桌面安装包一起出现在同一个 GitHub Release 中。

## 手动打包

生成源码包：

```bash
npm run package:source
```

生成便携包：

```bash
npm run package:portable
```

在 Windows 上，该命令会同时生成便携版压缩包和可直接运行的 `cc-session-manager-portable-v版本号-windows.exe`。

生成安装器包：

```bash
npm run package:product
```

生成 CLI 包：

```bash
npm run package:cli
```

打包输出位于 `release/` 目录，该目录不会提交到仓库。

## macOS 可执行文件处理

从 GitHub Release 下载的 macOS 应用可能被 Gatekeeper 阻止运行，需移除 quarantine 扩展属性：

```bash
# 移除 .app 包的隔离标记
xattr -d com.apple.quarantine "/Applications/CC Sessions.app"
```

若使用便携包中的独立二进制文件，需额外赋予可执行权限：

```bash
chmod +x cc-session-manager
xattr -d com.apple.quarantine cc-session-manager
```

## 特别感谢

[linux.do](https://linux.do) —— 真诚、友善、团结、专业，共建你我引以为荣之社区。

[codex-session-cloner](https://github.com/goodnightzsj/codex-session-cloner) —— 参考了修复和会话导出导入的代码

[thful](https://github.com/thful) —— 参与 Markdown 导出功能测试并反馈问题。

L站用户 @fengtang —— 帮忙测试了本次更新内容。

## License

MIT
