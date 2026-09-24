use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{SecondsFormat, Utc};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::models::{DeleteResult, PreviewEvent, SessionMetaBrief, SessionSummary, UserPromptList};
use crate::paths;

const PROVIDER: &str = "opencode";
const LOCATOR_PREFIX: &str = "opencode:";

#[derive(Debug, Serialize, Deserialize)]
struct SessionLocator {
    db: String,
    session: String,
}

#[derive(Default)]
struct SessionDetails {
    first_user_message: String,
    model: Option<String>,
    tokens_used: i64,
    bytes: u64,
}

#[derive(Clone)]
struct MessageRow {
    id: String,
    session_id: String,
    role: String,
    parent_id: Option<String>,
    finish: Option<String>,
    model: Option<String>,
    tokens: i64,
    created_at_ms: i64,
}

pub fn default_data_dir() -> PathBuf {
    paths::default_opencode_dir()
}

pub fn database_path(data_dir: &Path) -> PathBuf {
    data_dir.join("opencode.db")
}

pub fn validate_data_dir(data_dir: &Path) -> AppResult<u32> {
    let db = database_path(data_dir);
    if !data_dir.is_dir() || !db.is_file() {
        return Ok(0);
    }
    let connection = open_readonly(&db)?;
    let count = connection.query_row("SELECT COUNT(*) FROM session", [], |row| {
        row.get::<_, u32>(0)
    })?;
    Ok(count)
}

pub fn list_sessions(data_dir: &Path) -> AppResult<Vec<SessionSummary>> {
    let db_path = database_path(data_dir);
    let connection = open_readonly(&db_path)?;
    let details = load_session_details(&connection)?;
    let database_bytes = fs::metadata(&db_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let mut statement = connection.prepare(
        "SELECT id, project_id, parent_id, directory, title, version, time_created, time_updated, time_archived
         FROM session ORDER BY time_updated DESC, time_created DESC",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, Option<i64>>(8)?,
        ))
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        let (id, project_id, parent_id, directory, title, version, created, updated, archived) =
            row?;
        let detail = details.get(&id);
        let cwd = paths::strip_verbatim(&directory);
        sessions.push(SessionSummary {
            provider: PROVIDER.into(),
            id: id.clone(),
            rollout_path: encode_locator(&db_path, &id)?,
            cwd_display: paths::basename_display(&cwd),
            cwd,
            title,
            first_user_message: detail
                .map(|value| value.first_user_message.clone())
                .unwrap_or_default(),
            model: detail.and_then(|value| value.model.clone()),
            reasoning_effort: None,
            source: parent_id.map(|parent| format!("parent:{parent}")),
            agent_nickname: None,
            agent_role: None,
            conversion_origin: None,
            tokens_used: detail.map(|value| value.tokens_used).unwrap_or_default(),
            created_at: created / 1000,
            updated_at: updated / 1000,
            archived: archived.is_some(),
            git_branch: None,
            rollout_bytes: detail
                .map(|value| value.bytes)
                .filter(|bytes| *bytes > 0)
                .unwrap_or(database_bytes),
            logs_count: 0,
            has_backup: false,
            resume_command: crate::fs_ops::session_resume_command("opencode", &id),
        });
        let _ = (project_id, version);
    }
    Ok(sessions)
}

pub fn preview_range(locator: &str, offset: usize, limit: usize) -> AppResult<Vec<PreviewEvent>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let locator = decode_locator(locator)?;
    let connection = open_readonly(Path::new(&locator.db))?;
    load_preview_page(&connection, &locator.session, offset, limit)
}

pub(crate) fn load_preview_events_from_locator(locator: &str) -> AppResult<Vec<PreviewEvent>> {
    let locator = decode_locator(locator)?;
    let connection = open_readonly(Path::new(&locator.db))?;
    load_preview_events(&connection, &locator.session)
}

pub fn preview_user_prompts(locator: &str) -> AppResult<UserPromptList> {
    let locator = decode_locator(locator)?;
    let connection = open_readonly(Path::new(&locator.db))?;
    let events = load_preview_events(&connection, &locator.session)?;
    Ok(crate::rollout::user_prompts_from_events(events, |event| {
        matches!(event.role.as_str(), "assistant" | "reasoning" | "tool_call")
    }))
}

