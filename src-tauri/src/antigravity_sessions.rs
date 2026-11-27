//! Antigravity（`agy` CLI 与 Antigravity 桌面端）会话的只读接入。
//!
//! Antigravity 在本机有两套对称的存储根目录，本模块统一成一个 `antigravity` provider：
//! - **Antigravity CLI (`agy`)**：`~/.gemini/antigravity-cli`
//! - **Antigravity 2.0 App**：`~/.gemini/antigravity`
//!
//! 每个存储目录内部布局一致：
//! - `conversation_summaries.db`：会话摘要索引（标题、概述、步数、更新时间、`workspace_uris`、父子关系）
//! - `brain/<uuid>/.system_generated/logs/transcript_full.jsonl`（旧格式回退 `transcript.jsonl`）：明文 JSONL 对话日志
//! - `annotations/<uuid>.pbtxt`：文本格式 protobuf 注解（自定义标题、`archived` 状态）
//! - `cache/last_conversations.json` / `conversations/<uuid>.db`：工作区路径兜底来源
//!
//! 权威对话状态存在 `conversations/<uuid>.db` 的私有 protobuf `step_payload` 中，
//! `transcript_full.jsonl` 是单向镜像，因此本模块只提供只读查看、搜索与导出。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::models::{PreviewEvent, SessionMetaBrief, SessionSummary, UserPromptList};
use crate::paths;

pub(crate) const PROVIDER: &str = "antigravity";
const LOCATOR_PREFIX: &str = "antigravity:";
const TITLE_MAX_CHARS: usize = 120;
const TRANSCRIPT_CACHE_CAPACITY: usize = 4096;
const TOKEN_SCAN_BUDGET: Duration = Duration::from_millis(300);
const HEAD_SCAN_MAX_LINES: usize = 32;
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionLocator {
    /// `"cli"` = 主目录（`~/.gemini/antigravity-cli` 或用户自定义目录）；
    /// `"app"` = 伴随桌面端目录（`~/.gemini/antigravity`）。
    pub store: String,
    pub root: String,
    pub transcript: String,
    pub session: String,
}

#[derive(Debug, Clone, Default)]
struct SummaryRecord {
    conversation_id: String,
    title: String,
    preview: String,
    step_count: i64,
    last_modified_ms: i64,
    last_user_input_ms: i64,
    cwd: String,
    parent_conversation_id: String,
    agent_name: String,
}

#[derive(Debug, Clone, Default)]
struct AnnotationInfo {
    title: Option<String>,
    archived: bool,
}

#[derive(Debug, Clone, Default)]
struct TranscriptMeta {
    created_at_ms: i64,
    last_event_ms: i64,
    first_user_message: String,
    model: Option<String>,
    tokens_used: i64,
    complete: bool,
}

type TranscriptStamp = (u64, i64);

fn transcript_cache() -> &'static Mutex<HashMap<PathBuf, (TranscriptStamp, TranscriptMeta)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (TranscriptStamp, TranscriptMeta)>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn has_store_data(root: &Path) -> bool {
    root.join("conversation_summaries.db").is_file() || root.join("brain").is_dir()
}

pub fn validate_data_dir(cli_dir: &Path, app_dir: &Path) -> AppResult<u32> {
    Ok(collect_sessions(cli_dir, app_dir)?.len() as u32)
}

pub fn list_sessions(cli_dir: &Path, app_dir: &Path) -> AppResult<Vec<SessionSummary>> {
    if !has_store_data(cli_dir) && !has_store_data(app_dir) {
        return Err(AppError::NotFound(format!(
            "未找到 Antigravity 会话数据：{} 与 {} 都不存在",
            cli_dir.to_string_lossy(),
            app_dir.to_string_lossy()
        )));
    }
    let mut out = collect_sessions(cli_dir, app_dir)?;
    out.sort_by(SessionSummary::compare_activity_desc);
    Ok(out)
}

fn collect_sessions(cli_dir: &Path, app_dir: &Path) -> AppResult<Vec<SessionSummary>> {
    let mut by_id: HashMap<String, SessionSummary> = HashMap::new();
    let deadline = Instant::now() + TOKEN_SCAN_BUDGET;

    if has_store_data(cli_dir) {
        for session in scan_store(cli_dir, "cli", deadline)? {
            by_id.insert(session.id.clone(), session);
        }
    }
    if !same_dir(cli_dir, app_dir) && has_store_data(app_dir) {
        for session in scan_store(app_dir, "app", deadline)? {
            match by_id.get(&session.id) {
                Some(existing) if existing.updated_at >= session.updated_at => {}
                _ => {
                    by_id.insert(session.id.clone(), session);
                }
            }
        }
    }
    Ok(by_id.into_values().collect())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    if let Ok(true) = same_file::is_same_file(a, b) {
        return true;
    }
    let norm = |path: &Path| -> String {
        let cleaned = paths::strip_verbatim(&path.to_string_lossy());
        let trimmed = cleaned.trim_end_matches(['/', '\\']);
        #[cfg(windows)]
        {
            trimmed.replace('/', "\\").to_ascii_lowercase()
        }
        #[cfg(not(windows))]
        {
            trimmed.to_string()
        }
    };
    norm(a) == norm(b)
}

