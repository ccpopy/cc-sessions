//! Offline OpenCode V1 forks, following OpenCode v1.18.30:
//! https://github.com/anomalyco/opencode/blob/5cd8e68fdd72b27818d26d168b9c7a06b359567e/packages/opencode/src/session/session.ts
//! Only new rows are written, in one transaction; no database migration is performed.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{params, types::Value as SqlValue, Connection, OpenFlags, TransactionBehavior};
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::family::{self, FamilyLock};
use crate::models::{OpenCodeCopyReport, OpenCodeForkPoint};
use crate::{opencode_sessions, path_safety, paths};

type Row = BTreeMap<String, SqlValue>;

pub fn copy_session_with_lock(
    opencode_dir: String,
    session_id: String,
    rollout_path: String,
    cutoff: Option<OpenCodeForkPoint>,
    lock: &FamilyLock,
) -> AppResult<OpenCodeCopyReport> {
    family::with_lock(lock, &PathBuf::from(&opencode_dir), |_| {
        let root = PathBuf::from(paths::strip_verbatim(&opencode_dir));
        let (located_db, located_id) = opencode_sessions::resolve_locator(&rollout_path)?;
        let db = opencode_sessions::database_path(&root);
        path_safety::validate_descendant(
            &root,
            &db,
            path_safety::EntryKind::File,
            false,
            "OpenCode 数据库",
        )?;
        if located_id != session_id
            || PathBuf::from(paths::strip_verbatim(&located_db.to_string_lossy())) != db
        {
            return fail("OpenCode 会话定位符与所选目录或会话不一致");
        }
        // No CREATE flag: a stale locator must never create a new database.
        let mut connection = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA foreign_keys = ON")?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let report = copy_in_transaction(&tx, &session_id, cutoff.as_ref(), &db)?;
        tx.commit()?;
        Ok(report)
    })
}

fn fail<T>(message: &str) -> AppResult<T> {
    Err(AppError::Other(format!("{message}，未创建副本")))
}

fn table_exists(db: &Connection, table: &str) -> AppResult<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )?)
}

// Table/column identifiers below are fixed by this module, never caller input.
fn columns(
    db: &Connection,
    table: &str,
    required: &[&str],
    optional: &[&str],
) -> AppResult<Vec<String>> {
    let mut stmt = db.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(1)?, row.get::<_, bool>(3)?))
    })?;
    let mut names = Vec::new();
    for row in rows {
        let (name, not_null) = row?;
        if not_null && !required.contains(&name.as_str()) && !optional.contains(&name.as_str()) {
            return fail(&format!("OpenCode {table} 表包含不支持的必填列 {name}"));
        }
        names.push(name);
    }
    if required.iter().any(|name| !names.iter().any(|n| n == name)) {
        return fail(&format!("OpenCode {table} 表结构不受支持"));
    }
    Ok(names)
}

fn insert(db: &Connection, table: &str, row: &Row) -> AppResult<()> {
    let names = row
        .keys()
        .map(|key| format!("\"{key}\""))
        .collect::<Vec<_>>()
        .join(",");
    let placeholders = vec!["?"; row.len()].join(",");
    db.execute(
        &format!("INSERT INTO \"{table}\" ({names}) VALUES ({placeholders})"),
        rusqlite::params_from_iter(row.values()),
    )?;
    Ok(())
}

fn required_text(row: &Row, name: &str) -> AppResult<String> {
    match row.get(name) {
        Some(SqlValue::Text(value)) => Ok(value.clone()),
        _ => fail(&format!("OpenCode 会话的 {name} 无效")),
    }
}

fn object(data: &str) -> AppResult<Value> {
    let value: Value = serde_json::from_str(data)?;
    if !value.is_object() {
        return fail("OpenCode 消息或内容块不是 JSON 对象");
    }
    Ok(value)
}

struct Message {
    id: String,
    data: Value,
    parts: Vec<(String, Value)>,
}

