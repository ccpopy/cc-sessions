//! Cursor 会话的只读接入。
//!
//! Cursor 在本机有两套互不相干的会话存储，本模块统一成一个 provider 对外呈现：
//!
//! - **IDE Composer**：`<cursor_dir>/globalStorage/state.vscdb`，绝大多数会话都在这里。
//!   `composerHeaders` 表是权威列表，消息体散在 `cursorDiskKV` 的
//!   `composerData:<id>`（有序气泡索引）与 `bubbleId:<会话>:<气泡>`（消息本体）里。
//! - **cursor-agent CLI**：`~/.cursor/chats`，由 [`crate::cursor_agent_store`] 解析。
//!
//! `~/.cursor/projects/*/agent-transcripts/*.jsonl` 是同一批会话的展示副本，既没有工具
//! 结果也没有时间戳，并且实测独有会话数为 0，因此刻意不读。
//!
//! 这个库单文件可达数 GB，列表页只查 `composerHeaders`（有索引，毫秒级），气泡一律等到
//! 预览、搜索或迁移时按主键点查。**任何对 `cursorDiskKV` 的范围扫描都不可接受**：实测
//! 全量扫一遍 `bubbleId:` 前缀要 23 秒。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::models::{PreviewEvent, SessionMetaBrief, SessionSummary, UserPromptList};
use crate::paths;

pub(crate) const PROVIDER: &str = "cursor";
const LOCATOR_PREFIX: &str = "cursor:";
const TITLE_MAX_CHARS: usize = 120;

/// `composerHeaders` 里 `type` 字段的取值。
const BUBBLE_TYPE_USER: i64 = 1;
const BUBBLE_TYPE_ASSISTANT: i64 = 2;

/// 会话定位符。Cursor 与 OpenCode 一样，`SessionSummary.rollout_path` 不是文件路径。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionLocator {
    /// `composer` = IDE 的 state.vscdb；`agent` = cursor-agent 的会话目录。
    pub store: String,
    /// composer 存 state.vscdb 的路径，agent 存会话目录的路径。
    pub path: String,
    pub session: String,
}

impl SessionLocator {
    pub fn is_agent(&self) -> bool {
        self.store == "agent"
    }
}

pub fn default_data_dir() -> PathBuf {
    paths::default_cursor_dir()
}

pub fn default_agent_dir() -> PathBuf {
    paths::default_cursor_agent_dir()
}

pub fn state_db_path(cursor_dir: &Path) -> PathBuf {
    paths::cursor_state_db_path(cursor_dir)
}

