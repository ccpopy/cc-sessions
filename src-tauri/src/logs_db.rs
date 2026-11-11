use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::error::AppResult;
use crate::state_db::ReadOnlyConnection;

pub fn open(codex_dir: &Path) -> AppResult<Connection> {
    let conn = Connection::open_with_flags(
        crate::paths::logs_db_path(codex_dir)?,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    Ok(conn)
}

pub fn open_ro(codex_dir: &Path) -> AppResult<ReadOnlyConnection> {
    crate::state_db::open_ro_db(codex_dir, "logs_2.sqlite")
}

#[allow(dead_code)]
pub fn count_for_thread(conn: &Connection, thread_id: &str) -> AppResult<i64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM logs WHERE thread_id = ?",
        [thread_id],
        |r| r.get(0),
    )?;
    Ok(n)
}
