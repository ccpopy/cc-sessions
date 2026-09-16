# v0.6.8 性能与可靠性整改记录

日期：2026-09-16。基线：`2959e480cd239d173159666e21d4cc2b703d89e4`。

本文保留第一轮的实现与验证记录。按目录锁、写入凭据、磁盘补偿快照和扫描复用的后续进展见 [第二轮整改记录](cc-sessions-v0.6.8-remediation-round2.md)。下文“本轮”及未完成清单均指第一轮结束时的状态。

依据 `cc-sessions-v0.6.8-review-v2.md` 和 `cc-sessions-v0.6.8-review-pack/`，本轮处理现有功能，不加入 Handoff MVP。保留现有 Tauri/Rust、React/TypeScript、原子文件写入及后台任务入口；不增加外网服务或遥测。

## 本轮验收结果

1. 普通进入/刷新修复页只执行总览和服务商检查，归档正文的哈希工作量为零；深度检查可显式执行，显示上次检查时间。
2. 深度校验的正文读取不持有 family 写锁；family 快照过期或被校验文件在读取期间变化时拒绝发布结果。
3. 编辑继续使用已验证字节的指纹；导出拒绝覆盖来源，并保留导出期间发生的外部目标更新。
4. 普通会话批量同步共用一个观察窗口，逐条复核；分页批次和预演不等待该窗口。
5. 默认 Codex/Claude Markdown 文件导出及 ZIP 文件打包按流处理，WebUI 长检查不再占用唯一请求处理线程。

## 已落地内容与边界

| 审查项 | 本轮实现 | 适用范围与限制 |
| --- | --- | --- |
| PERF-01 | 项目配置、family 完整性/残留预演、归档来源改为显式检查；普通刷新保留结果；相关写操作使结果失效 | 缓存仅在当前修复页挂载期间保留；重新进入页面提示尚未检查。旧结果只用于展示，修复命令仍重新检查实际数据 |
| PERF-02 | 短锁读取 family 快照，锁外逐文件校验，短锁确认 family 版本；同轮重复路径复用摘要 | 校验文件长度、修改时间和文件身份；读取失败保留错误信息。单条 provider 修改仍沿用既有事务锁范围 |
| PERF-03 | 补偿快照以读取到的字节生成指纹，去掉最初一次重复的全文读取 | 保留最终来源复核；尚未引入 writer receipt、磁盘快照替代 Vec 或持久化跨存储恢复协议 |
| PERF-04 / R2 | 编辑比较令牌直接来自通过 expected_hash 检查的字节 | 外部追加和等长替换均拒绝旧计划；没有以 mtime/size 替代最终指纹检查 |
| PERF-05 | 批量计划和观察等待移到 family 锁外；共用一个窗口，每条独立令牌和结果 | 单条采样失败或来源变化不会中止其他目标；分页来源改成普通来源时要求重新规划；原生分页派生沿用既有入口 |
| PERF-07 | 默认 Codex/Claude 文件导出单遍读取 JSONL，正文临时落盘，再原子发布；文件结果不回传完整 Markdown；ZIP 使用 64 KiB 块计算 CRC 并写出；逐行改写使用 BufWriter 并显式 flush | 带工具/推理的 Markdown、OpenCode/Cursor 导出以及复制到剪贴板仍走原有内存渲染；没有宣称全部导出已常量内存 |
| PERF-08 | 保留原子写入和恢复数据的 sync_all | 没有全局删除 fsync，也没有更改第三方 SQLite 的 synchronous 设置 |
| PERF-09 | WebUI 使用 4 个工作线程和容量 8 的队列；现有状态、取消搜索和版本请求保留短路径；队列满返回 503 | 尚未把所有导出、导入、校验改成可取消任务；provider 同步原有接口不支持取消 |
| R1 / R5 | WebUI 仅允许回环监听；验证 Host、Origin 和跨站请求；正式资源不再来自 cwd；静态资源不能越过受信任根；增加 nonce CSP 等响应头 | SSH 隧道仍可使用；`CC_SESSIONS_WEBUI_DIST` 是显式的受信任资源覆盖；不提供局域网直接监听或账号平台 |
| R3 | Unix 从空锁实现改为 flock；Windows/Unix 跨进程锁都设置 30 秒超时；Unix 锁提供 PID 诊断 | 仍为应用级共享锁，未按数据源根目录拆分；不能协调不遵守此锁的原生程序，也没有解决 POSIX 旧描述符继续写入问题 |
| R6 | `.md`/`.markdown` 输出限制；拒绝来源同文件、硬链接及邻近已知数据库/索引/journal 别名；原子发布；在开始渲染前固定目标版本 | 文件输出不再使用 fs::write 直接覆盖；此检查不是所有第三方输入文件的通用授权系统 |
| R7 | Unix 原子写入临时文件从创建起使用 0600，替换前保留来源 mode；family/ZIP 暂存也使用 0600 | 未在当前 Windows 环境执行 Unix 权限/进程锁测试；没有声称保留全部平台 ACL |
| R8 | 集中校验续聊 ID 和 cwd；拒绝选项前缀、shell 语法和控制字符；WebUI 调用后端构造器 | 安全 ASCII ID 保持原有命令格式；目录继续使用对应平台的既有转义。异常历史 ID 不生成可执行命令 |
| 资源限制 | 图片预览 20 MiB 上限且先检查文件类型；WebUI 请求体 8 MiB 上限；流式 JSONL 单条记录 64 MiB 上限 | 超限明确报错；没有无提示截断会话正文 |

Codex Core 修复与 Desktop 私有项目状态的职责不变；本轮没有新增从 rollout cwd 推断或创建 Desktop 项目的行为。

## 本地工作量计数

