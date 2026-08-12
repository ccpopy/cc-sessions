# CC Sessions

[![Version](https://img.shields.io/github/v/release/ccpopy/cc-sessions?label=version&sort=semver)](https://github.com/ccpopy/cc-sessions/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/ccpopy/cc-sessions/total?label=downloads)](https://github.com/ccpopy/cc-sessions/releases)
[![Stars](https://img.shields.io/github/stars/ccpopy/cc-sessions?style=flat)](https://github.com/ccpopy/cc-sessions/stargazers)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey)
[![License](https://img.shields.io/badge/license-MIT-green)](LICENSE)

CC Sessions 用来管理 Codex、Claude Code 和 OpenCode 保存在本机的会话。你可以在一个界面里查找对话、预览内容、备份恢复、移动会话目录，也可以修复部分索引和可见性问题。

[下载最新版](https://github.com/ccpopy/cc-sessions/releases/latest) · [查看功能](#功能模块) · [进阶功能](#进阶功能) · [常见问题](#常见问题) · [开发与打包](#开发与打包)

![CC Sessions 模拟数据截图](img/readme-screenshot.png)

## 适合谁

| 你属于哪类用户 | 推荐方式 | 从哪里开始 |
| --- | --- | --- |
| 在电脑上使用 Codex、Claude Code 或 OpenCode | 桌面版 | [安装](#安装) |
| 在 WSL、服务器或 SSH 环境中管理会话 | `cc-sessions` 命令行或自带网页界面 | [命令行与 WSL](#命令行与-wsl) |
| 想修改源码或自行构建安装包 | 从源码运行 | [开发与打包](#开发与打包) |

会话读取、搜索、编辑和备份都在本机完成。应用检查更新时会访问 GitHub Releases。

## 功能模块

| 模块 | 可以做什么 | Codex | Claude Code | OpenCode |
| --- | --- | --- | --- | --- |
| 会话浏览 | 按标题、首条消息、目录或 ID 查找会话 | 支持 | 支持 | 支持 |
| 会话整理 | 重命名、删除、按项目分组，并复制继续对话的命令 | 支持 | 支持 | 支持 |
| 正文搜索与预览 | 搜索完整对话，按时间线查看消息和工具过程 | 支持 | 支持 | 支持 |
| 编辑与撤销 | 修改文本、删除上下文事件、撤销编辑或恢复原始快照 | 支持 | 支持 | 支持 |
| 备份与恢复 | 为选中的会话创建备份，并在需要时恢复 | 支持 | 支持 | 支持 |
| 导入与导出 | 生成可迁移的会话包，或导出为 Markdown | 支持 | 支持 | 支持 |
| 移动会话目录 | 把会话关联到新的项目目录，并更新相关记录 | 支持 | 支持 | 支持 |
| 归档 | 隐藏暂时不用的会话，之后可以取消归档 | 支持 | 不支持 | 支持 |
| 会话转换 | 在 Codex 与 Claude Code 之间创建新的可续聊会话 | 支持 | 支持 | 不支持 |
| Memory 管理 | 新建、浏览、编辑、重命名和删除项目 Memory 文件 | 不接管 | 支持 | 不支持 |
| 子代理会话 | 单独筛选由子代理产生的会话 | 支持 | 支持 | 支持 |
| 分支管理 | 切换模型服务配置后复制会话，或从较早的对话位置创建新分支 | 支持 | 不支持 | 不支持 |
| 使用统计 | 查看会话趋势、项目、模型和活跃时间 | 支持 | 支持 | 支持 |
| 修复工具 | 修复部分索引、项目配置和列表可见性问题 | 支持 | 部分支持 | 不支持 |

会话转换会创建新的目标会话，不修改来源文件。编辑、导入、移动、恢复和删除会写入本地数据。桌面版会在高风险操作前显示确认信息，CLI 交互菜单会要求输入 `yes`。

## 安装

前往 [Releases](https://github.com/ccpopy/cc-sessions/releases/latest)，按系统和使用方式选择文件。下表中的 `<版本号>` 对应 Release 显示的版本数字，例如 `0.5.3`。

| 系统与用途 | 推荐下载 | 说明 |
| --- | --- | --- |
| Windows 常规安装 | `CC.Sessions_<版本号>_x64-setup.exe` | 推荐大多数 Windows 用户使用 |
| Windows MSI 安装 | `CC.Sessions_<版本号>_x64_en-US.msi` | 适合需要 MSI 的安装环境 |
| Windows 便携版 | `cc-session-manager-portable-v<版本号>-windows.exe` | 下载后直接运行，不创建卸载项 |
| Windows 便携压缩包 | `cc-session-manager-portable-v<版本号>-windows.zip` | 解压后运行，应用更新会替换该目录中的程序 |
| macOS Apple Silicon | `CC.Sessions_<版本号>_aarch64.dmg` | 适用于 M 系列芯片 |
| macOS Intel | `CC.Sessions_<版本号>_x64.dmg` | 适用于 Intel Mac |
| macOS 解压版 | `CC.Sessions_aarch64.app.tar.gz` 或 `CC.Sessions_x64.app.tar.gz` | 根据芯片选择，普通用户优先下载 DMG |
| Debian / Ubuntu | `CC.Sessions_<版本号>_amd64.deb` | 使用系统软件包安装 |
| Fedora / RHEL | `CC.Sessions-<版本号>-1.x86_64.rpm` | 使用系统软件包安装 |
| 其他 Linux 桌面 | `CC.Sessions_<版本号>_amd64.AppImage` | 赋予执行权限后运行 |

命令行版本使用单独的 ZIP 包：

| 系统 | 推荐下载 |
| --- | --- |
| Windows | `cc-sessions-cli-v<版本号>-windows.zip` |
| macOS Apple Silicon | `cc-sessions-cli-v<版本号>-macos-arm64.zip` |
| macOS Intel | `cc-sessions-cli-v<版本号>-macos-intel.zip` |
| Linux | `cc-sessions-cli-v<版本号>-linux.zip` |

Release 最下方的 `Source code (zip)` 和 `Source code (tar.gz)` 是 GitHub 自动生成的源码压缩包，不是桌面版安装包。

第一次打开后，到设置页确认 Codex、Claude Code 和 OpenCode 的数据路径。应用会尝试使用默认位置，没有安装的工具可以留空。

### 快速开始

1. 在顶部选择要管理的工具。
2. 先打开会话列表，确认标题、项目和消息预览正常。
3. 需要修改、迁移或删除会话时，先创建备份。
4. 导入到另一台电脑时，在导入页面检查项目路径映射。

### 更新

所有桌面版都可以检查 GitHub Release。Windows 安装版和便携版还可以校验下载文件并自动安装，便携版会替换当前解压目录中的程序文件。macOS、Linux 和命令行自带的网页界面会提供下载入口，不会自动替换当前程序。

## 数据与安全

- 浏览、搜索和预览不会修改会话。
- CC Sessions 不要求账号，也不会把会话上传到第三方服务。只有检查更新或打开发布页时会访问 GitHub。
- 编辑前会保存快照，可以逐步撤销，也可以恢复到编辑前状态。
- 移动目录会检查目标冲突和写入结果。失败时会尝试恢复原状态。
- OpenCode 会话包只包含所选会话的数据，不包含账号信息、登录凭据或本机分享密钥。
- 跨机器导入时，桌面版和自带网页界面可以把原项目路径映射到新电脑的目录。
- 覆盖导入和覆盖恢复不会自动再创建一份备份。重要会话仍建议保留独立备份，尤其是在移动目录、覆盖恢复或批量删除前。

## 进阶功能

### 预览与编辑

普通预览只显示用户消息和每轮最终答复，避免把运行过程、工具调用和内部上下文混在正文里。需要排查问题时，可以切换到全部事件，查看完整时间线。

用户和助手的普通文本可以修改或删除。Codex 的加密推理和 Claude Code 的签名思考不能改写，只能整段删除。删除工具事件时，应用会同时处理能够确定属于同一次调用的相关记录。

### 归档

Codex 和 OpenCode 支持归档，Claude Code 暂不支持。归档不会删除会话，普通列表会隐藏归档记录，切换到归档视图后可以取消归档。

导出页会保留归档记录供手动选择，并显示“已归档”徽标。Codex 使用本地归档目录保存会话文件，OpenCode 使用自身的原生归档状态。

### 备份、会话包与 Markdown

备份用于在本机恢复误操作，会话包用于迁移到另一台电脑，Markdown 用于阅读、分享或交给其他 AI 作为上下文。Markdown 不能导回原工具成为可续聊会话。

跨机器导入时，桌面版和自带网页界面可以重新指定项目路径。直接使用命令行导入会沿用会话包中的原路径，目录结构不同的电脑更适合使用带界面的导入页面。

### 子代理与 Codex 分支

主会话和子代理会话可以分开查看。列表、搜索、项目分组和大小排序都支持子代理筛选，但子代理会话不参与 Codex 与 Claude Code 的格式转换。

Codex 用户还可以处理切换模型服务配置后留下的旧会话，或从较早的稳定对话位置创建新分支。创建回溯分支时，应用会保留来源会话，并把原来的当前分支归档。

### Codex 与 Claude Code 会话转换

简洁模式是默认选项，只保留用户消息和稳定的最终答复，适合继续对话。原生模式会尝试保留工具调用、图片和过程消息，适合需要完整上下文的场景，但兼容性不如简洁模式稳定。

转换始终创建新会话，不会修改来源文件。OpenCode 暂不参与格式转换。

### 统计

统计页汇总本机的 Codex、Claude Code 和 OpenCode 数据，可以查看会话数量、活跃趋势、常用项目、模型分布和活跃时段。没有安装或没有配置的数据源会按零会话处理，不影响其他工具。

### 修复工具

修复前可以打开“仅预览”，先查看将要发生的变化。

| 工具 | 适用情况 | 会不会改正文 |
| --- | --- | --- |
| 修复会话列表索引 | Codex 会话文件存在，但列表中找不到 | 不会 |
| 重建会话数据库记录 | Codex 列表缺少标题、目录或时间信息 | 不会 |
| 清理无效记录 | 列表指向的会话文件已经不存在 | 不会删除仍存在的会话文件 |
| 清理分支残留 | Codex 分支状态冲突，或当前分支记录丢失 | 不会改写会话正文 |
| 克隆到当前模型服务 | Codex 切换模型服务配置后，旧会话无法直接使用 | 创建副本，不改来源 |
| Claude 列表可见性修复 | Claude Code 能续聊，但会话列表不显示标题 | 会在文件末尾补充标题记录 |

## 命令行与 WSL

命令行版适合 WSL、服务器、SSH 和没有桌面环境的机器。请从 Release 下载[安装部分](#安装)列出的对应系统 CLI 压缩包。

Windows PowerShell：

```powershell
.\cc-sessions.exe
```

macOS 或 Linux：

```bash
chmod +x cc-sessions
./cc-sessions
```

把程序加入 `PATH` 后，可以直接使用 `cc-sessions`。不带子命令时会进入交互菜单。菜单支持翻页、多选、预览、搜索、备份、导入导出、移动目录和修复诊断。

菜单中使用 `n` 和 `p` 翻页，`b` 返回上一层，`m` 返回主菜单，`0` 退出。输入 `s` 可以多选当前页会话，`u` 取消选择，`c` 清空选择，`d` 删除已选会话。序号支持空格、逗号和 `1-3` 形式的范围。

### 常用命令

`--provider` 用来选择 Codex、Claude Code 或 OpenCode，可填写 `codex`、`claude` 或 `opencode`。列表、搜索、项目和统计命令还可以使用 `all` 汇总多个数据源。

| 用途 | 命令 |
| --- | --- |
| 查看完整帮助 | `cc-sessions --help` |
| 查看最近会话 | `cc-sessions list --limit 20` |
| 查看 OpenCode 会话 | `cc-sessions --provider opencode list --limit 20` |
| 只看子代理会话 | `cc-sessions --provider codex list --subagent` |
| 按项目查看并包含归档会话 | `cc-sessions --provider codex projects --archived` |
| 搜索 Claude Code 对话 | `cc-sessions --provider claude search "关键词"` |
| 按项目查看会话 | `cc-sessions --provider codex projects` |
| 预览会话 | `cc-sessions preview <会话文件> --limit 40` |
| 查看完整事件 | `cc-sessions preview <会话文件> --mode all --limit 40` |
| 创建备份 | `cc-sessions backup create --backup-dir ./backups --id <session-id> --name my-backup` |
| 导出会话包 | `cc-sessions bundle export --out-dir ./bundles --id <session-id>` |
| 导入 OpenCode 会话包 | `cc-sessions --provider opencode bundle import --src-dir ./bundles --mode overwrite --strict` |
| 检查 Codex 索引问题 | `cc-sessions repair diagnose --json` |
| 启动本地 Web UI | `cc-sessions webui --host 127.0.0.1 --port 17888` |

CLI 默认读取与桌面版相同的数据位置。需要指定其他目录时，可以使用 `--codex-dir`、`--claude-dir` 或 `--opencode-dir`。

`list`、`search` 和 `projects` 默认显示主会话。加入 `--subagent` 后只显示子代理会话。`list` 和 `search` 支持 `--sort size`，可以按会话大小排序。

`preview` 默认显示对话内容。`--mode all` 会加入过程消息、工具事件和元数据，`--summary` 输出一行摘要，`--raw` 输出筛选后的原始记录，`--all` 或 `--limit 0` 会读取到结尾。需要脚本处理结果时，可以加入 `--json`。

在 Windows 中读取 WSL 里的 Codex 数据：

```powershell
cc-sessions --codex-dir "\\wsl.localhost\Ubuntu\home\me\.codex" list
```

### 自带网页界面

```bash
cc-sessions webui --host 127.0.0.1 --port 17888
```

服务默认只接受本机连接。它会为当前页面生成一次性访问令牌，并保存你在页面中设置的数据路径。

WSL2 通常可以通过 `http://localhost:17888` 访问。如果必须绑定 `0.0.0.0`，请确认当前网络可信。Web UI 没有账号登录，不建议直接暴露到公网。

官方 CLI 包含 `cc-sessions.portable` 标记，设置保存在程序旁的 `cc-sessions-webui-settings.json`。自行构建且没有该标记时，设置会保存在当前系统的用户配置目录。环境变量 `CC_SESSIONS_WEBUI_SETTINGS` 可以指定其他设置文件。

## 快捷键

| 场景 | 快捷键 | 作用 |
| --- | --- | --- |
| 全局 | <kbd>Ctrl</kbd> / <kbd>Cmd</kbd> + <kbd>K</kbd> | 聚焦搜索框 |
| 全局 | <kbd>Ctrl</kbd> / <kbd>Cmd</kbd> + <kbd>B</kbd> | 展开或收起侧边栏 |
| 全局 | <kbd>Ctrl</kbd> / <kbd>Cmd</kbd> + <kbd>Shift</kbd> + <kbd>L</kbd> | 切换明暗主题 |
| 会话列表 | <kbd>Delete</kbd> / <kbd>Backspace</kbd> | 删除已选会话 |
| 会话预览 | <kbd>Home</kbd> | 回到已加载内容顶部 |
| 会话预览 | <kbd>End</kbd> | 到达底部并继续加载 |
| 会话预览 | <kbd>Page Up</kbd> | 向上翻页 |
| 会话预览 | <kbd>Page Down</kbd> | 向下翻页并按需加载 |

输入框、文本框和弹窗打开时，全局快捷键不会触发。预览滚动快捷键只在预览窗口内生效。

## 常见问题

<details>
<summary>CC Sessions 默认从哪里读取会话？</summary>

Codex 默认读取 `~/.codex`，Claude Code 默认读取 `~/.claude`，OpenCode 读取当前安装使用的 `opencode.db`。实际路径可以在设置页查看和修改，CLI 也可以通过目录参数覆盖。

</details>

<details>
<summary>会话包和 Markdown 导出有什么区别？</summary>

会话包用于备份和迁移，可以再次导入 CC Sessions。Markdown 适合阅读、归档或分享文本，不用于恢复原会话。

</details>

<details>
<summary>导出页面为什么比会话列表多一条或多几条？</summary>

导出页面面向备份和迁移，会列出更完整的底层记录。普通列表可能隐藏已归档会话、子代理会话，或把同一组 Codex 分支折叠成一个入口。归档记录会在导出页显示“已归档”徽标，仍可手动勾选并导出。

</details>

<details>
<summary>归档会删除会话吗？</summary>

不会。Codex 会把归档会话放到单独的归档目录，OpenCode 会记录归档时间。取消归档后，会话会重新出现在普通列表中。

</details>

<details>
<summary>OpenCode 本身有归档功能吗？</summary>

有。OpenCode 官方客户端提供归档操作，并把归档时间保存为会话状态。CC Sessions 使用的是这项原生状态，不是另外创建的标签。可以查看 OpenCode 官方源码中的 [归档操作](https://github.com/anomalyco/opencode/blob/dev/packages/app/src/pages/home-session-archive.ts) 和 [会话状态定义](https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/session/session.ts)。

</details>

<details>
<summary>移动会话目录后还能继续对话吗？</summary>

可以。CC Sessions 会同步更新会话和项目之间的关联，并在完成后检查结果。Claude Code 的相关会话文件和历史记录会一起处理，OpenCode 的子会话也会跟随主会话移动。这个功能不会移动你的项目源码，只会调整会话数据。建议移动前先创建备份。

</details>

<details>
<summary>跨机器导入时，项目路径不一样怎么办？</summary>

桌面版和自带网页界面会显示来源路径，可以把它映射到新电脑上的目录。直接使用命令行导入会保留会话包中的原路径，因此路径不同的情况更适合使用带界面的导入页面。

</details>

<details>
<summary>为什么看不到旧版 OpenCode storage 目录里的会话？</summary>

CC Sessions 读取当前 OpenCode 使用的数据库，不会把旧版 `storage/` JSON 与当前数据混在一起。如果旧会话还没有迁入当前 OpenCode，请先用 OpenCode 自身提供的方式处理。

</details>

<details>
<summary>CC Sessions 会管理 Codex Memory 吗？</summary>

不会。Codex 自己负责本地 Memory 的生成和生命周期。CC Sessions 只提供 Claude Code 项目 Memory 的文件管理。Codex 的说明见 [Codex Memories 官方文档](https://learn.chatgpt.com/docs/customization/memories)。

</details>

<details>
<summary>Windows 提示“已保护你的电脑”怎么办？</summary>

请先确认文件来自本项目的 [Releases](https://github.com/ccpopy/cc-sessions/releases/latest)。确认无误后，可以在提示窗口中选择“更多信息”，再选择继续运行；如果文件来源不明，请取消运行。

</details>

<details>
<summary>macOS 提示应用无法打开怎么办？</summary>

如果系统阻止未签名应用，可以在确认文件来自本项目 Release 后移除隔离标记：

```bash
xattr -d com.apple.quarantine "/Applications/CC Sessions.app"
```

</details>

<details>
<summary>修复功能会改写会话正文吗？</summary>

Codex 修复主要处理本地索引和列表可见性，不会重写对话正文，也不能恢复已经删除的会话文件。Claude Code 的列表可见性修复可能会在文件末尾补充标题记录，执行前可以先打开“仅预览”查看报告。

</details>

<details>
<summary>为什么有些推理内容只能删除，不能修改？</summary>

Codex 的部分推理内容经过加密，Claude Code 的部分思考内容带有签名。改写这些数据会让原工具无法识别，因此 CC Sessions 只允许整段删除。

</details>

## 反馈问题

如果遇到无法读取、导入失败或迁移后无法继续对话，请到 [GitHub Issues](https://github.com/ccpopy/cc-sessions/issues) 提交问题，并附上操作系统、CC Sessions 版本、所管理的工具、复现步骤和完整错误信息。会话内容可能含有隐私，上传日志或截图前请先删除敏感信息。

## 开发与打包

### 环境

- Node.js 20 或更高版本（前端 TypeScript 测试由 `tsx` 运行）
- npm
- Rust stable 工具链
- [Tauri 2 对应平台的构建依赖](https://v2.tauri.app/start/prerequisites/)

安装依赖并启动桌面开发环境：

```bash
npm ci
npm run tauri:dev
```

常用检查：

```bash
npm run build
npm run cli:check
cargo test --manifest-path src-tauri/Cargo.toml --no-default-features --lib
```

### 打包

| 产物 | 命令 |
| --- | --- |
| 源码包 | `npm run package:source` |
| 桌面便携包 | `npm run package:portable` |
| 桌面安装器 | `npm run package:product` |
| 独立 CLI | `npm run package:cli` |
| 当前平台全部产物 | `npm run package:all` |

打包结果位于 `release/`，该目录不会提交到仓库。

发布前需要保持 `package.json`、`package-lock.json`、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock` 和 `src-tauri/tauri.conf.json` 中的项目版本一致。推送版本 tag 后，GitHub Actions 会构建 Windows、Linux、macOS Apple Silicon 和 macOS Intel 产物。

```bash
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin main
git push origin vX.Y.Z
```

## 特别感谢

- [linux.do](https://linux.do) 社区提供了讨论、测试和问题反馈。
- [codex-session-cloner](https://github.com/goodnightzsj/codex-session-cloner) 为会话修复和导入导出实现提供了参考。
- [thful](https://github.com/thful) 参与了 Markdown 导出测试并反馈问题。
- [firesahc](https://github.com/firesahc) 为对话预览、时间线和过程消息交互提供建议，并持续参与测试。
- L 站用户 fengtang 参与了会话编辑、删除和归档功能测试。

## Star 历史

[![CC Sessions Star 历史](img/star-history.svg)](https://github.com/ccpopy/cc-sessions/stargazers)

图表根据 GitHub 公开的 Star 时间生成，点击可以查看当前 Star 用户列表。

## License

本项目使用 [MIT License](LICENSE)。