/// 返回可见会话总数，供设置页校验目录用。
///
/// 口径必须与 [`list_sessions`] 一致，否则设置页报的数和列表里看到的对不上。
pub fn validate_data_dir(cursor_dir: &Path, agent_dir: &Path) -> AppResult<u32> {
    let mut count = 0u32;
    let db = state_db_path(cursor_dir);
    if db.is_file() {
        let connection = open_readonly(&db)?;
        if table_exists(&connection, "composerHeaders")? {
            let mut statement = connection.prepare(
                "SELECT composerId, value FROM composerHeaders WHERE isSubagent IS NOT 1",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            let mut renderable = Vec::new();
            for row in rows {
                let (id, value) = row?;
                let header = value
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .unwrap_or(Value::Null);
                if header_is_renderable(&header) {
                    renderable.push(id);
                }
            }
            let counts = bubble_counts(&connection, renderable.iter().map(String::as_str))?;
            count = counts.values().filter(|count| **count > 0).count() as u32;
        }
    }
    count = count.saturating_add(crate::cursor_agent_store::list_sessions(agent_dir)?.len() as u32);
    Ok(count)
}

pub fn list_sessions(cursor_dir: &Path, agent_dir: &Path) -> AppResult<Vec<SessionSummary>> {
    let db = state_db_path(cursor_dir);
    let chats = paths::cursor_agent_chats_dir(agent_dir);
    if !db.is_file() && !chats.is_dir() {
        return Err(AppError::NotFound(format!(
            "未找到 Cursor 会话数据：{} 与 {} 都不存在",
            db.to_string_lossy(),
            chats.to_string_lossy()
        )));
    }
    let mut out = if db.is_file() {
        list_composer_sessions(cursor_dir)?
    } else {
        Vec::new()
    };
    out.extend(crate::cursor_agent_store::list_sessions(agent_dir)?);
    out.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    Ok(out)
}

fn list_composer_sessions(cursor_dir: &Path) -> AppResult<Vec<SessionSummary>> {
    let db_path = state_db_path(cursor_dir);
    let connection = open_readonly(&db_path)?;
    // 老版本 Cursor 把会话头存在 ItemTable 的 JSON 数组里，还没有这张表。
    if !table_exists(&connection, "composerHeaders")? {
        return Ok(Vec::new());
    }
    let workspaces = load_workspace_paths(cursor_dir);
    let mut statement = connection.prepare(
        "SELECT composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, value
         FROM composerHeaders",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    })?;

    let mut candidates = Vec::new();
    for row in rows {
        let (id, workspace_id, created, updated, archived, subagent, value) = row?;
        let header = value
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .unwrap_or(Value::Null);
        // 既没有项目路径也没有标题的行是 Cursor 留下的空草稿，列表里渲染不出
        // 任何可辨识的信息。这一步是纯字符串判断，先把大头挡掉。
        if !header_is_renderable(&header) {
            continue;
        }
        candidates.push((
            id,
            workspace_id,
            created,
            updated,
            archived,
            subagent,
            header,
        ));
    }

    // 光有标题或项目路径还不够：实测 446 个"可渲染"的会话里有 137 个气泡数为 0，
    // 它们在界面上点开是一片空白。气泡数从 SQLite 侧用 json_array_length 批量取，
    // 446 个会话实测 26ms，不需要把索引 JSON 搬到 Rust 里解析。
    let bubble_counts = bubble_counts(&connection, candidates.iter().map(|(id, ..)| id.as_str()))?;
    let mut sizes = SizeBudget::new(&connection);

    let mut sessions = Vec::new();
    for (id, workspace_id, created, updated, archived, subagent, header) in candidates {
        if bubble_counts.get(&id).copied().unwrap_or(0) == 0 {
            continue;
        }
        // 子代理不单独出现在列表里：它们记在父会话的 subagentComposerIds 上，
        // 单独删掉会让父会话留下悬空引用，所以只随父会话一起管理。
        if subagent == Some(1) || header.get("isSubagent").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let cwd =
            paths::strip_verbatim(&resolve_cwd(&header, workspace_id.as_deref(), &workspaces));
        let subtitle = string_field(&header, "subtitle");
        let created_ms = created
            .filter(|value| *value > 0)
            .or_else(|| header.get("createdAt").and_then(Value::as_i64))
            .unwrap_or(0);
        let updated_ms = updated
            .filter(|value| *value > 0)
            .or_else(|| header.get("lastUpdatedAt").and_then(Value::as_i64))
            .unwrap_or(created_ms)
            .max(created_ms);
        sessions.push(SessionSummary {
            provider: PROVIDER.into(),
            id: id.clone(),
            rollout_path: encode_composer_locator(&db_path, &id)?,
            cwd_display: paths::basename_display(&cwd),
            cwd,
            title: composer_title(&header, &subtitle, &id),
            // Cursor 不在会话头里存首条提问，`subtitle` 是它自己生成的一句话概述。
            first_user_message: subtitle,
            model: None,
            reasoning_effort: None,
            source: None,
            agent_nickname: None,
            agent_role: None,
            conversion_origin: None,
            // Cursor 全程不记录 token 用量：气泡里的 tokenCount 恒为 0，
            // composerData 里只有当前上下文窗口占用，不是累计消耗。
            tokens_used: 0,
            created_at: created_ms / 1000,
            updated_at: updated_ms / 1000,
            archived: archived == Some(1)
                || header.get("isArchived").and_then(Value::as_bool) == Some(true),
            git_branch: git_branch(&header),
            // 0 表示"这次没算出来"，前端按未知渲染；下次刷新会继续补。
            rollout_bytes: sizes.get(&id, updated_ms),
            logs_count: 0,
            has_backup: false,
            // Composer 会话只能在 Cursor 里打开，没有可续聊的命令行。
            resume_command: String::new(),
        });
    }
    Ok(sessions)
}

/// 批量取每个会话的气泡数。
///
/// `json_array_length` 在 SQLite 侧解析，比把几 MB 的索引 JSON 取回 Rust 再解析快得多。
/// 分批是为了不撞上 SQLite 的绑定参数上限。
pub(crate) fn bubble_counts<'a>(
    connection: &Connection,
    ids: impl Iterator<Item = &'a str>,
) -> AppResult<HashMap<String, i64>> {
    /// SQLite 默认上限是 32766，取一个远小于它的批量。
    const CHUNK: usize = 400;
    let ids = ids.collect::<Vec<_>>();
    let mut out = HashMap::with_capacity(ids.len());
    for chunk in ids.chunks(CHUNK) {
        let marks = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut statement = connection.prepare(&format!(
            "SELECT key, json_array_length(value, '$.fullConversationHeadersOnly')
             FROM cursorDiskKV WHERE key IN ({marks})"
        ))?;
        let keys = chunk
            .iter()
            .map(|id| format!("composerData:{id}"))
            .collect::<Vec<_>>();
        let rows = statement.query_map(rusqlite::params_from_iter(keys.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;
        for row in rows {
            let (key, count) = row?;
            if let Some(id) = key.strip_prefix("composerData:") {
                out.insert(id.to_string(), count.unwrap_or(0));
            }
        }
    }
    Ok(out)
}

/// 会话体积的按需计算器。
///
/// 单个会话的字节数要把它全部气泡扫一遍，实测平均 16ms、大会话能到 170ms，
/// 309 个会话一次算完要 5 秒——对一个每 5 秒刷新一次的列表来说太重（Claude 侧
/// 同一台机器只要 0.12s）。所以：结果按会话缓存，每次列表只花固定的时间预算去补
/// 还没算过的，几次刷新后自然补齐。算不出来的先给 0，前端按"未知"渲染。
struct SizeBudget<'a> {
    connection: &'a Connection,
    deadline: std::time::Instant,
}

/// 每次列举最多花在补算体积上的时间。
const SIZE_BUDGET: std::time::Duration = std::time::Duration::from_millis(300);

impl<'a> SizeBudget<'a> {
    fn new(connection: &'a Connection) -> Self {
        Self {
            connection,
            deadline: std::time::Instant::now() + SIZE_BUDGET,
        }
    }

    fn get(&mut self, id: &str, updated_ms: i64) -> u64 {
        let cache = size_cache();
        {
            let cache = cache.lock().unwrap_or_else(|error| error.into_inner());
            if let Some((cached_updated, bytes)) = cache.get(id) {
                if *cached_updated == updated_ms {
                    return *bytes;
                }
            }
        }
        if std::time::Instant::now() >= self.deadline {
            return 0;
        }
        // `octet_length` 直接从记录头拿字节数；`length` 要把 TEXT 解码成字符再数，
        // 实测同一批数据 24s 对 7s。
        let bytes = self
            .connection
            .query_row(
                "SELECT COALESCE(SUM(octet_length(value)), 0) FROM cursorDiskKV
                 WHERE key >= ?1 AND key < ?2",
                [format!("bubbleId:{id}:"), format!("bubbleId:{id};")],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            .max(0) as u64;
        let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());
        if cache.len() >= SIZE_CACHE_CAPACITY {
            cache.clear();
        }
        cache.insert(id.to_string(), (updated_ms, bytes));
        bytes
    }
}

/// 会话 id → (会话更新时间, 字节数)。更新时间变了就重算。
fn size_cache() -> &'static Mutex<HashMap<String, (i64, u64)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (i64, u64)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 缓存的会话数上限，避免长期运行后无限增长。
const SIZE_CACHE_CAPACITY: usize = 4096;

/// 判断一行会话头是否值得出现在列表里。
///
/// 只要有项目路径、标题或概述三者之一就保留——这是"能不能渲染"，不是"猜它是不是空的"。
fn header_is_renderable(header: &Value) -> bool {
    !string_field(header, "name").is_empty()
        || !string_field(header, "subtitle").is_empty()
        || header_workspace_path(header).is_some()
}

fn composer_title(header: &Value, subtitle: &str, id: &str) -> String {
    let name = string_field(header, "name");
    if !name.is_empty() {
        return truncate_title(&name);
    }
    if !subtitle.is_empty() {
        return truncate_title(subtitle);
    }
    header_workspace_path(header)
        .map(|path| paths::basename_display(&path))
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| id.to_string())
}

fn header_workspace_path(header: &Value) -> Option<String> {
    header
        .get("workspaceIdentifier")
        .and_then(|value| value.get("uri"))
        .and_then(|uri| uri.get("fsPath"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .or_else(|| {
            header
                .get("trackedGitRepos")
                .and_then(Value::as_array)?
                .iter()
                .find_map(|repo| {
                    repo.get("repoPath")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|path| !path.is_empty())
                        .map(str::to_string)
                })
        })
}

/// 项目路径三级回退：会话头 → 追踪的 git 仓库 → VS Code 的 workspaceStorage 索引。
fn resolve_cwd(
    header: &Value,
    workspace_id: Option<&str>,
    workspaces: &HashMap<String, String>,
) -> String {
    header_workspace_path(header)
        .or_else(|| {
            workspace_id
                .and_then(|id| workspaces.get(id))
                .map(String::clone)
        })
        .unwrap_or_default()
}

fn git_branch(header: &Value) -> Option<String> {
    header
        .get("trackedGitRepos")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|repo| {
            repo.get("branches")
                .and_then(Value::as_array)?
                .first()?
                .get("branchName")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .map(str::to_string)
        })
}

/// `workspaceStorage/<id>/workspace.json` 把工作区 id 映射到 `file://` 目录。
fn load_workspace_paths(cursor_dir: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(entries) = fs::read_dir(paths::cursor_workspace_storage_dir(cursor_dir)) else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(raw) = fs::read_to_string(entry.path().join("workspace.json")) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Some(folder) = value.get("folder").and_then(Value::as_str) {
            if let Some(path) = file_uri_to_path(folder) {
                out.insert(id, path);
            }
        }
    }
    out
}