fn read_messages(db: &Connection, session_id: &str) -> AppResult<Vec<Message>> {
    let mut messages = Vec::new();
    let mut positions = HashMap::new();
    let mut stmt =
        db.prepare("SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created, id")?;
    let rows = stmt.query_map([session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, data) = row?;
        let data = object(&data)?;
        if !matches!(data["role"].as_str(), Some("user" | "assistant"))
            || data
                .pointer("/time/created")
                .and_then(Value::as_i64)
                .is_none()
        {
            return fail("OpenCode 消息角色或创建时间无效");
        }
        positions.insert(id.clone(), messages.len());
        messages.push(Message {
            id,
            data,
            parts: Vec::new(),
        });
    }
    // Native message.parts() orders by part ID, not creation time. Also detect
    // inconsistent ownership instead of dropping parts from a damaged database.
    let mut stmt = db.prepare(
        "SELECT id, message_id, session_id, data FROM part
         WHERE session_id = ?1 OR message_id IN (SELECT id FROM message WHERE session_id = ?1)
         ORDER BY id",
    )?;
    let rows = stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (id, message_id, owner, data) = row?;
        let Some(&index) = positions.get(&message_id) else {
            return fail("OpenCode 内容块指向不存在的消息");
        };
        if owner != session_id {
            return fail("OpenCode 内容块与消息所属会话不一致");
        }
        let data = object(&data)?;
        if data["type"].as_str().is_none_or(str::is_empty) {
            return fail("OpenCode 内容块类型无效");
        }
        messages[index].parts.push((id, data));
    }
    Ok(messages)
}

