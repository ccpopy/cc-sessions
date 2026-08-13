//! OpenCode 会话级快照、导入与项目目录迁移。
//!
//! OpenCode 把会话保存在一个共享 SQLite 数据库中。这里不能复制整库：整库还包含
//! account / token / session_share.secret 等与单会话无关或敏感的数据。快照因此按目标
//! 数据库的实际 schema 动态读取会话拥有的行，导入时再取源/目标列交集。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rusqlite::types::{Value as SqliteValue, ValueRef};
use rusqlite::{params_from_iter, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::models::{ImportMode, MoveSessionCwdReport};

const SNAPSHOT_VERSION: u32 = 1;
const SESSION_TABLE: &str = "session";
const SENSITIVE_OR_SHARED_TABLES: &[&str] = &[
    "account",
    "account_state",
    "control_account",
    "permission",
    "project",
    "project_directory",
    "session_share",
    "workspace",
    "__drizzle_migrations",
    "sqlite_sequence",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum SnapshotValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenCodeTableSnapshot {
    pub name: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<SnapshotValue>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenCodeSessionSnapshot {
    pub version: u32,
    pub exported_at: String,
    pub session_id: String,
    pub source_cwd: String,
    pub source_project_id: String,
    pub source_updated_at: i64,
    pub tables: Vec<OpenCodeTableSnapshot>,
}

#[derive(Debug, Clone)]
pub struct SnapshotImportOutcome {
    pub written: bool,
    pub skipped_reason: Option<String>,
    pub target_cwd: String,
    pub target_project_id: String,
    pub requires_project_open: bool,
}

#[derive(Debug, Clone)]
struct ColumnInfo {
    name: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key: bool,
}

#[derive(Debug, Clone)]
enum SessionFilter {
    Session,
    Aggregate,
    Message(Vec<String>),
}

pub fn export_snapshot(data_dir: &Path, session_id: &str) -> AppResult<OpenCodeSessionSnapshot> {
    validate_session_id(session_id)?;
    let db = crate::opencode_sessions::database_path(data_dir);
    let mut connection = open_readonly(&db)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let tables = session_owned_tables(&transaction, session_id)?;
    let session = tables
        .iter()
        .find(|table| table.name == SESSION_TABLE)
        .ok_or_else(|| AppError::NotFound(format!("OpenCode 会话不存在: {session_id}")))?;
    let row = session
        .rows
        .first()
        .ok_or_else(|| AppError::NotFound(format!("OpenCode 会话不存在: {session_id}")))?;
    let source_cwd = table_text(session, row, "directory").unwrap_or_default();
    let source_project_id = table_text(session, row, "project_id").unwrap_or_default();
    let source_updated_at = table_integer(session, row, "time_updated").unwrap_or_default();
    let snapshot = OpenCodeSessionSnapshot {
        version: SNAPSHOT_VERSION,
        exported_at: chrono::Utc::now().to_rfc3339(),
        session_id: session_id.to_string(),
        source_cwd,
        source_project_id,
        source_updated_at,
        tables,
    };
    validate_snapshot(&snapshot)?;
    transaction.commit()?;
    Ok(snapshot)
}

pub fn write_snapshot(path: &Path, snapshot: &OpenCodeSessionSnapshot) -> AppResult<()> {
    validate_snapshot(snapshot)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(snapshot)?)?;
    Ok(())
}

pub fn read_snapshot(path: &Path, expected_id: &str) -> AppResult<OpenCodeSessionSnapshot> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        return Err(AppError::Path(format!(
            "OpenCode 会话快照必须是普通文件: {}",
            path.to_string_lossy()
        )));
    }
    let snapshot: OpenCodeSessionSnapshot = serde_json::from_slice(&fs::read(path)?)?;
    validate_snapshot(&snapshot)?;
    if snapshot.session_id != expected_id {
        return Err(AppError::Other(format!(
            "OpenCode 快照内部会话 ID 不匹配: 期望 {expected_id}，实际 {}",
            snapshot.session_id
        )));
    }
    Ok(snapshot)
}

pub fn verify_snapshot_file(path: &Path, expected_id: &str) -> AppResult<()> {
    read_snapshot(path, expected_id).map(|_| ())
}

