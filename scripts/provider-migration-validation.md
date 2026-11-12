# Codex 0.167.1 provider 兼容复现与实现

## 结论与范围

Magpie 可作为主要行为参考，但其 DB-only retag 不能直接视为完成迁移。独立运行 Codex 0.167.1 后确认：仅修改 `threads.model_provider` 会被 cc-sessions 的 `repair threads` 按旧历史改回；必须再由 Codex 原生 `thread/resume` 保存目标 provider 的 `thread_settings_applied`，并校验落盘结果。

本补丁只为 **0.167.1、未被加载的 paginated 会话**提供兼容路径，支持普通、zstd 历史及已验证的 history_base 继承链。旧版仍先走原来的原生迁移路径。未知版本、legacy 文件及持有原生 writer lock 的会话不使用新兼容写入。没有修改会话 ID，没有创建迁移分支，没有把 `thread/fork` 冒充原地迁移。

## 固定版本与许可

- cc-sessions 比较基线：[`8bdb72ec40465a8f49434638fdc3c326b42f9363`](https://github.com/ccpopy/cc-sessions/commit/8bdb72ec40465a8f49434638fdc3c326b42f9363)。
- Magpie 参考：[`1f99f6f0b54a0627be425c09af31260705c16857`](https://github.com/yetone/magpie/blob/1f99f6f0b54a0627be425c09af31260705c16857/internal/sessions/codex_provider.go)。许可 MIT，Copyright (c) 2026 yetone；完整声明保存在 `third_party_licenses/magpie-MIT.txt`。
- 原生验证：OpenAI 官方 [`rust-v0.167.1`](https://github.com/openai/codex/releases/tag/rust-v0.167.1)，Linux x86_64 musl。
- 兼容逻辑为独立实现，未移植 Magpie 对已加载会话的热改逻辑。保留 MIT 声明也便于后续参考与审查。

## 独立行为复现

复现脚本创建全新的 CODEX_HOME，以本地 HTTP/SSE mock 返回固定文本。所有会话、配置、备份、锁和续写内容均为合成数据。没有复制认证文件、调用真实模型服务或终止已有用户进程。

| 场景 | 0.167.1 实测结果 |
| --- | --- |
| legacy/plain、legacy/zst | 原生跨 provider resume 返回结构化 mismatch；独立复现 Magpie 的 header + row 修改后可重启续写。此文件改写策略未加入产品补丁。 |
| paginated/plain、paginated/zst | row retag 不改原文件字节；原生 resume 保存目标设置；重建 threads/index 后可冷启动并续写，mock 请求包含原对话。 |
| 不同长度的 provider | paginated row 方案不依赖 provider 名等长。 |
| 已加载 legacy/paginated | 在自建、空闲 app-server 上复现退出前 retag + SIGTERM 后续写成功，但需拥有进程生命周期；产品补丁仍拒绝持锁会话。 |
| 原生 thread/fork | 跨 provider 成功，但生成新 ID，不能代替原地迁移。 |
| 仅 DB retag 后 rebuild | provider 被恢复成旧值，证明必须检查持久化设置。 |

## 实现的安全边界

1. 只接受精确运行版本 `0.167.1` 的 `thread/resume` 结构化 `modelProviderMismatch`，且 from/to 必须吻合；文本相似错误、policy conflict、未知版本均不能触发写入。
2. 保留迁移前完整备份、SHA-256 历史校验与 durable receipt。成功不等于 RPC 返回成功，而是原生重新读取、DB 和 rollout 的有效 provider 一致。
3. 使用 Core 的 writer/coordination lock；持锁后复核 header ID、history_mode、历史与关键元数据。SQLite Immediate 事务只 CAS 更新指定 ID、原 provider、paginated 模式及原路径的唯一一行。
4. 释放锁后让原生 resume 保存合法设置，不伪造事件、不改分页字节偏移。失败且历史/设置未变时只撤销本次 row 更新，不全库回滚。
5. 首个物理 session_meta 的哈希涵盖 base_instructions；最新 owned settings 比较指定权限字段与嵌套对象，不接受其他 thread 的设置替代当前 thread。检查工作目录、来源、sandbox、approval、memory、标题/归档/创建秒等非路由元数据。Core 会把 created_at_ms 对齐到 header 的毫秒时间，所以不将这一已观察到的归一化误判为损坏。首次 resume 也可能从继承历史填充原本为空的自动标题；只允许这种首次填充，保护所有已有非空标题和显式 name。
6. 修复 DB + index 已为目标时直接跳过的误报；只有 rollout 也持久化目标 provider 才可返回 no-op。中断恢复同样不能仅信任 DB/native 返回值。

## 最终验证结果（2026-11-11 UTC）

- 原生 + mock 矩阵：14/14 个场景符合预期，包括安全拒绝的反例。
- 产品正例：plain、zstd、history_base 继承链；备份验证、重建索引/数据库、冷启动、实际续写、恢复原字节、恢复后原生 resume 全通过。继承父文件字节不变。
- Rust `--no-default-features --all-targets`：760 个库测试 + 7 个 CLI 测试通过，9 个既有 ignored 测试未运行。
- 前端 144 个测试通过，TypeScript/Vite 构建通过；Rust 格式与 diff 空白检查通过。
- default-features/GUI 检查未完成：环境缺少 GLib/GTK/WebKit 开发库，不能宣称整个跨平台 CI 通过。

## 重跑

需要已有 Codex 0.167.1、Python 3.10+ 与 `zstandard`，以及构建好的 CLI：

```sh
cargo build --manifest-path src-tauri/Cargo.toml --no-default-features --bin cc-sessions
python scripts/validate-provider-migration.py \
  --codex /path/to/codex-0.167.1 \
  --cc-sessions src-tauri/target/debug/cc-sessions \
  --output /tmp/provider-validation-new
```

输出目录必须不存在。脚本包含原生复现、产品正/反例、备份校验、真实恢复原始历史和恢复后的原生 resume，生成 `report.json` 与逐场景 RPC 证据。Linux 的 loaded-reference 测试只向脚本自己创建的空闲子进程发送 SIGTERM。配置 `project_doc_max_bytes=0` 仅避免合成测试扫描用户工程说明，未放宽 sandbox。

## 尚未覆盖和保留限制

- 本次不宣称所有 Codex 版本通用；新兼容路径只放行 0.167.1。
- legacy 头部改写虽完成行为复现，但未加入产品，仍需单独的跨文件事务/恢复方案。
- 不热改真实已加载会话，不自动关闭或重启用户的 Codex App。
- CAS 完成后、原生 resume 前被强制终止会保留 prepared receipt；下一次操作阻断并要求核对/恢复，不把不一致状态误报完成。
- 未测试真实 provider 鉴权、实际模型、Desktop GUI 冷启动、macOS 或 Windows 完整迁移。Windows 0.167.1 仅完成启动与无 turn 的生命周期探测。
- 其他不遵循 Core writer lock 的外部 DB 编辑仍可能竞争；后验校验会阻断成功声明，不执行任何用户 turn。
- GUI/default-features Rust 检查受当前 Linux 缺少 GLib/GTK/WebKit 开发库限制，不能等同于跨平台 CI 全绿。

以上为补丁开发阶段的验证记录；发行提交的跨平台 CI 与构建状态请以对应 GitHub Actions 运行结果为准。

## Issue #58 后续：Desktop originator 与分离数据库

带空格的 `Codex Desktop/0.167.1` 现在按 `/` 分隔产品名后再读取版本，避免先按空白拆分导致版本丢失。`0.167.0-alpha.4.1` 会被完整识别，但仍不满足兼容写入门禁。

可在合成数据中复现桌面运行环境，并让 SQLite 位于数据根之外：

```sh
python scripts/validate-provider-migration.py \
  --codex /path/to/codex-0.167.1 \
  --cc-sessions src-tauri/target/debug/cc-sessions \
  --output /tmp/provider-desktop-split-new \
  --originator 'Codex Desktop' --split-sqlite
```

两项参数均可省略；脚本仍会清除其他继承的 Codex 配置与认证环境。单独启用 Desktop originator、同时启用分离 SQLite 的两组矩阵分别 14/14 通过，包括普通/压缩/继承历史的迁移、重建、冷加载、模拟续写和备份恢复，以及 legacy/持锁场景的预期拒绝。此验证不代表已经在报告者的 Windows/WSL 桌面中验收。
