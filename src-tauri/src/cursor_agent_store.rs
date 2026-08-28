//! cursor-agent CLI 会话（`~/.cursor/chats`）的只读解析。
//!
//! 目录布局是 `chats/<md5(cwd)>/<agentId>/`，每个会话目录里：
//! - `meta.json`（并非总是存在）：`cwd` / `createdAtMs` / `updatedAtMs` / `hasConversation`
//! - `prompt_history.json`（可选）：用户提问的纯文本列表
//! - `store.db`：`meta` 表存 hex 编码的 JSON 会话头，`blobs` 表是内容寻址的消息体
//!
//! `blobs` 只是一个哈希到字节的映射，对话顺序记在 root blob 这个私有 protobuf 报文里。
//! 本模块只依赖一条最小假设：**root blob 中字段 1、wire type 2、长度 32 的取值，是按
//! 对话顺序排列的子 blob 摘要**。其余字段（上下文窗口分区、token 统计）一律跳过。
//! Cursor 调整报文结构时，最坏结果是这个会话读不出来，而不会拼出一段错误的对话。
//!
//! 子 blob 本身是纯 JSON 的 AI-SDK 消息，不需要再解 protobuf。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::models::{PreviewEvent, SessionMetaBrief, SessionSummary, UserPromptList};
use crate::paths;

/// blob 摘要是 32 字节的 SHA-256。
const BLOB_ID_LEN: usize = 32;

/// 一个会话目录解析出来的原始信息，`SessionSummary` 与预览都由它派生。
#[derive(Debug, Clone, Default)]
pub(crate) struct AgentSessionMeta {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub first_prompt: String,
    pub bytes: u64,
}

pub fn store_path(session_dir: &Path) -> PathBuf {
    session_dir.join("store.db")
}

/// 扫描 `<agent_dir>/chats` 下的全部会话。
///
/// 目录不存在时返回空列表而不是报错：用户可能只用 Cursor IDE、从没装过 CLI。
pub fn list_sessions(agent_dir: &Path) -> AppResult<Vec<SessionSummary>> {
    let chats = paths::cursor_agent_chats_dir(agent_dir);
    if !chats.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for workspace in read_dirs(&chats)? {
        // 目录名是 md5(cwd)，不可逆。但同一目录下的会话共享同一个 cwd，
        // 所以只要组内任何一个会话留下了 meta.json 就能补全其余会话。
        let sessions = read_dirs(&workspace)?;
        let shared_cwd = sessions
            .iter()
            .find_map(|dir| read_meta_json(dir).and_then(|meta| meta.cwd));
        for dir in sessions {
            match read_session_meta(&dir, shared_cwd.as_deref()) {
                Ok(Some(meta)) => out.push(summary_from_meta(&dir, meta)?),
                // 单个会话损坏不应该让整个列表不可用。
                Ok(None) | Err(_) => continue,
            }
        }
    }
    Ok(out)
}

fn summary_from_meta(session_dir: &Path, meta: AgentSessionMeta) -> AppResult<SessionSummary> {
    let title = if meta.name.trim().is_empty() {
        if meta.first_prompt.trim().is_empty() {
            paths::basename_display(&session_dir.to_string_lossy())
        } else {
            crate::cursor_sessions::truncate_title(&meta.first_prompt)
        }
    } else {
        meta.name.clone()
    };
    let cwd = paths::strip_verbatim(&meta.cwd);
    Ok(SessionSummary {
        provider: crate::cursor_sessions::PROVIDER.into(),
        id: meta.id.clone(),
        rollout_path: crate::cursor_sessions::encode_agent_locator(session_dir, &meta.id)?,
        cwd_display: paths::basename_display(&cwd),
        cwd,
        title,
        first_user_message: meta.first_prompt,
        model: meta.model,
        reasoning_effort: None,
        source: meta.mode,
        agent_nickname: None,
        agent_role: None,
        conversion_origin: None,
        // Cursor 全程不记录 token 用量，写 0 而不是拿上下文窗口占用冒充累计消耗。
        tokens_used: 0,
        created_at: meta.created_at_ms / 1000,
        updated_at: meta.updated_at_ms / 1000,
        // cursor-agent 没有归档概念。
        archived: false,
        git_branch: None,
        rollout_bytes: meta.bytes,
        logs_count: 0,
        has_backup: false,
        resume_command: format!("cursor-agent --resume {}", meta.id),
    })
}