/// 把 `file:///a/b%20c` 还原成本地路径。只处理百分号转义，不引入 URL 依赖。
fn file_uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let mut out = String::with_capacity(rest.len());
    let bytes = rest.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&rest[index + 1..index + 3], 16) {
                out.push(byte as char);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    Some(out).filter(|path| !path.is_empty())
}

// ---------------------------------------------------------------------------
// 预览
// ---------------------------------------------------------------------------

pub fn preview_range(locator: &str, offset: usize, limit: usize) -> AppResult<Vec<PreviewEvent>> {
    let decoded = decode_locator(locator)?;
    if decoded.is_agent() {
        return Ok(
            crate::cursor_agent_store::load_preview_events(Path::new(&decoded.path))?
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect(),
        );
    }
    // 大会话可以有上万条气泡，逐页全量展开代价太高：只读到本页末尾就停。
    let db = Path::new(&decoded.path);
    let connection = open_readonly(db)?;
    let needed = offset.saturating_add(limit);
    Ok(
        load_composer_events(&connection, db, &decoded.session, Some(needed))?
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect(),
    )
}

pub(crate) fn load_preview_events_from_locator(locator: &str) -> AppResult<Vec<PreviewEvent>> {
    let locator = decode_locator(locator)?;
    if locator.is_agent() {
        return crate::cursor_agent_store::load_preview_events(Path::new(&locator.path));
    }
    let db = Path::new(&locator.path);
    let connection = open_readonly(db)?;
    load_composer_events(&connection, db, &locator.session, None)
}

pub fn preview_user_prompts(locator: &str) -> AppResult<UserPromptList> {
    let decoded = decode_locator(locator)?;
    if decoded.is_agent() {
        return crate::cursor_agent_store::preview_user_prompts(Path::new(&decoded.path));
    }
    let events = load_preview_events_from_locator(locator)?;
    Ok(crate::rollout::user_prompts_from_events(events, |event| {
        matches!(event.role.as_str(), "assistant" | "reasoning" | "tool_call")
    }))
}

pub fn preview_meta(locator: &str) -> AppResult<SessionMetaBrief> {
    let locator = decode_locator(locator)?;
    if locator.is_agent() {
        return crate::cursor_agent_store::preview_meta(Path::new(&locator.path));
    }
    let db = Path::new(&locator.path);
    let connection = open_readonly(db)?;
    let header = composer_header(&connection, &locator.session)?;
    let index = session_index(&connection, db, &locator.session)?;
    Ok(SessionMetaBrief {
        id: Some(locator.session.clone()),
        timestamp: timestamp_from_millis(
            header.get("createdAt").and_then(Value::as_i64).unwrap_or(0),
        ),
        cwd: header_workspace_path(&header),
        originator: Some("Cursor".into()),
        cli_version: index
            .schema_version
            .map(|version| format!("composerData v{version}")),
        source: Some("state.vscdb".into()),
        model_provider: index.model.clone(),
    })
}

/// 按 `fullConversationHeadersOnly` 记录的顺序展开会话气泡。
///
/// `stop_after` 给出调用方需要的事件条数，达到即停止读取后续气泡；`None` 表示全量。
/// 一条气泡会产出 1~3 个事件，事件序号和气泡序号无法直接换算，所以只能顺序推进。
fn load_composer_events(
    connection: &Connection,
    db: &Path,
    session: &str,
    stop_after: Option<usize>,
) -> AppResult<Vec<PreviewEvent>> {
    let index = session_index(connection, db, session)?;
    let mut statement = connection.prepare("SELECT value FROM cursorDiskKV WHERE key = ?1")?;
    let mut events = Vec::new();
    for entry in index.order.iter() {
        if stop_after.is_some_and(|needed| events.len() >= needed) {
            break;
        }
        let key = format!("bubbleId:{session}:{}", entry.id);
        // 气泡被 Cursor 清理过时，退回索引里的预览文本，好过在时间线上留一个空洞。
        let Some(raw) = read_kv(&mut statement, &key)? else {
            push_event(&mut events, fallback_raw(entry));
            continue;
        };
        let Ok(bubble) = serde_json::from_slice::<Value>(&raw) else {
            push_event(&mut events, fallback_raw(entry));
            continue;
        };
        push_bubble_events(&mut events, &bubble);
    }
    Ok(events)
}