fn scan_store(root: &Path, store: &str, deadline: Instant) -> AppResult<Vec<SessionSummary>> {
    let mut records = load_summaries_db(root);
    let mut seen_ids: HashSet<String> = records
        .iter()
        .map(|record| record.conversation_id.clone())
        .collect();

    // 若会话尚未同步进 conversation_summaries.db（或早期目录无索引库），直接扫 brain/* 补全。
    let brain_dir = root.join("brain");
    if let Ok(entries) = fs::read_dir(&brain_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if id == "tempmediaStorage" || !is_safe_id(&id) || seen_ids.contains(&id) {
                continue;
            }
            if resolve_transcript_path(root, &id).is_some() {
                seen_ids.insert(id.clone());
                records.push(SummaryRecord {
                    conversation_id: id,
                    ..SummaryRecord::default()
                });
            }
        }
    }

    let last_conv_cwds = load_last_conversations_cwds(root);
    let mut out = Vec::new();
    for mut record in records {
        let Some(transcript) = resolve_transcript_path(root, &record.conversation_id) else {
            // 早期 .pb 会话没有明文 JSONL 日志，列表跳过避免预览报错。
            continue;
        };
        let Ok(file_meta) = fs::metadata(&transcript) else {
            continue;
        };
        let rollout_bytes = file_meta.len();
        let file_mtime_ms = modified_millis(&file_meta);
        if record.cwd.is_empty() {
            if let Some(cwd) = last_conv_cwds.get(&record.conversation_id) {
                record.cwd = cwd.clone();
            } else if let Some(cwd) = read_conversation_db_cwd(root, &record.conversation_id) {
                record.cwd = cwd;
            }
        }

        let annotation = read_annotation(root, &record.conversation_id);
        let tmeta = read_transcript_meta(&transcript, rollout_bytes, file_mtime_ms, deadline);

        let updated_ms = [
            record.last_modified_ms,
            record.last_user_input_ms,
            tmeta.last_event_ms,
            file_mtime_ms,
        ]
        .into_iter()
        .max()
        .unwrap_or(0);
        let created_ms = if tmeta.created_at_ms > 0 {
            tmeta.created_at_ms
        } else {
            updated_ms
        };
        let updated_ms = updated_ms.max(created_ms);

        let first_user_message = if !tmeta.first_user_message.trim().is_empty() {
            tmeta.first_user_message
        } else {
            record.preview.clone()
        };
        let title = pick_non_empty(&[
            record.title.as_str(),
            annotation.title.as_deref().unwrap_or(""),
            record.preview.as_str(),
        ])
        .map(|value| truncate_title(&value))
        .or_else(|| {
            let trimmed = first_user_message.trim();
            (!trimmed.is_empty()).then(|| truncate_title(trimmed))
        })
        .unwrap_or_else(|| record.conversation_id.clone());

        let cwd = paths::strip_verbatim(&record.cwd);
        let cwd_display = paths::basename_display(&cwd);
        let parent_id = record.parent_conversation_id.trim();
        let (source, agent_nickname) = if !parent_id.is_empty() {
            let nickname = (!record.agent_name.trim().is_empty())
                .then(|| record.agent_name.trim().to_string());
            (Some(format!("parent:{parent_id}")), nickname)
        } else {
            (Some(store.to_string()), None)
        };
        let resume_command = if store == "cli" {
            crate::fs_ops::session_resume_command(PROVIDER, &record.conversation_id)
        } else {
            String::new()
        };

        out.push(SessionSummary {
            provider: PROVIDER.into(),
            id: record.conversation_id.clone(),
            rollout_path: encode_locator(&SessionLocator {
                store: store.to_string(),
                root: root.to_string_lossy().into_owned(),
                transcript: transcript.to_string_lossy().into_owned(),
                session: record.conversation_id,
            })?,
            cwd,
            cwd_display,
            title,
            first_user_message,
            model: tmeta.model,
            reasoning_effort: None,
            source,
            agent_nickname,
            agent_role: None,
            conversion_origin: None,
            tokens_used: tmeta.tokens_used,
            created_at: created_ms / 1000,
            updated_at: updated_ms / 1000,
            activity_at_ms: (updated_ms > 0).then_some(updated_ms),
            archived: annotation.archived,
            git_branch: None,
            rollout_bytes,
            logs_count: record.step_count.max(0),
            has_backup: false,
            resume_command,
        });
    }
    Ok(out)
}

fn load_summaries_db(root: &Path) -> Vec<SummaryRecord> {
    let db = root.join("conversation_summaries.db");
    let Ok(connection) = open_readonly(&db) else {
        return Vec::new();
    };
    if !table_exists(&connection, "conversation_summaries") {
        return Vec::new();
    }
    let columns = table_columns(&connection, "conversation_summaries");
    if !columns.contains("conversation_id") {
        return Vec::new();
    }

    let parents = load_parent_map(&connection);
    let col_text = |name: &str| -> String {
        if columns.contains(name) {
            format!("COALESCE(CAST({name} AS TEXT), '')")
        } else {
            "''".to_string()
        }
    };
    let col_int = |name: &str| -> String {
        if columns.contains(name) {
            format!("COALESCE({name}, 0)")
        } else {
            "0".to_string()
        }
    };
    let sql = format!(
        "SELECT conversation_id, {}, {}, {}, {}, {}, {}, {}, {} FROM conversation_summaries",
        col_text("title"),
        col_text("preview"),
        col_int("step_count"),
        col_text("last_modified_time"),
        col_text("last_user_input_time"),
        col_text("workspace_uris"),
        col_text("parent_conversation_id"),
        col_text("agent_name"),
    );

    let Ok(mut stmt) = connection.prepare(&sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
        ))
    }) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for row in rows.flatten() {
        let (
            conversation_id,
            title,
            preview,
            step_count,
            last_modified_raw,
            last_user_input_raw,
            workspace_uris,
            mut parent_conversation_id,
            agent_name,
        ) = row;
        if !is_safe_id(&conversation_id) {
            continue;
        }
        if parent_conversation_id.trim().is_empty() {
            if let Some(parent) = parents.get(&conversation_id) {
                parent_conversation_id = parent.clone();
            }
        }
        out.push(SummaryRecord {
            conversation_id,
            title: title.trim().to_string(),
            preview: preview.trim().to_string(),
            step_count,
            last_modified_ms: parse_timestamp_ms(&last_modified_raw).unwrap_or(0),
            last_user_input_ms: parse_timestamp_ms(&last_user_input_raw).unwrap_or(0),
            cwd: parse_workspace_uris(&workspace_uris).unwrap_or_default(),
            parent_conversation_id: parent_conversation_id.trim().to_string(),
            agent_name: agent_name.trim().to_string(),
        });
    }
    out
}

fn load_parent_map(connection: &Connection) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if !table_exists(connection, "conversation_parents") {
        return out;
    }
    let Ok(mut stmt) =
        connection.prepare("SELECT conversation_id, parent_id FROM conversation_parents")
    else {
        return out;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) else {
        return out;
    };
    for (id, parent) in rows.flatten() {
        if !id.trim().is_empty() && !parent.trim().is_empty() {
            out.insert(id, parent);
        }
    }
    out
}

fn load_last_conversations_cwds(root: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let path = root.join("cache").join("last_conversations.json");
    let Ok(raw) = fs::read_to_string(path) else {
        return out;
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&raw) else {
        return out;
    };
    for (cwd, conv_id) in map {
        if let Some(id) = conv_id.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            out.insert(id.to_string(), paths::strip_verbatim(cwd.trim()));
        }
    }
    out
}