/// 读取单个会话目录的元信息；不含 `store.db` 时返回 `None`。
pub(crate) fn read_session_meta(
    session_dir: &Path,
    shared_cwd: Option<&str>,
) -> AppResult<Option<AgentSessionMeta>> {
    let store = store_path(session_dir);
    if !store.is_file() {
        return Ok(None);
    }
    let file_meta = fs::metadata(&store)?;
    let mtime_ms = file_meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|delta| delta.as_millis() as i64)
        .unwrap_or(0);

    let header = read_store_header(&store).unwrap_or(Value::Null);
    let json_meta = read_meta_json(session_dir);

    let id = header
        .get("agentId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| paths::basename_display(&session_dir.to_string_lossy()));
    let created_at_ms = json_meta
        .as_ref()
        .and_then(|meta| meta.created_at_ms)
        .or_else(|| header.get("createdAt").and_then(Value::as_i64))
        .filter(|value| *value > 0)
        .unwrap_or(mtime_ms);
    let updated_at_ms = json_meta
        .as_ref()
        .and_then(|meta| meta.updated_at_ms)
        .filter(|value| *value > 0)
        .unwrap_or(mtime_ms)
        .max(created_at_ms);

    Ok(Some(AgentSessionMeta {
        id,
        name: header
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        cwd: json_meta
            .as_ref()
            .and_then(|meta| meta.cwd.clone())
            .or_else(|| shared_cwd.map(str::to_string))
            .unwrap_or_default(),
        model: header
            .get("lastUsedModel")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string),
        mode: header
            .get("mode")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string),
        created_at_ms,
        updated_at_ms,
        first_prompt: read_first_prompt(session_dir),
        bytes: file_meta.len(),
    }))
}

pub(crate) fn preview_meta(session_dir: &Path) -> AppResult<SessionMetaBrief> {
    let meta = read_session_meta(session_dir, None)?
        .ok_or_else(|| AppError::NotFound("Cursor CLI 会话不存在".into()))?;
    Ok(SessionMetaBrief {
        id: Some(meta.id),
        timestamp: crate::cursor_sessions::timestamp_from_millis(meta.created_at_ms),
        cwd: Some(meta.cwd).filter(|value| !value.is_empty()),
        originator: Some("cursor-agent".into()),
        cli_version: None,
        source: Some("store.db".into()),
        model_provider: meta.model,
    })
}

/// 按 root blob 记录的顺序还原整段对话。
pub(crate) fn load_preview_events(session_dir: &Path) -> AppResult<Vec<PreviewEvent>> {
    let store = store_path(session_dir);
    let connection = open_readonly(&store)?;
    let header = read_store_header(&store)?;
    let Some(root) = header.get("latestRootBlobId").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };

    let mut blobs: HashMap<String, Vec<u8>> = HashMap::new();
    let mut statement = connection.prepare("SELECT id, data FROM blobs")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    for row in rows {
        let (id, data) = row?;
        blobs.insert(id, data);
    }

    let Some(root_blob) = blobs.get(root) else {
        return Ok(Vec::new());
    };
    let mut events = Vec::new();
    for id in child_blob_ids(root_blob) {
        let Some(data) = blobs.get(&id) else {
            continue;
        };
        let Ok(message) = serde_json::from_slice::<Value>(data) else {
            continue;
        };
        push_message_events(&mut events, &id, &message);
    }
    Ok(events)
}

