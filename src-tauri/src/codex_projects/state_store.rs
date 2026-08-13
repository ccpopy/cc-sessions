//! Compare-and-swap storage for Codex Desktop's global JSON state.
//!
//! This module owns byte snapshots, atomic publication, retries, and conditional compensation.
//! The parent module owns the meaning of project and thread-assignment fields.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::atomic_file::{self, FileFingerprint};
use crate::error::{AppError, AppResult};
use crate::paths;

const STATE_WRITE_ATTEMPTS: usize = 3;

#[cfg(test)]
thread_local! {
    static TEST_STATE_WRITE_CONFLICTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TEST_STATE_POST_COMMIT_ERRORS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Force every global-state compare-and-swap attempt in one test to lose a concurrent write.
#[cfg(test)]
pub(crate) struct StateWriteConflictTestGuard(usize);

#[cfg(test)]
impl StateWriteConflictTestGuard {
    pub(crate) fn all_attempts() -> Self {
        let previous = TEST_STATE_WRITE_CONFLICTS.replace(STATE_WRITE_ATTEMPTS);
        Self(previous)
    }
}

#[cfg(test)]
impl Drop for StateWriteConflictTestGuard {
    fn drop(&mut self) {
        TEST_STATE_WRITE_CONFLICTS.set(self.0);
    }
}

#[cfg(test)]
pub(super) struct StatePostCommitErrorTestGuard(usize);

#[cfg(test)]
impl StatePostCommitErrorTestGuard {
    pub(super) fn once() -> Self {
        let previous = TEST_STATE_POST_COMMIT_ERRORS.replace(1);
        Self(previous)
    }
}

#[cfg(test)]
impl Drop for StatePostCommitErrorTestGuard {
    fn drop(&mut self) {
        TEST_STATE_POST_COMMIT_ERRORS.set(self.0);
    }
}

pub(super) struct StateSnapshot {
    pub(super) path: PathBuf,
    fingerprint: FileFingerprint,
    raw: Vec<u8>,
    pub(super) root: Value,
}

/// Exact receipt for one successful global-state compare-and-swap mutation.
///
/// Compensation is itself compare-and-swap: it restores `before` only while the file still has
/// the exact bytes written by this mutation. A later Desktop write is never overwritten.
#[derive(Debug, Clone)]
pub(crate) struct StateMutationReceipt {
    path: PathBuf,
    before: Vec<u8>,
    before_fingerprint: FileFingerprint,
    after_fingerprint: FileFingerprint,
}

impl StateMutationReceipt {
    pub(crate) fn compensate(&self) -> AppResult<()> {
        ensure_state_path_desktop_not_running(&self.path)?;
        let metadata = fs::symlink_metadata(&self.path)?;
        if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "Codex 全局状态补偿目标不是普通文件: {}",
                self.path.to_string_lossy()
            )));
        }
        let current = atomic_file::fingerprint(&self.path)?;
        if current == self.before_fingerprint {
            return Ok(());
        }
        if current != self.after_fingerprint {
            return Err(AppError::Other(format!(
                "Codex 全局状态在本次写入后又发生变化，拒绝补偿并发数据: {}",
                self.path.to_string_lossy()
            )));
        }
        atomic_file::replace_with_writer_if_unchanged(&self.path, &self.after_fingerprint, |file| {
            file.write_all(&self.before)?;
            Ok(())
        })
    }
}