pub fn import_snapshot(
    data_dir: &Path,
    snapshot: &OpenCodeSessionSnapshot,
    target_cwd: Option<&str>,
    mode: &ImportMode,
) -> AppResult<SnapshotImportOutcome> {
    validate_snapshot(snapshot)?;
    let db = crate::opencode_sessions::database_path(data_dir);
    let mut connection = open_writable(&db)?;
    connection.execute_batch("PRAGMA foreign_keys = ON")?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let existing = session_state(&transaction, &snapshot.session_id)?;
    if let Some((_, local_updated_at, _)) = existing.as_ref() {
        match mode {
            ImportMode::Skip => {
                return Ok(SnapshotImportOutcome {
                    written: false,
                    skipped_reason: Some("本地已存在，Skip 模式".into()),
                    target_cwd: target_cwd.unwrap_or(&snapshot.source_cwd).to_string(),
                    target_project_id: existing
                        .as_ref()
                        .map(|value| value.2.clone())
                        .unwrap_or_default(),
                    requires_project_open: false,
                });
            }
            ImportMode::KeepLocal if *local_updated_at >= snapshot.source_updated_at => {
                return Ok(SnapshotImportOutcome {
                    written: false,
                    skipped_reason: Some(format!(
                        "本地会话更新时间不早于 Bundle（local={local_updated_at}, bundle={}）",
                        snapshot.source_updated_at
                    )),
                    target_cwd: target_cwd.unwrap_or(&snapshot.source_cwd).to_string(),
                    target_project_id: existing
                        .as_ref()
                        .map(|value| value.2.clone())
                        .unwrap_or_default(),
                    requires_project_open: false,
                });
            }
            ImportMode::KeepLocal | ImportMode::Overwrite => {}
        }
    }

    let target_cwd = normalize_target_cwd(target_cwd.unwrap_or(&snapshot.source_cwd))?;
    let target = resolve_target_project(&transaction, &target_cwd, existing.as_ref())?;
    delete_session_owned_rows(&transaction, &snapshot.session_id)?;
    insert_snapshot_rows(
        &transaction,
        snapshot,
        &target_cwd,
        &target.project_id,
        &target.worktree,
    )?;
    persist_project_directory(&transaction, &target.project_id, &target_cwd)?;
    ensure_foreign_keys(&transaction)?;
    verify_import(
        &transaction,
        &snapshot.session_id,
        &target_cwd,
        &target.project_id,
    )?;
    transaction.commit()?;

    Ok(SnapshotImportOutcome {
        written: true,
        skipped_reason: None,
        target_cwd,
        target_project_id: target.project_id,
        requires_project_open: target.requires_project_open,
    })
}

pub fn restore_snapshot(
    data_dir: &Path,
    snapshot: &OpenCodeSessionSnapshot,
    overwrite: bool,
) -> AppResult<SnapshotImportOutcome> {
    let mode = if overwrite {
        ImportMode::Overwrite
    } else {
        ImportMode::Skip
    };
    import_snapshot(data_dir, snapshot, Some(&snapshot.source_cwd), &mode)
}

pub fn move_session_cwd(
    data_dir: &Path,
    session_id: &str,
    target_cwd: &str,
) -> AppResult<MoveSessionCwdReport> {
    validate_session_id(session_id)?;
    let target_cwd = normalize_target_cwd(target_cwd)?;
    let db = crate::opencode_sessions::database_path(data_dir);
    let mut connection = open_writable(&db)?;
    connection.execute_batch("PRAGMA foreign_keys = ON")?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing = session_state(&transaction, session_id)?
        .ok_or_else(|| AppError::NotFound(format!("OpenCode 会话不存在: {session_id}")))?;
    let old_cwd = existing.0.clone();
    let target = resolve_target_project(&transaction, &target_cwd, Some(&existing))?;
    let ids = descendant_session_ids(&transaction, session_id)?;
    if ids.is_empty() {
        return Err(AppError::NotFound(format!(
            "OpenCode 会话不存在: {session_id}"
        )));
    }

    let columns = table_columns(&transaction, SESSION_TABLE)?;
    let has_path = columns.iter().any(|column| column.name == "path");
    let has_workspace = columns.iter().any(|column| column.name == "workspace_id");
    let relative_path = has_path
        .then(|| project_relative_path(&target.worktree, &target_cwd))
        .flatten();
    let mut updated = 0u32;
    for id in &ids {
        let mut sets = vec!["project_id = ?1".to_string(), "directory = ?2".to_string()];
        let mut values = vec![
            SqliteValue::Text(target.project_id.clone()),
            SqliteValue::Text(target_cwd.clone()),
        ];
        if has_path {
            sets.push(format!("path = ?{}", values.len() + 1));
            values.push(
                relative_path
                    .clone()
                    .map(SqliteValue::Text)
                    .unwrap_or(SqliteValue::Null),
            );
        }
        if has_workspace {
            sets.push(format!("workspace_id = ?{}", values.len() + 1));
            values.push(SqliteValue::Null);
        }
        values.push(SqliteValue::Text(id.clone()));
        let sql = format!(
            "UPDATE {} SET {} WHERE id = ?{}",
            quote_ident(SESSION_TABLE)?,
            sets.join(", "),
            values.len()
        );
        updated = updated
            .saturating_add(transaction.execute(&sql, params_from_iter(values.iter()))? as u32);
    }
    persist_project_directory(&transaction, &target.project_id, &target_cwd)?;
    ensure_foreign_keys(&transaction)?;
    verify_import(&transaction, session_id, &target_cwd, &target.project_id)?;
    transaction.commit()?;

    Ok(MoveSessionCwdReport {
        old_cwd,
        new_cwd: target_cwd,
        threads_updated: updated,
        rollout_rewritten: true,
        desktop_project_synced: false,
        artifacts_moved: 0,
        history_rows_updated: 0,
        target_project_id: Some(target.project_id),
        requires_project_open: target.requires_project_open,
    })
}