/// 把一条 AI-SDK 消息拆成若干 `PreviewEvent`。
///
/// 合成的 `raw` 刻意做成 Claude 记录的形状，直接复用
/// `claude_sessions::classify_preview` 的角色判定，前端也就不必为 Cursor 再写渲染分支。
fn push_message_events(events: &mut Vec<PreviewEvent>, blob_id: &str, message: &Value) {
    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
    let content = message.get("content");
    let origin = json!({ "blob_id": blob_id, "store": "agent" });

    // 系统提示词与 <user_info> 这类注入上下文不是对话内容，标成 meta 让预览默认折叠。
    if role == "system" {
        push(events, meta_raw("system_prompt", content, &origin));
        return;
    }
    let Some(blocks) = content.and_then(Value::as_array) else {
        let text = content.and_then(Value::as_str).unwrap_or("");
        if role == "user" && crate::cursor_sessions::is_injected_context(text) {
            push(events, meta_raw("environment_context", content, &origin));
        } else {
            push(
                events,
                message_raw(role, json!([text_block(text)]), &origin),
            );
        }
        return;
    };

    let mut texts = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                if role == "user" && crate::cursor_sessions::is_injected_context(text) {
                    push(events, meta_raw("environment_context", content, &origin));
                } else {
                    texts.push(text_block(text));
                }
            }
            "tool-call" => {
                // 工具调用单独成事件，与 Ⓐ 侧和 Claude 侧的时间线粒度保持一致。
                // 先把此前攒下的正文冲出去，否则同一条消息里 text 会排到工具后面。
                flush_texts(events, role, &mut texts, &origin);
                push(
                    events,
                    message_raw(
                        "assistant",
                        json!([{
                            "type": "tool_use",
                            "id": block.get("toolCallId").cloned().unwrap_or(Value::Null),
                            "name": block.get("toolName").cloned().unwrap_or(Value::Null),
                            "input": block.get("args").cloned().unwrap_or(Value::Null),
                        }]),
                        &origin,
                    ),
                );
            }
            "tool-result" => {
                flush_texts(events, role, &mut texts, &origin);
                push(
                    events,
                    message_raw(
                        "user",
                        json!([{
                            "type": "tool_result",
                            "tool_use_id": block.get("toolCallId").cloned().unwrap_or(Value::Null),
                            "content": crate::cursor_sessions::tool_result_content(block.get("result")),
                        }]),
                        &origin,
                    ),
                );
            }
            _ => {}
        }
    }
    flush_texts(events, role, &mut texts, &origin);
}

/// 把攒下的连续 text 块合成一条消息事件。相邻的正文属于同一次发言，不该拆开。
fn flush_texts(events: &mut Vec<PreviewEvent>, role: &str, texts: &mut Vec<Value>, origin: &Value) {
    if texts.is_empty() {
        return;
    }
    push(
        events,
        message_raw(role, Value::Array(std::mem::take(texts)), origin),
    );
}

fn push(events: &mut Vec<PreviewEvent>, raw: Value) {
    if let Some(event) = crate::claude_sessions::classify_preview(events.len(), raw) {
        events.push(event);
    }
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": crate::cursor_sessions::unwrap_user_query(text) })
}

/// cursor-agent 不给每条消息记时间戳，`timestamp` 留空表示"未知"而不是编一个。
fn message_raw(role: &str, content: Value, origin: &Value) -> Value {
    json!({
        "type": role,
        "timestamp": "",
        "message": { "role": role, "content": content },
        "cursor": origin,
    })
}

fn meta_raw(kind: &str, content: Option<&Value>, origin: &Value) -> Value {
    json!({
        "type": kind,
        "timestamp": "",
        "isMeta": true,
        "content": content.and_then(Value::as_str).unwrap_or(""),
        "cursor": origin,
    })
}

pub(crate) fn preview_user_prompts(session_dir: &Path) -> AppResult<UserPromptList> {
    let events = load_preview_events(session_dir)?;
    Ok(crate::rollout::user_prompts_from_events(events, |event| {
        matches!(event.role.as_str(), "assistant" | "reasoning" | "tool_call")
    }))
}

// ---------------------------------------------------------------------------
// root blob 的最小 protobuf 解析
// ---------------------------------------------------------------------------

/// 按报文顺序取出字段 1 中长度为 32 的取值，即对话中各条消息的 blob 摘要。
fn child_blob_ids(data: &[u8]) -> Vec<String> {
    let mut cursor = 0usize;
    let mut out = Vec::new();
    while cursor < data.len() {
        let Some(key) = read_varint(data, &mut cursor) else {
            break;
        };
        let field = key >> 3;
        match key & 7 {
            // varint
            0 => {
                if read_varint(data, &mut cursor).is_none() {
                    break;
                }
            }
            // 64 位定长
            1 => match cursor.checked_add(8).filter(|end| *end <= data.len()) {
                Some(end) => cursor = end,
                None => break,
            },
            // 长度前缀
            2 => {
                let Some(len) = read_varint(data, &mut cursor) else {
                    break;
                };
                let Ok(len) = usize::try_from(len) else {
                    break;
                };
                let Some(end) = cursor.checked_add(len).filter(|end| *end <= data.len()) else {
                    break;
                };
                if field == 1 && len == BLOB_ID_LEN {
                    out.push(hex::encode(&data[cursor..end]));
                }
                cursor = end;
            }
            // 32 位定长
            5 => match cursor.checked_add(4).filter(|end| *end <= data.len()) {
                Some(end) => cursor = end,
                None => break,
            },
            // 3/4 是已废弃的 group 编码，遇到直接停止而不是猜测长度。
            _ => break,
        }
    }
    out
}