fn copy_in_transaction(
    db: &Connection,
    source_id: &str,
    cutoff: Option<&OpenCodeForkPoint>,
    path: &std::path::Path,
) -> AppResult<OpenCodeCopyReport> {
    let session_columns = columns(
        db,
        "session",
        &[
            "id",
            "project_id",
            "slug",
            "directory",
            "title",
            "version",
            "time_created",
            "time_updated",
        ],
        &[
            "workspace_id",
            "path",
            "parent_id",
            "metadata",
            "share_url",
            "summary_additions",
            "summary_deletions",
            "summary_files",
            "summary_diffs",
            "cost",
            "tokens_input",
            "tokens_output",
            "tokens_reasoning",
            "tokens_cache_read",
            "tokens_cache_write",
            "revert",
            "permission",
            "agent",
            "model",
            "time_compacting",
            "time_archived",
        ],
    )?;
    columns(
        db,
        "message",
        &["id", "session_id", "time_created", "time_updated", "data"],
        &[],
    )?;
    columns(
        db,
        "part",
        &[
            "id",
            "message_id",
            "session_id",
            "time_created",
            "time_updated",
            "data",
        ],
        &[],
    )?;
    if table_exists(db, "session_message")? {
        let count: i64 = db.query_row(
            "SELECT count(*) FROM session_message WHERE session_id = ?1",
            [source_id],
            |r| r.get(0),
        )?;
        if count > 0 {
            return fail("该 OpenCode 会话使用 session_message 新格式，暂不支持复制");
        }
    }
    let journal = table_exists(db, "event")?;
    if journal != table_exists(db, "event_sequence")? {
        return fail("OpenCode 事件日志表不完整");
    }
    if journal {
        columns(
            db,
            "event",
            &["id", "aggregate_id", "seq", "type", "data"],
            &[],
        )?;
        columns(
            db,
            "event_sequence",
            &["aggregate_id", "seq"],
            &["owner_id"],
        )?;
    }
    let mut stmt = db.prepare("SELECT * FROM session WHERE id = ?1")?;
    let mut rows = stmt.query([source_id])?;
    let Some(source) = rows.next()? else {
        return fail("OpenCode 来源会话不存在");
    };
    let mut source_row = Row::new();
    for name in &session_columns {
        source_row.insert(name.clone(), source.get::<_, SqlValue>(name.as_str())?);
    }
    drop(rows);
    drop(stmt);

    let mut messages = read_messages(db, source_id)?;
    if let Some(point) = cutoff {
        let events = opencode_sessions::load_preview_events(db, source_id)?;
        let valid = events.get(point.event_index).is_some_and(|event| {
            event
                .raw
                .pointer("/opencode/message_id")
                .and_then(Value::as_str)
                == Some(&point.message_id)
                && event
                    .raw
                    .pointer("/opencode/part_id")
                    .and_then(Value::as_str)
                    == Some(&point.part_id)
                && matches!(
                    event
                        .raw
                        .pointer("/opencode/part_type")
                        .and_then(Value::as_str),
                    Some("text" | "reasoning" | "tool")
                )
        });
        let index = messages
            .iter()
            .position(|message| message.id == point.message_id);
        if !valid || index.is_none() {
            return fail("所选 OpenCode 消息已变化或不存在，请刷新预览后重试");
        }
        // UI is inclusive; native fork uses the next message as an exclusive boundary.
        messages.truncate(index.unwrap() + 1);
    }

    let now = chrono::Utc::now().timestamp_millis();
    let mut ids = IdGenerator {
        millis: now as u64,
        counter: 0,
    };
    let new_id = ids.next("ses", true)?;
    let slug = format!("fork-{}", &new_id[new_id.len() - 14..]);
    let title = fork_title(&required_text(&source_row, "title")?);
    let mut row = Row::new();
    let mut info = json!({
        "id": new_id, "slug": slug, "title": title,
        "time": {"created": now, "updated": now}, "cost": 0,
        "tokens": {"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}},
    });
    for (column, field) in [
        ("project_id", "projectID"),
        ("directory", "directory"),
        ("version", "version"),
    ] {
        let value = required_text(&source_row, column)?;
        let value = if column == "directory" {
            storage_path(&value)
        } else {
            value
        };
        info[field] = json!(if column == "directory" {
            directory_for_event(&value)
        } else {
            value.clone()
        });
        row.insert(column.into(), SqlValue::Text(value));
    }
    for (column, field) in [
        ("workspace_id", "workspaceID"),
        ("path", "path"),
        ("metadata", "metadata"),
    ] {
        if let Some(value) = source_row
            .get(column)
            .filter(|value| **value != SqlValue::Null)
        {
            let SqlValue::Text(text) = value else {
                return fail("OpenCode 会话元数据格式无效");
            };
            let text = if column == "path" {
                storage_path(text)
            } else {
                text.clone()
            };
            info[field] = if column == "metadata" {
                object(&text)?
            } else {
                json!(text)
            };
            row.insert(column.into(), SqlValue::Text(text));
        }
    }
    for (key, value) in [
        ("id", new_id.as_str()),
        ("slug", slug.as_str()),
        ("title", title.as_str()),
    ] {
        row.insert(key.into(), SqlValue::Text(value.into()));
    }
    for key in ["time_created", "time_updated"] {
        row.insert(key.into(), SqlValue::Integer(now));
    }
    // These fields are not inherited by Session.fork/createNext.
    for key in [
        "parent_id",
        "share_url",
        "summary_additions",
        "summary_deletions",
        "summary_files",
        "summary_diffs",
        "revert",
        "permission",
        "agent",
        "model",
        "time_compacting",
        "time_archived",
    ] {
        if session_columns.iter().any(|name| name == key) {
            row.insert(key.into(), SqlValue::Null);
        }
    }
    let mut usage = Usage::default();
    for message in &messages {
        for (_, part) in &message.parts {
            usage.add(part)?;
        }
    }
    for (key, value) in usage.columns() {
        if session_columns.iter().any(|name| name == key) {
            row.insert(key.into(), value);
        }
    }
    insert(db, "session", &row)?;
    let mut seq = -1;
    if journal {
        db.execute(
            "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, -1)",
            [&new_id],
        )?;
        append_event(
            db,
            &mut ids,
            &new_id,
            &mut seq,
            "session.created.1",
            json!({"sessionID":new_id,"info":info}),
        )?;
    }

    let mut mapping = HashMap::new();
    let mut part_count = 0;
    for message in &messages {
        let message_id = ids.next("msg", false)?;
        mapping.insert(message.id.clone(), message_id.clone());
        let mut data = message.data.clone();
        if data["role"] == "assistant" {
            if let Some(parent) = data["parentID"].as_str().and_then(|id| mapping.get(id)) {
                data["parentID"] = json!(parent);
            }
        }
        data.as_object_mut().unwrap().remove("id");
        data.as_object_mut().unwrap().remove("sessionID");
        let created = data["time"]["created"].as_i64().unwrap();
        db.execute("INSERT INTO message (id,session_id,time_created,time_updated,data) VALUES (?1,?2,?3,?4,?5)",
            params![message_id, new_id, created, now, serde_json::to_string(&data)?])?;
        if journal {
            let mut info = data;
            info["id"] = json!(message_id);
            info["sessionID"] = json!(new_id);
            append_event(
                db,
                &mut ids,
                &new_id,
                &mut seq,
                "message.updated.1",
                json!({"sessionID":new_id,"info":info}),
            )?;
        }
        for (_, original) in &message.parts {
            let part_id = ids.next("prt", false)?;
            let mut part = original.clone();
            if part["type"] == "compaction" {
                if let Some(old) = part["tail_start_id"].as_str().filter(|s| !s.is_empty()) {
                    if let Some(new) = mapping.get(old) {
                        part["tail_start_id"] = json!(new);
                    } else {
                        part.as_object_mut().unwrap().remove("tail_start_id");
                    }
                }
            }
            for key in ["id", "sessionID", "messageID"] {
                part.as_object_mut().unwrap().remove(key);
            }
            db.execute("INSERT INTO part (id,message_id,session_id,time_created,time_updated,data) VALUES (?1,?2,?3,?4,?4,?5)",
                params![part_id, message_id, new_id, now, serde_json::to_string(&part)?])?;
            if journal {
                part["id"] = json!(part_id);
                part["messageID"] = json!(message_id);
                part["sessionID"] = json!(new_id);
                append_event(
                    db,
                    &mut ids,
                    &new_id,
                    &mut seq,
                    "message.part.updated.1",
                    json!({"sessionID":new_id,"part":part,"time":now}),
                )?;
            }
            part_count += 1;
        }
    }
    if journal {
        db.execute(
            "UPDATE event_sequence SET seq = ?1 WHERE aggregate_id = ?2",
            params![seq, new_id],
        )?;
    }
    Ok(OpenCodeCopyReport {
        source_id: source_id.into(),
        new_rollout_path: opencode_sessions::encode_locator(path, &new_id)?,
        new_id,
        message_count: messages.len() as u64,
        part_count,
    })
}

fn append_event(
    db: &Connection,
    ids: &mut IdGenerator,
    session: &str,
    seq: &mut i64,
    kind: &str,
    data: Value,
) -> AppResult<()> {
    *seq += 1;
    db.execute(
        "INSERT INTO event (id,aggregate_id,seq,type,data) VALUES (?1,?2,?3,?4,?5)",
        params![
            ids.next("evt", false)?,
            session,
            *seq,
            kind,
            serde_json::to_string(&data)?
        ],
    )?;
    Ok(())
}

fn fork_title(title: &str) -> String {
    if let Some((prefix, count)) = title
        .strip_suffix(')')
        .and_then(|s| s.rsplit_once(" (fork #"))
    {
        if !prefix.is_empty() && !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()) {
            if let Some(next) = count.parse::<u64>().ok().and_then(|n| n.checked_add(1)) {
                return format!("{prefix} (fork #{next})");
            }
        }
    }
    format!("{title} (fork #1)")
}

