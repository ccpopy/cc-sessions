//! Cursor 会话的快照导出与导入，供备份与会话包使用。
//!
//! **绝不整库拷贝**：`state.vscdb` 里除了会话，还有 `ItemTable` 保存的登录态、
//! 工作区状态和各种扩展数据；而且这个文件实测有 8 GB。快照只带走一个会话真正拥有的
//! 三部分内容：
//!
//! - `composerHeaders` 的那一行（会话头，含标题、时间、归档位、项目路径）
//! - `cursorDiskKV` 的 `composerData:<id>`（有序气泡索引）
//! - `cursorDiskKV` 的 `bubbleId:<id>:<气泡>`（消息本体）
//!
//! 导入时按当前库的列做交集写入，Cursor 升级新增列不会导致导入失败。

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::cursor_sessions;
use crate::error::{AppError, AppResult};

const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorSessionSnapshot {
    pub version: u32,
    pub exported_at: String,
    pub session_id: String,
    pub source_cwd: String,
    pub source_updated_at: i64,
    /// `composerHeaders` 的列名与取值，按列名存以便导入时做交集。
    pub header: HashMap<String, SnapshotValue>,
    /// 会话正文的 `composerData:<id>` 原文。
    pub composer_data: Option<String>,
    /// 气泡：键去掉 `bubbleId:<id>:` 前缀后的气泡 id → 原文。
    pub bubbles: Vec<(String, String)>,
}

/// SQLite 的取值在 JSON 里的表示。`cursorDiskKV` 同一列既有 TEXT 也有 BLOB。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum SnapshotValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    /// base64 编码的二进制。
    Blob(String),
}

impl SnapshotValue {
    fn from_sql(value: SqlValue) -> Self {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        match value {
            SqlValue::Null => Self::Null,
            SqlValue::Integer(value) => Self::Integer(value),
            SqlValue::Real(value) => Self::Real(value),
            SqlValue::Text(value) => Self::Text(value),
            SqlValue::Blob(value) => Self::Blob(STANDARD.encode(value)),
        }
    }

    fn to_sql(&self) -> AppResult<SqlValue> {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        Ok(match self {
            Self::Null => SqlValue::Null,
            Self::Integer(value) => SqlValue::Integer(*value),
            Self::Real(value) => SqlValue::Real(*value),
            Self::Text(value) => SqlValue::Text(value.clone()),
            Self::Blob(value) => SqlValue::Blob(
                STANDARD
                    .decode(value)
                    .map_err(|error| AppError::Other(format!("快照中的二进制无法解码: {error}")))?,
            ),
        })
    }
}

pub fn export_snapshot(cursor_dir: &Path, session_id: &str) -> AppResult<CursorSessionSnapshot> {
    validate_session_id(session_id)?;
    let db = cursor_sessions::state_db_path(cursor_dir);
    let connection = cursor_sessions::open_readonly(&db)?;
    if !cursor_sessions::table_exists(&connection, "composerHeaders")? {
        return Err(AppError::Other(
            "这个 Cursor 版本还没有 composerHeaders 表，无法导出会话".into(),
        ));
    }

    let columns = table_columns(&connection, "composerHeaders")?;
    let placeholders = columns.join(", ");
    let header = connection
        .query_row(
            &format!("SELECT {placeholders} FROM composerHeaders WHERE composerId = ?1"),
            [session_id],
            |row| {
                let mut out = HashMap::new();
                for (index, name) in columns.iter().enumerate() {
                    out.insert(name.clone(), SnapshotValue::from_sql(row.get(index)?));
                }
                Ok(out)
            },
        )
        .optional()?
        .ok_or_else(|| AppError::NotFound(format!("Cursor 会话不存在: {session_id}")))?;

    let composer_data = read_text(&connection, &format!("composerData:{session_id}"))?;
    let mut bubbles = Vec::new();
    if let Some(raw) = composer_data.as_deref() {
        let data =
            serde_json::from_str::<serde_json::Value>(raw).unwrap_or(serde_json::Value::Null);
        for entry in data
            .get("fullConversationHeadersOnly")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(bubble) = entry.get("bubbleId").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if let Some(text) = read_text(&connection, &format!("bubbleId:{session_id}:{bubble}"))?
            {
                bubbles.push((bubble.to_string(), text));
            }
        }
    }

    let source_cwd = header
        .get("value")
        .and_then(|value| match value {
            SnapshotValue::Text(text) => serde_json::from_str::<serde_json::Value>(text).ok(),
            _ => None,
        })
        .and_then(|value| {
            value
                .get("workspaceIdentifier")?
                .get("uri")?
                .get("fsPath")?
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_default();
    let source_updated_at = match header.get("lastUpdatedAt") {
        Some(SnapshotValue::Integer(value)) => *value,
        _ => 0,
    };

    Ok(CursorSessionSnapshot {
        version: SNAPSHOT_VERSION,
        exported_at: chrono::Utc::now().to_rfc3339(),
        session_id: session_id.to_string(),
        source_cwd,
        source_updated_at,
        header,
        composer_data,
        bubbles,
    })
}

pub fn write_snapshot(path: &Path, snapshot: &CursorSessionSnapshot) -> AppResult<()> {
    validate_snapshot(snapshot)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(snapshot)?)?;
    Ok(())
}