设置 `CC_SESSIONS_PROFILE` 为本机 JSONL 输出路径后，记录总览、family 深度校验、补偿快照、批量同步的耗时及相关计数。默认关闭。输出不包含会话 ID、会话路径、正文或令牌，不上传数据。

字段：`operation`、`elapsed_us`、`counters.read_bytes`、`hash_bytes`、`observation_windows`、`lock_wait_us`、`lock_hold_us`。

**这些是已插桩调用路径的逻辑工作量，不是整个进程的磁盘 I/O。** 例如总览仍读取身份头和数据库，但这些读取未计入 read_bytes；其零 hash_bytes 说明没有走已插桩的归档正文摘要路径，不能解释成完全不读磁盘。嵌套操作累积到当前线程的外层测量，后台线程单独记录；目前没有跨线程 operation_id、RSS 或 fsync 分阶段统计。

## 验证证据

### 回归和构建

- 先复现失败，再修正：编辑基线漂移、导出覆盖来源/硬链接、文件导出冗余正文、异常续聊参数、深度校验长锁、WebUI 非回环暴露、分页批次多余等待、单条采样失败影响整批、导出期间目标更新。
- `cargo test --manifest-path src-tauri/Cargo.toml --lib review_ -- --nocapture`：31 项通过。该过滤器也匹配部分既有 `preview_` 测试，不应把它们全部计为新增测试。
- 定向 Rust 测试：127 项通过，覆盖 atomic_file、edit、family、fs_ops、markdown_export、mutation_journal、codex_activity、webui、provider 同步、ZIP/打包及分页批次。
- `npm run test:frontend`：72 项通过。
- `npm run build`：通过；仍有既有的 charts chunk 超过 500 kB 的 Vite 提示。
- `cargo build --manifest-path src-tauri/Cargo.toml --no-default-features --bin cc-sessions`：最终 CLI 调试构建通过；同参数的 `cargo check` 也通过。
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`、`git diff --check`：通过。

127 项定向测试使用 Cargo 生成的 lib 测试程序，过滤器为：

```text
atomic_file::tests edit::tests family::tests fs_ops::tests markdown_export::tests
mutation_journal::tests codex_activity::tests webui::tests provider_
tests::zip_ tests::pack_ review_paginated_batch
```

Unix 已补 0600 权限回归，以及子进程争锁、超时和释放后重新获取的回归。当前宿主是 Windows，WSL 没有 Rust 工具链，这些 cfg(unix) 测试未运行；不能以 Windows 测试通过替代它们。没有执行全量慢测试、跨平台打包、远程 CI 或 Release 流程。

### 修复页与 WebUI 实测

使用隔离的人工数据目录：1 条 active 会话、1 条 sealed 分支；sealed 文件为 3,270,370 字节、10,002 行。使用 CLI 调试构建和构建后的前端，未对用户真实会话进行修复或基准写入。

| 操作 | 观测 |
| --- | --- |
| 首次进入、普通刷新 | 仅总览和 provider 请求；3 次总览的 hash_bytes 均为 0 |
| 点击深度校验 | 1 条 sealed 分支通过；read_bytes/hash_bytes 均为 3,270,370，恰好一遍；检查 75,298 µs，写锁累计 585 µs |
| 深度检查后普通刷新 | 保留上次检查时间和通过结果，不重新调用深度检查 |
| 点击项目配置/归档来源 | 对应检查仅在点击后调用，并分别记录时间 |
| 四个深度检查与版本请求并发 | 全部 HTTP 200；版本请求约 64 ms 完成，深度检查约 136–141 ms 完成 |
| 760 px 窄窗口 | 页面宽度仍为 760 px，检查按钮和结果没有造成页面横向溢出 |
| 来源保持不变 | 人工 active/sealed JSONL 的测试前后 SHA-256 完全一致 |

这是结构性烟雾验证，**不是 release 性能基准或加速比例承诺**。快照计数测试另验证：1 MiB 来源的 capture 产生 2 MiB 读取、2 MiB 哈希输入，比旧 capture 少一遍；不包含修改后其他阶段的成本。批量测试验证窗口数量，不把 cfg(test) 中的无 sleep 耗时当作生产速度。

本地 QA 产物位于忽略目录 `output/review-validation/`，截图位于 `output/playwright/repair-explicit-checks.png` 和 `repair-narrow.png`。浏览器初次列表报错来自人工 SQLite fixture 缺列，补齐后列表和修复流程通过；另有本机 AdGuard 注入脚本连接失败，与应用请求无关。

## 后续仍需独立完成

1. **R4：持久化跨存储恢复。** 当前 MutationJournal 仍是内存补偿，writer receipt 的写入归属问题也尚未解决。需要以明确的文件/SQLite 事务边界实现恢复记录，再进行 kill/restart 故障节点验证；不能把本轮减少读取说成已实现崩溃恢复。
2. **R3：原生写入协调和按根目录的锁。** flock 只协调 CC Sessions；POSIX 校验到 rename 的窗口、已打开旧 inode 的原生写入、不同数据源相互阻塞仍需处理。
3. **PERF-06：请求/批次扫描复用及局部复核。** 本轮保留已有目标路径复用和头部扫描；未新增 ScanContext、追加解析或持久化摘要缓存。部分单条成功分支仍会全表/全索引复核。
4. **PERF-07 / PERF-09：其余来源流式导出与任务取消。** 工具/推理内容的归属必须保持既有语义，再推进有界读取；导出、导入、深度检查仍缺独立取消接口。
5. **退出/更新屏障与平台验证。** 需与持久化恢复协议一起完成，不应以固定退出延迟代替安全提交边界。Linux/macOS 原生写入、权限和多进程行为仍需在对应平台验证。

这些条目未标为完成。本轮没有修改版本、创建提交、推送或发布，也没有修改原始审查资料。