fn storage_path(value: &str) -> String {
    if cfg!(windows) {
        value.replace('\\', "/")
    } else {
        value.into()
    }
}

// Mirrors packages/core/src/database/path.ts: only Windows drive/UNC paths
// are converted for native event payloads; POSIX/legacy empty paths stay intact.
fn directory_for_event(value: &str) -> String {
    let drive = value.as_bytes();
    if cfg!(windows)
        && (value.starts_with("//")
            || (drive.len() >= 3 && drive[0].is_ascii_alphabetic() && &drive[1..3] == b":/"))
    {
        value.replace('/', "\\")
    } else {
        value.into()
    }
}

#[derive(Default)]
struct Usage {
    cost: f64,
    tokens: [i64; 5],
}

impl Usage {
    fn add(&mut self, part: &Value) -> AppResult<()> {
        if part["type"] != "step-finish"
            || part.get("cost").is_none()
            || part.get("tokens").is_none()
        {
            return Ok(());
        }
        let Some(cost) = part["cost"].as_f64() else {
            return fail("OpenCode 费用记录无效");
        };
        self.cost += cost;
        if !self.cost.is_finite() {
            return fail("OpenCode 费用记录溢出");
        }
        for (index, path) in [
            "/tokens/input",
            "/tokens/output",
            "/tokens/reasoning",
            "/tokens/cache/read",
            "/tokens/cache/write",
        ]
        .iter()
        .enumerate()
        {
            let Some(value) = part.pointer(path).and_then(Value::as_i64) else {
                return fail("OpenCode token 记录无效");
            };
            let Some(total) = self.tokens[index].checked_add(value) else {
                return fail("OpenCode token 记录溢出");
            };
            self.tokens[index] = total;
        }
        Ok(())
    }
    fn columns(&self) -> Vec<(&'static str, SqlValue)> {
        let mut out = vec![("cost", SqlValue::Real(self.cost))];
        for (name, value) in [
            "tokens_input",
            "tokens_output",
            "tokens_reasoning",
            "tokens_cache_read",
            "tokens_cache_write",
        ]
        .into_iter()
        .zip(self.tokens)
        {
            out.push((name, SqlValue::Integer(value)));
        }
        out
    }
}

