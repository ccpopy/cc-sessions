//! OpenCode SQLite 会话上下文编辑。
//!
//! OpenCode 把消息与内容块保存在 `opencode.db`，不能复用 JSONL 会话编辑器。
//! 本模块只快照和替换目标 session 的 message / part 行，绝不整库覆盖。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::atomic_file;
use crate::error::{AppError, AppResult};
use crate::models::{
    DeletePlan, DeletePlanLine, EditApplyReport, EditHistory, EditHistoryEntry, EditSnapshotInfo,
    PreviewEvent,
};
use crate::paths;

const SNAPSHOT_VERSION: u32 = 1;
const JOURNAL_VERSION: u32 = 1;
const REASON_SELECTED: &str = "selected";
const REASON_CONTEXT_MESSAGE: &str = "context_message";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct StoredMessage {
    id: String,
    session_id: String,
    time_created: i64,
    time_updated: i64,
    data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct StoredPart {
    id: String,
    message_id: String,
    session_id: String,
    time_created: i64,
    time_updated: i64,
    data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionSnapshot {
    version: u32,
    database_path: String,
    session_id: String,
    captured_at: String,
    messages: Vec<StoredMessage>,
    parts: Vec<StoredPart>,
    hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalEntry {
    op_id: String,
    ts: String,
    kind: String,
    provider: String,
    session_id: String,
    rollout_path: String,
    description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_snapshot: Option<String>,
    before_hash: String,
    after_hash: String,
    before_snapshot: String,
    after_snapshot: String,
    changes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalFile {
    version: u32,
    entries: Vec<JournalEntry>,
}

impl Default for JournalFile {
    fn default() -> Self {
        Self {
            version: JOURNAL_VERSION,
            entries: Vec::new(),
        }
    }
}

struct DeleteExpansion {
    plan: DeletePlan,
    message_ids: BTreeSet<String>,
    part_count: u32,
}

#[derive(Debug)]
struct EventRef {
    part_id: String,
    message_id: String,
    part_type: String,
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn sha_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn snapshot_hash(messages: &[StoredMessage], parts: &[StoredPart]) -> AppResult<String> {
    Ok(sha_hex(&serde_json::to_vec(&(messages, parts))?))
}

fn edit_dir(backup_dir: &str, session_id: &str) -> PathBuf {
    PathBuf::from(paths::strip_verbatim(backup_dir))
        .join("session-edits")
        .join(format!("opencode-{}", paths::sanitize_slug(session_id)))
}

fn journal_path(dir: &Path) -> PathBuf {
    dir.join("journal.json")
}

fn resolve_context(locator: &str, session_id: &str) -> AppResult<(PathBuf, String)> {
    let (database_path, locator_session_id) = crate::opencode_sessions::resolve_locator(locator)?;
    if locator_session_id != session_id {
        return Err(AppError::Other(format!(
            "OpenCode 会话定位符与目标会话不一致: {locator_session_id} != {session_id}"
        )));
    }
    Ok((database_path, locator_session_id))
}

fn open_readonly(path: &Path) -> AppResult<Connection> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

fn open_writable(path: &Path) -> AppResult<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA foreign_keys = ON")?;
    Ok(connection)
}

fn load_snapshot(
    connection: &Connection,
    database_path: &Path,
    session_id: &str,
) -> AppResult<SessionSnapshot> {
    let exists = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM session WHERE id = ?1)",
        [session_id],
        |row| row.get::<_, bool>(0),
    )?;
    if !exists {
        return Err(AppError::NotFound(format!(
            "OpenCode 会话不存在: {session_id}"
        )));
    }

    let mut message_statement = connection.prepare(
        "SELECT id, session_id, time_created, time_updated, data
         FROM message WHERE session_id = ?1 ORDER BY time_created ASC, id ASC",
    )?;
    let messages = message_statement
        .query_map([session_id], |row| {
            Ok(StoredMessage {
                id: row.get(0)?,
                session_id: row.get(1)?,
                time_created: row.get(2)?,
                time_updated: row.get(3)?,
                data: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(message_statement);

    let mut part_statement = connection.prepare(
        "SELECT id, message_id, session_id, time_created, time_updated, data
         FROM part WHERE session_id = ?1 ORDER BY time_created ASC, id ASC",
    )?;
    let parts = part_statement
        .query_map([session_id], |row| {
            Ok(StoredPart {
                id: row.get(0)?,
                message_id: row.get(1)?,
                session_id: row.get(2)?,
                time_created: row.get(3)?,
                time_updated: row.get(4)?,
                data: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let hash = snapshot_hash(&messages, &parts)?;
    Ok(SessionSnapshot {
        version: SNAPSHOT_VERSION,
        database_path: database_path.to_string_lossy().into_owned(),
        session_id: session_id.to_string(),
        captured_at: now_rfc3339(),
        messages,
        parts,
        hash,
    })
}

fn validate_snapshot(snapshot: &SessionSnapshot) -> AppResult<()> {
    if snapshot.version != SNAPSHOT_VERSION {
        return Err(AppError::Other(format!(
            "不支持的 OpenCode 快照版本: {}",
            snapshot.version
        )));
    }
    if snapshot
        .messages
        .iter()
        .any(|message| message.session_id != snapshot.session_id)
        || snapshot
            .parts
            .iter()
            .any(|part| part.session_id != snapshot.session_id)
    {
        return Err(AppError::Other("OpenCode 快照包含其他会话的数据".into()));
    }
    let message_ids = snapshot
        .messages
        .iter()
        .map(|message| message.id.as_str())
        .collect::<BTreeSet<_>>();
    if snapshot
        .parts
        .iter()
        .any(|part| !message_ids.contains(part.message_id.as_str()))
    {
        return Err(AppError::Other("OpenCode 快照包含无主内容块".into()));
    }
    let actual_hash = snapshot_hash(&snapshot.messages, &snapshot.parts)?;
    if actual_hash != snapshot.hash {
        return Err(AppError::Other(
            "OpenCode 快照校验失败，内容可能已损坏".into(),
        ));
    }
    Ok(())
}

fn write_json_absent<T: Serialize>(path: &Path, value: &T) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_file::create_with_writer_if_absent(path, |file| {
        serde_json::to_writer_pretty(&mut *file, value)?;
        file.write_all(b"\n")?;
        Ok(())
    })
}

fn replace_json<T: Serialize>(path: &Path, value: &T) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.is_file() {
        let expected = atomic_file::fingerprint(path)?;
        atomic_file::replace_with_writer_if_unchanged(path, &expected, |file| {
            serde_json::to_writer_pretty(&mut *file, value)?;
            file.write_all(b"\n")?;
            Ok(())
        })
    } else {
        write_json_absent(path, value)
    }
}

fn read_journal(dir: &Path) -> AppResult<JournalFile> {
    let path = journal_path(dir);
    if !path.is_file() {
        return Ok(JournalFile::default());
    }
    let journal: JournalFile = serde_json::from_slice(&fs::read(path)?)?;
    if journal.version != JOURNAL_VERSION {
        return Err(AppError::Other(format!(
            "不支持的 OpenCode 编辑历史版本: {}",
            journal.version
        )));
    }
    Ok(journal)
}

fn write_journal(dir: &Path, journal: &JournalFile) -> AppResult<()> {
    replace_json(&journal_path(dir), journal)
}

fn read_snapshot(path: &Path) -> AppResult<SessionSnapshot> {
    if !path.is_file() {
        return Err(AppError::NotFound(path.to_string_lossy().into_owned()));
    }
    let snapshot: SessionSnapshot = serde_json::from_slice(&fs::read(path)?)?;
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn unique_public_snapshot_name(dir: &Path, prefix: &str) -> AppResult<String> {
    let timestamp = chrono::Utc::now().timestamp_millis();
    for sequence in 0..1000u32 {
        let suffix = if sequence == 0 {
            String::new()
        } else {
            format!("-{sequence}")
        };
        let name = format!("{prefix}-{timestamp}{suffix}.json");
        if !dir.join(&name).try_exists()? {
            return Ok(name);
        }
    }
    Err(AppError::Other("无法生成唯一的 OpenCode 快照名称".into()))
}

fn ensure_public_snapshot(
    dir: &Path,
    current: &SessionSnapshot,
    journal: &JournalFile,
) -> AppResult<Option<String>> {
    if journal
        .entries
        .last()
        .is_some_and(|entry| entry.after_hash == current.hash)
    {
        return Ok(None);
    }
    let name = unique_public_snapshot_name(dir, "original")?;
    write_json_absent(&dir.join(&name), current)?;
    Ok(Some(name))
}

fn new_op_id(journal_len: usize) -> String {
    format!(
        "op-{}-{}",
        chrono::Utc::now().timestamp_millis(),
        journal_len + 1
    )
}

fn write_operation_snapshot(
    dir: &Path,
    op_id: &str,
    state: &str,
    snapshot: &SessionSnapshot,
) -> AppResult<String> {
    let relative = PathBuf::from("ops").join(format!("{op_id}-{state}.json"));
    write_json_absent(&dir.join(&relative), snapshot)?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn safe_relative_snapshot_path(dir: &Path, relative: &str) -> AppResult<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(AppError::Other("编辑历史中的快照路径不合法".into()));
    }
    Ok(dir.join(path))
}

fn safe_public_snapshot_name(name: &str) -> AppResult<()> {
    let path = Path::new(name);
    let mut components = path.components();
    if name.contains('/')
        || name.contains('\\')
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || path.extension().and_then(|value| value.to_str()) != Some("json")
    {
        return Err(AppError::Other("快照名称不合法".into()));
    }
    Ok(())
}

fn message_value(message: &StoredMessage) -> Value {
    serde_json::from_str(&message.data).unwrap_or(Value::Null)
}

fn message_role(message: &StoredMessage) -> &str {
    let value = message_value(message);
    match value.get("role").and_then(Value::as_str) {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => "other",
    }
}

fn message_parent_id(message: &StoredMessage) -> Option<String> {
    message_value(message)
        .get("parentID")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn event_ref(event: &PreviewEvent) -> Option<EventRef> {
    let raw = event.raw.get("opencode")?;
    Some(EventRef {
        part_id: raw.get("part_id")?.as_str()?.to_string(),
        message_id: raw.get("message_id")?.as_str()?.to_string(),
        part_type: raw.get("part_type")?.as_str()?.to_string(),
    })
}

fn expand_delete(
    connection: &Connection,
    database_path: &Path,
    locator: &str,
    session_id: &str,
    line_nos: &[usize],
) -> AppResult<DeleteExpansion> {
    let snapshot = load_snapshot(connection, database_path, session_id)?;
    let events = crate::opencode_sessions::load_preview_events(connection, session_id)?;
    let message_map = snapshot
        .messages
        .iter()
        .map(|message| (message.id.as_str(), message))
        .collect::<HashMap<_, _>>();
    let selected = line_nos.iter().copied().collect::<BTreeSet<_>>();
    let mut message_ids = BTreeSet::new();
    let mut blocked = Vec::new();

    for line_no in &selected {
        let Some(event) = events.iter().find(|event| event.index == *line_no) else {
            blocked.push(format!("事件 {} 不存在", line_no + 1));
            continue;
        };
        let Some(reference) = event_ref(event) else {
            blocked.push(format!("事件 {} 缺少 OpenCode 行标识", line_no + 1));
            continue;
        };
        let Some(message) = message_map.get(reference.message_id.as_str()).copied() else {
            blocked.push(format!("事件 {} 的消息不存在", line_no + 1));
            continue;
        };
        match message_role(message) {
            "user" => {
                message_ids.insert(message.id.clone());
                for candidate in &snapshot.messages {
                    if message_role(candidate) == "assistant"
                        && message_parent_id(candidate).as_deref() == Some(message.id.as_str())
                    {
                        message_ids.insert(candidate.id.clone());
                    }
                }
            }
            "assistant" => {
                if let Some(parent_id) = message_parent_id(message) {
                    for candidate in &snapshot.messages {
                        if message_role(candidate) == "assistant"
                            && message_parent_id(candidate).as_deref() == Some(parent_id.as_str())
                        {
                            message_ids.insert(candidate.id.clone());
                        }
                    }
                } else {
                    message_ids.insert(message.id.clone());
                }
            }
            _ => blocked.push(format!("事件 {} 的消息角色不支持删除", line_no + 1)),
        }
    }

    let mut line_reasons = BTreeMap::new();
    for event in &events {
        let Some(reference) = event_ref(event) else {
            continue;
        };
        if message_ids.contains(&reference.message_id) {
            line_reasons.insert(
                event.index,
                if selected.contains(&event.index) {
                    REASON_SELECTED.to_string()
                } else {
                    REASON_CONTEXT_MESSAGE.to_string()
                },
            );
        }
    }
    let lines = line_reasons
        .into_iter()
        .filter_map(|(index, reason)| {
            let event = events.iter().find(|event| event.index == index)?;
            let reference = event_ref(event)?;
            Some(DeletePlanLine {
                line_no: index,
                role: event.role.clone(),
                kind: format!("opencode/{}", reference.part_type),
                summary: event.text_summary.clone(),
                reason,
            })
        })
        .collect();
    let part_count = snapshot
        .parts
        .iter()
        .filter(|part| message_ids.contains(&part.message_id))
        .count() as u32;
    Ok(DeleteExpansion {
        plan: DeletePlan {
            rollout_path: locator.to_string(),
            lines,
            blocked,
        },
        message_ids,
        part_count,
    })
}

fn replace_session_rows(
    transaction: &Transaction<'_>,
    session_id: &str,
    snapshot: &SessionSnapshot,
) -> AppResult<()> {
    validate_snapshot(snapshot)?;
    if snapshot.session_id != session_id {
        return Err(AppError::Other(format!(
            "快照属于其他 OpenCode 会话: {}",
            snapshot.session_id
        )));
    }
    transaction.execute("DELETE FROM part WHERE session_id = ?1", [session_id])?;
    transaction.execute("DELETE FROM message WHERE session_id = ?1", [session_id])?;
    for message in &snapshot.messages {
        transaction.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                message.id,
                message.session_id,
                message.time_created,
                message.time_updated,
                message.data
            ],
        )?;
    }
    for part in &snapshot.parts {
        transaction.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                part.id,
                part.message_id,
                part.session_id,
                part.time_created,
                part.time_updated,
                part.data
            ],
        )?;
    }
    Ok(())
}

fn part_diff(before: &SessionSnapshot, after: &SessionSnapshot) -> (u32, u32, u32) {
    let before_map = before
        .parts
        .iter()
        .map(|part| (part.id.as_str(), part))
        .collect::<HashMap<_, _>>();
    let after_map = after
        .parts
        .iter()
        .map(|part| (part.id.as_str(), part))
        .collect::<HashMap<_, _>>();
    let changed = before_map
        .iter()
        .filter(|(id, part)| after_map.get(**id).is_some_and(|after| after != *part))
        .count() as u32;
    let deleted = before_map
        .keys()
        .filter(|id| !after_map.contains_key(**id))
        .count() as u32;
    let restored = after_map
        .keys()
        .filter(|id| !before_map.contains_key(**id))
        .count() as u32;
    (changed, deleted, restored)
}

fn append_entry(dir: &Path, journal: &mut JournalFile, entry: JournalEntry) -> AppResult<()> {
    journal.entries.push(entry);
    write_journal(dir, journal)
}

fn journal_entry(
    op_id: String,
    kind: &str,
    locator: &str,
    session_id: &str,
    description: String,
    base_description: Option<String>,
    base_snapshot: Option<String>,
    before: &SessionSnapshot,
    after: &SessionSnapshot,
    before_snapshot: String,
    after_snapshot: String,
    changes: u32,
) -> JournalEntry {
    JournalEntry {
        op_id,
        ts: now_rfc3339(),
        kind: kind.to_string(),
        provider: "opencode".into(),
        session_id: session_id.to_string(),
        rollout_path: locator.to_string(),
        description,
        base_description,
        base_snapshot,
        before_hash: before.hash.clone(),
        after_hash: after.hash.clone(),
        before_snapshot,
        after_snapshot,
        changes,
    }
}

pub fn plan_delete(locator: &str, line_nos: &[usize]) -> AppResult<DeletePlan> {
    let (database_path, session_id) = crate::opencode_sessions::resolve_locator(locator)?;
    let connection = open_readonly(&database_path)?;
    Ok(expand_delete(&connection, &database_path, locator, &session_id, line_nos)?.plan)
}

pub fn apply_edit_text(
    locator: &str,
    session_id: &str,
    backup_dir: &str,
    line_no: usize,
    new_text: &str,
) -> AppResult<EditApplyReport> {
    let (database_path, session_id) = resolve_context(locator, session_id)?;
    let mut connection = open_writable(&database_path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let before = load_snapshot(&transaction, &database_path, &session_id)?;
    let events = crate::opencode_sessions::load_preview_events(&transaction, &session_id)?;
    let event = events
        .iter()
        .find(|event| event.index == line_no)
        .ok_or_else(|| AppError::Other(format!("OpenCode 事件 {} 不存在", line_no + 1)))?;
    let reference =
        event_ref(event).ok_or_else(|| AppError::Other("该事件缺少 OpenCode 内容块标识".into()))?;
    if reference.part_type != "text" {
        return Err(AppError::Other("OpenCode 只允许改写 text 内容块".into()));
    }
    let message = before
        .messages
        .iter()
        .find(|message| message.id == reference.message_id)
        .ok_or_else(|| AppError::Other("OpenCode 消息不存在".into()))?;
    if !matches!(message_role(message), "user" | "assistant") {
        return Err(AppError::Other("该 OpenCode 消息角色不允许改写".into()));
    }

    let dir = edit_dir(backup_dir, &session_id);
    let mut journal = read_journal(&dir)?;
    let public_snapshot = ensure_public_snapshot(&dir, &before, &journal)?;
    let op_id = new_op_id(journal.entries.len());
    let before_snapshot = write_operation_snapshot(&dir, &op_id, "before", &before)?;

    let part = before
        .parts
        .iter()
        .find(|part| part.id == reference.part_id)
        .ok_or_else(|| AppError::Other("OpenCode 内容块不存在".into()))?;
    let mut data: Value = serde_json::from_str(&part.data)?;
    if data.get("type").and_then(Value::as_str) != Some("text") {
        return Err(AppError::Other("OpenCode 内容块已不再是 text 类型".into()));
    }
    let object = data
        .as_object_mut()
        .ok_or_else(|| AppError::Other("OpenCode 内容块结构无效".into()))?;
    object.insert("text".into(), Value::String(new_text.to_string()));
    transaction.execute(
        "UPDATE part SET data = ?1 WHERE id = ?2 AND session_id = ?3",
        params![serde_json::to_string(&data)?, reference.part_id, session_id],
    )?;
    let after = load_snapshot(&transaction, &database_path, &session_id)?;
    if after.hash == before.hash {
        return Err(AppError::Other("消息文本没有变化".into()));
    }
    let after_snapshot = write_operation_snapshot(&dir, &op_id, "after", &after)?;
    transaction.commit()?;

    let description = format!("改写 OpenCode 第 {} 个事件文本", line_no + 1);
    let entry = journal_entry(
        op_id.clone(),
        "edit_text",
        locator,
        &session_id,
        description.clone(),
        Some(description),
        public_snapshot.clone(),
        &before,
        &after,
        before_snapshot,
        after_snapshot,
        1,
    );
    append_entry(&dir, &mut journal, entry)?;
    Ok(EditApplyReport {
        op_id,
        kind: "edit_text".into(),
        snapshot_created: public_snapshot,
        changed_lines: 1,
        deleted_lines: 0,
        restored_lines: 0,
    })
}

pub fn apply_delete(
    locator: &str,
    session_id: &str,
    backup_dir: &str,
    line_nos: &[usize],
) -> AppResult<EditApplyReport> {
    if line_nos.is_empty() {
        return Err(AppError::Other("未选择要删除的 OpenCode 事件".into()));
    }
    let (database_path, session_id) = resolve_context(locator, session_id)?;
    let mut connection = open_writable(&database_path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let before = load_snapshot(&transaction, &database_path, &session_id)?;
    let expansion = expand_delete(&transaction, &database_path, locator, &session_id, line_nos)?;
    if !expansion.plan.blocked.is_empty() {
        return Err(AppError::Other(format!(
            "存在不可删除的 OpenCode 事件：{}",
            expansion.plan.blocked.join("；")
        )));
    }
    if expansion.message_ids.is_empty() {
        return Err(AppError::Other("没有可删除的 OpenCode 消息".into()));
    }

    let dir = edit_dir(backup_dir, &session_id);
    let mut journal = read_journal(&dir)?;
    let public_snapshot = ensure_public_snapshot(&dir, &before, &journal)?;
    let op_id = new_op_id(journal.entries.len());
    let before_snapshot = write_operation_snapshot(&dir, &op_id, "before", &before)?;
    for message_id in &expansion.message_ids {
        transaction.execute(
            "DELETE FROM message WHERE id = ?1 AND session_id = ?2",
            params![message_id, session_id],
        )?;
    }
    let after = load_snapshot(&transaction, &database_path, &session_id)?;
    let after_snapshot = write_operation_snapshot(&dir, &op_id, "after", &after)?;
    transaction.commit()?;

    let selected_count = line_nos.iter().copied().collect::<BTreeSet<_>>().len();
    let description = format!(
        "删除 {} 个 OpenCode 内容块（选中 {}，按同轮消息级联）",
        expansion.part_count, selected_count
    );
    let entry = journal_entry(
        op_id.clone(),
        "delete_events",
        locator,
        &session_id,
        description.clone(),
        Some(description),
        public_snapshot.clone(),
        &before,
        &after,
        before_snapshot,
        after_snapshot,
        expansion.part_count,
    );
    append_entry(&dir, &mut journal, entry)?;
    Ok(EditApplyReport {
        op_id,
        kind: "delete_events".into(),
        snapshot_created: public_snapshot,
        changed_lines: 0,
        deleted_lines: expansion.part_count,
        restored_lines: 0,
    })
}

pub fn undo_last(locator: &str, session_id: &str, backup_dir: &str) -> AppResult<EditApplyReport> {
    let (database_path, session_id) = resolve_context(locator, session_id)?;
    let dir = edit_dir(backup_dir, &session_id);
    let mut journal = read_journal(&dir)?;
    let last = journal
        .entries
        .last()
        .cloned()
        .ok_or_else(|| AppError::Other("该 OpenCode 会话没有编辑记录".into()))?;
    let mut connection = open_writable(&database_path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let before = load_snapshot(&transaction, &database_path, &session_id)?;
    if before.hash != last.after_hash {
        return Err(AppError::Other(
            "OpenCode 会话上下文已在本工具之外发生变化，无法直接撤销；可从快照还原".into(),
        ));
    }
    let target_path = safe_relative_snapshot_path(&dir, &last.before_snapshot)?;
    let target = read_snapshot(&target_path)?;
    if target.session_id != session_id {
        return Err(AppError::Other("撤销快照属于其他 OpenCode 会话".into()));
    }

    let op_id = new_op_id(journal.entries.len());
    let before_snapshot = write_operation_snapshot(&dir, &op_id, "before", &before)?;
    replace_session_rows(&transaction, &session_id, &target)?;
    let after = load_snapshot(&transaction, &database_path, &session_id)?;
    if after.hash != target.hash {
        return Err(AppError::Other("OpenCode 撤销后的会话校验失败".into()));
    }
    let after_snapshot = write_operation_snapshot(&dir, &op_id, "after", &after)?;
    transaction.commit()?;

    let redo = last.kind == "undo";
    let base = last
        .base_description
        .clone()
        .unwrap_or_else(|| last.description.clone());
    let description = if redo {
        format!("重做：{base}")
    } else {
        format!("撤销：{base}")
    };
    let (changed, deleted, restored) = part_diff(&before, &after);
    let entry = journal_entry(
        op_id.clone(),
        "undo",
        locator,
        &session_id,
        description,
        Some(base),
        None,
        &before,
        &after,
        before_snapshot,
        after_snapshot,
        changed + deleted + restored,
    );
    append_entry(&dir, &mut journal, entry)?;
    Ok(EditApplyReport {
        op_id,
        kind: "undo".into(),
        snapshot_created: None,
        changed_lines: changed,
        deleted_lines: deleted,
        restored_lines: restored,
    })
}

pub fn restore_snapshot(
    locator: &str,
    session_id: &str,
    backup_dir: &str,
    snapshot_name: &str,
) -> AppResult<EditApplyReport> {
    safe_public_snapshot_name(snapshot_name)?;
    let (database_path, session_id) = resolve_context(locator, session_id)?;
    let dir = edit_dir(backup_dir, &session_id);
    let target = read_snapshot(&dir.join(snapshot_name))?;
    if target.session_id != session_id {
        return Err(AppError::Other("快照属于其他 OpenCode 会话".into()));
    }

    let mut journal = read_journal(&dir)?;
    let mut connection = open_writable(&database_path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let before = load_snapshot(&transaction, &database_path, &session_id)?;
    let pre_restore_name = unique_public_snapshot_name(&dir, "pre-restore")?;
    write_json_absent(&dir.join(&pre_restore_name), &before)?;
    let op_id = new_op_id(journal.entries.len());
    let before_snapshot = write_operation_snapshot(&dir, &op_id, "before", &before)?;
    replace_session_rows(&transaction, &session_id, &target)?;
    let after = load_snapshot(&transaction, &database_path, &session_id)?;
    if after.hash != target.hash {
        return Err(AppError::Other("OpenCode 快照还原后的会话校验失败".into()));
    }
    let after_snapshot = write_operation_snapshot(&dir, &op_id, "after", &after)?;
    transaction.commit()?;

    let (changed, deleted, restored) = part_diff(&before, &after);
    let description =
        format!("还原 OpenCode 快照 {snapshot_name}（还原前状态已保存为 {pre_restore_name}）");
    let entry = journal_entry(
        op_id.clone(),
        "restore_snapshot",
        locator,
        &session_id,
        description,
        None,
        Some(snapshot_name.to_string()),
        &before,
        &after,
        before_snapshot,
        after_snapshot,
        changed + deleted + restored,
    );
    append_entry(&dir, &mut journal, entry)?;
    Ok(EditApplyReport {
        op_id,
        kind: "restore_snapshot".into(),
        snapshot_created: Some(pre_restore_name),
        changed_lines: changed,
        deleted_lines: deleted,
        restored_lines: restored,
    })
}

pub fn history(locator: &str, session_id: &str, backup_dir: &str) -> AppResult<EditHistory> {
    let (database_path, session_id) = resolve_context(locator, session_id)?;
    let dir = edit_dir(backup_dir, &session_id);
    let journal = read_journal(&dir)?;
    let connection = open_readonly(&database_path)?;
    let current = load_snapshot(&connection, &database_path, &session_id)?;

    let mut snapshots = Vec::new();
    if dir.is_dir() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".json") || name == "journal.json" {
                continue;
            }
            let metadata = entry.metadata()?;
            let created_at = read_snapshot(&entry.path())
                .map(|snapshot| snapshot.captured_at)
                .unwrap_or_else(|_| {
                    metadata
                        .modified()
                        .ok()
                        .map(chrono::DateTime::<chrono::Utc>::from)
                        .map(|value| value.to_rfc3339())
                        .unwrap_or_default()
                });
            snapshots.push(EditSnapshotInfo {
                name,
                created_at,
                bytes: metadata.len(),
            });
        }
    }
    snapshots.sort_by(|a, b| b.name.cmp(&a.name));

    let (undo_available, undo_blocked_reason) = match journal.entries.last() {
        None => (false, None),
        Some(last) if last.after_hash != current.hash => (
            false,
            Some("OpenCode 会话上下文已在本工具之外发生变化，只能从快照还原".into()),
        ),
        Some(last) => {
            let path = safe_relative_snapshot_path(&dir, &last.before_snapshot)?;
            if path.is_file() {
                (true, None)
            } else {
                (false, Some("撤销所需的内部快照已丢失".into()))
            }
        }
    };
    let entries = journal
        .entries
        .iter()
        .rev()
        .map(|entry| EditHistoryEntry {
            op_id: entry.op_id.clone(),
            ts: entry.ts.clone(),
            kind: entry.kind.clone(),
            description: entry.description.clone(),
            changes: entry.changes,
        })
        .collect();
    Ok(EditHistory {
        entries,
        snapshots,
        undo_available,
        undo_blocked_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use rusqlite::{params, Connection};
    use serde_json::{json, Value};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        data_dir: PathBuf,
        backup_dir: PathBuf,
        locator: String,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).ok();
        }
    }

    fn fixture() -> AppResult<Fixture> {
        let root = std::env::temp_dir().join(format!(
            "cc-opencode-edit-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let data_dir = root.join("opencode");
        let backup_dir = root.join("backups");
        fs::create_dir_all(&data_dir)?;
        fs::create_dir_all(&backup_dir)?;
        let connection = Connection::open(crate::opencode_sessions::database_path(&data_dir))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE, session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
        )?;
        for (id, title) in [("ses_target", "Target"), ("ses_other", "Other")] {
            connection.execute(
                "INSERT INTO session VALUES (?1, 'global', NULL, 'slug', 'F:\\project', ?2, '1.0', 1000, 9000, NULL)",
                params![id, title],
            )?;
        }
        for (id, session, created, data) in [
            ("msg_u1", "ses_target", 1000, json!({"role":"user"})),
            (
                "msg_a1_process",
                "ses_target",
                1100,
                json!({"role":"assistant","parentID":"msg_u1","finish":"tool-calls"}),
            ),
            (
                "msg_a1_final",
                "ses_target",
                1200,
                json!({"role":"assistant","parentID":"msg_u1","finish":"stop"}),
            ),
            ("msg_u2", "ses_target", 2000, json!({"role":"user"})),
            (
                "msg_a2_final",
                "ses_target",
                2100,
                json!({"role":"assistant","parentID":"msg_u2","finish":"stop"}),
            ),
            ("msg_other", "ses_other", 3000, json!({"role":"user"})),
        ] {
            connection.execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?3, ?4)",
                params![id, session, created, data.to_string()],
            )?;
        }
        for (id, message, session, created, data) in [
            (
                "part_u1",
                "msg_u1",
                "ses_target",
                1000,
                json!({"type":"text","text":"hello"}),
            ),
            (
                "part_reasoning",
                "msg_a1_process",
                "ses_target",
                1100,
                json!({"type":"reasoning","text":"inspect"}),
            ),
            (
                "part_tool",
                "msg_a1_process",
                "ses_target",
                1150,
                json!({"type":"tool","callID":"call_1","tool":"read","state":{"input":{}}}),
            ),
            (
                "part_a1",
                "msg_a1_final",
                "ses_target",
                1200,
                json!({"type":"text","text":"answer one"}),
            ),
            (
                "part_u2",
                "msg_u2",
                "ses_target",
                2000,
                json!({"type":"text","text":"keep"}),
            ),
            (
                "part_a2",
                "msg_a2_final",
                "ses_target",
                2100,
                json!({"type":"text","text":"keep answer"}),
            ),
            (
                "part_other",
                "msg_other",
                "ses_other",
                3000,
                json!({"type":"text","text":"other"}),
            ),
        ] {
            connection.execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                params![id, message, session, created, data.to_string()],
            )?;
        }
        drop(connection);

        let locator = crate::opencode_sessions::list_sessions(&data_dir)?
            .into_iter()
            .find(|session| session.id == "ses_target")
            .expect("target session")
            .rollout_path;
        Ok(Fixture {
            root,
            data_dir,
            backup_dir,
            locator,
        })
    }

    fn open_connection(fixture: &Fixture) -> AppResult<Connection> {
        Ok(Connection::open(crate::opencode_sessions::database_path(
            &fixture.data_dir,
        ))?)
    }

    fn part_data(connection: &Connection, id: &str) -> AppResult<Value> {
        let raw = connection.query_row("SELECT data FROM part WHERE id = ?1", [id], |row| {
            row.get::<_, String>(0)
        })?;
        Ok(serde_json::from_str(&raw)?)
    }

    fn session_rows(
        connection: &Connection,
        session_id: &str,
    ) -> AppResult<(Vec<String>, Vec<String>)> {
        fn rows(connection: &Connection, sql: &str, session_id: &str) -> AppResult<Vec<String>> {
            let mut statement = connection.prepare(sql)?;
            let result = statement
                .query_map([session_id], |row| {
                    Ok(format!(
                        "{}|{}|{}|{}|{}",
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(result)
        }
        Ok((
            rows(
                connection,
                "SELECT id, session_id, time_created, time_updated, data FROM message WHERE session_id = ?1 ORDER BY time_created, id",
                session_id,
            )?,
            rows(
                connection,
                "SELECT id, session_id, time_created, time_updated, data FROM part WHERE session_id = ?1 ORDER BY time_created, id",
                session_id,
            )?,
        ))
    }

    fn backup_dir(fixture: &Fixture) -> String {
        fixture.backup_dir.to_string_lossy().into_owned()
    }

    #[test]
    fn edit_text_updates_only_selected_part_and_preserves_timestamps() -> AppResult<()> {
        let fixture = fixture()?;
        let before = session_rows(&open_connection(&fixture)?, "ses_target")?;

        let report = apply_edit_text(
            &fixture.locator,
            "ses_target",
            &backup_dir(&fixture),
            0,
            "updated prompt",
        )?;
        assert_eq!(report.changed_lines, 1);
        assert!(report.snapshot_created.is_some());

        let connection = open_connection(&fixture)?;
        assert_eq!(part_data(&connection, "part_u1")?["text"], "updated prompt");
        let after = session_rows(&connection, "ses_target")?;
        assert_eq!(
            before.0, after.0,
            "message rows and timestamps must stay exact"
        );
        assert_eq!(before.1.len(), after.1.len());
        assert_eq!(part_data(&connection, "part_u2")?["text"], "keep");
        Ok(())
    }

    #[test]
    fn deleting_process_event_removes_complete_assistant_chain_only() -> AppResult<()> {
        let fixture = fixture()?;
        let plan = plan_delete(&fixture.locator, &[2])?;
        assert_eq!(plan.lines.len(), 3);
        assert!(plan.lines.iter().any(|line| line.reason == "selected"));
        assert!(plan
            .lines
            .iter()
            .any(|line| line.reason == "context_message"));

        apply_delete(&fixture.locator, "ses_target", &backup_dir(&fixture), &[2])?;
        let connection = open_connection(&fixture)?;
        let remaining = session_rows(&connection, "ses_target")?;
        assert!(remaining.0.iter().any(|row| row.starts_with("msg_u1|")));
        assert!(!remaining
            .0
            .iter()
            .any(|row| row.starts_with("msg_a1_process|")));
        assert!(!remaining
            .0
            .iter()
            .any(|row| row.starts_with("msg_a1_final|")));
        assert!(remaining.0.iter().any(|row| row.starts_with("msg_u2|")));
        assert_eq!(part_data(&connection, "part_other")?["text"], "other");
        Ok(())
    }

    #[test]
    fn deleting_user_event_removes_its_response_chain_but_keeps_other_turns() -> AppResult<()> {
        let fixture = fixture()?;
        apply_delete(&fixture.locator, "ses_target", &backup_dir(&fixture), &[0])?;
        let connection = open_connection(&fixture)?;
        let remaining = session_rows(&connection, "ses_target")?;
        for removed in ["msg_u1", "msg_a1_process", "msg_a1_final"] {
            assert!(!remaining
                .0
                .iter()
                .any(|row| row.starts_with(&format!("{removed}|"))));
        }
        assert!(remaining.0.iter().any(|row| row.starts_with("msg_u2|")));
        assert!(remaining
            .0
            .iter()
            .any(|row| row.starts_with("msg_a2_final|")));
        Ok(())
    }

    #[test]
    fn undo_restores_exact_session_rows() -> AppResult<()> {
        let fixture = fixture()?;
        let before = session_rows(&open_connection(&fixture)?, "ses_target")?;
        apply_delete(&fixture.locator, "ses_target", &backup_dir(&fixture), &[2])?;
        undo_last(&fixture.locator, "ses_target", &backup_dir(&fixture))?;
        let after = session_rows(&open_connection(&fixture)?, "ses_target")?;
        assert_eq!(after, before);
        Ok(())
    }

    #[test]
    fn history_supports_undo_and_redo_without_losing_snapshots() -> AppResult<()> {
        let fixture = fixture()?;
        apply_edit_text(
            &fixture.locator,
            "ses_target",
            &backup_dir(&fixture),
            0,
            "edited",
        )?;
        let first_history = history(&fixture.locator, "ses_target", &backup_dir(&fixture))?;
        assert_eq!(first_history.entries.len(), 1);
        assert_eq!(first_history.snapshots.len(), 1);
        assert!(first_history.undo_available);

        undo_last(&fixture.locator, "ses_target", &backup_dir(&fixture))?;
        assert_eq!(
            part_data(&open_connection(&fixture)?, "part_u1")?["text"],
            "hello"
        );
        let undo_history = history(&fixture.locator, "ses_target", &backup_dir(&fixture))?;
        assert_eq!(undo_history.entries.len(), 2);
        assert!(undo_history.undo_available);

        undo_last(&fixture.locator, "ses_target", &backup_dir(&fixture))?;
        assert_eq!(
            part_data(&open_connection(&fixture)?, "part_u1")?["text"],
            "edited"
        );
        Ok(())
    }

    #[test]
    fn undo_blocks_after_external_target_session_change() -> AppResult<()> {
        let fixture = fixture()?;
        apply_edit_text(
            &fixture.locator,
            "ses_target",
            &backup_dir(&fixture),
            0,
            "edited",
        )?;
        let connection = open_connection(&fixture)?;
        connection.execute(
            "UPDATE part SET data = ?1 WHERE id = 'part_u2'",
            [json!({"type":"text","text":"external"}).to_string()],
        )?;
        drop(connection);

        let error = undo_last(&fixture.locator, "ses_target", &backup_dir(&fixture))
            .expect_err("external change must block undo");
        assert!(error.to_string().contains("外部") || error.to_string().contains("变化"));
        Ok(())
    }

    #[test]
    fn restoring_snapshot_only_replaces_target_session() -> AppResult<()> {
        let fixture = fixture()?;
        let original_target = session_rows(&open_connection(&fixture)?, "ses_target")?;
        let report = apply_edit_text(
            &fixture.locator,
            "ses_target",
            &backup_dir(&fixture),
            0,
            "edited",
        )?;
        let snapshot = report.snapshot_created.expect("original snapshot");

        let connection = open_connection(&fixture)?;
        connection.execute(
            "UPDATE part SET data = ?1 WHERE id = 'part_other'",
            [json!({"type":"text","text":"other changed"}).to_string()],
        )?;
        drop(connection);

        restore_snapshot(
            &fixture.locator,
            "ses_target",
            &backup_dir(&fixture),
            &snapshot,
        )?;
        let connection = open_connection(&fixture)?;
        assert_eq!(session_rows(&connection, "ses_target")?, original_target);
        assert_eq!(
            part_data(&connection, "part_other")?["text"],
            "other changed"
        );
        Ok(())
    }

    #[test]
    fn snapshot_names_are_confined_to_the_edit_directory() -> AppResult<()> {
        let fixture = fixture()?;
        let error = restore_snapshot(
            &fixture.locator,
            "ses_target",
            &backup_dir(&fixture),
            Path::new("..\\outside.json").to_string_lossy().as_ref(),
        )
        .expect_err("path traversal must be rejected");
        assert!(error.to_string().contains("快照名称"));
        Ok(())
    }
}
