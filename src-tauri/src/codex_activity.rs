//! Observe only the threads and rollouts touched by an operation. A quiet window is not an
//! agent-idle guarantee: callers must retain their transaction/CAS checks as well.
use crate::error::{AppError, AppResult};
use crate::paths;
use rusqlite::{types::Value, Connection, OpenFlags, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, PartialEq)]
struct FileSample {
    len: u64,
    modified: Option<SystemTime>,
    tail: Vec<u8>,
}
#[derive(Debug, PartialEq)]
struct Sample {
    row: Option<Vec<Value>>,
    files: Vec<(PathBuf, Option<FileSample>)>,
}

pub(crate) struct SessionActivityGuard {
    codex: PathBuf,
    targets: Vec<(String, Vec<PathBuf>)>,
    samples: Vec<Sample>,
}
impl SessionActivityGuard {
    pub(crate) fn observe(codex: &Path, targets: Vec<(String, Vec<PathBuf>)>) -> AppResult<Self> {
        let samples = sample(codex, &targets)?;
        let guard = Self {
            codex: codex.to_path_buf(),
            targets,
            samples,
        };
        // One wait for all affected branches. New sessions have no live data to observe.
        if guard
            .samples
            .iter()
            .any(|s| s.row.is_some() || s.files.iter().any(|(_, f)| f.is_some()))
        {
            observation_window();
            guard.ensure_unchanged()?;
        }
        Ok(guard)
    }

    /// Recheck after preparation and before the first write, inside the SQLite transaction
    /// where applicable. Do not check after the operation has changed its own data.
    pub(crate) fn ensure_unchanged(&self) -> AppResult<()> {
        let current = sample(&self.codex, &self.targets)?;
        for ((id, _), (before, after)) in self.targets.iter().zip(self.samples.iter().zip(current))
        {
            if *before != after {
                return Err(AppError::SessionBusy(id.clone()));
            }
        }
        Ok(())
    }
}

fn sample(codex: &Path, targets: &[(String, Vec<PathBuf>)]) -> AppResult<Vec<Sample>> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    // Query the live DB by primary key. Never compare DB/WAL timestamps or copy the database:
    // activity in a different thread must not block this selection.
    let database = paths::state_db_path(codex);
    let conn = if database.is_file() {
        Some(Connection::open_with_flags(
            database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?)
    } else {
        None
    };
    let has_threads = match &conn {
        Some(conn) => conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='threads')",
            [],
            |row| row.get::<_, bool>(0),
        )?,
        None => false,
    };
    targets
        .iter()
        .map(|(id, explicit_paths)| {
            let mut files: BTreeSet<PathBuf> = explicit_paths
                .iter()
                .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
                .collect();
            let row = if has_threads {
                let mut statement = conn
                    .as_ref()
                    .unwrap()
                    .prepare("SELECT * FROM threads WHERE id = ?1")?;
                let path_column = statement.column_index("rollout_path").ok();
                let columns = statement.column_count();
                let row = statement
                    .query_row([id], |row| {
                        (0..columns)
                            .map(|i| row.get::<_, Value>(i))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .optional()?;
                if let Some(Value::Text(raw_path)) =
                    path_column.and_then(|i| row.as_ref().and_then(|r| r.get(i)))
                {
                    let path = paths::host_path_from_codex_record(codex, raw_path);
                    // Do not follow an old external path into another Codex home.
                    if let (Ok(root), Ok(path)) = (codex.canonicalize(), path.canonicalize()) {
                        if path.starts_with(root) {
                            files.insert(path);
                        }
                    }
                }
                row
            } else {
                None
            };
            let files = files
                .into_iter()
                .map(|path| file_sample(&path).map(|value| (path, value)))
                .collect::<AppResult<_>>()?;
            Ok(Sample { row, files })
        })
        .collect()
}

fn file_sample(path: &Path) -> AppResult<Option<FileSample>> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    // Bound the read even for multi-GB transcripts. The tail also detects buffered writes on
    // filesystems that do not immediately advance the last-write timestamp.
    file.seek(SeekFrom::Start(metadata.len().saturating_sub(8192)))?;
    let mut tail = Vec::new();
    file.take(8192).read_to_end(&mut tail)?;
    Ok(Some(FileSample {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        tail: Sha256::digest(&tail).to_vec(),
    }))
}

#[cfg(not(test))]
fn observation_window() {
    std::thread::sleep(std::time::Duration::from_millis(350));
}
#[cfg(test)]
thread_local! {
    static OBSERVATION_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
fn observation_window() {
    if let Some(hook) = OBSERVATION_HOOK.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}
#[cfg(test)]
pub(crate) fn during_observation(hook: impl FnOnce() + 'static) {
    OBSERVATION_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn detects_target_append_and_thread_repoint_but_ignores_other_sessions() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "cc-sessions-activity-{}",
            crate::repair::new_session_id()
        ));
        fs::create_dir_all(&root)?;
        let rollout = root.join("selected.jsonl");
        let continuation = root.join("continuation.jsonl");
        fs::write(&rollout, b"selected\n")?;
        fs::write(&continuation, b"continuation\n")?;
        let conn = Connection::open(paths::state_db_path(&root))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, updated_at INTEGER);")?;
        conn.execute(
            "INSERT INTO threads VALUES ('selected', ?1, 0), ('other', '', 0)",
            [rollout.to_string_lossy().as_ref()],
        )?;
        let targets = || vec![("selected".into(), vec![rollout.clone()])];
        let guard = SessionActivityGuard::observe(&root, targets())?;
        conn.execute("UPDATE threads SET updated_at=1 WHERE id='other'", [])?;
        fs::write(root.join("unrelated.jsonl"), b"unrelated write")?;
        guard.ensure_unchanged()?;
        let writing = rollout.clone();
        during_observation(move || {
            writeln!(
                fs::OpenOptions::new().append(true).open(writing).unwrap(),
                "event"
            )
            .unwrap();
        });
        assert!(matches!(
            SessionActivityGuard::observe(&root, targets()),
            Err(AppError::SessionBusy(_))
        ));
        let guard = SessionActivityGuard::observe(&root, targets())?;
        conn.execute(
            "UPDATE threads SET rollout_path=?1 WHERE id='selected'",
            [continuation.to_string_lossy().as_ref()],
        )?;
        assert!(matches!(
            guard.ensure_unchanged(),
            Err(AppError::SessionBusy(_))
        ));
        // Observe the continuation selected by the thread record, even if the caller has a stub.
        let guard = SessionActivityGuard::observe(&root, targets())?;
        fs::write(&continuation, b"new continuation bytes\n")?;
        assert!(matches!(
            guard.ensure_unchanged(),
            Err(AppError::SessionBusy(_))
        ));
        drop(conn);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