fn session_owned_tables(
    connection: &Connection,
    session_id: &str,
) -> AppResult<Vec<OpenCodeTableSnapshot>> {
    let names = table_names(connection)?;
    let mut filters = Vec::new();
    let mut message_ids = Vec::new();
    if names.iter().any(|name| name == "message") {
        let message_columns = table_columns(connection, "message")?;
        if message_columns
            .iter()
            .any(|column| column.name == "session_id")
        {
            message_ids = query_text_column(connection, "message", "id", "session_id", session_id)?;
        }
    }

    for name in names {
        if SENSITIVE_OR_SHARED_TABLES.contains(&name.as_str()) {
            continue;
        }
        let columns = table_columns(connection, &name)?;
        let column_names = columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<HashSet<_>>();
        let filter = if name == SESSION_TABLE {
            column_names
                .contains("id")
                .then_some(SessionFilter::Session)
        } else if column_names.contains("session_id") {
            Some(SessionFilter::Session)
        } else if matches!(name.as_str(), "event" | "event_sequence")
            && column_names.contains("aggregate_id")
        {
            Some(SessionFilter::Aggregate)
        } else if column_names.contains("message_id") && !message_ids.is_empty() {
            Some(SessionFilter::Message(message_ids.clone()))
        } else {
            None
        };
        if let Some(filter) = filter {
            filters.push((table_priority(&name), name, columns, filter));
        }
    }
    filters.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));

    let mut out = Vec::new();
    for (_, name, columns, filter) in filters {
        let table = read_table_rows(connection, &name, &columns, filter, session_id)?;
        if name == SESSION_TABLE && table.rows.len() != 1 {
            if table.rows.is_empty() {
                return Err(AppError::NotFound(format!(
                    "OpenCode 会话不存在: {session_id}"
                )));
            }
            return Err(AppError::Other(format!(
                "OpenCode session 表出现重复 ID: {session_id}"
            )));
        }
        if name == SESSION_TABLE || !table.rows.is_empty() {
            out.push(table);
        }
    }
    Ok(out)
}

fn read_table_rows(
    connection: &Connection,
    table: &str,
    columns: &[ColumnInfo],
    filter: SessionFilter,
    session_id: &str,
) -> AppResult<OpenCodeTableSnapshot> {
    let names = columns
        .iter()
        .map(|column| quote_ident(&column.name))
        .collect::<AppResult<Vec<_>>>()?;
    let table_ident = quote_ident(table)?;
    let (sql, values) = match filter {
        SessionFilter::Session if table == SESSION_TABLE => (
            format!(
                "SELECT {} FROM {table_ident} WHERE id = ?1",
                names.join(", ")
            ),
            vec![SqliteValue::Text(session_id.to_string())],
        ),
        SessionFilter::Session => (
            format!(
                "SELECT {} FROM {table_ident} WHERE session_id = ?1",
                names.join(", ")
            ),
            vec![SqliteValue::Text(session_id.to_string())],
        ),
        SessionFilter::Aggregate => (
            format!(
                "SELECT {} FROM {table_ident} WHERE aggregate_id = ?1",
                names.join(", ")
            ),
            vec![SqliteValue::Text(session_id.to_string())],
        ),
        SessionFilter::Message(ids) => {
            let placeholders = (1..=ids.len())
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            (
                format!(
                    "SELECT {} FROM {table_ident} WHERE message_id IN ({placeholders})",
                    names.join(", ")
                ),
                ids.into_iter().map(SqliteValue::Text).collect(),
            )
        }
    };
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(values.iter()))?;
    let mut output = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            values.push(snapshot_value(row.get_ref(index)?));
        }
        output.push(values);
    }
    Ok(OpenCodeTableSnapshot {
        name: table.to_string(),
        columns: columns.iter().map(|column| column.name.clone()).collect(),
        rows: output,
    })
}

fn insert_snapshot_rows(
    transaction: &rusqlite::Transaction<'_>,
    snapshot: &OpenCodeSessionSnapshot,
    target_cwd: &str,
    target_project_id: &str,
    target_worktree: &str,
) -> AppResult<()> {
    let mut tables = snapshot.tables.iter().collect::<Vec<_>>();
    tables.sort_by_key(|table| table_priority(&table.name));
    for table in tables {
        if !table_exists(transaction, &table.name)? {
            continue;
        }
        if SENSITIVE_OR_SHARED_TABLES.contains(&table.name.as_str()) {
            return Err(AppError::Other(format!(
                "OpenCode 快照包含不允许导入的表: {}",
                table.name
            )));
        }
        let target_columns = table_columns(transaction, &table.name)?;
        let target_names = target_columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<HashSet<_>>();
        let selected = table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, name)| target_names.contains(name.as_str()))
            .collect::<Vec<_>>();
        if selected.is_empty() {
            continue;
        }
        let selected_names = selected
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<HashSet<_>>();
        let missing_required = target_columns
            .iter()
            .filter(|column| {
                column.not_null
                    && column.default_value.is_none()
                    && !column.primary_key
                    && !selected_names.contains(column.name.as_str())
            })
            .map(|column| column.name.clone())
            .collect::<Vec<_>>();
        if !missing_required.is_empty() {
            if table.name == SESSION_TABLE || matches!(table.name.as_str(), "message" | "part") {
                return Err(AppError::Other(format!(
                    "OpenCode 目标表 {} 有快照无法提供的必填列: {}",
                    table.name,
                    missing_required.join(", ")
                )));
            }
            continue;
        }

        for row in &table.rows {
            let mut values = selected
                .iter()
                .map(|(index, _)| sqlite_value(&row[*index]))
                .collect::<AppResult<Vec<_>>>()?;
            if table.name == SESSION_TABLE {
                for (position, (_, name)) in selected.iter().enumerate() {
                    values[position] = match name.as_str() {
                        "project_id" => SqliteValue::Text(target_project_id.to_string()),
                        "directory" => SqliteValue::Text(target_cwd.to_string()),
                        "path" => project_relative_path(target_worktree, target_cwd)
                            .map(SqliteValue::Text)
                            .unwrap_or(SqliteValue::Null),
                        "workspace_id" => SqliteValue::Null,
                        _ => values[position].clone(),
                    };
                }
            }
            insert_row(transaction, &table.name, &selected, &values)?;
        }
    }
    Ok(())
}