fn read_conversation_db_cwd(root: &Path, id: &str) -> Option<String> {
    if !is_safe_id(id) {
        return None;
    }
    let db = root.join("conversations").join(format!("{id}.db"));
    let connection = open_readonly(&db).ok()?;
    if !table_exists(&connection, "trajectory_metadata_blob") {
        return None;
    }
    let blob: Vec<u8> = connection
        .query_row(
            "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'",
            [],
            |row| row.get(0),
        )
        .optional()
        .ok()??;
    let marker = b"file:///";
    let start = blob
        .windows(marker.len())
        .position(|window| window == marker)?;
    let end = blob[start..]
        .iter()
        .position(|&byte| {
            byte < 0x20 || matches!(byte, b'"' | b'\'' | b'<' | b'>' | b'\0' | b' ' | b'\t')
        })
        .map(|offset| start + offset)
        .unwrap_or(blob.len());
    let uri = std::str::from_utf8(&blob[start..end]).ok()?;
    file_uri_to_local_path(uri)
}

fn resolve_transcript_path(root: &Path, id: &str) -> Option<PathBuf> {
    if !is_safe_id(id) {
        return None;
    }
    let logs = root.join("brain").join(id).join(".system_generated").join("logs");
    let full = logs.join("transcript_full.jsonl");
    if full.is_file() {
        return Some(full);
    }
    let compact = logs.join("transcript.jsonl");
    compact.is_file().then_some(compact)
}

fn is_safe_id(id: &str) -> bool {
    let trimmed = id.trim();
    !trimmed.is_empty()
        && trimmed == id
        && !trimmed.contains('/')
        && !trimmed.contains('\\')
        && !trimmed.contains(':')
        && trimmed != "."
        && trimmed != ".."
        && !trimmed.chars().any(char::is_control)
}

fn read_transcript_meta(
    path: &Path,
    bytes: u64,
    mtime_ms: i64,
    deadline: Instant,
) -> TranscriptMeta {
    let stamp = (bytes, mtime_ms);
    {
        let cache = transcript_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((cached_stamp, meta)) = cache.get(path) {
            if *cached_stamp == stamp && (meta.complete || Instant::now() >= deadline) {
                return meta.clone();
            }
        }
    }

    let scan_full = Instant::now() < deadline;
    let meta = scan_transcript_file(path, scan_full).unwrap_or_default();

    let mut cache = transcript_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if cache.len() >= TRANSCRIPT_CACHE_CAPACITY {
        cache.clear();
    }
    cache.insert(path.to_path_buf(), (stamp, meta.clone()));
    meta
}

fn scan_transcript_file(path: &Path, full: bool) -> AppResult<TranscriptMeta> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut meta = TranscriptMeta {
        complete: full,
        ..TranscriptMeta::default()
    };

    for (line_no, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(ts) = obj.get("created_at").and_then(Value::as_str).and_then(parse_timestamp_ms)
        {
            if meta.created_at_ms == 0 {
                meta.created_at_ms = ts;
            }
            meta.last_event_ms = ts;
        }
        match obj.get("type").and_then(Value::as_str).unwrap_or("") {
            "USER_INPUT" if meta.first_user_message.is_empty() => {
                if let Some(content) = obj.get("content").and_then(Value::as_str) {
                    let cleaned = unwrap_user_request(content);
                    if !cleaned.is_empty() {
                        meta.first_user_message = cleaned;
                    }
                }
            }
            "PLANNER_RESPONSE" => {
                if let Some(model) = obj
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                {
                    meta.model = Some(model.to_string());
                }
                if full {
                    meta.tokens_used += step_tokens(&obj);
                }
            }
            "CHECKPOINT" if full => {
                meta.tokens_used += step_tokens(&obj);
            }
            _ => {}
        }

        if !full
            && line_no + 1 >= HEAD_SCAN_MAX_LINES
            && meta.created_at_ms > 0
            && !meta.first_user_message.is_empty()
            && meta.model.is_some()
        {
            break;
        }
    }
    Ok(meta)
}

fn step_tokens(obj: &Value) -> i64 {
    let input = obj.get("input_tokens").and_then(Value::as_i64).unwrap_or(0).max(0);
    let cache = obj
        .get("cache_read_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let output = obj
        .get("output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    input.saturating_add(cache).saturating_add(output)
}

fn read_annotation(root: &Path, id: &str) -> AnnotationInfo {
    if !is_safe_id(id) {
        return AnnotationInfo::default();
    }
    let path = root.join("annotations").join(format!("{id}.pbtxt"));
    let Ok(content) = fs::read_to_string(path) else {
        return AnnotationInfo::default();
    };
    parse_pbtxt_annotation(&content)
}

/// 解析 `annotations/<id>.pbtxt` 中的顶层 `title` 与 `archived` 字段。
fn parse_pbtxt_annotation(content: &str) -> AnnotationInfo {
    let mut info = AnnotationInfo::default();
    let bytes = content.as_bytes();
    let mut i = 0usize;
    let mut depth = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'{' {
            depth += 1;
            i += 1;
            continue;
        }
        if b == b'}' {
            depth = depth.saturating_sub(1);
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' {
            let _ = parse_pbtxt_string(bytes, &mut i);
            continue;
        }
        if is_ident_start(b) {
            let start = i;
            while i < bytes.len() && is_ident_char(bytes[i]) {
                i += 1;
            }
            let key = &content[start..i];
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b':' {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if depth == 0 && key == "archived" {
                    let val_start = i;
                    while i < bytes.len() && is_ident_char(bytes[i]) {
                        i += 1;
                    }
                    let val = &content[val_start..i];
                    info.archived = val == "true" || val == "1";
                } else if depth == 0 && key == "title" && i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                    let mut combined = String::new();
                    while i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                        combined.push_str(&parse_pbtxt_string(bytes, &mut i));
                        let mut peek = i;
                        while peek < bytes.len() && bytes[peek].is_ascii_whitespace() {
                            peek += 1;
                        }
                        if peek < bytes.len() && (bytes[peek] == b'"' || bytes[peek] == b'\'') {
                            i = peek;
                        } else {
                            break;
                        }
                    }
                    if !combined.trim().is_empty() {
                        info.title = Some(combined.trim().to_string());
                    }
                }
            }
            continue;
        }
        i += 1;
    }
    info
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn parse_pbtxt_string(bytes: &[u8], idx: &mut usize) -> String {
    let quote = bytes[*idx];
    *idx += 1;
    let mut raw = Vec::new();
    while *idx < bytes.len() {
        let b = bytes[*idx];
        *idx += 1;
        if b == quote {
            break;
        }
        if b != b'\\' || *idx >= bytes.len() {
            raw.push(b);
            continue;
        }
        let esc = bytes[*idx];
        *idx += 1;
        match esc {
            b'n' => raw.push(b'\n'),
            b'r' => raw.push(b'\r'),
            b't' => raw.push(b'\t'),
            b'\\' => raw.push(b'\\'),
            b'"' => raw.push(b'"'),
            b'\'' => raw.push(b'\''),
            b'x' if *idx + 2 <= bytes.len() => {
                if let Ok(hex) = std::str::from_utf8(&bytes[*idx..*idx + 2]) {
                    if let Ok(val) = u8::from_str_radix(hex, 16) {
                        raw.push(val);
                        *idx += 2;
                        continue;
                    }
                }
                raw.push(b'x');
            }
            b'0'..=b'7' => {
                let mut val = (esc - b'0') as u32;
                for _ in 0..2 {
                    if *idx < bytes.len() && matches!(bytes[*idx], b'0'..=b'7') {
                        val = val * 8 + (bytes[*idx] - b'0') as u32;
                        *idx += 1;
                    } else {
                        break;
                    }
                }
                raw.push((val & 0xff) as u8);
            }
            other => raw.push(other),
        }
    }
    String::from_utf8_lossy(&raw).into_owned()
}