pub fn preview_meta(locator: &str) -> AppResult<SessionMetaBrief> {
    let locator = decode_locator(locator)?;
    let connection = open_readonly(Path::new(&locator.db))?;
    connection
        .query_row(
            "SELECT id, directory, version, time_created FROM session WHERE id = ?1",
            [&locator.session],
            |row| {
                let created: i64 = row.get(3)?;
                Ok(SessionMetaBrief {
                    id: Some(row.get(0)?),
                    timestamp: timestamp(created),
                    cwd: Some(row.get(1)?),
                    originator: Some("OpenCode".into()),
                    cli_version: Some(row.get(2)?),
                    source: Some("opencode.db".into()),
                    model_provider: Some(PROVIDER.into()),
                })
            },
        )
        .map_err(Into::into)
}

pub fn set_archived(data_dir: &Path, id: &str, archived: bool) -> AppResult<()> {
    let connection = open_writable(&database_path(data_dir))?;
    let changed = connection.execute(
        "UPDATE session SET time_archived = ?1 WHERE id = ?2",
        params![archived.then(current_millis), id],
    )?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("OpenCode 会话不存在: {id}")));
    }
    Ok(())
}

pub fn rename_session(data_dir: &Path, id: &str, title: &str) -> AppResult<u32> {
    let title = title.trim();
    if title.is_empty() {
        return Err(AppError::Other("会话名称不能为空".into()));
    }
    if title.chars().count() > 120 {
        return Err(AppError::Other("会话名称过长（最多 120 个字符）".into()));
    }
    let connection = open_writable(&database_path(data_dir))?;
    let changed = connection.execute(
        "UPDATE session SET title = ?1 WHERE id = ?2",
        params![title, id],
    )? as u32;
    if changed == 0 {
        return Err(AppError::NotFound(format!("OpenCode 会话不存在: {id}")));
    }
    Ok(changed)
}