struct IdGenerator {
    millis: u64,
    counter: u64,
}

impl IdGenerator {
    fn next(&mut self, prefix: &str, descending: bool) -> AppResult<String> {
        self.counter += 1;
        if self.counter > 4095 {
            self.millis += 1;
            self.counter = 0;
        }
        let value = (self.millis << 12) | self.counter;
        let value = (if descending { !value } else { value }) & 0xffffffffffff;
        let mut id = format!("{prefix}_{value:012x}");
        const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
        while id.len() < prefix.len() + 1 + 26 {
            let mut bytes = [0u8; 32];
            getrandom::getrandom(&mut bytes)
                .map_err(|e| AppError::Other(format!("生成 OpenCode ID 失败: {e}")))?;
            for byte in bytes {
                if byte < 248 {
                    id.push(ALPHABET[(byte % 62) as usize] as char);
                }
                if id.len() == prefix.len() + 1 + 26 {
                    break;
                }
            }
        }
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let mut random = [0u8; 16];
            getrandom::getrandom(&mut random).unwrap();
            let path = std::env::temp_dir()
                .join(format!("cc-sessions-opencode-fork-{}", hex::encode(random)));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (TempDir, Connection) {
        let dir = TempDir::new();
        let db = Connection::open(dir.path().join("opencode.db")).unwrap();
        db.execute_batch(include_str!("../tests/fixtures/opencode-fork.sql"))
            .unwrap();
        let messages: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/opencode-fork.json")).unwrap();
        for message in messages {
            db.execute(
                "INSERT INTO message VALUES (?1,'ses_fixture',?2,?2,?3)",
                params![
                    message["id"].as_str().unwrap(),
                    message["data"]["time"]["created"].as_i64().unwrap(),
                    message["data"].to_string(),
                ],
            )
            .unwrap();
            for part in message["parts"].as_array().unwrap() {
                db.execute(
                    "INSERT INTO part VALUES (?1,?2,'ses_fixture',?3,?3,?4)",
                    params![
                        part["id"].as_str().unwrap(),
                        message["id"].as_str().unwrap(),
                        part["created"].as_i64().unwrap(),
                        part["data"].to_string(),
                    ],
                )
                .unwrap();
            }
        }
        (dir, db)
    }

    fn copy(dir: &TempDir, point: Option<OpenCodeForkPoint>) -> AppResult<OpenCodeCopyReport> {
        copy_session_with_lock(
            dir.path().to_string_lossy().into_owned(),
            "ses_fixture".into(),
            opencode_sessions::encode_locator(&dir.path().join("opencode.db"), "ses_fixture")?,
            point,
            &FamilyLock::default(),
        )
    }

    fn point(db: &Connection, part_id: &str) -> OpenCodeForkPoint {
        let event = opencode_sessions::load_preview_events(db, "ses_fixture")
            .unwrap()
            .into_iter()
            .find(|event| event.raw["opencode"]["part_id"] == part_id)
            .unwrap();
        OpenCodeForkPoint {
            event_index: event.index,
            part_id: part_id.into(),
            message_id: event.raw["opencode"]["message_id"].as_str().unwrap().into(),
        }
    }

    fn snapshot(db: &Connection, exclude: &str) -> Vec<Vec<Vec<SqlValue>>> {
        [
            ("session", "id"),
            ("message", "session_id"),
            ("part", "session_id"),
            ("event_sequence", "aggregate_id"),
            ("event", "aggregate_id"),
            ("todo", "session_id"),
            ("session_share", "session_id"),
            ("account", "id"),
            ("project", "id"),
            ("session_input", "session_id"),
            ("session_message", "session_id"),
        ]
        .into_iter()
        .map(|(table, owner)| {
            let mut stmt = db
                .prepare(&format!(
                    "SELECT * FROM {table} WHERE {owner} != ?1 ORDER BY rowid"
                ))
                .unwrap();
            let n = stmt.column_count();
            stmt.query_map([exclude], |r| (0..n).map(|i| r.get(i)).collect())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        })
        .collect()
    }

    fn assert_payloads(db: &Connection, id: &str, count: usize) {
        let original = read_messages(db, "ses_fixture").unwrap();
        let copied = read_messages(db, id).unwrap();
        assert_eq!(copied.len(), count);
        let mut mapping = HashMap::new();
        for (before, after) in original.iter().zip(copied) {
            assert_ne!(before.id, after.id);
            assert!(after.id.starts_with("msg_"));
            assert_eq!(after.id.len(), 30);
            mapping.insert(before.id.clone(), after.id.clone());
            let mut expected = before.data.clone();
            if let Some(parent) = expected["parentID"].as_str().and_then(|p| mapping.get(p)) {
                expected["parentID"] = json!(parent);
            }
            assert_eq!(after.data, expected);
            assert_eq!(after.parts.len(), before.parts.len());
            for ((old_id, old), (new_id, actual)) in before.parts.iter().zip(after.parts) {
                assert_ne!(*old_id, new_id);
                assert_eq!(new_id.len(), 30);
                let mut expected = old.clone();
                if expected["type"] == "compaction" {
                    if let Some(tail) = expected["tail_start_id"].as_str() {
                        if let Some(id) = mapping.get(tail) {
                            expected["tail_start_id"] = json!(id);
                        } else {
                            expected.as_object_mut().unwrap().remove("tail_start_id");
                        }
                    }
                }
                assert_eq!(actual, expected);
            }
        }
    }

    #[test]
    fn full_copy_preserves_payloads_and_all_source_shared_state() {
        let (dir, db) = fixture();
        let before = snapshot(&db, "");
        let report = copy(&dir, None).unwrap();
        assert_eq!((report.message_count, report.part_count), (4, 10));
        assert_eq!(snapshot(&db, &report.new_id), before);
        assert_payloads(&db, &report.new_id, 4);
        let (path, id) = opencode_sessions::resolve_locator(&report.new_rollout_path).unwrap();
        assert_eq!(path, dir.path().join("opencode.db"));
        assert_eq!(id, report.new_id);
        let (title, parent, archived, share, permission, metadata, cost, tokens):
            (String, Option<String>, Option<i64>, Option<String>, Option<String>, String, f64, i64) = db.query_row(
            "SELECT title,parent_id,time_archived,share_url,permission,metadata,cost,tokens_input FROM session WHERE id=?1",
            [&id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).unwrap();
        assert_eq!(title, "Copy fixture (fork #1)");
        assert_eq!(
            (parent, archived, share, permission),
            (None, None, None, None)
        );
        assert_eq!(metadata, "{\"nested\":{\"keep\":true}}");
        assert_eq!((cost, tokens), (0.75, 40));
        let (seq, owner): (i64, Option<String>) = db
            .query_row(
                "SELECT seq,owner_id FROM event_sequence WHERE aggregate_id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((seq, owner), (14, None));
        let mut stmt = db
            .prepare("SELECT seq,type,data FROM event WHERE aggregate_id=?1 ORDER BY seq")
            .unwrap();
        let events: Vec<(i64, String, Value)> = stmt
            .query_map([&id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    serde_json::from_str::<Value>(&r.get::<_, String>(2)?).unwrap(),
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(events.len(), 15);
        assert_eq!(events[0].1, "session.created.1");
        assert_eq!(events[0].2["info"]["cost"], 0);
        for (index, (seq, kind, data)) in events.iter().enumerate() {
            assert_eq!(*seq, index as i64);
            assert_eq!(data["sessionID"], id);
            if kind == "message.updated.1" {
                assert_eq!(data["info"]["sessionID"], id);
            }
            if kind == "message.part.updated.1" {
                assert_eq!(data["part"]["sessionID"], id);
            }
        }
    }

    #[test]
    fn inclusive_cut_copies_whole_message_in_chronological_not_id_or_preview_order() {
        for (part_id, count, parts) in [
            ("prt_a10", 1, 1),
            ("prt_z20", 2, 5),
            ("prt_a20", 2, 5),
            ("prt_b20", 2, 5),
            ("prt_b30", 3, 8),
            ("prt_a40", 4, 10),
        ] {
            let (dir, db) = fixture();
            let before = snapshot(&db, "");
            let report = copy(&dir, Some(point(&db, part_id))).unwrap();
            assert_eq!((report.message_count, report.part_count), (count, parts));
            assert_eq!(snapshot(&db, &report.new_id), before);
            assert_payloads(&db, &report.new_id, count as usize);
        }
    }

    #[test]
    fn compaction_tail_outside_retained_history_is_removed_like_native_fork() {
        let (dir, db) = fixture();
        db.execute("UPDATE part SET data=json_set(data,'$.tail_start_id','msg_a1-after') WHERE id='prt_a30'", []).unwrap();
        let report = copy(&dir, Some(point(&db, "prt_b30"))).unwrap();
        assert_payloads(&db, &report.new_id, 3);
    }

    #[test]
    fn stale_cutoff_and_wrong_message_identity_do_not_create_rows() {
        let (dir, db) = fixture();
        let before = snapshot(&db, "");
        let mut bad = point(&db, "prt_a20");
        bad.event_index += 1;
        assert!(copy(&dir, Some(bad)).is_err());
        let mut bad = point(&db, "prt_a20");
        bad.message_id = "msg_a0-after-wrap".into();
        assert!(copy(&dir, Some(bad)).is_err());
        let mut bad = point(&db, "prt_a20");
        bad.part_id = "prt_missing".into();
        assert!(copy(&dir, Some(bad)).is_err());
        assert_eq!(snapshot(&db, ""), before);
    }

    #[test]
    fn rejects_malformed_json_or_ownership_and_v2_or_required_unknown_schema() {
        for mutation in [
            "UPDATE message SET data='{' WHERE id='msg_a1-after'",
            "UPDATE part SET data='[]' WHERE id='prt_a40'",
            "UPDATE part SET session_id='ses_other' WHERE id='prt_a40'",
            "INSERT INTO session_message VALUES ('ses_fixture','{}')",
            "ALTER TABLE message ADD COLUMN future_state TEXT NOT NULL DEFAULT 'required'",
        ] {
            let (dir, db) = fixture();
            db.execute_batch(mutation).unwrap();
            let before = snapshot(&db, "");
            assert!(copy(&dir, None).is_err(), "{mutation}");
            assert_eq!(snapshot(&db, ""), before);
        }
    }

    #[test]
    fn sqlite_failure_rolls_back_session_messages_and_journal_together() {
        let (dir, db) = fixture();
        db.execute_batch("CREATE TRIGGER reject_copy BEFORE INSERT ON part WHEN NEW.session_id != 'ses_fixture' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
        let before = snapshot(&db, "");
        assert!(copy(&dir, None).is_err());
        assert_eq!(snapshot(&db, ""), before);
    }

    #[test]
    fn old_schema_without_journal_or_optional_session_fields_and_empty_session() {
        let (dir, db) = fixture();
        db.execute_batch(
            "DROP TABLE event; DROP TABLE event_sequence; DELETE FROM part; DELETE FROM message;",
        )
        .unwrap();
        for column in [
            "workspace_id",
            "path",
            "metadata",
            "cost",
            "tokens_input",
            "tokens_output",
            "tokens_reasoning",
            "tokens_cache_read",
            "tokens_cache_write",
        ] {
            db.execute_batch(&format!("ALTER TABLE session DROP COLUMN {column}"))
                .unwrap();
        }
        let report = copy(&dir, None).unwrap();
        assert_eq!((report.message_count, report.part_count), (0, 0));
    }

    #[test]
    fn rejects_mismatched_locator_and_incomplete_event_schema() {
        let (dir, db) = fixture();
        let (other, _) = fixture();
        let before = snapshot(&db, "");
        for locator in [
            opencode_sessions::encode_locator(&dir.path().join("opencode.db"), "ses_other")
                .unwrap(),
            opencode_sessions::encode_locator(&other.path().join("opencode.db"), "ses_fixture")
                .unwrap(),
        ] {
            assert!(copy_session_with_lock(
                dir.path().to_string_lossy().into_owned(),
                "ses_fixture".into(),
                locator,
                None,
                &FamilyLock::default()
            )
            .is_err());
        }
        assert_eq!(snapshot(&db, ""), before);
        db.execute_batch("DROP TABLE event").unwrap();
        assert!(copy(&dir, None)
            .unwrap_err()
            .to_string()
            .contains("事件日志表不完整"));
    }

    #[test]
    fn native_title_suffix_and_id_format_remain_ordered_after_counter_rollover() {
        assert_eq!(fork_title("Task (fork #9)"), "Task (fork #10)");
        assert_eq!(fork_title("Task (fork #no)"), "Task (fork #no) (fork #1)");
        let mut ids = IdGenerator {
            millis: 1234,
            counter: 0,
        };
        let session = ids.next("ses", true).unwrap();
        assert_eq!(
            u64::from_str_radix(&session[4..16], 16).unwrap(),
            (!(1234u64 * 4096 + 1)) & 0xffffffffffff
        );
        let mut prior = String::new();
        for _ in 0..4200 {
            let next = ids.next("msg", false).unwrap();
            assert_eq!(next.len(), 30);
            assert!(next > prior);
            prior = next;
        }
    }
}