pub(super) fn mutate_existing_state_with_receipt<T>(
    codex: &Path,
    mut mutation: impl FnMut(&mut Map<String, Value>) -> AppResult<T>,
) -> AppResult<Option<(T, Option<StateMutationReceipt>)>> {
    super::ensure_desktop_not_running(codex)?;
    for attempt in 0..STATE_WRITE_ATTEMPTS {
        let snapshot = match load_state(codex) {
            Ok(snapshot) => snapshot,
            Err(error)
                if attempt + 1 < STATE_WRITE_ATTEMPTS
                    && error.retryable_atomic_write_conflict() =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let Some(mut snapshot) = snapshot else {
            return Ok(None);
        };
        let before = snapshot.root.clone();
        let state = snapshot.root.as_object_mut().ok_or_else(|| {
            AppError::Other(format!(
                "Codex 全局状态必须是 JSON 对象: {}",
                snapshot.path.to_string_lossy()
            ))
        })?;
        let result = mutation(state)?;
        if snapshot.root == before {
            return Ok(Some((result, None)));
        }
        let after = serde_json::to_vec(&snapshot.root)?;
        let receipt = StateMutationReceipt {
            path: snapshot.path.clone(),
            before: snapshot.raw,
            before_fingerprint: snapshot.fingerprint.clone(),
            after_fingerprint: atomic_file::fingerprint_bytes(&after),
        };
        // Recheck immediately before every compare-and-swap attempt to narrow the process-start
        // race after the earlier business-level preflight.
        super::ensure_desktop_not_running(codex)?;
        match write_state_bytes_if_unchanged(&snapshot.path, &snapshot.fingerprint, &after) {
            Ok(()) => return Ok(Some((result, Some(receipt)))),
            Err(error)
                if attempt + 1 < STATE_WRITE_ATTEMPTS
                    && error.retryable_atomic_write_conflict() =>
            {
                continue;
            }
            Err(error) if error.atomic_write_committed() => {
                return Err(match receipt.compensate() {
                    Ok(()) => error,
                    Err(compensation_error) => AppError::Other(format!(
                        "{error}; Codex 全局状态晚失败补偿失败: {compensation_error}"
                    )),
                });
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("state mutation attempts are non-zero")
}

fn ensure_state_path_desktop_not_running(state_path: &Path) -> AppResult<()> {
    match fs::symlink_metadata(state_path) {
        Ok(metadata) => validate_state_file_metadata(state_path, &metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    super::desktop_guard::ensure_official_desktop_not_running()
}

pub(super) fn load_state(codex: &Path) -> AppResult<Option<StateSnapshot>> {
    let path = paths::codex_global_state_json_path(codex);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_state_file_metadata(&path, &metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }

    let fingerprint = atomic_file::fingerprint(&path)?;
    let raw = fs::read(&path)?;
    if atomic_file::fingerprint_bytes(&raw) != fingerprint {
        return Err(AppError::AtomicWriteConflict(format!(
            "Codex 全局状态在读取期间发生变化，已拒绝使用不一致快照: {}",
            path.to_string_lossy()
        )));
    }
    let root = serde_json::from_slice(&raw).map_err(|error| {
        AppError::Other(format!(
            "Codex 全局状态 JSON 损坏 {}: {error}",
            path.to_string_lossy()
        ))
    })?;
    Ok(Some(StateSnapshot {
        path,
        fingerprint,
        raw,
        root,
    }))
}

pub(super) fn validate_state_file_metadata(path: &Path, metadata: &fs::Metadata) -> AppResult<()> {
    if metadata.is_file() && !crate::path_safety::metadata_is_link_or_reparse(metadata) {
        Ok(())
    } else {
        Err(AppError::Path(format!(
            "Codex 全局状态路径不是普通文件或属于链接/junction: {}",
            path.to_string_lossy()
        )))
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn write_state_bytes_if_unchanged(
    path: &Path,
    expected: &FileFingerprint,
    bytes: &[u8],
) -> AppResult<()> {
    #[cfg(test)]
    inject_test_state_write_conflict(path)?;
    atomic_file::replace_with_writer_if_unchanged(path, expected, |file| {
        file.write_all(bytes)?;
        Ok(())
    })?;
    #[cfg(test)]
    inject_test_state_post_commit_error()?;
    Ok(())
}

#[cfg(test)]
fn inject_test_state_write_conflict(path: &Path) -> AppResult<()> {
    let should_conflict = TEST_STATE_WRITE_CONFLICTS.with(|remaining| {
        let current = remaining.get();
        if current == 0 {
            false
        } else {
            remaining.set(current - 1);
            true
        }
    });
    if !should_conflict {
        return Ok(());
    }
    let mut state: Value = serde_json::from_slice(&fs::read(path)?)?;
    let root = state
        .as_object_mut()
        .ok_or_else(|| AppError::Other("测试并发写入要求全局状态为 JSON 对象".to_string()))?;
    let version = root
        .get("test-concurrent-write")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        + 1;
    root.insert("test-concurrent-write".to_string(), Value::from(version));
    fs::write(path, serde_json::to_vec(&state)?)?;
    Ok(())
}

#[cfg(test)]
fn inject_test_state_post_commit_error() -> AppResult<()> {
    let should_fail = TEST_STATE_POST_COMMIT_ERRORS.with(|remaining| {
        let current = remaining.get();
        if current == 0 {
            false
        } else {
            remaining.set(current - 1);
            true
        }
    });
    if should_fail {
        Err(AppError::AtomicWriteCommitted(
            "测试注入：Codex 全局状态已写入但收尾失败".to_string(),
        ))
    } else {
        Ok(())
    }
}