fn read_varint(data: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *data.get(*cursor)?;
        *cursor += 1;
        result |= u64::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

// ---------------------------------------------------------------------------
// 文件与数据库读取
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct MetaJson {
    cwd: Option<String>,
    created_at_ms: Option<i64>,
    updated_at_ms: Option<i64>,
}

fn read_meta_json(session_dir: &Path) -> Option<MetaJson> {
    let raw = fs::read_to_string(session_dir.join("meta.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    Some(MetaJson {
        cwd: value
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.trim().is_empty())
            .map(str::to_string),
        created_at_ms: value.get("createdAtMs").and_then(Value::as_i64),
        updated_at_ms: value.get("updatedAtMs").and_then(Value::as_i64),
    })
}

/// `prompt_history.json` 是用户提问的纯文本列表，用来在列表页免去解析 blob。
fn read_first_prompt(session_dir: &Path) -> String {
    let Ok(raw) = fs::read_to_string(session_dir.join("prompt_history.json")) else {
        return String::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return String::new();
    };
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        // 开头的斜杠命令是客户端指令，不能代表这个会话在聊什么。
        .find(|prompt| !prompt.is_empty() && !prompt.starts_with('/'))
        .unwrap_or_default()
        .to_string()
}

/// `store.db` 的 `meta` 表把会话头存成 hex 编码的 JSON。
fn read_store_header(store: &Path) -> AppResult<Value> {
    let connection = open_readonly(store)?;
    let mut statement = connection.prepare("SELECT value FROM meta ORDER BY key ASC")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        let encoded = row?;
        let Ok(bytes) = hex::decode(encoded.trim()) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            if value.get("agentId").is_some() || value.get("latestRootBlobId").is_some() {
                return Ok(value);
            }
        }
    }
    Ok(Value::Null)
}

fn read_dirs(root: &Path) -> AppResult<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