fn insert_row(
    transaction: &rusqlite::Transaction<'_>,
    table: &str,
    selected: &[(usize, &String)],
    values: &[SqliteValue],
) -> AppResult<()> {
    let columns = selected
        .iter()
        .map(|(_, name)| quote_ident(name))
        .collect::<AppResult<Vec<_>>>()?;
    let placeholders = (1..=values.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let conflict_column = if selected.iter().any(|(_, name)| name.as_str() == "id") {
        Some("id")
    } else if table == "event_sequence"
        && selected
            .iter()
            .any(|(_, name)| name.as_str() == "aggregate_id")
    {
        Some("aggregate_id")
    } else {
        None
    };
    let conflict = if let Some(column) = conflict_column {
        let updates = selected
            .iter()
            .filter(|(_, name)| name.as_str() != column)
            .map(|(_, name)| {
                quote_ident(name).map(|quoted| format!("{quoted} = excluded.{quoted}"))
            })
            .collect::<AppResult<Vec<_>>>()?;
        if updates.is_empty() {
            format!(" ON CONFLICT({}) DO NOTHING", quote_ident(column)?)
        } else {
            format!(
                " ON CONFLICT({}) DO UPDATE SET {}",
                quote_ident(column)?,
                updates.join(", ")
            )
        }
    } else {
        String::new()
    };
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({}){}",
        quote_ident(table)?,
        columns.join(", "),
        placeholders,
        conflict
    );
    transaction.execute(&sql, params_from_iter(values.iter()))?;
    Ok(())
}

fn delete_session_owned_rows(
    transaction: &rusqlite::Transaction<'_>,
    session_id: &str,
) -> AppResult<()> {
    let mut tables = table_names(transaction)?
        .into_iter()
        .filter(|name| name != SESSION_TABLE)
        .filter(|name| !SENSITIVE_OR_SHARED_TABLES.contains(&name.as_str()))
        .collect::<Vec<_>>();
    tables.sort_by_key(|name| std::cmp::Reverse(table_priority(name)));
    for table in tables {
        let columns = table_columns(transaction, &table)?;
        let names = columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<HashSet<_>>();
        let predicate = if names.contains("session_id") {
            Some("session_id")
        } else if matches!(table.as_str(), "event" | "event_sequence")
            && names.contains("aggregate_id")
        {
            Some("aggregate_id")
        } else {
            None
        };
        if let Some(column) = predicate {
            transaction.execute(
                &format!(
                    "DELETE FROM {} WHERE {} = ?1",
                    quote_ident(&table)?,
                    quote_ident(column)?
                ),
                [session_id],
            )?;
        }
    }
    Ok(())
}

#[derive(Debug)]
struct TargetProject {
    project_id: String,
    worktree: String,
    requires_project_open: bool,
}

fn resolve_target_project(
    connection: &Connection,
    target_cwd: &str,
    existing: Option<&(String, i64, String)>,
) -> AppResult<TargetProject> {
    if !table_exists(connection, "project")? {
        return Ok(TargetProject {
            project_id: "global".into(),
            worktree: String::new(),
            requires_project_open: is_git_directory(Path::new(target_cwd)),
        });
    }

    if let Some(project_id) = project_id_for_directory(connection, target_cwd)? {
        let worktree =
            project_worktree(connection, &project_id)?.unwrap_or_else(|| target_cwd.into());
        return Ok(TargetProject {
            project_id,
            worktree,
            requires_project_open: false,
        });
    }
    if existing.is_some_and(|(cwd, _, project)| cwd == target_cwd && !project.is_empty()) {
        let project_id = existing.map(|value| value.2.clone()).unwrap_or_default();
        if project_exists(connection, &project_id)? {
            let worktree =
                project_worktree(connection, &project_id)?.unwrap_or_else(|| target_cwd.into());
            return Ok(TargetProject {
                project_id,
                worktree,
                requires_project_open: false,
            });
        }
    }
    if !project_exists(connection, "global")? {
        return Err(AppError::Other(
            "OpenCode 数据库缺少 global 项目，请先正常启动一次 OpenCode 后重试".into(),
        ));
    }
    Ok(TargetProject {
        project_id: "global".into(),
        worktree: project_worktree(connection, "global")?.unwrap_or_else(|| "/".into()),
        // OpenCode 官方 Project.fromDirectory 会在首次打开目标 Git 项目时，把
        // project_id=global 且 directory 匹配的会话迁到计算出的真实项目 ID。
        requires_project_open: is_git_directory(Path::new(target_cwd)),
    })
}