pub fn delete_session(data_dir: &Path, id: &str) -> AppResult<DeleteResult> {
    let db = database_path(data_dir);
    let mut connection = open_writable(&db)?;
    connection.execute_batch("PRAGMA foreign_keys = ON")?;
    let transaction = connection.transaction()?;
    let descendant_ids = {
        let mut statement = transaction.prepare(
            "WITH RECURSIVE descendants(id) AS (
                SELECT id FROM session WHERE id = ?1
                UNION
                SELECT child.id
                FROM session child
                JOIN descendants parent ON child.parent_id = parent.id
             )
             SELECT id FROM descendants",
        )?;
        let rows = statement.query_map([id], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    if descendant_ids.is_empty() {
        return Ok(DeleteResult {
            id: id.into(),
            rollout_path: None,
            threads_rows_deleted: 0,
            logs_rows_deleted: 0,
            history_rows_deleted: 0,
            rollout_deleted: false,
            rollout_missing: true,
            sidecar_deleted: false,
            tasks_deleted: false,
            file_history_deleted: false,
            shared_data_preserved: false,
            desktop_restart_required: false,
            ok: true,
            error: None,
        });
    }

    let has_event = table_exists(&transaction, "event")?;
    let has_event_sequence = table_exists(&transaction, "event_sequence")?;
    let mut related_rows = 0u32;
    for descendant_id in &descendant_ids {
        related_rows = related_rows.saturating_add(transaction.query_row(
            "SELECT COUNT(*) FROM message WHERE session_id = ?1",
            [descendant_id],
            |row| row.get::<_, u32>(0),
        )?);
        related_rows = related_rows.saturating_add(transaction.query_row(
            "SELECT COUNT(*) FROM part WHERE session_id = ?1",
            [descendant_id],
            |row| row.get::<_, u32>(0),
        )?);
        if has_event {
            related_rows = related_rows.saturating_add(transaction.query_row(
                "SELECT COUNT(*) FROM event WHERE aggregate_id = ?1",
                [descendant_id],
                |row| row.get::<_, u32>(0),
            )?);
            transaction.execute("DELETE FROM event WHERE aggregate_id = ?1", [descendant_id])?;
        }
        if has_event_sequence {
            related_rows = related_rows.saturating_add(transaction.execute(
                "DELETE FROM event_sequence WHERE aggregate_id = ?1",
                [descendant_id],
            )? as u32);
        }
    }

    let mut deleted = 0u32;
    for descendant_id in descendant_ids.iter().rev() {
        deleted = deleted.saturating_add(
            transaction.execute("DELETE FROM session WHERE id = ?1", [descendant_id])? as u32,
        );
    }
    transaction.commit()?;
    Ok(DeleteResult {
        id: id.into(),
        rollout_path: Some(encode_locator(&db, id)?),
        threads_rows_deleted: deleted,
        logs_rows_deleted: related_rows,
        history_rows_deleted: 0,
        rollout_deleted: true,
        rollout_missing: false,
        sidecar_deleted: false,
        tasks_deleted: false,
        file_history_deleted: false,
        shared_data_preserved: false,
        desktop_restart_required: false,
        ok: true,
        error: None,
    })
}

fn load_session_details(connection: &Connection) -> AppResult<HashMap<String, SessionDetails>> {
    let messages = load_messages(connection)?;
    let mut details: HashMap<String, SessionDetails> = HashMap::new();
    for message in messages {
        let detail = details.entry(message.session_id.clone()).or_default();
        detail.model = message.model.clone().or_else(|| detail.model.clone());
        detail.tokens_used = detail.tokens_used.saturating_add(message.tokens);
        detail.bytes = detail
            .bytes
            .saturating_add(message.id.len() as u64 + message.role.len() as u64);
    }
    let mut statement = connection
        .prepare("SELECT session_id, SUM(octet_length(data)) FROM part GROUP BY session_id")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
    })?;
    for row in rows {
        let (session_id, bytes) = row?;
        crate::operation_metrics::record(|c| c.sql_rows += 1);
        let detail = details.entry(session_id).or_default();
        detail.bytes = detail.bytes.saturating_add(bytes);
    }
    // Only the first useful user text crosses the SQLite boundary. Tool payloads are never
    // materialized in Rust just to calculate their byte length.
    let mut first_prompt = connection.prepare(
        "SELECT p.id FROM part p JOIN message m ON m.id = p.message_id
         WHERE p.session_id = ?1
           AND CASE WHEN json_valid(m.data) THEN json_extract(m.data, '$.role') END = 'user'
         ORDER BY p.time_created, p.id",
    )?;
    let mut part_data = connection.prepare("SELECT data FROM part WHERE id = ?1")?;
    for (id, detail) in &mut details {
        let candidates = first_prompt.query_map([id], |row| row.get::<_, String>(0))?;
        for candidate in candidates {
            let data = part_data.query_row([candidate?], |row| row.get::<_, String>(0))?;
            crate::operation_metrics::record(|c| {
                c.sql_rows += 2;
                c.read_bytes += data.len() as u64;
            });
            let Ok(value) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("text") {
                continue;
            }
            let text = value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if !text.is_empty() {
                detail.first_user_message = text.to_string();
                break;
            }
        }
    }
    Ok(details)
}

pub(crate) fn load_preview_events(
    connection: &Connection,
    session_id: &str,
) -> AppResult<Vec<PreviewEvent>> {
    load_preview_page(connection, session_id, 0, usize::MAX)
}

pub(crate) fn load_preview_page(
    connection: &Connection,
    session_id: &str,
    offset: usize,
    limit: usize,
) -> AppResult<Vec<PreviewEvent>> {
    // Today each valid native part with a message emits exactly one display event (including
    // metadata parts). Filter invalid/orphan records BEFORE applying display-event offsets.
    // Materialize only the ordered keys so sorting never retains all part payloads.
    let mut statement = connection.prepare(
        "WITH page AS MATERIALIZED (
            SELECT p.id, p.time_created FROM part p
            WHERE p.session_id = ?1 AND json_valid(p.data)
              AND EXISTS (SELECT 1 FROM message m WHERE m.id = p.message_id AND m.session_id = ?1)
            ORDER BY p.time_created, p.id LIMIT ?2 OFFSET ?3
         )
         SELECT p.id, p.message_id, p.time_created, p.data, m.session_id, m.time_created, m.data
         FROM page JOIN part p ON p.id = page.id JOIN message m ON m.id = p.message_id
         ORDER BY page.time_created, page.id",
    )?;
    let rows = statement.query_map(
        params![
            session_id,
            i64::try_from(limit).unwrap_or(i64::MAX),
            i64::try_from(offset).unwrap_or(i64::MAX)
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
            ))
        },
    )?;
    let mut events = Vec::new();
    for row in rows {
        let (part_id, message_id, created, data, source_session, message_created, message_data) =
            row?;
        crate::operation_metrics::record(|c| {
            c.sql_rows += 1;
            c.read_bytes += (data.len() + message_data.len()) as u64;
        });
        let message = parse_message(message_id, source_session, message_created, &message_data);
        let Ok(part) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let raw = opencode_part_to_preview_raw(&message, &part_id, created, part);
        if let Some(event) = crate::claude_sessions::classify_preview(offset + events.len(), raw) {
            events.push(event);
        }
    }
    Ok(events)
}

