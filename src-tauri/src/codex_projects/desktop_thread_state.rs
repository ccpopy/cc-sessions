//! Current Codex Desktop SQLite state that is keyed directly by a Core thread id.
//!
//! These stores are not part of Codex Core's `state_5.sqlite`. Desktop keeps its own persisted
//! catalog and generated turn summaries under `.codex/sqlite`; leaving either row behind after
//! deleting the Core rollout creates a stale entry that can survive a Desktop restart.

use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use crate::error::{AppError, AppResult};

const DESKTOP_SQLITE_DIR: &str = "sqlite";
const THREAD_CATALOG_DB: &str = "codex-dev.db";
const THREAD_CATALOG_TABLE: &str = "local_thread_catalog";
const THREAD_SUMMARIES_DB: &str = "codex-thread-summaries-dev.db";
const THREAD_SUMMARIES_TABLE: &str = "thread_turn_summaries";

pub(super) fn clear_deleted_thread_rows(codex: &Path, thread_ids: &[String]) -> AppResult<()> {
    if thread_ids.is_empty() {
        return Ok(());
    }
    clear_rows_in_store(codex, THREAD_CATALOG_DB, THREAD_CATALOG_TABLE, thread_ids)?;
    clear_rows_in_store(
        codex,
        THREAD_SUMMARIES_DB,
        THREAD_SUMMARIES_TABLE,
        thread_ids,
    )
}

fn clear_rows_in_store(
    codex: &Path,
    database_name: &str,
    table_name: &str,
    thread_ids: &[String],
) -> AppResult<()> {
    let database_path = codex.join(DESKTOP_SQLITE_DIR).join(database_name);
    let metadata = match fs::symlink_metadata(&database_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        return Err(AppError::Path(format!(
            "Codex Desktop 数据库不是普通文件: {}",
            database_path.to_string_lossy()
        )));
    }

    let mut connection = Connection::open_with_flags(
        &database_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    if !table_exists(&connection, table_name)? {
        return Ok(());
    }
    if !column_exists(&connection, table_name, "thread_id")? {
        return Err(AppError::Other(format!(
            "Codex Desktop 数据库表 {table_name} 缺少 thread_id 字段: {}",
            database_path.to_string_lossy()
        )));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let sql = format!("DELETE FROM {table_name} WHERE thread_id = ?1");
    for thread_id in thread_ids {
        transaction.execute(&sql, [thread_id])?;
    }
    transaction.commit()?;
    Ok(())
}

fn table_exists(connection: &Connection, table_name: &str) -> AppResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1
        )",
        [table_name],
        |row| row.get(0),
    )?)
}

fn column_exists(connection: &Connection, table_name: &str, column_name: &str) -> AppResult<bool> {
    // Identifiers come only from the private constants above; values remain bound parameters.
    let pragma = format!("PRAGMA table_info({table_name})");
    let mut statement = connection.prepare(&pragma)?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let current: String = row.get(1)?;
        if current == column_name {
            return Ok(true);
        }
    }
    Ok(false)
}