fn project_id_for_directory(
    connection: &Connection,
    target_cwd: &str,
) -> AppResult<Option<String>> {
    if table_exists(connection, "project_directory")? {
        let columns = table_columns(connection, "project_directory")?;
        let names = columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<HashSet<_>>();
        if names.contains("project_id") && names.contains("directory") {
            let found = connection
                .query_row(
                    "SELECT project_id FROM project_directory WHERE directory = ?1 LIMIT 1",
                    [target_cwd],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if found.is_some() {
                return Ok(found);
            }
        }
    }
    let columns = table_columns(connection, "project")?;
    if columns.iter().any(|column| column.name == "worktree") {
        if let Some(found) = connection
            .query_row(
                "SELECT id FROM project WHERE worktree = ?1 LIMIT 1",
                [target_cwd],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(Some(found));
        }
    }
    if columns.iter().any(|column| column.name == "sandboxes") {
        let mut statement = connection.prepare("SELECT id, sandboxes FROM project")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (id, sandboxes) = row?;
            let matches = sandboxes
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
                .is_some_and(|items| items.iter().any(|item| item == target_cwd));
            if matches {
                return Ok(Some(id));
            }
        }
    }
    if table_exists(connection, "workspace")? {
        let columns = table_columns(connection, "workspace")?;
        let names = columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<HashSet<_>>();
        if names.contains("directory") && names.contains("project_id") {
            return connection
                .query_row(
                    "SELECT project_id FROM workspace WHERE directory = ?1 LIMIT 1",
                    [target_cwd],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(Into::into);
        }
    }
    Ok(None)
}

fn persist_project_directory(
    connection: &Connection,
    project_id: &str,
    target_cwd: &str,
) -> AppResult<()> {
    if project_id == "global" || !table_exists(connection, "project_directory")? {
        return Ok(());
    }
    let columns = table_columns(connection, "project_directory")?;
    let names = columns
        .iter()
        .map(|column| column.name.as_str())
        .collect::<HashSet<_>>();
    if !names.contains("project_id") || !names.contains("directory") {
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp_millis();
    let mut insert_columns = vec!["project_id", "directory"];
    let mut values = vec![
        SqliteValue::Text(project_id.to_string()),
        SqliteValue::Text(target_cwd.to_string()),
    ];
    for candidate in ["time_created", "time_updated"] {
        if names.contains(candidate) {
            insert_columns.push(candidate);
            values.push(SqliteValue::Integer(now));
        }
    }
    let quoted = insert_columns
        .iter()
        .map(|name| quote_ident(name))
        .collect::<AppResult<Vec<_>>>()?;
    let placeholders = (1..=values.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT OR IGNORE INTO {} ({}) VALUES ({})",
        quote_ident("project_directory")?,
        quoted.join(", "),
        placeholders
    );
    connection.execute(&sql, params_from_iter(values.iter()))?;
    Ok(())
}

fn session_state(
    connection: &Connection,
    session_id: &str,
) -> AppResult<Option<(String, i64, String)>> {
    if !table_exists(connection, SESSION_TABLE)? {
        return Err(AppError::Other("OpenCode 数据库缺少 session 表".into()));
    }
    let columns = table_columns(connection, SESSION_TABLE)?;
    let names = columns
        .iter()
        .map(|column| column.name.as_str())
        .collect::<HashSet<_>>();
    for required in ["id", "directory", "project_id"] {
        if !names.contains(required) {
            return Err(AppError::Other(format!(
                "OpenCode session 表缺少必要列: {required}"
            )));
        }
    }
    let updated = if names.contains("time_updated") {
        "COALESCE(time_updated, 0)"
    } else {
        "0"
    };
    connection
        .query_row(
            &format!("SELECT directory, {updated}, project_id FROM session WHERE id = ?1"),
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(Into::into)
}

fn descendant_session_ids(connection: &Connection, session_id: &str) -> AppResult<Vec<String>> {
    let columns = table_columns(connection, SESSION_TABLE)?;
    if !columns.iter().any(|column| column.name == "parent_id") {
        return Ok(vec![session_id.to_string()]);
    }
    let mut statement = connection.prepare(
        "WITH RECURSIVE descendants(id) AS (
            SELECT id FROM session WHERE id = ?1
            UNION ALL
            SELECT child.id FROM session child JOIN descendants parent ON child.parent_id = parent.id
         ) SELECT id FROM descendants",
    )?;
    let rows = statement.query_map([session_id], |row| row.get::<_, String>(0))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn verify_import(
    connection: &Connection,
    session_id: &str,
    cwd: &str,
    project_id: &str,
) -> AppResult<()> {
    let found = connection
        .query_row(
            "SELECT directory, project_id FROM session WHERE id = ?1",
            [session_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    match found {
        Some((actual_cwd, actual_project)) if actual_cwd == cwd && actual_project == project_id => {
            Ok(())
        }
        Some((actual_cwd, actual_project)) => Err(AppError::Other(format!(
            "OpenCode 会话写入后校验失败: cwd={actual_cwd} project_id={actual_project}"
        ))),
        None => Err(AppError::Other(format!(
            "OpenCode 会话写入后无法按 ID 读取: {session_id}"
        ))),
    }
}

fn validate_snapshot(snapshot: &OpenCodeSessionSnapshot) -> AppResult<()> {
    if snapshot.version != SNAPSHOT_VERSION {
        return Err(AppError::Other(format!(
            "不支持的 OpenCode 快照版本: {}",
            snapshot.version
        )));
    }
    validate_session_id(&snapshot.session_id)?;
    let mut seen = HashSet::new();
    let mut session_rows = 0usize;
    for table in &snapshot.tables {
        quote_ident(&table.name)?;
        if SENSITIVE_OR_SHARED_TABLES.contains(&table.name.as_str()) {
            return Err(AppError::Other(format!(
                "OpenCode 快照包含敏感或共享表: {}",
                table.name
            )));
        }
        if !seen.insert(table.name.as_str()) {
            return Err(AppError::Other(format!(
                "OpenCode 快照包含重复表: {}",
                table.name
            )));
        }
        let mut columns = HashSet::new();
        for column in &table.columns {
            quote_ident(column)?;
            if !columns.insert(column.as_str()) {
                return Err(AppError::Other(format!(
                    "OpenCode 快照表 {} 包含重复列: {column}",
                    table.name
                )));
            }
        }
        if table
            .rows
            .iter()
            .any(|row| row.len() != table.columns.len())
        {
            return Err(AppError::Other(format!(
                "OpenCode 快照表 {} 的行列数量不一致",
                table.name
            )));
        }
        if table.name == SESSION_TABLE {
            session_rows = table.rows.len();
            let id_index = table
                .columns
                .iter()
                .position(|column| column == "id")
                .ok_or_else(|| AppError::Other("OpenCode 快照 session 表缺少 id".into()))?;
            if table.rows.first().and_then(|row| match &row[id_index] {
                SnapshotValue::Text(value) => Some(value.as_str()),
                _ => None,
            }) != Some(snapshot.session_id.as_str())
            {
                return Err(AppError::Other(
                    "OpenCode 快照 session 行的 id 与顶层 session_id 不一致".into(),
                ));
            }
        }
    }
    if session_rows != 1 {
        return Err(AppError::Other(format!(
            "OpenCode 快照必须且只能包含一行 session，实际 {session_rows} 行"
        )));
    }
    Ok(())
}

fn normalize_target_cwd(raw: &str) -> AppResult<String> {
    let raw = crate::paths::strip_verbatim(raw.trim());
    if raw.is_empty() || raw.chars().any(char::is_control) {
        return Err(AppError::Path("OpenCode 目标工作目录无效".into()));
    }
    let path = PathBuf::from(&raw);
    if !path.is_absolute() {
        return Err(AppError::Path(format!("工作目录必须是绝对路径: {raw}")));
    }
    let canonical = path.canonicalize().map_err(|error| {
        AppError::NotFound(format!("工作目录不存在或无法访问: {raw} ({error})"))
    })?;
    if !canonical.is_dir() {
        return Err(AppError::Path(format!(
            "目标工作目录不是文件夹: {}",
            canonical.to_string_lossy()
        )));
    }
    Ok(crate::paths::strip_verbatim(&canonical.to_string_lossy()))
}

fn project_relative_path(worktree: &str, directory: &str) -> Option<String> {
    if worktree.trim().is_empty() || worktree == "/" {
        return None;
    }
    let root = PathBuf::from(crate::paths::strip_verbatim(worktree));
    let directory = PathBuf::from(crate::paths::strip_verbatim(directory));
    directory
        .strip_prefix(root)
        .ok()
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
}

fn ensure_foreign_keys(connection: &Connection) -> AppResult<()> {
    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    let mut rows = statement.query([])?;
    if let Some(row) = rows.next()? {
        let table: String = row.get(0)?;
        let rowid: Option<i64> = row.get(1)?;
        return Err(AppError::Other(format!(
            "OpenCode 写入后的外键校验失败: table={table} rowid={rowid:?}"
        )));
    }
    Ok(())
}

fn table_names(connection: &Connection) -> AppResult<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn table_columns(connection: &Connection, table: &str) -> AppResult<Vec<ColumnInfo>> {
    let mut statement =
        connection.prepare(&format!("PRAGMA table_info({})", quote_ident(table)?))?;
    let rows = statement.query_map([], |row| {
        Ok(ColumnInfo {
            name: row.get(1)?,
            not_null: row.get::<_, i64>(3)? != 0,
            default_value: row.get(4)?,
            primary_key: row.get::<_, i64>(5)? != 0,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn table_exists(connection: &Connection, table: &str) -> AppResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn project_exists(connection: &Connection, project_id: &str) -> AppResult<bool> {
    if !table_exists(connection, "project")? {
        return Ok(project_id == "global");
    }
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM project WHERE id=?1)",
        [project_id],
        |row| row.get(0),
    )?)
}

fn project_worktree(connection: &Connection, project_id: &str) -> AppResult<Option<String>> {
    if !table_exists(connection, "project")?
        || !table_columns(connection, "project")?
            .iter()
            .any(|column| column.name == "worktree")
    {
        return Ok(None);
    }
    connection
        .query_row(
            "SELECT worktree FROM project WHERE id=?1",
            [project_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

fn query_text_column(
    connection: &Connection,
    table: &str,
    selected: &str,
    filtered: &str,
    value: &str,
) -> AppResult<Vec<String>> {
    let sql = format!(
        "SELECT {} FROM {} WHERE {} = ?1",
        quote_ident(selected)?,
        quote_ident(table)?,
        quote_ident(filtered)?
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([value], |row| row.get::<_, String>(0))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn snapshot_value(value: ValueRef<'_>) -> SnapshotValue {
    match value {
        ValueRef::Null => SnapshotValue::Null,
        ValueRef::Integer(value) => SnapshotValue::Integer(value),
        ValueRef::Real(value) => SnapshotValue::Real(value),
        ValueRef::Text(value) => SnapshotValue::Text(String::from_utf8_lossy(value).into_owned()),
        ValueRef::Blob(value) => SnapshotValue::Blob(STANDARD.encode(value)),
    }
}

fn sqlite_value(value: &SnapshotValue) -> AppResult<SqliteValue> {
    Ok(match value {
        SnapshotValue::Null => SqliteValue::Null,
        SnapshotValue::Integer(value) => SqliteValue::Integer(*value),
        SnapshotValue::Real(value) => SqliteValue::Real(*value),
        SnapshotValue::Text(value) => SqliteValue::Text(value.clone()),
        SnapshotValue::Blob(value) => {
            SqliteValue::Blob(STANDARD.decode(value).map_err(|error| {
                AppError::Other(format!("OpenCode 快照包含无效 base64 blob: {error}"))
            })?)
        }
    })
}

fn table_text(
    table: &OpenCodeTableSnapshot,
    row: &[SnapshotValue],
    column: &str,
) -> Option<String> {
    let index = table.columns.iter().position(|name| name == column)?;
    match row.get(index)? {
        SnapshotValue::Text(value) => Some(value.clone()),
        _ => None,
    }
}

fn table_integer(
    table: &OpenCodeTableSnapshot,
    row: &[SnapshotValue],
    column: &str,
) -> Option<i64> {
    let index = table.columns.iter().position(|name| name == column)?;
    match row.get(index)? {
        SnapshotValue::Integer(value) => Some(*value),
        _ => None,
    }
}

fn quote_ident(identifier: &str) -> AppResult<String> {
    if identifier.is_empty()
        || !identifier
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Err(AppError::Path(format!(
            "OpenCode SQLite 标识符无效: {identifier}"
        )));
    }
    Ok(format!("\"{identifier}\""))
}

fn validate_session_id(session_id: &str) -> AppResult<()> {
    if session_id.trim().is_empty()
        || session_id.chars().any(char::is_control)
        || session_id.chars().count() > 256
    {
        return Err(AppError::Other("OpenCode 会话 ID 无效".into()));
    }
    Ok(())
}

fn table_priority(name: &str) -> u8 {
    match name {
        "session" => 0,
        "message" | "session_message" | "session_input" | "session_entry" => 1,
        "part" | "todo" | "event_sequence" => 2,
        "event" => 3,
        _ => 2,
    }
}

fn is_git_directory(path: &Path) -> bool {
    if path.join(".git").exists() {
        return true;
    }
    std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(path)
        .output()
        .ok()
        .is_some_and(|output| output.status.success() && output.stdout.starts_with(b"true"))
}

fn open_readonly(path: &Path) -> AppResult<Connection> {
    if !path.is_file() {
        return Err(AppError::NotFound(format!(
            "OpenCode 数据库不存在: {}",
            path.to_string_lossy()
        )));
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    Ok(connection)
}

fn open_writable(path: &Path) -> AppResult<Connection> {
    if !path.is_file() {
        return Err(AppError::NotFound(format!(
            "OpenCode 数据库不存在: {}",
            path.to_string_lossy()
        )));
    }
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "cc-sessions-opencode-transfer-{label}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("temp dir");
        root
    }

    fn create_db(root: &Path, modern: bool) -> AppResult<()> {
        let connection = Connection::open(crate::opencode_sessions::database_path(root))?;
        let path_column = if modern { ", path TEXT" } else { "" };
        let workspace_column = if modern { ", workspace_id TEXT" } else { "" };
        connection.execute_batch(&format!(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL, vcs TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, sandboxes TEXT NOT NULL);
             INSERT INTO project VALUES ('global','/',NULL,1,1,'[]');
             CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES project(id), parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL{path_column}{workspace_column});
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE, session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE todo (session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE, position INTEGER NOT NULL, content TEXT NOT NULL, PRIMARY KEY(session_id, position));
             CREATE TABLE session_share (session_id TEXT PRIMARY KEY REFERENCES session(id) ON DELETE CASCADE, id TEXT NOT NULL, secret TEXT NOT NULL, url TEXT NOT NULL);
             CREATE TABLE account (id TEXT PRIMARY KEY, access_token TEXT NOT NULL);
             CREATE TABLE event_sequence (aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL);
             CREATE TABLE event (id TEXT PRIMARY KEY, aggregate_id TEXT NOT NULL REFERENCES event_sequence(aggregate_id) ON DELETE CASCADE, seq INTEGER NOT NULL, type TEXT NOT NULL, data TEXT NOT NULL);"
        ))?;
        Ok(())
    }

    fn seed(root: &Path, cwd: &Path) -> AppResult<()> {
        let connection = Connection::open(crate::opencode_sessions::database_path(root))?;
        connection.execute(
            "INSERT INTO session (id,project_id,parent_id,slug,directory,title,version,time_created,time_updated) VALUES ('ses_test','global',NULL,'slug',?1,'title','1.0',1000,4000)",
            [cwd.to_string_lossy().as_ref()],
        )?;
        connection.execute(
            "INSERT INTO message VALUES ('msg_1','ses_test',1000,1000,?1)",
            [serde_json::json!({"role":"user"}).to_string()],
        )?;
        connection.execute(
            "INSERT INTO part VALUES ('part_1','msg_1','ses_test',1000,1000,?1)",
            [serde_json::json!({"type":"text","text":"hello"}).to_string()],
        )?;
        connection.execute("INSERT INTO todo VALUES ('ses_test',0,'task')", [])?;
        connection.execute(
            "INSERT INTO session_share VALUES ('ses_test','share','do-not-export','https://example.invalid')",
            [],
        )?;
        connection.execute("INSERT INTO account VALUES ('acct','token')", [])?;
        connection.execute("INSERT INTO event_sequence VALUES ('ses_test',1)", [])?;
        connection.execute(
            "INSERT INTO event VALUES ('evt','ses_test',1,'test','{}')",
            [],
        )?;
        Ok(())
    }

    #[test]
    fn snapshot_excludes_accounts_and_share_secrets_and_imports_across_schema_versions(
    ) -> AppResult<()> {
        let source = temp_dir("source");
        let target = temp_dir("target");
        let source_cwd = temp_dir("source-cwd");
        let target_cwd = temp_dir("target-cwd");
        create_db(&source, false)?;
        create_db(&target, true)?;
        seed(&source, &source_cwd)?;

        let snapshot = export_snapshot(&source, "ses_test")?;
        assert!(snapshot.tables.iter().any(|table| table.name == "session"));
        assert!(snapshot.tables.iter().any(|table| table.name == "event"));
        assert!(!snapshot.tables.iter().any(|table| table.name == "account"));
        assert!(!snapshot
            .tables
            .iter()
            .any(|table| table.name == "session_share"));
        assert!(!serde_json::to_string(&snapshot)?.contains("do-not-export"));

        let outcome = import_snapshot(
            &target,
            &snapshot,
            Some(target_cwd.to_string_lossy().as_ref()),
            &ImportMode::Overwrite,
        )?;
        assert!(outcome.written);
        let connection = Connection::open(crate::opencode_sessions::database_path(&target))?;
        let (cwd, project, path, workspace): (String, String, Option<String>, Option<String>) =
            connection.query_row(
                "SELECT directory,project_id,path,workspace_id FROM session WHERE id='ses_test'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        assert_eq!(cwd, outcome.target_cwd);
        assert_eq!(project, "global");
        assert_eq!(path, None);
        assert_eq!(workspace, None);
        assert_eq!(
            connection.query_row("SELECT COUNT(*) FROM message", [], |row| row
                .get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            connection.query_row("SELECT COUNT(*) FROM session_share", [], |row| row
                .get::<_, i64>(0))?,
            0
        );

        for path in [source, target, source_cwd, target_cwd] {
            fs::remove_dir_all(path).ok();
        }
        Ok(())
    }

    #[test]
    fn overwrite_preserves_local_share_secret() -> AppResult<()> {
        let source = temp_dir("share-source");
        let target = temp_dir("share-target");
        let cwd = temp_dir("share-cwd");
        create_db(&source, false)?;
        create_db(&target, false)?;
        seed(&source, &cwd)?;
        seed(&target, &cwd)?;
        let target_connection = Connection::open(crate::opencode_sessions::database_path(&target))?;
        target_connection.execute(
            "UPDATE session_share SET secret='keep-local-secret' WHERE session_id='ses_test'",
            [],
        )?;
        drop(target_connection);

        let snapshot = export_snapshot(&source, "ses_test")?;
        import_snapshot(&target, &snapshot, None, &ImportMode::Overwrite)?;
        let connection = Connection::open(crate::opencode_sessions::database_path(&target))?;
        let secret: String = connection.query_row(
            "SELECT secret FROM session_share WHERE session_id='ses_test'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(secret, "keep-local-secret");

        for path in [source, target, cwd] {
            fs::remove_dir_all(path).ok();
        }
        Ok(())
    }

    #[test]
    fn move_updates_root_and_descendants_and_reuses_registered_project() -> AppResult<()> {
        let root = temp_dir("move");
        let old_cwd = temp_dir("move-old");
        let target_cwd = temp_dir("move-target");
        create_db(&root, true)?;
        seed(&root, &old_cwd)?;
        let normalized_target_cwd = normalize_target_cwd(target_cwd.to_string_lossy().as_ref())?;
        let connection = Connection::open(crate::opencode_sessions::database_path(&root))?;
        connection.execute(
            "INSERT INTO project VALUES ('project-target',?1,'git',1,1,'[]')",
            [&normalized_target_cwd],
        )?;
        connection.execute(
            "INSERT INTO session (id,project_id,parent_id,slug,directory,title,version,time_created,time_updated) VALUES ('ses_child','global','ses_test','child',?1,'child','1.0',1000,2000)",
            [old_cwd.to_string_lossy().as_ref()],
        )?;
        drop(connection);

        let report = move_session_cwd(&root, "ses_test", target_cwd.to_string_lossy().as_ref())?;
        assert_eq!(report.threads_updated, 2);
        assert_eq!(report.target_project_id.as_deref(), Some("project-target"));
        assert!(!report.requires_project_open);
        let connection = Connection::open(crate::opencode_sessions::database_path(&root))?;
        let rows: i64 = connection.query_row(
            "SELECT COUNT(*) FROM session WHERE project_id='project-target' AND directory=?1",
            [&normalized_target_cwd],
            |row| row.get(0),
        )?;
        assert_eq!(rows, 2);

        for path in [root, old_cwd, target_cwd] {
            fs::remove_dir_all(path).ok();
        }
        Ok(())
    }
}