fn load_messages(connection: &Connection) -> AppResult<Vec<MessageRow>> {
    let mut statement = connection.prepare(
        "SELECT id, session_id, time_created, data FROM message ORDER BY time_created ASC, id ASC",
    )?;
    let mut rows = statement.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let data: String = row.get(3)?;
        crate::operation_metrics::record(|c| {
            c.sql_rows += 1;
            c.read_bytes += data.len() as u64;
        });
        out.push(parse_message(row.get(0)?, row.get(1)?, row.get(2)?, &data));
    }
    Ok(out)
}

fn parse_message(id: String, session_id: String, created_at_ms: i64, data: &str) -> MessageRow {
    let value = serde_json::from_str::<Value>(data).unwrap_or(Value::Null);
    MessageRow {
        id,
        session_id,
        role: value
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("other")
            .to_string(),
        parent_id: value
            .get("parentID")
            .and_then(Value::as_str)
            .map(str::to_string),
        finish: value
            .get("finish")
            .and_then(Value::as_str)
            .map(str::to_string),
        model: value
            .get("modelID")
            .or_else(|| value.get("model").and_then(|model| model.get("modelID")))
            .and_then(Value::as_str)
            .map(str::to_string),
        tokens: message_tokens(&value),
        created_at_ms,
    }
}

fn opencode_part_to_preview_raw(
    message: &MessageRow,
    part_id: &str,
    created: i64,
    part: Value,
) -> Value {
    let part_type = part.get("type").and_then(Value::as_str).unwrap_or("part");
    let timestamp = timestamp(created.or_else_ms(message.created_at_ms));
    let phase = if message.role == "assistant" {
        match message.finish.as_deref() {
            Some("tool-calls") => Some("commentary"),
            Some(_) if part_type == "text" => Some("final_answer"),
            Some(_) => Some("commentary"),
            None if matches!(part_type, "reasoning" | "tool") => Some("commentary"),
            None => None,
        }
    } else {
        None
    };
    let opencode = json!({
        "session_id": message.session_id,
        "part_id": part_id,
        "message_id": message.id,
        "parent_id": message.parent_id,
        "finish": message.finish,
        "part_type": part_type,
        "phase": phase,
        "part": part
    });
    let content = match part_type {
        "text" => {
            json!({"type":"text","text":part.get("text").and_then(Value::as_str).unwrap_or("")})
        }
        "reasoning" => {
            json!({"type":"thinking","thinking":part.get("text").and_then(Value::as_str).unwrap_or("")})
        }
        "tool" => json!({
            "type":"tool_use",
            "id":part.get("callID").cloned().unwrap_or(Value::Null),
            "name":part.get("tool").cloned().unwrap_or(Value::String("tool".into())),
            "input":part.get("state").and_then(|state| state.get("input")).cloned().unwrap_or(Value::Null),
            "state":part.get("state").cloned().unwrap_or(Value::Null)
        }),
        _ => {
            return json!({
                "type": part_type,
                "timestamp": timestamp,
                "opencode": opencode
            })
        }
    };
    json!({
        "type": message.role,
        "timestamp": timestamp,
        "message": {
            "role":message.role,
            "phase":phase,
            "content":[content],
            "model":message.model
        },
        "opencode": opencode
    })
}

pub(crate) fn resolve_locator(value: &str) -> AppResult<(PathBuf, String)> {
    let locator = decode_locator(value)?;
    Ok((PathBuf::from(locator.db), locator.session))
}