/// 一条气泡在会话索引中的位置信息。
#[derive(Debug, Clone)]
struct BubbleRef {
    id: String,
    kind: i64,
    created_at: String,
    preview: String,
}

/// 一个会话的索引：气泡顺序 + 从 `composerData` 顶层取到的少量元信息。
#[derive(Debug, Clone)]
struct SessionIndex {
    order: Vec<BubbleRef>,
    model: Option<String>,
    schema_version: Option<i64>,
}

/// 索引缓存条目的有效性指纹。
type IndexFingerprint = (i64, i64);

/// 按会话缓存 `composerData` 的解析结果。
///
/// 这个 JSON 在大会话里有好几 MB（近万条气泡索引），而翻页、时间线、全文搜索都要
/// 反复拿它。不缓存的话每次调用固定要花 100ms 上下，分页会退化成 O(页数²)——实测
/// CLI 预览一个 8799 气泡的会话要 7 秒。
///
/// 只缓存索引本身，不缓存展开后的事件：那份数据可以到几百 MB。
fn index_cache() -> &'static Mutex<HashMap<String, (IndexFingerprint, Arc<SessionIndex>)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (IndexFingerprint, Arc<SessionIndex>)>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 缓存的会话数上限。预览一次只看一个会话，留几个位置够覆盖来回切换。
const INDEX_CACHE_CAPACITY: usize = 8;

fn session_index(
    connection: &Connection,
    db: &Path,
    session: &str,
) -> AppResult<Arc<SessionIndex>> {
    let fingerprint = index_fingerprint(connection, session)?;
    let key = format!("{}\u{0}{session}", db.to_string_lossy());
    {
        let cache = index_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((cached, index)) = cache.get(&key) {
            if *cached == fingerprint {
                return Ok(Arc::clone(index));
            }
        }
    }

    let data = composer_data(connection, session)?;
    let order = data
        .get("fullConversationHeadersOnly")
        .and_then(Value::as_array)
        .map(|entries| entries.iter().map(bubble_ref).collect::<Vec<_>>())
        .unwrap_or_default();
    let index = Arc::new(SessionIndex {
        order,
        model: session_model(&data),
        schema_version: data.get("_v").and_then(Value::as_i64),
    });

    let mut cache = index_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    // 简单封顶即可：这里只是省掉重复解析，命中率不值得再维护一套 LRU 记账。
    if cache.len() >= INDEX_CACHE_CAPACITY {
        cache.clear();
    }
    cache.insert(key, (fingerprint, Arc::clone(&index)));
    Ok(index)
}

