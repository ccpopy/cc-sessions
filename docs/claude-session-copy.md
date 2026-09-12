# Claude 会话复制的实现依据

对应 [issue #45](https://github.com/ccpopy/cc-sessions/issues/45)。桌面版和 Web UI 提供两个入口：会话卡片的「复制会话」复制完整主对话；预览消息上的「复制到此处」复制开头至所选消息，**包含所选消息**。新副本保存在原项目中，标题添加 ` (fork)`，原会话保留。可使用副本卡片上的继续对话命令打开。

## 官方依据

- [Claude Code CLI reference](https://code.claude.com/docs/en/cli-reference)：`--fork-session` 的定义是恢复会话时创建新会话 ID，配合 `--resume` 或 `--continue` 使用。
- [Anthropic Python Agent SDK 的 `fork_session` 和 `_build_fork_lines`](https://github.com/anthropics/claude-agent-sdk-python/blob/f101a76aed20655fde8e2c67cd1002bd7e700c3a/src/claude_agent_sdk/_internal/session_mutations.py#L240)：明确支持离线复制、按消息 UUID 包含式截取、重建 UUID 与父链，并说明副本不继承文件撤销历史。
- [同一提交的官方测试](https://github.com/anthropics/claude-agent-sdk-python/blob/f101a76aed20655fde8e2c67cd1002bd7e700c3a/tests/test_session_mutations.py#L600)：覆盖新会话、UUID 重映射、包含式截取、标题、来源字段和清除旧状态。

核对时将 SDK 源码固定在提交 `f101a76aed20655fde8e2c67cd1002bd7e700c3a`，避免用可变分支作为格式依据。CC Sessions 在 Rust 中实现相同的存储转换，不需要安装 Python SDK，也不调用模型。

## 数据规则

1. 复制带 UUID 的 `user`、`assistant`、`attachment`、`system` 主链记录；`isMeta` 记录可以连接父链，需保留。排除 `isSidechain` 子代理记录。
2. 为会话和消息分配新的 UUIDv4。重建 `parentUuid`，跳过仅用于 UI 的 `progress` 祖先；缺失父节点置空。重映射压缩记录的 `logicalParentUuid`。
3. 保留消息内容和其他数据值，不递归替换工具 ID、模型消息 ID、签名思考或图片内容。每条消息添加 `forkedFrom`，指向来源会话和消息。
4. 清除顶层 `teamName`、`agentName`、`slug`、`sourceToolAssistantUUID`。不复制 `progress`、文件撤销快照、任务队列、标签、子代理目录或 companion 文件。
5. 末条保留消息使用当前时间；其他已有消息时间保持不变。汇总来源会话的 `content-replacement.replacements` 并写入新会话，最后写入新 `custom-title`。标题优先使用原自定义标题、AI 标题，再回退到首条普通用户消息。
6. 与 SDK 一样，截取副本仍保留来源会话的内容替换记录；这类记录是恢复内容所用的元数据，不按对话截止行过滤。

## CC Sessions 的边界检查

只接受配置的 `projects/<project>/<session-id>.jsonl` 主会话普通文件。精确路径与 ID 同时定位来源，拒绝子代理路径、链接/junction 和目录外文件。按消息复制时同时核对预览的物理行号和消息 UUID，避免文件变化后复制到错误位置。用户/助手消息（包括工具和思考消息）可以作为截止点，内部元数据不提供截止入口。

有意比 SDK 更严格的部分：无效 UTF-8、损坏 JSON 和 progress 父链循环会报错，不跳过损坏行继续复制。写入独立临时文件后再次核对来源指纹，发现并发变化则清理临时文件并提示重试；最终通过原子、不覆盖的方式发布副本。原会话、任务和附属文件均不改写。

## 验证

`src-tauri/tests/fixtures/claude-fork.jsonl` 是合成样例，包含工具调用与结果、签名思考、图片、progress、isMeta、压缩回指、子代理和内容替换元数据，不含真实用户对话。Rust 测试验证完整复制、包含式截取、父链、原始内容、独立 ID、列表发现及失败不落盘；前端测试验证可选消息边界，并保留 Codex 原有行为。

运行 `cargo test --manifest-path src-tauri/Cargo.toml --lib claude_fork::tests` 和 `npm run test:frontend` 可复查核心行为。格式兼容性依据为上述 SDK；这些测试不等同于向 Claude 服务发送真实续聊请求。

`scripts/check-claude-fork-parity.py` 还会在临时目录启动真实 Web UI 后端，将完整复制及 4 个消息截止点的输出与官方 SDK 的纯转换函数进行比较。比较仅归一化随机 UUID 和新生成的时间戳，同时检查 UUID 格式、父链目标、旧时间戳保留、来源不变及过期截止点报错。5 组对照均通过。

复查步骤：先运行 `npm run build` 和 `cargo build --manifest-path src-tauri/Cargo.toml --no-default-features --bin cc-sessions`；下载上述固定提交的原始 `session_mutations.py`，然后运行：

```sh
python scripts/check-claude-fork-parity.py --sdk-source /path/to/session_mutations.py
```

脚本使用 Python 标准库，核对 SDK 源码 SHA-256 后仅载入两段纯转换函数及其类型常量。所有接口请求都发给临时的本机服务；测试数据、配置和生成文件会在退出时清理。