// ---------------------------------------------------------------------------
// 预览、时间线、元信息
// ---------------------------------------------------------------------------

pub fn preview_range(locator: &str, offset: usize, limit: usize) -> AppResult<Vec<PreviewEvent>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let decoded = decode_locator(locator)?;
    let needed = offset.saturating_add(limit);
    let events = stream_transcript_events(&decoded, Some(needed))?;
    Ok(events.into_iter().skip(offset).take(limit).collect())
}

pub(crate) fn load_preview_events_from_locator(locator: &str) -> AppResult<Vec<PreviewEvent>> {
    let decoded = decode_locator(locator)?;
    stream_transcript_events(&decoded, None)
}

pub fn preview_user_prompts(locator: &str) -> AppResult<UserPromptList> {
    let events = load_preview_events_from_locator(locator)?;
    Ok(crate::rollout::user_prompts_from_events(events, |event| {
        event.kind == "turn_error"
            || matches!(event.role.as_str(), "assistant" | "reasoning" | "tool_call")
    }))
}

pub fn preview_meta(locator: &str) -> AppResult<SessionMetaBrief> {
    let decoded = decode_locator(locator)?;
    let root = Path::new(&decoded.root);
    let transcript = Path::new(&decoded.transcript);

    let summary = load_summaries_db(root)
        .into_iter()
        .find(|row| row.conversation_id == decoded.session);

    let mut cwd = summary
        .as_ref()
        .map(|row| row.cwd.clone())
        .unwrap_or_default();
    if cwd.is_empty() {
        cwd = load_last_conversations_cwds(root)
            .remove(&decoded.session)
            .or_else(|| read_conversation_db_cwd(root, &decoded.session))
            .unwrap_or_default();
    }
    let cwd = paths::strip_verbatim(&cwd);

    let meta = scan_transcript_file(transcript, false).unwrap_or_default();
    let timestamp = if meta.created_at_ms > 0 {
        timestamp_from_millis(meta.created_at_ms)
    } else {
        summary
            .as_ref()
            .and_then(|row| {
                let ms = row.last_modified_ms.max(row.last_user_input_ms);
                (ms > 0).then(|| timestamp_from_millis(ms)).flatten()
            })
    };

    Ok(SessionMetaBrief {
        id: Some(decoded.session),
        timestamp,
        cwd: (!cwd.is_empty()).then_some(cwd),
        originator: Some(if decoded.store == "cli" {
            "Antigravity CLI".into()
        } else {
            "Antigravity App".into()
        }),
        cli_version: None,
        source: transcript
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string),
        model_provider: meta.model,
    })
}

fn stream_transcript_events(
    locator: &SessionLocator,
    stop_after: Option<usize>,
) -> AppResult<Vec<PreviewEvent>> {
    let file = File::open(&locator.transcript)?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut pending_tools: VecDeque<(String, String)> = VecDeque::new();

    for (line_idx, line) in reader.lines().enumerate() {
        if stop_after.is_some_and(|needed| events.len() >= needed) {
            break;
        }
        let line = line?;
        crate::operation_metrics::record(|c| {
            c.read_bytes += line.len() as u64;
            c.parsed_lines += 1;
        });
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        expand_transcript_step(&obj, line_idx, &locator.store, &mut pending_tools, &mut events);
    }
    Ok(events)
}