pub fn read_snapshot(path: &Path, expected_id: &str) -> AppResult<CursorSessionSnapshot> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        return Err(AppError::Path(format!(
            "Cursor 会话快照必须是普通文件: {}",
            path.to_string_lossy()
        )));
    }
    let snapshot: CursorSessionSnapshot = serde_json::from_slice(&fs::read(path)?)?;
    validate_snapshot(&snapshot)?;
    if snapshot.session_id != expected_id {
        return Err(AppError::Other(format!(
            "Cursor 会话快照 id 与清单不一致: 快照 {}，清单 {expected_id}",
            snapshot.session_id
        )));
    }
    Ok(snapshot)
}

pub fn verify_snapshot_file(path: &Path, expected_id: &str) -> AppResult<()> {
    read_snapshot(path, expected_id).map(|_| ())
}

/// 把快照写回当前的 Cursor 数据库。
///
/// 与其它写操作一样先确认 Cursor 已退出；整个导入在一个事务里完成。
pub fn import_snapshot(
    cursor_dir: &Path,
    snapshot: &CursorSessionSnapshot,
    overwrite: bool,
) -> AppResult<bool> {
    validate_snapshot(snapshot)?;
    crate::cursor_mutate::ensure_cursor_not_running()?;
    let db = cursor_sessions::state_db_path(cursor_dir);
    if !db.is_file() {
        return Err(AppError::NotFound(format!(
            "Cursor 数据库不存在: {}",
            db.to_string_lossy()
        )));
    }
    let mut connection = Connection::open(&db)?;
    if !cursor_sessions::table_exists(&connection, "composerHeaders")? {
        return Err(AppError::Other(
            "这个 Cursor 版本还没有 composerHeaders 表，无法导入会话".into(),
        ));
    }
    let transaction = connection.transaction()?;
    let exists = transaction
        .query_row(
            "SELECT 1 FROM composerHeaders WHERE composerId = ?1",
            [&snapshot.session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();
    if exists && !overwrite {
        return Ok(false);
    }

    // 取当前库与快照的列交集：Cursor 升级新增列时旧快照仍然可用，反之亦然。
    let columns = table_columns(&transaction, "composerHeaders")?
        .into_iter()
        .filter(|name| snapshot.header.contains_key(name))
        .collect::<Vec<_>>();
    if !columns.iter().any(|name| name == "composerId") {
        return Err(AppError::Other("快照缺少 composerId，无法导入".into()));
    }
    let names = columns.join(", ");
    let marks = (1..=columns.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let values = columns
        .iter()
        .map(|name| snapshot.header[name].to_sql())
        .collect::<AppResult<Vec<_>>>()?;
    transaction.execute(
        &format!("INSERT OR REPLACE INTO composerHeaders ({names}) VALUES ({marks})"),
        rusqlite::params_from_iter(values),
    )?;

    // 覆盖导入时先清掉旧气泡，避免残留上一版会话的内容。
    transaction.execute(
        "DELETE FROM cursorDiskKV WHERE key LIKE ?1 ESCAPE '\\'",
        [format!("bubbleId:{}:%", escape_like(&snapshot.session_id))],
    )?;
    if let Some(data) = snapshot.composer_data.as_deref() {
        transaction.execute(
            "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![format!("composerData:{}", snapshot.session_id), data],
        )?;
    }
    for (bubble, raw) in &snapshot.bubbles {
        transaction.execute(
            "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![format!("bubbleId:{}:{bubble}", snapshot.session_id), raw],
        )?;
    }
    transaction.commit()?;
    Ok(true)
}

fn validate_snapshot(snapshot: &CursorSessionSnapshot) -> AppResult<()> {
    if snapshot.version == 0 || snapshot.version > SNAPSHOT_VERSION {
        return Err(AppError::Other(format!(
            "不支持的 Cursor 会话快照版本: {}",
            snapshot.version
        )));
    }
    validate_session_id(&snapshot.session_id)?;
    for (bubble, _) in &snapshot.bubbles {
        validate_session_id(bubble)?;
    }
    Ok(())
}

/// 会话与气泡 id 都会拼进 SQL 的 key，只允许安全字符。
fn validate_session_id(id: &str) -> AppResult<()> {
    let trimmed = id.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return Err(AppError::Other(format!("Cursor 会话 id 无效: {id}")));
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return Err(AppError::Other(format!("Cursor 会话 id 含非法字符: {id}")));
    }
    Ok(())
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn table_columns(connection: &Connection, table: &str) -> AppResult<Vec<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn read_text(connection: &Connection, key: &str) -> AppResult<Option<String>> {
    let value = connection
        .query_row(
            "SELECT value FROM cursorDiskKV WHERE key = ?1",
            [key],
            |row| row.get::<_, SqlValue>(0),
        )
        .optional()?;
    Ok(match value {
        Some(SqlValue::Text(text)) => Some(text),
        Some(SqlValue::Blob(bytes)) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn fixture(name: &str) -> AppResult<Fixture> {
        let root = std::env::temp_dir().join(format!(
            "cc-sessions-cursor-transfer-{name}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("globalStorage"))?;
        let connection = Connection::open(cursor_sessions::state_db_path(&root))?;
        connection.execute_batch(
            "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT,
                createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER,
                isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT);
             CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
             CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);",
        )?;
        connection.execute(
            "INSERT INTO composerHeaders VALUES ('s1', 'ws', 1000, 2000, 0, 0, 2000, NULL, ?1)",
            [json!({
                "name": "会话",
                "workspaceIdentifier": {"uri": {"fsPath": "/work/demo"}}
            })
            .to_string()],
        )?;
        connection.execute(
            "INSERT INTO cursorDiskKV VALUES ('composerData:s1', ?1)",
            [
                json!({"fullConversationHeadersOnly": [{"bubbleId": "b1"}, {"bubbleId": "b2"}]})
                    .to_string(),
            ],
        )?;
        for (bubble, text) in [("b1", "第一条"), ("b2", "第二条")] {
            connection.execute(
                "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
                rusqlite::params![
                    format!("bubbleId:s1:{bubble}"),
                    json!({"type": 1, "text": text}).to_string()
                ],
            )?;
        }
        // 另一个会话的数据，导入导出都不能碰。
        connection.execute(
            "INSERT INTO cursorDiskKV VALUES ('bubbleId:s2:b1', ?1)",
            [json!({"type": 1, "text": "别人的"}).to_string()],
        )?;
        connection.execute("INSERT INTO ItemTable VALUES ('secret', 'token')", [])?;
        drop(connection);
        Ok(Fixture { root })
    }

    #[test]
    fn a_snapshot_carries_only_the_session_it_names() -> AppResult<()> {
        let fixture = fixture("export")?;
        let snapshot = export_snapshot(&fixture.root, "s1")?;
        assert_eq!(snapshot.session_id, "s1");
        assert_eq!(snapshot.source_cwd, "/work/demo");
        assert_eq!(snapshot.source_updated_at, 2000);
        assert_eq!(snapshot.bubbles.len(), 2);
        // 快照里不该出现别的会话或 ItemTable 的内容。
        let serialized = serde_json::to_string(&snapshot)?;
        assert!(!serialized.contains("别人的"));
        assert!(!serialized.contains("token"));
        Ok(())
    }

    #[test]
    fn importing_into_a_fresh_database_restores_the_conversation() -> AppResult<()> {
        let source = fixture("import-src")?;
        let target = fixture("import-dst")?;
        let snapshot = export_snapshot(&source.root, "s1")?;

        // 目标库先清掉这个会话，模拟"另一台机器上没有它"。
        let connection = Connection::open(cursor_sessions::state_db_path(&target.root))?;
        connection.execute("DELETE FROM composerHeaders WHERE composerId = 's1'", [])?;
        connection.execute(
            "DELETE FROM cursorDiskKV WHERE key LIKE 'bubbleId:s1:%'",
            [],
        )?;
        connection.execute("DELETE FROM cursorDiskKV WHERE key = 'composerData:s1'", [])?;
        drop(connection);

        let _probe = crate::cursor_mutate::CursorRunningProbe::not_running();
        assert!(import_snapshot(&target.root, &snapshot, false)?);

        let connection = Connection::open(cursor_sessions::state_db_path(&target.root))?;
        let name: String = connection.query_row(
            "SELECT value FROM composerHeaders WHERE composerId = 's1'",
            [],
            |row| row.get(0),
        )?;
        assert!(name.contains("会话"));
        let bubbles: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key LIKE 'bubbleId:s1:%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(bubbles, 2);
        // 目标库里别的会话不受影响。
        let others: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key = 'bubbleId:s2:b1'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(others, 1);
        Ok(())
    }

    #[test]
    fn an_existing_session_is_kept_unless_overwrite_is_requested() -> AppResult<()> {
        let fixture = fixture("import-existing")?;
        let snapshot = export_snapshot(&fixture.root, "s1")?;
        let _probe = crate::cursor_mutate::CursorRunningProbe::not_running();
        assert!(!import_snapshot(&fixture.root, &snapshot, false)?);
        assert!(import_snapshot(&fixture.root, &snapshot, true)?);
        Ok(())
    }

    /// 覆盖导入要把旧气泡清干净，不能留下上一版的残留。
    #[test]
    fn overwriting_replaces_the_previous_bubbles() -> AppResult<()> {
        let fixture = fixture("import-overwrite")?;
        let mut snapshot = export_snapshot(&fixture.root, "s1")?;
        snapshot.bubbles = vec![("b3".into(), json!({"type": 1, "text": "新的"}).to_string())];
        snapshot.composer_data =
            Some(json!({"fullConversationHeadersOnly": [{"bubbleId": "b3"}]}).to_string());

        let _probe = crate::cursor_mutate::CursorRunningProbe::not_running();
        assert!(import_snapshot(&fixture.root, &snapshot, true)?);

        let connection = Connection::open(cursor_sessions::state_db_path(&fixture.root))?;
        let keys: Vec<String> = connection
            .prepare("SELECT key FROM cursorDiskKV WHERE key LIKE 'bubbleId:s1:%' ORDER BY key")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        assert_eq!(keys, vec!["bubbleId:s1:b3"]);
        Ok(())
    }

    #[test]
    fn imports_are_refused_while_cursor_is_running() -> AppResult<()> {
        let fixture = fixture("import-running")?;
        let snapshot = export_snapshot(&fixture.root, "s1")?;
        let _probe = crate::cursor_mutate::CursorRunningProbe::running();
        assert!(import_snapshot(&fixture.root, &snapshot, true).is_err());
        Ok(())
    }

    /// 会话 id 会拼进 LIKE 模式，`%` `_` 不能变成通配符。
    #[test]
    fn like_wildcards_in_session_ids_cannot_widen_the_delete() {
        assert_eq!(escape_like("a%b_c"), "a\\%b\\_c");
        assert_eq!(escape_like("a\\b"), "a\\\\b");
    }

    #[test]
    fn snapshot_ids_are_rejected_when_they_could_break_out_of_a_key() {
        assert!(validate_session_id("00000000-1111-2222-3333-444444444444").is_ok());
        assert!(validate_session_id("a:b").is_err());
        assert!(validate_session_id("a%b").is_err());
        assert!(validate_session_id("../x").is_err());
        assert!(validate_session_id("  ").is_err());
    }
}