fn open_readonly(path: &Path) -> AppResult<Connection> {
    if !path.is_file() {
        return Err(AppError::NotFound(format!(
            "Cursor CLI 会话数据库不存在: {}",
            path.to_string_lossy()
        )));
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cc-sessions-cursor-agent-{name}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// 按 protobuf 的 `field 1, wire type 2` 编码一串 32 字节摘要。
    fn encode_root(children: &[[u8; BLOB_ID_LEN]]) -> Vec<u8> {
        let mut out = Vec::new();
        for child in children {
            out.push((1 << 3) | 2);
            out.push(BLOB_ID_LEN as u8);
            out.extend_from_slice(child);
        }
        out
    }

    fn blob_id(seed: u8) -> [u8; BLOB_ID_LEN] {
        [seed; BLOB_ID_LEN]
    }

    fn write_store(dir: &Path, messages: &[Value]) -> AppResult<()> {
        fs::create_dir_all(dir)?;
        let connection = Connection::open(store_path(dir))?;
        connection.execute_batch(
            "CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);",
        )?;
        let mut ids = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            let id = blob_id(index as u8 + 1);
            ids.push(id);
            connection.execute(
                "INSERT INTO blobs VALUES (?1, ?2)",
                rusqlite::params![hex::encode(id), serde_json::to_vec(message)?],
            )?;
        }
        let root = blob_id(0xff);
        connection.execute(
            "INSERT INTO blobs VALUES (?1, ?2)",
            rusqlite::params![hex::encode(root), encode_root(&ids)],
        )?;
        let header = json!({
            "agentId": "agent-1",
            "latestRootBlobId": hex::encode(root),
            "name": "示例会话",
            "mode": "default",
            "lastUsedModel": "composer-2.5",
            "createdAt": 1_700_000_000_000i64,
        });
        connection.execute(
            "INSERT INTO meta VALUES ('0', ?1)",
            [hex::encode(header.to_string())],
        )?;
        Ok(())
    }

    #[test]
    fn child_blob_ids_follows_root_order_and_ignores_other_fields() {
        let mut data = Vec::new();
        // 字段 2 的 varint 与字段 3 的字符串都应被跳过。
        data.push((2 << 3) | 0);
        data.push(0x7f);
        data.push((1 << 3) | 2);
        data.push(BLOB_ID_LEN as u8);
        data.extend_from_slice(&blob_id(0xaa));
        data.push((3 << 3) | 2);
        data.push(3);
        data.extend_from_slice(b"cli");
        data.push((1 << 3) | 2);
        data.push(BLOB_ID_LEN as u8);
        data.extend_from_slice(&blob_id(0xbb));

        let ids = child_blob_ids(&data);
        assert_eq!(
            ids,
            vec![hex::encode(blob_id(0xaa)), hex::encode(blob_id(0xbb))]
        );
    }

    #[test]
    fn child_blob_ids_stops_on_truncated_payload_instead_of_panicking() {
        // 长度前缀声明 32 字节但只剩 3 字节，必须安全停止。
        let data = vec![(1 << 3) | 2, BLOB_ID_LEN as u8, 1, 2, 3];
        assert!(child_blob_ids(&data).is_empty());
    }

    #[test]
    fn load_preview_events_splits_tool_calls_and_results() -> AppResult<()> {
        let dir = temp_root("preview").join("agent-1");
        write_store(
            &dir,
            &[
                json!({"role": "system", "content": "system prompt"}),
                json!({"role": "user", "content": "<user_info>OS darwin</user_info>"}),
                json!({"role": "user", "content": [{"type": "text", "text": "<user_query>\n查一下日志\n</user_query>"}]}),
                json!({"role": "assistant", "content": [
                    {"type": "text", "text": "好的"},
                    {"type": "tool-call", "toolCallId": "tool_1", "toolName": "Shell", "args": {"command": "ls"}}
                ]}),
                json!({"role": "tool", "content": [
                    {"type": "tool-result", "toolCallId": "tool_1", "toolName": "Shell", "result": "a.txt"}
                ]}),
            ],
        )?;

        let events = load_preview_events(&dir)?;
        let roles = events.iter().map(|e| e.role.as_str()).collect::<Vec<_>>();
        // 同一条 assistant 消息里，正文必须排在它触发的工具调用之前。
        assert_eq!(
            roles,
            vec![
                "meta",
                "meta",
                "user",
                "assistant",
                "tool_call",
                "tool_result"
            ]
        );
        assert_eq!(events[2].text_summary, "查一下日志");
        assert_eq!(events[3].text_summary, "好的");
        fs::remove_dir_all(dir.parent().unwrap())?;
        Ok(())
    }

    #[test]
    fn list_sessions_borrows_cwd_from_a_sibling_meta_json() -> AppResult<()> {
        let root = temp_root("list");
        let workspace =
            paths::cursor_agent_chats_dir(&root).join("d41d8cd98f00b204e9800998ecf8427e");
        let with_meta = workspace.join("agent-with-meta");
        let without_meta = workspace.join("agent-without-meta");
        write_store(&with_meta, &[json!({"role": "user", "content": "hi"})])?;
        write_store(&without_meta, &[json!({"role": "user", "content": "hi"})])?;
        fs::write(
            with_meta.join("meta.json"),
            json!({"cwd": "/tmp/demo", "createdAtMs": 1_700_000_000_000i64, "updatedAtMs": 1_700_000_100_000i64})
                .to_string(),
        )?;
        fs::write(
            with_meta.join("prompt_history.json"),
            json!(["/status", "第一句提问"]).to_string(),
        )?;

        let sessions = list_sessions(&root)?;
        assert_eq!(sessions.len(), 2);
        // 没有 meta.json 的会话应当借用同组兄弟的 cwd。
        assert!(sessions.iter().all(|s| s.cwd == "/tmp/demo"));
        assert!(sessions.iter().all(|s| s.provider == "cursor"));
        let with_prompt = sessions
            .iter()
            .find(|s| !s.first_user_message.is_empty())
            .expect("prompt_history 应该被读到");
        // 斜杠命令不能当作首条提问。
        assert_eq!(with_prompt.first_user_message, "第一句提问");
        fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn list_sessions_returns_empty_when_cli_was_never_used() -> AppResult<()> {
        let root = temp_root("absent");
        fs::create_dir_all(&root)?;
        assert!(list_sessions(&root)?.is_empty());
        fs::remove_dir_all(&root)?;
        Ok(())
    }
}