/// 会话头的更新时间加索引字节数：Cursor 改动会话时两者必变其一，且都是单行点查。
fn index_fingerprint(connection: &Connection, session: &str) -> AppResult<IndexFingerprint> {
    use rusqlite::OptionalExtension;
    let updated = connection
        .query_row(
            "SELECT lastUpdatedAt FROM composerHeaders WHERE composerId = ?1",
            [session],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten()
        .unwrap_or(0);
    let bytes = connection
        .query_row(
            "SELECT length(value) FROM cursorDiskKV WHERE key = ?1",
            [format!("composerData:{session}")],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten()
        .unwrap_or(0);
    Ok((updated, bytes))
}

fn bubble_ref(entry: &Value) -> BubbleRef {
    BubbleRef {
        id: entry
            .get("bubbleId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        kind: entry.get("type").and_then(Value::as_i64).unwrap_or(0),
        created_at: entry
            .get("createdAt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        preview: entry
            .get("grouping")
            .and_then(|grouping| grouping.get("textPreview"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }
}

/// 一个气泡最多产出三类事件：思考、正文、工具调用与其结果。
///
/// Cursor 把工具调用和它的输出放在同一个气泡的 `toolFormerData` 里，这里拆成
/// `tool_use` / `tool_result` 一对，与 Claude、Codex 的时间线粒度对齐。
fn push_bubble_events(events: &mut Vec<PreviewEvent>, bubble: &Value) {
    let bubble_type = bubble.get("type").and_then(Value::as_i64).unwrap_or(0);
    let role = if bubble_type == BUBBLE_TYPE_USER {
        "user"
    } else {
        "assistant"
    };
    let timestamp = bubble
        .get("createdAt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let origin = json!({
        "store": "composer",
        "bubble_id": bubble.get("bubbleId").cloned().unwrap_or(Value::Null),
        "capability_type": bubble.get("capabilityType").cloned().unwrap_or(Value::Null),
    });

    if bubble_type == BUBBLE_TYPE_ASSISTANT {
        if let Some(thinking) = thinking_text(bubble) {
            push_event(
                events,
                message_raw(
                    "assistant",
                    json!([{ "type": "thinking", "thinking": thinking }]),
                    &timestamp,
                    &origin,
                ),
            );
        }
    }

    let text = bubble
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if !text.is_empty() {
        push_event(
            events,
            message_raw(
                role,
                json!([{ "type": "text", "text": unwrap_user_query(text) }]),
                &timestamp,
                &origin,
            ),
        );
    }

    let Some(tool) = bubble
        .get("toolFormerData")
        .filter(|value| value.is_object())
    else {
        return;
    };
    let tool_id = tool
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    push_event(
        events,
        message_raw(
            "assistant",
            json!([{
                "type": "tool_use",
                "id": tool_id,
                "name": tool.get("name").cloned().unwrap_or(Value::Null),
                "input": tool_input(tool),
            }]),
            &timestamp,
            &origin,
        ),
    );
    if let Some(result) = tool.get("result").filter(|value| !value.is_null()) {
        push_event(
            events,
            message_raw(
                "user",
                json!([{
                    "type": "tool_result",
                    "tool_use_id": tool_id,
                    "content": tool_result_content(Some(result)),
                    "is_error": tool_is_error(tool),
                }]),
                &timestamp,
                &origin,
            ),
        );
    }
}

/// `thinking` 存的是一段 JSON 字符串，真正的文本在它的 `text` 字段里。
fn thinking_text(bubble: &Value) -> Option<String> {
    let raw = bubble.get("thinking")?;
    let text = match raw {
        Value::String(encoded) => serde_json::from_str::<Value>(encoded)
            .ok()
            .and_then(|value| {
                value
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| encoded.clone()),
        other => other
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    };
    Some(text).filter(|text| !text.trim().is_empty())
}

/// `rawArgs` 是模型给出的原始入参，`params` 是 Cursor 补全后的版本；优先用后者。
fn tool_input(tool: &Value) -> Value {
    for key in ["params", "rawArgs"] {
        match tool.get(key) {
            Some(Value::String(encoded)) => {
                if let Ok(value) = serde_json::from_str::<Value>(encoded) {
                    return value;
                }
                if !encoded.trim().is_empty() {
                    return json!({ "raw": encoded });
                }
            }
            Some(value) if value.is_object() || value.is_array() => return value.clone(),
            _ => {}
        }
    }
    Value::Null
}

fn tool_is_error(tool: &Value) -> bool {
    matches!(
        tool.get("status").and_then(Value::as_str),
        Some("error") | Some("failed") | Some("cancelled")
    )
}

/// 会话使用的模型。
///
/// `modelConfig` 是 `composerData` 的顶层字段，一次读取即可；气泡里的 `modelInfo`
/// 出现得非常稀疏（实测 8799 条气泡里只有 18 条带），不能靠扫描一段窗口来找。
fn session_model(data: &Value) -> Option<String> {
    data.get("modelConfig")?
        .get("modelName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn push_event(events: &mut Vec<PreviewEvent>, raw: Value) {
    if let Some(event) = crate::claude_sessions::classify_preview(events.len(), raw) {
        events.push(event);
    }
}

/// 合成 Claude 记录的形状，直接复用它的角色判定与前端渲染。
fn message_raw(role: &str, content: Value, timestamp: &str, origin: &Value) -> Value {
    json!({
        "type": role,
        "timestamp": timestamp,
        "message": { "role": role, "content": content },
        "cursor": origin,
    })
}

fn fallback_raw(entry: &BubbleRef) -> Value {
    let role = if entry.kind == BUBBLE_TYPE_USER {
        "user"
    } else {
        "assistant"
    };
    message_raw(
        role,
        json!([{ "type": "text", "text": entry.preview }]),
        &entry.created_at,
        &json!({ "store": "composer", "bubble_missing": true }),
    )
}

// ---------------------------------------------------------------------------
// 共享工具
// ---------------------------------------------------------------------------

pub(crate) fn composer_header(connection: &Connection, session: &str) -> AppResult<Value> {
    let raw = connection
        .query_row(
            "SELECT value FROM composerHeaders WHERE composerId = ?1",
            [session],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten();
    Ok(raw
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null))
}

pub(crate) fn composer_data(connection: &Connection, session: &str) -> AppResult<Value> {
    let mut statement = connection.prepare("SELECT value FROM cursorDiskKV WHERE key = ?1")?;
    let key = format!("composerData:{session}");
    let Some(raw) = read_kv(&mut statement, &key)? else {
        return Err(AppError::NotFound(format!("Cursor 会话不存在: {session}")));
    };
    Ok(serde_json::from_slice::<Value>(&raw).unwrap_or(Value::Null))
}

/// 读取一个 `cursorDiskKV` 取值。
///
/// 这张表把列声明成 BLOB，但 Cursor 实际按 TEXT 写入 JSON，同一张表里还混着真正的
/// 二进制（`agentKv:blob:` 下是图片）。所以必须两种存储类都接受，直接按 `Vec<u8>` 取会
/// 在 TEXT 上报类型错误。查不到行返回 `Ok(None)`，真正的 SQL 错误照常上抛。
fn read_kv(statement: &mut rusqlite::Statement<'_>, key: &str) -> AppResult<Option<Vec<u8>>> {
    use rusqlite::types::Value as SqlValue;
    use rusqlite::OptionalExtension;

    let value = statement
        .query_row([key], |row| row.get::<_, SqlValue>(0))
        .optional()?;
    Ok(match value {
        Some(SqlValue::Text(text)) => Some(text.into_bytes()),
        Some(SqlValue::Blob(bytes)) => Some(bytes),
        _ => None,
    })
}

/// 工具输出可能是字符串，也可能是结构化对象；统一成前端能直接展示的文本。
pub(crate) fn tool_result_content(result: Option<&Value>) -> Value {
    match result {
        Some(Value::String(text)) => Value::String(text.clone()),
        Some(value) if !value.is_null() => {
            Value::String(serde_json::to_string(value).unwrap_or_default())
        }
        _ => Value::String(String::new()),
    }
}

/// Cursor 会把用户提问包在 `<user_query>` 里，展示时剥掉。
pub(crate) fn unwrap_user_query(text: &str) -> String {
    let trimmed = text.trim();
    trimmed
        .strip_prefix("<user_query>")
        .and_then(|inner| inner.strip_suffix("</user_query>"))
        .map(str::trim)
        .filter(|inner| !inner.is_empty())
        .map(String::from)
        .unwrap_or_else(|| text.to_string())
}

/// 判断一段"用户消息"其实是客户端注入的环境上下文而非真人输入。
pub(crate) fn is_injected_context(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.contains("<user_query>") {
        return false;
    }
    ["<user_info>", "<environment_context>", "<additional_data>"]
        .iter()
        .any(|marker| trimmed.starts_with(marker))
}

pub(crate) fn truncate_title(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= TITLE_MAX_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(TITLE_MAX_CHARS).collect();
    out.push('…');
    out
}

pub(crate) fn timestamp_from_millis(millis: i64) -> Option<String> {
    chrono::DateTime::<Utc>::from_timestamp_millis(millis)
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

pub(crate) fn encode_composer_locator(db: &Path, session: &str) -> AppResult<String> {
    encode_locator(&SessionLocator {
        store: "composer".into(),
        path: db.to_string_lossy().into_owned(),
        session: session.to_string(),
    })
}

pub(crate) fn encode_agent_locator(dir: &Path, session: &str) -> AppResult<String> {
    encode_locator(&SessionLocator {
        store: "agent".into(),
        path: dir.to_string_lossy().into_owned(),
        session: session.to_string(),
    })
}

fn encode_locator(locator: &SessionLocator) -> AppResult<String> {
    let raw = serde_json::to_vec(locator)?;
    Ok(format!("{LOCATOR_PREFIX}{}", URL_SAFE_NO_PAD.encode(raw)))
}

pub(crate) fn decode_locator(value: &str) -> AppResult<SessionLocator> {
    let encoded = value
        .strip_prefix(LOCATOR_PREFIX)
        .ok_or_else(|| AppError::Path("Cursor 会话定位符格式无效".into()))?;
    let raw = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|error| AppError::Path(format!("Cursor 会话定位符无法解码: {error}")))?;
    let locator: SessionLocator = serde_json::from_slice(&raw)?;
    if locator.session.trim().is_empty() {
        return Err(AppError::Path("Cursor 会话定位符缺少会话 id".into()));
    }
    let path = Path::new(&locator.path);
    let exists = if locator.is_agent() {
        path.is_dir()
    } else {
        path.is_file()
    };
    if !exists {
        return Err(AppError::Path(format!(
            "Cursor 会话定位符指向的位置不存在: {}",
            locator.path
        )));
    }
    Ok(locator)
}

pub(crate) fn open_readonly(path: &Path) -> AppResult<Connection> {
    if !path.is_file() {
        return Err(AppError::NotFound(format!(
            "Cursor 数据库不存在: {}",
            path.to_string_lossy()
        )));
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(Into::into)
}

pub(crate) fn table_exists(connection: &Connection, table: &str) -> AppResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get::<_, bool>(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cc-sessions-cursor-{name}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    struct Fixture {
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// 造一个最小可用的 state.vscdb，表结构与真实 Cursor 一致。
    fn fixture(name: &str) -> AppResult<Fixture> {
        let root = temp_root(name);
        let storage = root.join("globalStorage");
        fs::create_dir_all(&storage)?;
        let connection = Connection::open(storage.join("state.vscdb"))?;
        connection.execute_batch(
            "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT,
                createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER,
                isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT);
             CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
        )?;
        Ok(Fixture { root })
    }

    fn connect(fixture: &Fixture) -> AppResult<Connection> {
        Ok(Connection::open(state_db_path(&fixture.root))?)
    }

    fn insert_header(
        connection: &Connection,
        id: &str,
        archived: i64,
        subagent: i64,
        value: Value,
    ) -> AppResult<()> {
        connection.execute(
            "INSERT INTO composerHeaders VALUES (?1, 'ws-1', 1000, 2000, ?2, ?3, 2000, NULL, ?4)",
            rusqlite::params![id, archived, subagent, value.to_string()],
        )?;
        Ok(())
    }

    /// 列虽然声明成 BLOB，Cursor 实际写的是 TEXT，夹具必须照做才能覆盖真实读路径。
    fn insert_kv(connection: &Connection, key: &str, value: Value) -> AppResult<()> {
        connection.execute(
            "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
            rusqlite::params![key, value.to_string()],
        )?;
        Ok(())
    }

    fn insert_kv_blob(connection: &Connection, key: &str, value: Value) -> AppResult<()> {
        connection.execute(
            "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
            rusqlite::params![key, value.to_string().into_bytes()],
        )?;
        Ok(())
    }

    #[test]
    fn list_sessions_skips_headers_without_anything_to_render() -> AppResult<()> {
        let fixture = fixture("blank")?;
        let connection = connect(&fixture)?;
        insert_header(
            &connection,
            "real",
            0,
            0,
            json!({
                "name": "真实会话",
                "subtitle": "改了两个文件",
                "workspaceIdentifier": {"uri": {"fsPath": "/tmp/proj"}}
            }),
        )?;
        insert_kv(
            &connection,
            "composerData:real",
            json!({"fullConversationHeadersOnly": [{"bubbleId": "b1", "type": 1}]}),
        )?;
        insert_kv(
            &connection,
            "bubbleId:real:b1",
            json!({"type": 1, "text": "问题"}),
        )?;
        // 空草稿：无标题、无概述、无项目路径。
        insert_header(&connection, "draft", 0, 0, json!({ "isDraft": true }))?;
        drop(connection);

        let sessions = list_sessions(&fixture.root, &fixture.root.join("agent"))?;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "real");
        assert_eq!(sessions[0].cwd, "/tmp/proj");
        assert_eq!(sessions[0].cwd_display, "proj");
        assert_eq!(sessions[0].title, "真实会话");
        assert_eq!(sessions[0].first_user_message, "改了两个文件");
        // Cursor 不记录 token 用量，不能拿上下文占用冒充。
        assert_eq!(sessions[0].tokens_used, 0);
        Ok(())
    }

    /// 有标题或项目路径、但一条气泡都没有的会话点开是空白，不该出现在列表里。
    #[test]
    fn list_sessions_skips_sessions_that_have_no_bubbles() -> AppResult<()> {
        let fixture = fixture("empty-conversation")?;
        let connection = connect(&fixture)?;
        // 只有项目路径、没有任何内容——本机 whisper 那个会话就是这样。
        insert_header(
            &connection,
            "drafted",
            0,
            0,
            json!({ "workspaceIdentifier": {"uri": {"fsPath": "/tmp/gone"}} }),
        )?;
        insert_kv(
            &connection,
            "composerData:drafted",
            json!({"fullConversationHeadersOnly": []}),
        )?;
        // 有标题但连 composerData 都没有。
        insert_header(&connection, "headless", 0, 0, json!({ "name": "只有标题" }))?;
        // 正常会话。
        insert_header(&connection, "real", 0, 0, json!({ "name": "有内容" }))?;
        insert_kv(
            &connection,
            "composerData:real",
            json!({"fullConversationHeadersOnly": [{"bubbleId": "b1", "type": 1}]}),
        )?;
        insert_kv(
            &connection,
            "bubbleId:real:b1",
            json!({"type": 1, "text": "问题"}),
        )?;
        drop(connection);

        let sessions = list_sessions(&fixture.root, &fixture.root.join("agent"))?;
        assert_eq!(
            sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["real"]
        );
        // 设置页的计数要和列表一致。
        assert_eq!(
            validate_data_dir(&fixture.root, &fixture.root.join("agent"))?,
            1
        );
        Ok(())
    }

    #[test]
    fn list_sessions_reports_the_conversation_byte_size() -> AppResult<()> {
        let fixture = fixture("size")?;
        let connection = connect(&fixture)?;
        insert_header(&connection, "s1", 0, 0, json!({ "name": "会话" }))?;
        insert_kv(
            &connection,
            "composerData:s1",
            json!({"fullConversationHeadersOnly": [{"bubbleId": "b1"}, {"bubbleId": "b2"}]}),
        )?;
        let first = json!({"type": 1, "text": "第一条"});
        let second = json!({"type": 2, "text": "第二条稍微长一点"});
        insert_kv(&connection, "bubbleId:s1:b1", first.clone())?;
        insert_kv(&connection, "bubbleId:s1:b2", second.clone())?;
        // 别的会话的气泡不能算进来。
        insert_kv(
            &connection,
            "bubbleId:s2:b1",
            json!({"type": 1, "text": "别人的"}),
        )?;
        drop(connection);

        let expected = (first.to_string().len() + second.to_string().len()) as u64;
        let sessions = list_sessions(&fixture.root, &fixture.root.join("agent"))?;
        assert_eq!(sessions[0].rollout_bytes, expected);
        Ok(())
    }

    /// 子代理只随父会话管理，不单独出现在列表里。
    #[test]
    fn list_sessions_marks_archived_and_hides_subagent_rows() -> AppResult<()> {
        let fixture = fixture("flags")?;
        let connection = connect(&fixture)?;
        insert_header(&connection, "archived", 1, 0, json!({ "name": "已归档" }))?;
        insert_header(&connection, "sub", 0, 1, json!({ "name": "子代理" }))?;
        for id in ["archived", "sub"] {
            insert_kv(
                &connection,
                &format!("composerData:{id}"),
                json!({"fullConversationHeadersOnly": [{"bubbleId": "b1", "type": 1}]}),
            )?;
            insert_kv(
                &connection,
                &format!("bubbleId:{id}:b1"),
                json!({"type": 1, "text": "问题"}),
            )?;
        }
        drop(connection);

        let sessions = list_sessions(&fixture.root, &fixture.root.join("agent"))?;
        let archived = sessions.iter().find(|s| s.id == "archived").unwrap();
        assert!(archived.archived);
        // 子代理不出现在列表里。
        assert!(sessions.iter().all(|s| s.id != "sub"));
        Ok(())
    }

    #[test]
    fn cwd_falls_back_to_git_repo_then_workspace_storage() -> AppResult<()> {
        let fixture = fixture("cwd")?;
        let workspace = paths::cursor_workspace_storage_dir(&fixture.root).join("ws-1");
        fs::create_dir_all(&workspace)?;
        fs::write(
            workspace.join("workspace.json"),
            json!({ "folder": "file:///tmp/from%20storage" }).to_string(),
        )?;
        let connection = connect(&fixture)?;
        insert_header(
            &connection,
            "git",
            0,
            0,
            json!({
                "name": "走 git 回退",
                "trackedGitRepos": [{"repoPath": "/tmp/repo", "branches": [{"branchName": "main"}]}]
            }),
        )?;
        insert_header(
            &connection,
            "storage",
            0,
            0,
            json!({ "name": "走索引回退" }),
        )?;
        for id in ["git", "storage"] {
            insert_kv(
                &connection,
                &format!("composerData:{id}"),
                json!({"fullConversationHeadersOnly": [{"bubbleId": "b1", "type": 1}]}),
            )?;
            insert_kv(
                &connection,
                &format!("bubbleId:{id}:b1"),
                json!({"type": 1, "text": "问题"}),
            )?;
        }
        drop(connection);

        let sessions = list_sessions(&fixture.root, &fixture.root.join("agent"))?;
        let git = sessions.iter().find(|s| s.id == "git").unwrap();
        let storage = sessions.iter().find(|s| s.id == "storage").unwrap();
        assert_eq!(git.cwd, "/tmp/repo");
        assert_eq!(git.git_branch.as_deref(), Some("main"));
        // 百分号转义要还原成真实路径。
        assert_eq!(storage.cwd, "/tmp/from storage");
        Ok(())
    }

    #[test]
    fn preview_splits_thinking_text_and_tool_pairs() -> AppResult<()> {
        let fixture = fixture("preview")?;
        let connection = connect(&fixture)?;
        insert_header(&connection, "s1", 0, 0, json!({ "name": "会话" }))?;
        insert_kv(
            &connection,
            "composerData:s1",
            json!({
                "_v": 18,
                "fullConversationHeadersOnly": [
                    {"bubbleId": "b1", "type": 1},
                    {"bubbleId": "b2", "type": 2},
                    {"bubbleId": "b3", "type": 2}
                ]
            }),
        )?;
        insert_kv(
            &connection,
            "bubbleId:s1:b1",
            json!({"type": 1, "text": "<user_query>\n帮我看看\n</user_query>", "createdAt": "2026-08-13T15:34:11.009Z"}),
        )?;
        insert_kv(
            &connection,
            "bubbleId:s1:b2",
            json!({
                "type": 2,
                "thinking": r#"{"text":"先读文件","isLastThinkingChunk":true}"#,
                "createdAt": "2026-08-13T15:34:12.000Z"
            }),
        )?;
        insert_kv(
            &connection,
            "bubbleId:s1:b3",
            json!({
                "type": 2,
                "toolFormerData": {
                    "toolCallId": "toolu_1",
                    "name": "read_file_v2",
                    "rawArgs": "{\"target_file\":\"a.rs\"}",
                    "params": "{\"target_file\":\"a.rs\",\"explanation\":\"读\"}",
                    "status": "completed",
                    "result": "文件内容"
                },
                "createdAt": "2026-08-13T15:34:13.000Z"
            }),
        )?;
        drop(connection);

        let locator = encode_composer_locator(&state_db_path(&fixture.root), "s1")?;
        let events = load_preview_events_from_locator(&locator)?;
        let roles = events.iter().map(|e| e.role.as_str()).collect::<Vec<_>>();
        assert_eq!(roles, vec!["user", "reasoning", "tool_call", "tool_result"]);
        // <user_query> 包裹要剥掉。
        assert_eq!(events[0].text_summary, "帮我看看");
        assert_eq!(events[1].text_summary, "先读文件");
        // params 比 rawArgs 更完整，应当优先。
        let input = events[2].raw["message"]["content"][0]["input"].clone();
        assert_eq!(input["explanation"], "读");
        assert_eq!(events[3].raw["message"]["content"][0]["is_error"], false);

        // 分页要作用在展开后的事件上。
        assert_eq!(preview_range(&locator, 2, 1)?.len(), 1);
        assert_eq!(preview_range(&locator, 2, 1)?[0].role, "tool_call");
        Ok(())
    }

    #[test]
    fn preview_falls_back_to_index_preview_when_a_bubble_is_gone() -> AppResult<()> {
        let fixture = fixture("missing")?;
        let connection = connect(&fixture)?;
        insert_header(&connection, "s1", 0, 0, json!({ "name": "会话" }))?;
        insert_kv(
            &connection,
            "composerData:s1",
            json!({
                "fullConversationHeadersOnly": [
                    {"bubbleId": "gone", "type": 1, "grouping": {"textPreview": "被清理的提问"}}
                ]
            }),
        )?;
        drop(connection);

        let locator = encode_composer_locator(&state_db_path(&fixture.root), "s1")?;
        let events = load_preview_events_from_locator(&locator)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].role, "user");
        assert_eq!(events[0].text_summary, "被清理的提问");
        Ok(())
    }

    #[test]
    fn cancelled_tool_calls_are_reported_as_errors() -> AppResult<()> {
        let fixture = fixture("cancelled")?;
        let connection = connect(&fixture)?;
        insert_header(&connection, "s1", 0, 0, json!({ "name": "会话" }))?;
        insert_kv(
            &connection,
            "composerData:s1",
            json!({"fullConversationHeadersOnly": [{"bubbleId": "b1", "type": 2}]}),
        )?;
        insert_kv(
            &connection,
            "bubbleId:s1:b1",
            json!({
                "type": 2,
                "toolFormerData": {
                    "toolCallId": "toolu_1",
                    "name": "run_terminal_command_v2",
                    "status": "cancelled",
                    "result": "中断"
                }
            }),
        )?;
        drop(connection);

        let locator = encode_composer_locator(&state_db_path(&fixture.root), "s1")?;
        let events = load_preview_events_from_locator(&locator)?;
        assert_eq!(events[1].raw["message"]["content"][0]["is_error"], true);
        Ok(())
    }

    /// 同一张表里 TEXT 与 BLOB 混存，两种都必须能读出来。
    #[test]
    fn kv_values_are_read_as_text_or_blob() -> AppResult<()> {
        let fixture = fixture("storage-class")?;
        let connection = connect(&fixture)?;
        insert_header(&connection, "s1", 0, 0, json!({ "name": "会话" }))?;
        insert_kv(
            &connection,
            "composerData:s1",
            json!({"fullConversationHeadersOnly": [{"bubbleId": "b1", "type": 1}]}),
        )?;
        insert_kv_blob(
            &connection,
            "bubbleId:s1:b1",
            json!({"type": 1, "text": "以二进制存的气泡"}),
        )?;
        drop(connection);

        let locator = encode_composer_locator(&state_db_path(&fixture.root), "s1")?;
        let events = load_preview_events_from_locator(&locator)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text_summary, "以二进制存的气泡");
        Ok(())
    }

    /// 索引缓存必须跟着会话内容失效，否则改完会话还会读到旧的气泡顺序。
    #[test]
    fn the_session_index_cache_is_invalidated_when_the_session_changes() -> AppResult<()> {
        let fixture = fixture("index-cache")?;
        let connection = connect(&fixture)?;
        insert_header(&connection, "s1", 0, 0, json!({ "name": "会话" }))?;
        insert_kv(
            &connection,
            "composerData:s1",
            json!({"fullConversationHeadersOnly": [{"bubbleId": "b1", "type": 1}]}),
        )?;
        insert_kv(
            &connection,
            "bubbleId:s1:b1",
            json!({"type": 1, "text": "第一条"}),
        )?;
        drop(connection);

        let locator = encode_composer_locator(&state_db_path(&fixture.root), "s1")?;
        assert_eq!(load_preview_events_from_locator(&locator)?.len(), 1);

        // 追加一条气泡，并按 Cursor 的行为同步更新会话头的时间戳。
        let connection = connect(&fixture)?;
        connection.execute(
            "UPDATE cursorDiskKV SET value = ?1 WHERE key = 'composerData:s1'",
            [json!({"fullConversationHeadersOnly": [
                {"bubbleId": "b1", "type": 1},
                {"bubbleId": "b2", "type": 2}
            ]})
            .to_string()],
        )?;
        insert_kv(
            &connection,
            "bubbleId:s1:b2",
            json!({"type": 2, "text": "第二条"}),
        )?;
        connection.execute(
            "UPDATE composerHeaders SET lastUpdatedAt = 9999 WHERE composerId = 's1'",
            [],
        )?;
        drop(connection);

        let events = load_preview_events_from_locator(&locator)?;
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].text_summary, "第二条");
        Ok(())
    }

    #[test]
    fn locator_round_trips_and_rejects_foreign_prefixes() -> AppResult<()> {
        let fixture = fixture("locator")?;
        let db = state_db_path(&fixture.root);
        let encoded = encode_composer_locator(&db, "s1")?;
        let decoded = decode_locator(&encoded)?;
        assert_eq!(decoded.session, "s1");
        assert!(!decoded.is_agent());
        assert!(decode_locator("opencode:abc").is_err());
        Ok(())
    }

    #[test]
    fn missing_both_stores_is_an_error_but_a_missing_cli_store_is_not() -> AppResult<()> {
        let fixture = fixture("stores")?;
        // state.vscdb 在，CLI 目录不在：正常返回。
        assert!(list_sessions(&fixture.root, &fixture.root.join("agent")).is_ok());

        let empty = temp_root("nothing");
        fs::create_dir_all(&empty)?;
        assert!(list_sessions(&empty, &empty).is_err());
        fs::remove_dir_all(&empty)?;
        Ok(())
    }

    #[test]
    fn injected_context_is_detected_only_without_a_real_query() {
        assert!(is_injected_context("<user_info>OS darwin</user_info>"));
        assert!(!is_injected_context(
            "<user_info>OS</user_info>\n<user_query>真的提问</user_query>"
        ));
        assert!(!is_injected_context("普通提问"));
    }
}