trait MillisFallback {
    fn or_else_ms(self, fallback: i64) -> i64;
}

impl MillisFallback for i64 {
    fn or_else_ms(self, fallback: i64) -> i64 {
        if self > 0 {
            self
        } else {
            fallback
        }
    }
}

fn message_tokens(value: &Value) -> i64 {
    let Some(tokens) = value.get("tokens") else {
        return 0;
    };
    tokens
        .get("total")
        .and_then(Value::as_i64)
        .or_else(|| {
            Some(
                tokens.get("input").and_then(Value::as_i64).unwrap_or(0)
                    + tokens.get("output").and_then(Value::as_i64).unwrap_or(0),
            )
        })
        .unwrap_or(0)
        .max(0)
}

pub(crate) fn encode_locator(db: &Path, session_id: &str) -> AppResult<String> {
    let raw = serde_json::to_vec(&SessionLocator {
        db: db.to_string_lossy().into_owned(),
        session: session_id.to_string(),
    })?;
    Ok(format!("{LOCATOR_PREFIX}{}", URL_SAFE_NO_PAD.encode(raw)))
}

fn decode_locator(value: &str) -> AppResult<SessionLocator> {
    let encoded = value
        .strip_prefix(LOCATOR_PREFIX)
        .ok_or_else(|| AppError::Path("OpenCode 会话定位符格式无效".into()))?;
    let raw = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|error| AppError::Path(format!("OpenCode 会话定位符无法解码: {error}")))?;
    let locator: SessionLocator = serde_json::from_slice(&raw)?;
    if locator.session.trim().is_empty() || !Path::new(&locator.db).is_file() {
        return Err(AppError::Path("OpenCode 会话定位符指向无效数据库".into()));
    }
    Ok(locator)
}

fn open_readonly(path: &Path) -> AppResult<Connection> {
    if !path.is_file() {
        return Err(AppError::NotFound(format!(
            "OpenCode 数据库不存在: {}",
            path.to_string_lossy()
        )));
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(Into::into)
}

fn open_writable(path: &Path) -> AppResult<Connection> {
    if !path.is_file() {
        return Err(AppError::NotFound(format!(
            "OpenCode 数据库不存在: {}",
            path.to_string_lossy()
        )));
    }
    Connection::open(path).map_err(Into::into)
}

fn table_exists(connection: &Connection, table: &str) -> AppResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get::<_, bool>(0),
    )?)
}

