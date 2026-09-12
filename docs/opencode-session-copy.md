# OpenCode 会话复制：依据与验证

## 官方依据

基准为 OpenCode **v1.18.30**（2026-09-09 发布），固定提交 `5cd8e68fdd72b27818d26d168b9c7a06b359567e`。

- [官方 Server 文档](https://opencode.ai/docs/server/)：`POST /session/:id/fork`，请求参数为可选的 `messageID`。
- [Session.fork / createNext / getForkedTitle](https://github.com/anomalyco/opencode/blob/5cd8e68fdd72b27818d26d168b9c7a06b359567e/packages/opencode/src/session/session.ts)：创建独立会话、遍历消息、生成新 ID、重映射引用、递增标题编号。
- [V1 消息与内容块读取](https://github.com/anomalyco/opencode/blob/5cd8e68fdd72b27818d26d168b9c7a06b359567e/packages/opencode/src/session/message-v2.ts)：消息按 `time_created, id` 排序；同一消息的内容块按 `id` 排序。
- [V1 数据与事件定义](https://github.com/anomalyco/opencode/blob/5cd8e68fdd72b27818d26d168b9c7a06b359567e/packages/schema/src/v1/session.ts)、[ID 生成规则](https://github.com/anomalyco/opencode/blob/5cd8e68fdd72b27818d26d168b9c7a06b359567e/packages/schema/src/identifier.ts)。
- [SQLite 会话表结构](https://github.com/anomalyco/opencode/blob/5cd8e68fdd72b27818d26d168b9c7a06b359567e/packages/core/src/session/sql.ts)、[事件投影与用量累计](https://github.com/anomalyco/opencode/blob/5cd8e68fdd72b27818d26d168b9c7a06b359567e/packages/core/src/session/projector.ts)。
- [事件日志表](https://github.com/anomalyco/opencode/blob/5cd8e68fdd72b27818d26d168b9c7a06b359567e/packages/core/src/event/sql.ts)、[事件类型的版本后缀](https://github.com/anomalyco/opencode/blob/5cd8e68fdd72b27818d26d168b9c7a06b359567e/packages/schema/src/event.ts)、[Windows 路径的存储和读取转换](https://github.com/anomalyco/opencode/blob/5cd8e68fdd72b27818d26d168b9c7a06b359567e/packages/core/src/database/path.ts)。

## 用户操作与官方语义

「复制会话」复制来源的全部消息。「复制到此处」包含所选内容所属的**整条消息及其全部内容块**；不会在文字、思考、工具等内容块之间截断一条消息。

官方 fork 的 `messageID` 截止点是**不包含式**的。因此界面的「复制到此处」相当于把**下一条消息**作为官方截止点；所选消息已经是末尾时，相当于不传截止点。不能仅按 ID 大小判断前后，官方测试专门覆盖了时间排序与 ID 字典序不同的情况。

两种操作都保留来源，新会话没有 `parent_id`，使用来源项目和目录，并保留 `workspace_id`、`metadata`。标题按官方规则生成 `标题 (fork #1)`；已有末尾编号时递增。会话、消息、内容块均获得新 ID，助手 `parentID` 以及 compaction 的 `tail_start_id` 按官方可用的已复制消息映射更新。无法映射的 compaction 引用移除；助手的无法映射引用沿用官方保留原值的行为。

文字、思考签名、工具参数、结果、调用 ID、附件等负载保留，避免修改模型所需的不透明内容。不会递归替换负载中碰巧与消息 ID 相同的字符串。副本不继承来源的待办、分享凭据、权限、撤销、归档、待处理输入、子会话或会话级 agent/model 设置；消息自身的 agent/model 保留。

## 离线实现与边界

- 无需安装或启动 OpenCode。读取用户配置目录下现有的 `opencode.db`，不创建数据库、不运行 schema 迁移。
- 只支持 V1 `session` / `message` / `part` 格式。若来源在 V2 `session_message` 表中有记录，拒绝复制，避免只取到部分历史。旧版 `storage/` JSON 文件不在范围内。
- 使用进程锁和 SQLite `IMMEDIATE` 事务；来源快照、截断位置验证与新记录写入处于同一事务。任何失败回滚整个副本。
- 定位符须匹配会话和配置目录，数据库文件不可为链接或 junction。验证核心列、必填扩展列、JSON 结构以及内容块归属。截止点同时校验预览索引、消息 ID 和内容块 ID；失效时明确报错。官方未知截止 ID 会复制全量，本应用不会沿用该退化行为。
- 有 `event` / `event_sequence` 时写入全新的 V1 创建、消息更新和内容块更新日志，序号从 0 开始，不复制来源日志或 owner。缺少一张日志表、或表结构不支持时拒绝复制；旧 schema 没有这两张表时只写会话数据。
- 费用和 token 计数按保留的 `step-finish` 内容块重算；消息保留原创建时间，副本会话和内容块使用当前时间。
- 离线复制保留来源的 `version` 字段，避免冒充本机已安装的 OpenCode 版本。官方运行时会写自己的安装版本。slug 使用独立的随机名称，官方使用随机词组。
- 离线复制固定在来源项目、目录和工作区。官方 API 使用调用实例的项目和目录；在同一项目目录调用时结果一致。不会像运行中的官方实例那样更新工作区的最近使用时间或广播事件，已打开的 OpenCode 客户端可能需要刷新会话列表。

## 验证

`src-tauri/tests/fixtures/opencode-fork.json` 为合成数据，包含消息 ID 排序跨越、多内容块助手消息、带不透明字段的思考和工具、图片、compaction 和用量。没有使用真实用户对话。

Rust 测试覆盖完整复制、6 个内容位置的包含式复制、过期截止点、引用重映射、来源与其他会话及共享表保持原样、异常 JSON、跨会话内容块、新旧 schema、空会话以及中途 SQLite 失败回滚。

差分脚本启动隔离的 CC Sessions Web UI 和官方 OpenCode 1.18.30，无模型请求。通过真实 HTTP 入口分别创建副本，比较 SQLite 会话、消息、内容块、用量、事件日志和序号；只归一化随机 ID、slug、当前时间和 JSON 空白。还通过官方消息读取接口读取本应用生成的副本。

```powershell
npm run build
cargo build --manifest-path src-tauri/Cargo.toml --no-default-features --bin cc-sessions
python scripts/check-opencode-fork-parity.py --opencode <官方1.18.30可执行文件路径>
```

Windows 上已验证 8 种情况：完整复制、首条消息、同一助手消息的文字/思考/工具三个位置、compaction 后、末条消息、空会话。SQLite 数据和新事件日志均与官方结果一致，来源保持原样。复制带有 ` (fork #2)` 的标题时，双方都生成 ` (fork #3)`。
