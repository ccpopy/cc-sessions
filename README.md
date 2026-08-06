# CC Sessions

![Version](https://img.shields.io/github/v/release/ccpopy/cc-sessions?label=version&sort=semver)
![License](https://img.shields.io/badge/license-MIT-green)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey)
![Tauri](https://img.shields.io/badge/Tauri-2-ff9900)

CC Sessions 是一个本地 Agent 会话管理工具，用来查看、搜索、备份、迁移和修复 Codex、Claude Code 与 OpenCode 的会话。桌面版基于 Tauri、React、TypeScript 和 Rust 构建，默认读取本机的 `.codex`、`.claude` 和 OpenCode 数据目录。

![CC Sessions 模拟数据截图](img/readme-screenshot.png)

## 0.5.0 焕新

- 左侧导航改为顶部 Agent 切换器，下方只展示当前 Agent 的功能。视觉风格保持原有低饱和配色与细边框，同时为后续接入更多 Agent 留出空间。
- 新增 Claude Memory 管理，读取 `~/.claude/projects/*/memory/*.md`，支持项目与文件浏览、新建、编辑、重命名和删除。保存使用原子替换与 SHA-256 版本校验，避免覆盖其他 Claude 实例刚写入的内容。
- 新增 OpenCode 会话管理，直接读取当前版本的 `opencode.db`，支持项目分组、搜索、预览、对话时间线、归档、重命名、删除、Markdown 导出和复制 `opencode --session <id>` 续聊命令。旧版 `storage/` JSON 不与 SQLite 数据混合读取，OpenCode 会话也不支持消息级编辑。
- 全局统计新增 OpenCode 维度：统计页可以单看 OpenCode，「全部」也会把 OpenCode 会话计入 KPI、趋势、项目 Top 10、模型分布和热力图。没有安装或未配置 OpenCode 时按零会话处理，不影响其他 Agent 的统计。
- Family 清理从“删除残留”升级为安全恢复：当记录中的当前分支文件丢失、但只存在一个可确定的 active 候选时自动接管；当多个分支被误标为 active 时，以 `active_id` 为准校正状态。流程不会改写仍存在的会话 JSONL。

Codex 目前也提供独立的本地 Memory：可通过 `/memories` 与“设置 → 个性化”控制，默认存储在 `~/.codex/memories/`。官方将这些文件定义为生成状态，不建议把手工编辑作为主要控制方式，因此 0.5.0 只为缺少同类管理界面的 Claude Memory 提供文件管理，不接管 Codex 自身的 Memory 生命周期。参见 [Codex Memories 官方说明](https://learn.chatgpt.com/docs/customization/memories)。

## 功能

- 分别查看 Codex、Claude Code 和 OpenCode 会话，并按 ID、标题、首条消息或工作目录搜索。
- 预览 JSONL 内容，区分用户消息、助手回复、推理过程、工具调用和工具返回。
- 管理 Claude 项目 Memory Markdown 文件，提供并发冲突保护。
- 管理 OpenCode SQLite 会话，并按 `project → session → message → part` 结构生成统一预览。
- 在 Codex 与 Claude Code 之间转换会话。简洁续聊模式只保留稳定内容，原生实验模式会尽量保留过程消息和工具事件。转换会新建会话，不修改原文件。
- 在预览中修改用户或助手文本，也可以删除单条上下文事件。删除时会同步处理配对的工具事件、Codex 镜像行和关联推理；Claude 会话会重新连接 `parentUuid` 链。
- 每次编辑前保存原始快照并记录日志，可逐步撤销或一键还原。编辑后仍可通过 `resume` 续聊。Codex 的加密推理和 Claude 的签名思考只能删除，不能改写。
- 管理 Codex 归档会话。归档时把 rollout 移入 `archived_sessions/`，同步 `threads` 表和 `session_index.jsonl`；取消归档时按文件名日期移回 `sessions/YYYY/MM/DD/`。列表也会显示缺少 `threads` 记录的归档文件。
- 备份、恢复、导入和导出会话包，也可导出 Markdown。
- 修复 Codex 本地索引、重建 `threads` 表并清理 orphan 记录。
- 管理 Codex provider 分支，或从稳定的对话节点创建回溯分支。
- 检查 GitHub Release 更新。桌面版可下载、校验并安装更新，CLI Web UI 提供手动下载入口。

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

全局快捷键在输入框、文本框和弹窗内不会触发。预览滚动快捷键只在预览弹窗内生效，也不会干扰过滤输入框。

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

仓库还提供无桌面的 `cc-sessions` CLI。它关闭 Tauri 的 `desktop` feature，不启动窗口，也不依赖 WebView 或 WebKitGTK，可用于 WSL、服务器和 SSH 环境。

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

直接运行 CLI 会进入交互菜单。菜单包含会话列表、搜索、项目分组、预览、Codex/Claude 互转、备份、导入导出和修复诊断：

```bash
npm run cli:run
```

Windows release 版构建后可直接运行：

```powershell
.\src-tauri\target\release\cc-sessions.exe
```

菜单列表用 `n` 和 `p` 翻页，`b` 返回上一层，`m` 返回主菜单，`0` 退出。输入 `s` 可以多选当前页会话，`u` 取消选择，`c` 清空选择，`d` 删除已选会话。序号可用空格或逗号分隔，也支持 `1-3` 这样的范围。

删除、覆盖恢复、清理和分支切换等操作需要输入 `yes` 确认。

Codex 和 Claude 会话入口默认只显示主会话。查看列表、搜索、项目或大小排序时，可以切换为只看子代理会话。子代理会话沿用相同的分页、分组和排序方式，但不提供会话转换。

交互菜单中的 `预览会话内容` 默认显示用户消息和每轮最终答复，并过滤 Codex 注入的 AGENTS 指令与环境上下文。选择 `全部事件` 可以查看过程消息、工具事件、元数据和完整 JSONL 事件流。

需要脚本或机器可读输出时，可以直接使用子命令：

```bash
cargo run --manifest-path src-tauri/Cargo.toml --no-default-features --bin cc-sessions -- list --limit 20 --sort size
cargo run --manifest-path src-tauri/Cargo.toml --no-default-features --bin cc-sessions -- --json repair diagnose
```

安装后的入口：

```bash
cc-sessions
cc-sessions menu
```

常用命令：

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
cc-sessions --provider opencode webui --host 127.0.0.1 --port 17888
cc-sessions --provider codex convert ~/.codex/sessions/.../rollout-xxx.jsonl --mode native
cc-sessions --provider claude convert ~/.claude/projects/.../<session-id>.jsonl --mode simple
cc-sessions backup create --backup-dir ./backups --id <session-id> --name first-backup
cc-sessions repair diagnose --json
cc-sessions repair index --dry-run
cc-sessions bundle export --out-dir ./bundles --id <session-id>
```

默认路径与桌面端相同：Codex 读取 `~/.codex`，Claude Code 读取 `~/.claude`，OpenCode 读取 `~/.local/share/opencode/opencode.db`。CLI 子命令可以用 `--codex-dir`、`--claude-dir` 和 `--opencode-dir` 指定其他位置；`--opencode-dir` 作用于交互菜单与 `stats`，Web UI 的 OpenCode 路径仍在设置页修改。在 Windows 中读取 WSL 的 Codex 数据时，`--codex-dir` 可使用 `\\wsl.localhost\<发行版>\home\<用户>\.codex` 形式的 UNC 路径。

`list`、`search` 和 `projects` 默认只处理主会话。加入 `--subagent` 后只处理子代理会话。`list` 和 `search` 支持 `--sort size`，会按 token 从小到大排序。

`preview` 默认使用 `--mode conversation`，输出用户消息和每轮最终答复。`--summary` 输出一行摘要，`--raw` 输出筛选后的原始 JSONL。`--all` 或 `--limit 0` 会读取到文件末尾，`--mode all` 则保留过程消息、工具事件和元数据。

`convert` 使用 `--provider` 指定来源，目标自动取另一端。默认的 `--mode simple` 用于稳定续聊，`--mode native` 会按实验格式转换工具事件。加入 `--json` 可获得机器可读输出。

### CLI Web UI

CLI 可以启动内置 Web UI：

```bash
cc-sessions webui --host 127.0.0.1 --port 17888
```

服务默认绑定 `127.0.0.1`，只接受本机连接。启动时会生成一次性 API token，并写入当前返回的页面。浏览器调用本地 API 时必须携带这个 token。

Web UI 会保存设置。官方 CLI 便携包包含 `cc-sessions.portable`，配置文件位于可执行文件旁的 `cc-sessions-webui-settings.json`。没有该标记的构建会使用系统用户配置目录下的 `cc-sessions/cc-sessions-webui-settings.json`。

环境变量 `CC_SESSIONS_WEBUI_SETTINGS` 可以指定配置文件位置。首次启动会创建文件，以后沿用页面中保存的 Codex、Claude 与 OpenCode 数据路径。只有在启动命令中明确传入 `--codex-dir` 或 `--claude-dir` 时，命令行路径才会覆盖并写回配置；OpenCode 路径在设置页修改。配置目录不可写时，保存会报错。

`--provider codex|claude|opencode` 决定根路径 `/` 默认打开哪组会话，例如：

```bash
cc-sessions --provider claude webui --host 127.0.0.1 --port 17888
cc-sessions --provider opencode webui --host 127.0.0.1 --port 17888
```

在 WSL2 中启动后，Windows 通常可以通过 `http://localhost:17888` 访问。如果 localhost 转发不可用，可在 WSL 内查看 IP：

```bash
hostname -I
```

然后绑定所有网卡：

```bash
cc-sessions webui --host 0.0.0.0 --port 17888
```

绑定 `0.0.0.0` 后，局域网内的其他设备也可能访问服务。Web UI 没有账号登录，只应在确实需要通过 WSL IP 访问时使用。

浏览器版 Web UI 与桌面版使用同一套页面。桌面版会打开系统文件对话框，浏览器版则要求手动输入路径。路径以运行 `cc-sessions webui` 的环境为准；如果服务运行在 WSL 中，就要填写 WSL 可访问的路径。

### CLI 修复项说明

CLI 和桌面版的修复功能只处理 Codex 本地索引及可见性，不改写会话正文，也不能恢复已经删除的 JSONL 文件。

- `修复 session_index.jsonl`：扫描 `~/.codex/sessions/` 中仍存在的 active rollout，重建 `session_index.jsonl`。适用于会话文件存在但索引缺失的情况。
- `重建 threads 表`：根据 rollout 元数据更新 `~/.codex/state_5.sqlite` 中的 `threads` 表，修复列表、搜索、标题或工作目录记录缺失的问题。
- `清理 orphan 记录`：删除 `session_index.jsonl` 或 `threads` 表中指向不存在 rollout 的记录，不会删除有效会话文件。
- `清理 family 残留`：移除指向不存在 rollout 的分支；在只有一个现存 active 候选时恢复 `active_id`，并以 `active_id` 为准校正重复的 active 标记。它不会删除仍存在的 rollout，但会更新 `session_family.json` 和相关索引；建议先 dry-run 查看报告。
- `克隆会话到 provider` / `批量克隆到当前 provider`：处理切换 Codex `model_provider` 后，历史会话 provider 与当前配置不一致的问题。
- `从事件创建回溯分支`：从稳定事件复制新分支，并归档原 active 分支。执行前需要确认。
- `Claude GUI 会话列表修复`（`repair claude-gui [--fix] [--dry-run]`）：Claude Code GUI（例如 VS Code 插件）读取会话文件头尾各 64KB 来推导标题。推导失败时，会话可能从历史列表中消失，但 `claude --resume` 不受影响。修复操作会在 JSONL 末尾补写 `custom-title`，不会改动原有记录。

## 发布

以下文件的版本号必须一致：

- `package.json`
- `src-tauri/Cargo.toml`
- `src-tauri/tauri.conf.json`

推送 `v0.5.0` 形式的 tag 会触发 GitHub Actions，并创建 Release：

```bash
git tag -a v0.5.0 -m "v0.5.0"
git push origin main
git push origin v0.5.0
```

工作流分别为 Windows、macOS Apple Silicon、macOS Intel 和 Linux 构建 Tauri 产物。macOS 打包需要 `src-tauri/icons/icon.icns`，仓库中已包含 Tauri 生成的跨平台图标。

Windows Release 还会上传 `cc-session-manager-portable-v版本号-windows.exe`。便携 ZIP 包含 `cc-session-manager.portable` 标记。应用内更新会关闭程序，在当前目录替换可执行文件后重新启动。NSIS/MSI 安装版会继续使用当前安装目录。

Release 同时上传各平台的 `cc-sessions-cli-v版本号-平台.zip`。CLI 包不依赖桌面环境，macOS 文件名会区分 `macos-arm64` 和 `macos-intel`。CLI 与桌面安装包位于同一个 GitHub Release。

## 手动打包

生成源码包：

```bash
npm run package:source
```

生成便携包：

```bash
npm run package:portable
```

在 Windows 上，该命令会生成便携 ZIP 和可直接运行的 `cc-session-manager-portable-v版本号-windows.exe`。

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

从 GitHub Release 下载的 macOS 应用可能被 Gatekeeper 阻止，需要移除 quarantine 扩展属性：

```bash
# 移除 .app 包的隔离标记
xattr -d com.apple.quarantine "/Applications/CC Sessions.app"
```

使用便携包中的独立二进制文件时，还要赋予可执行权限：

```bash
chmod +x cc-session-manager
xattr -d com.apple.quarantine cc-session-manager
```

## 特别感谢

[linux.do](https://linux.do)：真诚、友善、团结、专业，共建你我引以为荣之社区。

[codex-session-cloner](https://github.com/goodnightzsj/codex-session-cloner)：参考了修复和会话导出导入的代码。

[thful](https://github.com/thful)：参与 Markdown 导出功能测试并反馈问题。

[firesahc](https://github.com/firesahc)：为对话预览、时间线和过程消息交互提供建议，并持续参与测试和反馈。

L站用户 @fengtang：参与会话编辑、删除和官方归档会话相关功能测试并反馈问题。

## License

MIT