fn timestamp(millis: i64) -> Option<String> {
    chrono::DateTime::<Utc>::from_timestamp_millis(millis)
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn current_millis() -> i64 {
    Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> AppResult<PathBuf> {
        let root = std::env::temp_dir().join(format!(
            "cc-sessions-opencode-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let connection = Connection::open(database_path(&root))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE, session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE event_sequence (aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL);
             CREATE TABLE event (id TEXT PRIMARY KEY, aggregate_id TEXT NOT NULL REFERENCES event_sequence(aggregate_id) ON DELETE CASCADE, seq INTEGER NOT NULL, type TEXT NOT NULL, data TEXT NOT NULL);",
        )?;
        connection.execute(
            "INSERT INTO session VALUES (?1, 'global', NULL, 'slug', 'F:\\project', 'Hello', '1.0', 1000, 4000, NULL)",
            ["ses_test"],
        )?;
        connection.execute(
            "INSERT INTO message VALUES ('msg_user', 'ses_test', 1000, 1000, ?1)",
            [json!({"role":"user"}).to_string()],
        )?;
        connection.execute(
            "INSERT INTO message VALUES ('msg_assistant', 'ses_test', 2000, 2000, ?1)",
            [json!({
                "role":"assistant",
                "parentID":"msg_user",
                "finish":"stop",
                "modelID":"gpt-test",
                "tokens":{"total":12}
            })
            .to_string()],
        )?;
        connection.execute(
            "INSERT INTO part VALUES ('part_user', 'msg_user', 'ses_test', 1000, 1000, ?1)",
            [json!({"type":"text","text":"你好"}).to_string()],
        )?;
        connection.execute(
            "INSERT INTO part VALUES ('part_assistant', 'msg_assistant', 'ses_test', 2000, 2000, ?1)",
            [json!({"type":"text","text":"你好，我是 OpenCode"}).to_string()],
        )?;
        Ok(root)
    }

    #[test]
    fn opencode_sessions_support_preview_and_management() -> AppResult<()> {
        let root = fixture()?;
        let sessions = list_sessions(&root)?;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].first_user_message, "你好");
        assert_eq!(sessions[0].model.as_deref(), Some("gpt-test"));
        assert_eq!(sessions[0].tokens_used, 12);
        let preview = preview_range(&sessions[0].rollout_path, 0, 10)?;
        assert_eq!(preview.len(), 2);
        assert_eq!(preview[0].role, "user");
        assert_eq!(preview[1].role, "assistant");
        let timeline = preview_user_prompts(&sessions[0].rollout_path)?;
        assert_eq!(timeline.prompts.len(), 1);
        assert_eq!(timeline.prompts[0].text, "你好");
        assert_eq!(
            timeline.prompts[0]
                .response
                .as_ref()
                .map(|item| item.text.as_str()),
            Some("你好，我是 OpenCode")
        );

        rename_session(&root, "ses_test", "Renamed")?;
        set_archived(&root, "ses_test", true)?;
        let renamed = list_sessions(&root)?;
        assert_eq!(renamed[0].title, "Renamed");
        assert!(renamed[0].archived);
        let deleted = delete_session(&root, "ses_test")?;
        assert!(deleted.ok && deleted.rollout_deleted);
        assert!(list_sessions(&root)?.is_empty());
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn performance_opencode_preview_reads_only_page_payloads() -> AppResult<()> {
        let root = fixture()?;
        let connection = Connection::open(database_path(&root))?;
        for index in 0..100 {
            connection.execute(
                "INSERT INTO part VALUES (?1, 'msg_assistant', 'ses_test', ?2, ?2, ?3)",
                params![
                    format!("more-{index}"),
                    3000 + index,
                    json!({"type":"text","text":"x".repeat(1000)}).to_string()
                ],
            )?;
        }
        // Invalid native records do not consume a display-event offset.
        connection.execute(
            "INSERT INTO part VALUES ('bad', 'msg_user', 'ses_test', 500, 500, 'broken')",
            [],
        )?;
        let locator = encode_locator(&database_path(&root), "ses_test")?;
        let full = load_preview_events(&connection, "ses_test")?;
        let (page, counters) =
            crate::operation_metrics::measured(|| preview_range(&locator, 80, 3));
        let page = page?;
        assert_eq!(
            serde_json::to_value(&page)?,
            serde_json::to_value(&full[80..83])?
        );
        assert!(preview_range(&locator, 0, 0)?.is_empty());
        drop(connection);
        fs::remove_dir_all(root)?;
        assert!(counters.sql_rows <= 6, "read {} rows", counters.sql_rows);
        assert!(
            counters.read_bytes < 5000,
            "read {} bytes",
            counters.read_bytes
        );
        Ok(())
    }

    #[test]
    fn performance_opencode_list_does_not_transfer_tool_payloads() -> AppResult<()> {
        let root = fixture()?;
        let connection = Connection::open(database_path(&root))?;
        connection.execute(
            "INSERT INTO part VALUES ('large-tool', 'msg_assistant', 'ses_test', 3000, 3000, ?1)",
            [json!({"type":"tool","state":{"output":"x".repeat(1_000_000)}}).to_string()],
        )?;
        let (sessions, counters) = crate::operation_metrics::measured(|| list_sessions(&root));
        let sessions = sessions?;
        assert_eq!(sessions[0].first_user_message, "你好");
        assert_eq!(sessions[0].tokens_used, 12);
        assert!(sessions[0].rollout_bytes > 1_000_000);
        drop(connection);
        fs::remove_dir_all(root)?;
        assert!(
            counters.read_bytes < 10_000,
            "transferred {} bytes",
            counters.read_bytes
        );
        Ok(())
    }

    #[test]
    fn rename_and_archive_preserve_last_activity_time() -> AppResult<()> {
        let root = fixture()?;
        rename_session(&root, "ses_test", "Renamed")?;
        set_archived(&root, "ses_test", true)?;

        let connection = Connection::open(database_path(&root))?;
        let (updated, archived) = connection.query_row(
            "SELECT time_updated, time_archived FROM session WHERE id = 'ses_test'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
        )?;
        assert_eq!(updated, 4000);
        assert!(archived.is_some());

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn preview_preserves_turn_and_process_metadata() -> AppResult<()> {
        let root = fixture()?;
        let connection = Connection::open(database_path(&root))?;
        connection.execute(
            "INSERT INTO message VALUES ('msg_process', 'ses_test', 1500, 1500, ?1)",
            [json!({
                "role":"assistant",
                "parentID":"msg_user",
                "finish":"tool-calls",
                "modelID":"gpt-test"
            })
            .to_string()],
        )?;
        connection.execute(
            "INSERT INTO part VALUES ('part_reasoning', 'msg_process', 'ses_test', 1500, 1500, ?1)",
            [json!({"type":"reasoning","text":"先检查工作区"}).to_string()],
        )?;
        connection.execute(
            "INSERT INTO part VALUES ('part_tool', 'msg_process', 'ses_test', 1600, 1600, ?1)",
            [json!({
                "type":"tool",
                "callID":"call_1",
                "tool":"read",
                "state":{"input":{"path":"README.md"}}
            })
            .to_string()],
        )?;
        drop(connection);

        let locator = list_sessions(&root)?[0].rollout_path.clone();
        let preview = preview_range(&locator, 0, 20)?;
        assert_eq!(preview.len(), 4);
        assert_eq!(preview[1].role, "reasoning");
        assert_eq!(preview[2].role, "tool_call");

        for event in &preview[1..=2] {
            assert_eq!(event.raw["opencode"]["parent_id"], "msg_user");
            assert_eq!(event.raw["opencode"]["finish"], "tool-calls");
            assert_eq!(event.raw["opencode"]["phase"], "commentary");
            assert!(event.raw["opencode"]["part_id"].is_string());
            assert!(event.raw["opencode"]["message_id"].is_string());
        }
        assert_eq!(preview[3].raw["opencode"]["finish"], "stop");
        assert_eq!(preview[3].raw["opencode"]["phase"], "final_answer");
        assert_eq!(preview[3].raw["message"]["phase"], "final_answer");

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn deleting_parent_removes_all_descendants_and_aggregate_events() -> AppResult<()> {
        let root = fixture()?;
        let connection = Connection::open(database_path(&root))?;
        connection.execute(
            "INSERT INTO session VALUES ('ses_child', 'global', 'ses_test', 'child', 'F:\\project', 'Child', '1.0', 2000, 3000, NULL)",
            [],
        )?;
        connection.execute(
            "INSERT INTO session VALUES ('ses_grandchild', 'global', 'ses_child', 'grandchild', 'F:\\project', 'Grandchild', '1.0', 2500, 3500, NULL)",
            [],
        )?;
        for (session_id, message_id, part_id) in [
            ("ses_child", "msg_child", "part_child"),
            ("ses_grandchild", "msg_grandchild", "part_grandchild"),
        ] {
            connection.execute(
                "INSERT INTO message VALUES (?1, ?2, 2000, 2000, ?3)",
                params![message_id, session_id, json!({"role":"user"}).to_string()],
            )?;
            connection.execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, 2000, 2000, ?4)",
                params![
                    part_id,
                    message_id,
                    session_id,
                    json!({"type":"text","text":"child"}).to_string()
                ],
            )?;
        }
        for session_id in ["ses_test", "ses_child", "ses_grandchild"] {
            connection.execute("INSERT INTO event_sequence VALUES (?1, 1)", [session_id])?;
            connection.execute(
                "INSERT INTO event VALUES (?1, ?2, 1, 'session.updated', '{}')",
                params![format!("event_{session_id}"), session_id],
            )?;
        }
        drop(connection);

        let deleted = delete_session(&root, "ses_test")?;
        assert_eq!(deleted.threads_rows_deleted, 3);

        let connection = Connection::open(database_path(&root))?;
        for table in ["session", "message", "part", "event", "event_sequence"] {
            let count =
                connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, u32>(0)
                })?;
            assert_eq!(count, 0, "expected {table} to be empty");
        }

        fs::remove_dir_all(root).ok();
        Ok(())
    }
}