fn expand_transcript_step(
    obj: &Value,
    line_idx: usize,
    store: &str,
    pending_tools: &mut VecDeque<(String, String)>,
    events: &mut Vec<PreviewEvent>,
) {
    let step_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
    let step_index = obj
        .get("step_index")
        .and_then(Value::as_u64)
        .unwrap_or(line_idx as u64);
    let created_at = obj
        .get("created_at")
        .and_then(Value::as_str)
        .unwrap_or("");
    let status = obj.get("status").and_then(Value::as_str).unwrap_or("");
    let mut sub_idx = 0usize;

    let mut next_uuid = || {
        let uuid = format!("ag:{step_index}:{sub_idx}");
        sub_idx += 1;
        uuid
    };

    match step_type {
        "USER_INPUT" => {
            pending_tools.clear();
            let raw_content = obj.get("content").and_then(Value::as_str).unwrap_or("");
            let text = unwrap_user_request(raw_content);
            let media = extract_media_attachments(obj.get("media"));
            if text.is_empty() && media.is_empty() {
                return;
            }
            let uuid = next_uuid();
            let mut origin = json!({
                "store": store,
                "step_index": step_index,
                "step_type": step_type,
            });
            if !media.is_empty() {
                origin["media"] = Value::Array(media);
            }
            push_claude_event(
                events,
                json!({
                    "type": "user",
                    "timestamp": created_at,
                    "uuid": uuid,
                    "message": {
                        "role": "user",
                        "content": [{ "type": "text", "text": text }],
                    },
                    "antigravity": origin,
                }),
            );
        }
        "PLANNER_RESPONSE" => {
            let model = obj.get("model").and_then(Value::as_str);
            let input_tokens = obj.get("input_tokens").and_then(Value::as_i64);
            let cache_read_tokens = obj.get("cache_read_tokens").and_then(Value::as_i64);
            let output_tokens = obj.get("output_tokens").and_then(Value::as_i64);
            let base_origin = || {
                let mut origin = json!({
                    "store": store,
                    "step_index": step_index,
                    "step_type": step_type,
                });
                if let Some(m) = model {
                    origin["model"] = json!(m);
                }
                origin
            };

            if let Some(thinking) = obj
                .get("thinking")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let uuid = next_uuid();
                push_claude_event(
                    events,
                    json!({
                        "type": "assistant",
                        "timestamp": created_at,
                        "uuid": uuid,
                        "message": {
                            "role": "assistant",
                            "content": [{ "type": "thinking", "thinking": thinking }],
                        },
                        "antigravity": base_origin(),
                    }),
                );
            }

            if let Some(content) = obj
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let uuid = next_uuid();
                let mut origin = base_origin();
                if let Some(v) = input_tokens {
                    origin["input_tokens"] = json!(v);
                }
                if let Some(v) = cache_read_tokens {
                    origin["cache_read_tokens"] = json!(v);
                }
                if let Some(v) = output_tokens {
                    origin["output_tokens"] = json!(v);
                }
                push_claude_event(
                    events,
                    json!({
                        "type": "assistant",
                        "timestamp": created_at,
                        "uuid": uuid,
                        "message": {
                            "role": "assistant",
                            "content": [{ "type": "text", "text": content }],
                        },
                        "antigravity": origin,
                    }),
                );
            }

            if let Some(calls) = obj.get("tool_calls").and_then(Value::as_array) {
                for (call_idx, call) in calls.iter().enumerate() {
                    let name = call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let tool_id = format!("ag_{step_index}_{call_idx}");
                    pending_tools.push_back((tool_id.clone(), name.clone()));
                    let input = normalize_tool_args(call.get("args"));
                    let uuid = next_uuid();
                    push_claude_event(
                        events,
                        json!({
                            "type": "assistant",
                            "timestamp": created_at,
                            "uuid": uuid,
                            "message": {
                                "role": "assistant",
                                "content": [{
                                    "type": "tool_use",
                                    "id": tool_id,
                                    "name": name,
                                    "input": input,
                                }],
                            },
                            "antigravity": base_origin(),
                        }),
                    );
                }
            }
        }
        "GENERIC" => {
            let (tool_id, tool_name) = pending_tools.pop_front().unwrap_or_default();
            let content = obj.get("content").and_then(Value::as_str).unwrap_or("");
            let error_str = obj
                .get("error")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let media = extract_media_attachments(obj.get("media"));
            let result_text = combine_content_and_error(content, error_str);
            let is_error = error_str.is_some() || status.eq_ignore_ascii_case("ERROR");
            let uuid = next_uuid();
            let mut origin = json!({
                "store": store,
                "step_index": step_index,
                "step_type": step_type,
                "status": status,
            });
            if !tool_name.is_empty() {
                origin["tool_name"] = json!(tool_name);
            }
            if !media.is_empty() {
                origin["media"] = Value::Array(media);
            }
            push_claude_event(
                events,
                json!({
                    "type": "user",
                    "timestamp": created_at,
                    "uuid": uuid,
                    "message": {
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": result_text,
                            "is_error": is_error,
                        }],
                    },
                    "antigravity": origin,
                }),
            );
        }
        "ERROR_MESSAGE" => {
            let err_text = pick_non_empty(&[
                obj.get("error").and_then(Value::as_str).unwrap_or(""),
                obj.get("content").and_then(Value::as_str).unwrap_or(""),
            ])
            .unwrap_or_else(|| "未知错误".to_string());
            let uuid = next_uuid();
            let idx = events.len();
            events.push(PreviewEvent {
                index: idx,
                timestamp: created_at.to_string(),
                role: "other".into(),
                kind: "turn_error".into(),
                text_summary: truncate_summary(&err_text, 200),
                raw: json!({
                    "type": "turn_error",
                    "timestamp": created_at,
                    "uuid": uuid,
                    "error": obj.get("error").cloned().unwrap_or(Value::Null),
                    "content": obj.get("content").cloned().unwrap_or(Value::Null),
                    "antigravity": {
                        "store": store,
                        "step_index": step_index,
                        "step_type": step_type,
                    },
                }),
            });
        }
        "CHECKPOINT" | "SYSTEM_MESSAGE" => {
            let content = obj.get("content").and_then(Value::as_str).unwrap_or("");
            let kind = if step_type == "CHECKPOINT" {
                "checkpoint"
            } else {
                "system_message"
            };
            let uuid = next_uuid();
            push_claude_event(
                events,
                json!({
                    "type": kind,
                    "timestamp": created_at,
                    "uuid": uuid,
                    "isMeta": true,
                    "content": content,
                    "antigravity": {
                        "store": store,
                        "step_index": step_index,
                        "step_type": step_type,
                    },
                }),
            );
        }
        other => {
            let content = obj.get("content").and_then(Value::as_str).unwrap_or("");
            let uuid = next_uuid();
            push_claude_event(
                events,
                json!({
                    "type": if other.is_empty() { "unknown" } else { other },
                    "timestamp": created_at,
                    "uuid": uuid,
                    "isMeta": true,
                    "content": content,
                    "antigravity": {
                        "store": store,
                        "step_index": step_index,
                        "step_type": step_type,
                    },
                }),
            );
        }
    }
}

fn push_claude_event(events: &mut Vec<PreviewEvent>, raw: Value) {
    if let Some(event) = crate::claude_sessions::classify_preview(events.len(), raw) {
        events.push(event);
    }
}

fn combine_content_and_error(content: &str, error: Option<&str>) -> String {
    let trimmed = content.trim();
    let Some(err) = error else {
        return content.to_string();
    };
    if trimmed.is_empty() {
        return format!("Error: {err}");
    }
    if trimmed.contains(err) {
        return content.to_string();
    }
    format!("{trimmed}\n\nError: {err}")
}

fn normalize_tool_args(args: Option<&Value>) -> Value {
    let Some(Value::Object(map)) = args else {
        return args.cloned().unwrap_or(Value::Null);
    };
    let mut out = serde_json::Map::with_capacity(map.len());
    for (key, value) in map {
        let normalized = match value {
            Value::String(raw) => serde_json::from_str::<Value>(raw).unwrap_or_else(|_| value.clone()),
            other => other.clone(),
        };
        out.insert(key.clone(), normalized);
    }
    Value::Object(out)
}

fn extract_media_attachments(media: Option<&Value>) -> Vec<Value> {
    let Some(Value::Array(items)) = media else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let mime = item.get("mime_type").and_then(Value::as_str).unwrap_or("");
        let Some(uri) = item.get("uri").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let is_image = mime.starts_with("image/") || has_image_extension(uri);
        if !is_image {
            continue;
        }
        let local_path = if uri.len() >= 7 && uri[..7].eq_ignore_ascii_case("file://") {
            file_uri_to_local_path(uri).unwrap_or_else(|| uri.to_string())
        } else {
            paths::strip_verbatim(uri)
        };
        let name = paths::basename_display(&local_path);
        out.push(json!({
            "name": name,
            "path": local_path,
            "mime_type": mime,
        }));
    }
    out
}

fn has_image_extension(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
}

/// 剥离 Antigravity CLI 注入的 `<USER_REQUEST>` / `<ADDITIONAL_METADATA>` / `<USER_SETTINGS_CHANGE>` 外壳。
pub(crate) fn unwrap_user_request(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n");
    if let Some(start) = normalized.find("<USER_REQUEST>") {
        let after_start = &normalized[start + "<USER_REQUEST>".len()..];
        let search_limit = after_start
            .find("<ADDITIONAL_METADATA>")
            .or_else(|| after_start.find("<USER_SETTINGS_CHANGE>"))
            .unwrap_or(after_start.len());
        if let Some(end) = after_start[..search_limit].rfind("</USER_REQUEST>") {
            let inner = after_start[..end].trim();
            if !inner.is_empty() {
                return inner.to_string();
            }
        }
    }
    let mut cleaned = strip_xml_block(&normalized, "ADDITIONAL_METADATA");
    cleaned = strip_xml_block(&cleaned, "USER_SETTINGS_CHANGE");
    cleaned.trim().to_string()
}

fn strip_xml_block(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = text.to_string();
    while let Some(start) = out.find(&open) {
        if let Some(end_rel) = out[start..].find(&close) {
            let end = start + end_rel + close.len();
            out.replace_range(start..end, "");
        } else {
            out.truncate(start);
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// URI、时间、定位符与只读 SQLite 工具
// ---------------------------------------------------------------------------

fn parse_workspace_uris(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(trimmed) {
        for item in items {
            if let Some(uri) = item.as_str() {
                if let Some(path) = file_uri_to_local_path(uri) {
                    return Some(path);
                }
            }
        }
    }
    file_uri_to_local_path(trimmed)
}

/// 把 `file:///f%3A/hanweb/%E5%AE%81%E6%B3%A2` 正确解码为 UTF-8 本地路径。
///
/// 必须先按字节还原百分号编码再做 UTF-8 解码，不能按单字节 `char` 拼接；
/// 同时清理 Windows 驱动器号前的 `/`。
pub(crate) fn file_uri_to_local_path(uri: &str) -> Option<String> {
    let trimmed = uri.trim();
    if trimmed.len() < 7 || !trimmed[..7].eq_ignore_ascii_case("file://") {
        return None;
    }
    let mut rest = &trimmed[7..];
    let has_host = !rest.starts_with('/');
    if !has_host {
        // `file:///path` -> rest starts with `/`
    } else if rest.len() >= 10 && rest[..10].eq_ignore_ascii_case("localhost/") {
        rest = &rest[9..];
    }

    let mut raw_bytes = Vec::with_capacity(rest.len());
    let src = rest.as_bytes();
    let mut idx = 0usize;
    while idx < src.len() {
        if src[idx] == b'%' && idx + 2 < src.len() {
            if let Ok(hex_str) = std::str::from_utf8(&src[idx + 1..idx + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex_str, 16) {
                    raw_bytes.push(byte);
                    idx += 3;
                    continue;
                }
            }
        }
        raw_bytes.push(src[idx]);
        idx += 1;
    }
    let mut decoded = String::from_utf8_lossy(&raw_bytes).into_owned();
    if has_host && !decoded.starts_with("//") {
        decoded = format!("//{decoded}");
    }
    let bytes = decoded.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        decoded.remove(0);
    }
    #[cfg(windows)]
    {
        let b = decoded.as_bytes();
        let is_drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
        let is_unc = decoded.starts_with("//");
        if is_drive || is_unc {
            decoded = decoded.replace('/', "\\");
        }
    }
    let cleaned = paths::strip_verbatim(&decoded);
    (!cleaned.trim().is_empty()).then_some(cleaned)
}

/// 解析 Go `time.Time` 字符串（如 `2026-11-27 03:49:29.2010986+00:00`）与标准 RFC 3339。
pub(crate) fn parse_timestamp_ms(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(num) = trimmed.parse::<i64>() {
            if num <= 0 {
                return None;
            }
            return Some(if num >= 1_000_000_000_000_000 {
                num / 1_000_000
            } else if num >= 1_000_000_000_000 {
                num
            } else {
                num * 1000
            });
        }
    }
    let mut normalized = trimmed.to_string();
    let bytes = normalized.as_bytes();
    if bytes.len() >= 19 && bytes[10] == b' ' {
        // Go `2006-01-02 15:04:05.999999999+00:00` 可能是空格分隔，归一化成 `T`
        unsafe {
            normalized.as_bytes_mut()[10] = b'T';
        }
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(&normalized) {
        return Some(dt.timestamp_millis());
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(trimmed, fmt) {
            return Some(naive.and_utc().timestamp_millis());
        }
    }
    None
}

fn timestamp_from_millis(millis: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(millis)
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn modified_millis(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|delta| delta.as_millis() as i64)
        .unwrap_or(0)
}

fn pick_non_empty(candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

fn truncate_title(text: &str) -> String {
    let single_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_summary(&single_line, TITLE_MAX_CHARS)
}

fn truncate_summary(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn encode_locator(locator: &SessionLocator) -> AppResult<String> {
    let raw = serde_json::to_vec(locator)?;
    Ok(format!("{LOCATOR_PREFIX}{}", URL_SAFE_NO_PAD.encode(raw)))
}

pub(crate) fn decode_locator(value: &str) -> AppResult<SessionLocator> {
    let encoded = value
        .strip_prefix(LOCATOR_PREFIX)
        .ok_or_else(|| AppError::Path("Antigravity 会话定位符格式无效".into()))?;
    let raw = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|error| AppError::Path(format!("Antigravity 会话定位符无法解码: {error}")))?;
    let locator: SessionLocator = serde_json::from_slice(&raw)?;
    if locator.session.trim().is_empty() {
        return Err(AppError::Path("Antigravity 会话定位符缺少会话 id".into()));
    }
    if !Path::new(&locator.transcript).is_file() {
        return Err(AppError::Path(format!(
            "Antigravity 会话日志不存在: {}",
            locator.transcript
        )));
    }
    Ok(locator)
}

fn open_readonly(path: &Path) -> AppResult<Connection> {
    if !path.is_file() {
        return Err(AppError::NotFound(format!(
            "Antigravity 数据库不存在: {}",
            path.to_string_lossy()
        )));
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let _ = connection.busy_timeout(SQLITE_BUSY_TIMEOUT);
    Ok(connection)
}

fn table_exists(connection: &Connection, table: &str) -> bool {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(false)
}

fn table_columns(connection: &Connection, table: &str) -> HashSet<String> {
    let mut cols = HashSet::new();
    let Ok(mut stmt) = connection.prepare(&format!("PRAGMA table_info({table})")) else {
        return cols;
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) else {
        return cols;
    };
    for name in rows.flatten() {
        cols.insert(name);
    }
    cols
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "cc-sessions-antigravity-{name}-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create test root");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_transcript(root: &Path, id: &str, lines: &[Value], compact_only: bool) -> PathBuf {
        let dir = root
            .join("brain")
            .join(id)
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&dir).expect("create logs dir");
        let name = if compact_only {
            "transcript.jsonl"
        } else {
            "transcript_full.jsonl"
        };
        let file = dir.join(name);
        let body = lines
            .iter()
            .map(|value| serde_json::to_string(value).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&file, format!("{body}\n")).expect("write transcript");
        file
    }

    fn create_summaries_db(root: &Path, rows: &[(&str, &str, &str, i64, &str, &str, &str, &str)]) {
        let db = root.join("conversation_summaries.db");
        let conn = Connection::open(db).expect("create summaries db");
        conn.execute(
            "CREATE TABLE conversation_summaries (
                conversation_id TEXT PRIMARY KEY,
                title TEXT NOT NULL DEFAULT '',
                preview TEXT NOT NULL DEFAULT '',
                step_count INTEGER NOT NULL DEFAULT 0,
                last_modified_time DATETIME NOT NULL,
                workspace_uris TEXT NOT NULL DEFAULT '',
                parent_conversation_id TEXT NOT NULL DEFAULT '',
                agent_name TEXT NOT NULL DEFAULT '',
                last_user_input_time DATETIME NOT NULL DEFAULT ''
            )",
            [],
        )
        .expect("create table");
        for &(id, title, preview, steps, mod_time, uris, parent, agent) in rows {
            conn.execute(
                "INSERT INTO conversation_summaries
                 (conversation_id, title, preview, step_count, last_modified_time, workspace_uris, parent_conversation_id, agent_name, last_user_input_time)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?5)",
                rusqlite::params![id, title, preview, steps, mod_time, uris, parent, agent],
            )
            .expect("insert summary");
        }
    }

    #[test]
    fn parses_go_timestamps_and_utf8_file_uris() {
        let ts = parse_timestamp_ms("2026-11-27 03:49:29.2010986+00:00").expect("go timestamp");
        assert_eq!(ts / 1000, 1795751369);
        assert_eq!(
            parse_timestamp_ms("2026-11-25T01:58:11Z").expect("rfc3339"),
            1795571891000
        );

        let decoded = file_uri_to_local_path(
            "file:///f%3A/hanweb/project/%E5%AE%81%E6%B3%A2%E4%BC%81%E4%B8%9A/app",
        )
        .expect("utf8 file uri");
        #[cfg(windows)]
        assert_eq!(decoded, r"f:\hanweb\project\宁波企业\app");
        #[cfg(not(windows))]
        assert_eq!(decoded, "f:/hanweb/project/宁波企业/app");
    }

    #[test]
    fn lists_cli_and_app_stores_deduplicating_same_dir_and_ids() {
        let tmp = TestDir::new("dual-store");
        let cli_dir = tmp.path().join("antigravity-cli");
        let app_dir = tmp.path().join("antigravity");
        fs::create_dir_all(&cli_dir).unwrap();
        fs::create_dir_all(&app_dir).unwrap();

        let shared_id = "11111111-1111-1111-1111-111111111111";
        let cli_only_id = "22222222-2222-2222-2222-222222222222";
        let app_only_id = "33333333-3333-3333-3333-333333333333";
        let legacy_pb_id = "44444444-4444-4444-4444-444444444444";

        write_transcript(
            &cli_dir,
            shared_id,
            &[
                json!({"step_index": 0, "type": "USER_INPUT", "created_at": "2026-11-27T01:00:00Z", "content": "<USER_REQUEST>\ncli prompt\n</USER_REQUEST>"}),
                json!({"step_index": 1, "type": "PLANNER_RESPONSE", "created_at": "2026-11-27T01:00:05Z", "model": "gemini-4-neon-xhigh", "input_tokens": 100, "cache_read_tokens": 50, "output_tokens": 25, "content": "done"}),
            ],
            false,
        );
        write_transcript(
            &cli_dir,
            cli_only_id,
            &[json!({"step_index": 0, "type": "USER_INPUT", "created_at": "2026-11-26T01:00:00Z", "content": "<USER_REQUEST>\ncli subagent\n</USER_REQUEST>"})],
            false,
        );
        create_summaries_db(
            &cli_dir,
            &[
                (
                    shared_id,
                    "Shared CLI",
                    "Shared CLI",
                    2,
                    "2026-11-27 01:00:10.000000+00:00",
                    r#"["file:///tmp/cli-proj"]"#,
                    "",
                    "",
                ),
                (
                    cli_only_id,
                    "Subagent Task",
                    "",
                    1,
                    "2026-11-26 01:00:00+00:00",
                    r#"["file:///tmp/cli-proj"]"#,
                    shared_id,
                    "Researcher",
                ),
            ],
        );

        // App dir has older copy of shared_id, unique app_only_id, and legacy .pb-only row without transcript
        write_transcript(
            &app_dir,
            shared_id,
            &[json!({"step_index": 0, "type": "USER_INPUT", "created_at": "2026-11-25T01:00:00Z", "content": "old app"})],
            false,
        );
        write_transcript(
            &app_dir,
            app_only_id,
            &[json!({"step_index": 0, "type": "USER_INPUT", "created_at": "2026-11-27T02:00:00Z", "content": "<USER_REQUEST>\napp task\n</USER_REQUEST>"})],
            true,
        );
        create_summaries_db(
            &app_dir,
            &[
                (
                    shared_id,
                    "Older App Copy",
                    "",
                    1,
                    "2026-11-25 01:00:00+00:00",
                    r#"["file:///tmp/app-proj"]"#,
                    "",
                    "",
                ),
                (
                    app_only_id,
                    "",
                    "",
                    1,
                    "2026-11-27 02:00:00+00:00",
                    r#"["file:///tmp/app-only"]"#,
                    "",
                    "",
                ),
                (
                    legacy_pb_id,
                    "Legacy PB Only",
                    "",
                    4,
                    "2026-11-20 01:00:00+00:00",
                    r#"["file:///tmp/legacy"]"#,
                    "",
                    "",
                ),
            ],
        );
        fs::create_dir_all(app_dir.join("annotations")).unwrap();
        fs::write(
            app_dir.join("annotations").join(format!("{app_only_id}.pbtxt")),
            "title:\"Annotated App Title\" archived:true\n",
        )
        .unwrap();

        let list = list_sessions(&cli_dir, &app_dir).expect("list sessions");
        assert_eq!(list.len(), 3);
        assert_eq!(
            validate_data_dir(&cli_dir, &app_dir).unwrap(),
            list.len() as u32
        );

        let app_item = list.iter().find(|s| s.id == app_only_id).unwrap();
        assert_eq!(app_item.title, "Annotated App Title");
        assert!(app_item.archived);
        assert_eq!(app_item.resume_command, "");
        assert_eq!(app_item.source.as_deref(), Some("app"));
        assert!(app_item.agent_nickname.is_none());

        let shared_item = list.iter().find(|s| s.id == shared_id).unwrap();
        assert_eq!(shared_item.title, "Shared CLI");
        assert_eq!(shared_item.tokens_used, 175);
        assert_eq!(shared_item.model.as_deref(), Some("gemini-4-neon-xhigh"));
        assert_eq!(shared_item.resume_command, format!("agy --conversation {shared_id}"));
        assert_eq!(shared_item.source.as_deref(), Some("cli"));
        assert!(shared_item.agent_nickname.is_none());

        let subagent_item = list.iter().find(|s| s.id == cli_only_id).unwrap();
        assert_eq!(
            subagent_item.source.as_deref(),
            Some(format!("parent:{shared_id}").as_str())
        );
        assert_eq!(subagent_item.agent_nickname.as_deref(), Some("Researcher"));

        // Passing the same directory for both cli_dir and app_dir must not duplicate or fail
        let same_list = list_sessions(&cli_dir, &cli_dir).unwrap();
        assert_eq!(same_list.len(), 2);
    }

    #[test]
    fn falls_back_to_brain_and_last_conversations_without_summaries_db() {
        let tmp = TestDir::new("brain-fallback");
        let cli_dir = tmp.path().join("antigravity-cli");
        let app_dir = tmp.path().join("antigravity");
        let id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        write_transcript(
            &cli_dir,
            id,
            &[json!({"step_index": 0, "type": "USER_INPUT", "created_at": "2026-11-27T01:00:00Z", "content": "<USER_REQUEST>\nhello fallback\n</USER_REQUEST>"})],
            false,
        );
        fs::create_dir_all(cli_dir.join("cache")).unwrap();
        fs::write(
            cli_dir.join("cache").join("last_conversations.json"),
            json!({ "/work/my-repo": id }).to_string(),
        )
        .unwrap();

        let list = list_sessions(&cli_dir, &app_dir).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].cwd, "/work/my-repo");
        assert_eq!(list[0].title, "hello fallback");
        assert_eq!(list[0].first_user_message, "hello fallback");
    }

    #[test]
    fn streams_preview_events_timeline_and_meta() {
        let tmp = TestDir::new("preview");
        let cli_dir = tmp.path().join("antigravity-cli");
        let app_dir = tmp.path().join("antigravity");
        let id = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        write_transcript(
            &cli_dir,
            id,
            &[
                json!({
                    "step_index": 0,
                    "type": "USER_INPUT",
                    "created_at": "2026-11-27T01:00:00Z",
                    "content": "<USER_REQUEST>\ncheck image\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nmeta\n</ADDITIONAL_METADATA>",
                    "media": [{"mime_type": "image/png", "uri": "C:/tmp/shot.png"}]
                }),
                json!({
                    "step_index": 1,
                    "type": "PLANNER_RESPONSE",
                    "created_at": "2026-11-27T01:00:01Z",
                    "model": "gemini-4-neon-xhigh",
                    "thinking": "let me read",
                    "tool_calls": [{"name": "read_file", "args": {"Path": "\"/work/a.rs\"", "StartLine": "1"}}]
                }),
                json!({
                    "step_index": 2,
                    "type": "GENERIC",
                    "status": "ERROR",
                    "error": "file not found",
                    "created_at": "2026-11-27T01:00:02Z",
                    "content": "read failed"
                }),
                json!({
                    "step_index": 3,
                    "type": "CHECKPOINT",
                    "created_at": "2026-11-27T01:00:03Z",
                    "content": "compacted"
                }),
                json!({
                    "step_index": 4,
                    "type": "PLANNER_RESPONSE",
                    "created_at": "2026-11-27T01:00:04Z",
                    "model": "gemini-4-neon-xhigh",
                    "content": "Here is the answer."
                }),
                json!({
                    "step_index": 5,
                    "type": "ERROR_MESSAGE",
                    "created_at": "2026-11-27T01:00:05Z",
                    "error": "stream interrupted"
                }),
            ],
            false,
        );

        let list = list_sessions(&cli_dir, &app_dir).unwrap();
        let locator = &list[0].rollout_path;
        let events = preview_range(locator, 0, 50).unwrap();
        assert_eq!(events.len(), 7);
        assert_eq!(events[0].role, "user");
        assert_eq!(events[0].text_summary, "check image");
        assert_eq!(events[0].raw["antigravity"]["media"][0]["path"], "C:/tmp/shot.png");
        assert_eq!(events[1].role, "reasoning");
        assert_eq!(events[2].role, "tool_call");
        assert_eq!(events[2].raw["message"]["content"][0]["input"]["StartLine"], 1);
        assert_eq!(events[3].role, "tool_result");
        assert_eq!(events[3].raw["message"]["content"][0]["is_error"], true);
        assert_eq!(
            events[3].raw["message"]["content"][0]["tool_use_id"],
            events[2].raw["message"]["content"][0]["id"]
        );
        assert_eq!(events[4].role, "meta");
        assert_eq!(events[5].role, "assistant");
        assert_eq!(events[6].kind, "turn_error");

        let page = preview_range(locator, 2, 2).unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].role, "tool_call");
        assert_eq!(page[1].role, "tool_result");

        let prompts = preview_user_prompts(locator).unwrap();
        assert_eq!(prompts.prompts.len(), 1);
        assert_eq!(prompts.prompts[0].text, "check image");
        assert_eq!(
            prompts.prompts[0].response.as_ref().unwrap().text,
            "Here is the answer."
        );

        let meta = preview_meta(locator).unwrap();
        assert_eq!(meta.originator.as_deref(), Some("Antigravity CLI"));
        assert_eq!(meta.model_provider.as_deref(), Some("gemini-4-neon-xhigh"));
        assert_eq!(meta.source.as_deref(), Some("transcript_full.jsonl"));
    }
}
