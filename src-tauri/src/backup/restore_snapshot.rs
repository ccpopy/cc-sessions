//! File snapshots used to compensate a multi-store Codex backup restore.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::atomic_file;
use crate::error::{AppError, AppResult};
use crate::path_safety;

static RESTORE_SNAPSHOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct RestoreFileSnapshot {
    label: &'static str,
    path: PathBuf,
    snapshot_path: Option<PathBuf>,
    original_fingerprint: Option<atomic_file::FileFingerprint>,
    mutation_started: bool,
    mutation_committed: bool,
    /// `None` means the mutation may have committed but its resulting path state could not be
    /// observed. `Some(None)` is a trustworthy observation that the path was absent.
    expected_after: Option<Option<atomic_file::FileFingerprint>>,
}

pub(super) struct RestoreFileSnapshots {
    root: PathBuf,
    files: Vec<RestoreFileSnapshot>,
}

#[cfg(test)]
enum RestoreFileTestFault {
    None,
    ReplaceAndConflict {
        label: &'static str,
        path: PathBuf,
        contents: Vec<u8>,
    },
}

#[cfg(test)]
thread_local! {
    static RESTORE_FILE_TEST_FAULT: std::cell::RefCell<RestoreFileTestFault> =
        const { std::cell::RefCell::new(RestoreFileTestFault::None) };
}

#[cfg(test)]
pub(super) struct RestoreFileTestFaultGuard;

#[cfg(test)]
impl RestoreFileTestFaultGuard {
    pub(super) fn replace_and_conflict(
        label: &'static str,
        path: PathBuf,
        contents: Vec<u8>,
    ) -> Self {
        RESTORE_FILE_TEST_FAULT.replace(RestoreFileTestFault::ReplaceAndConflict {
            label,
            path,
            contents,
        });
        Self
    }
}

#[cfg(test)]
impl Drop for RestoreFileTestFaultGuard {
    fn drop(&mut self) {
        RESTORE_FILE_TEST_FAULT.set(RestoreFileTestFault::None);
    }
}

#[cfg(test)]
pub(super) fn inject_restore_file_fault(label: &'static str) -> AppResult<()> {
    RESTORE_FILE_TEST_FAULT.with_borrow_mut(|fault| {
        let pending = std::mem::replace(fault, RestoreFileTestFault::None);
        match pending {
            RestoreFileTestFault::ReplaceAndConflict {
                label: expected,
                path,
                contents,
            } if expected == label => {
                fs::write(&path, contents)?;
                Err(AppError::AtomicWriteConflict(format!(
                    "测试注入的 {label} 并发写入冲突"
                )))
            }
            other => {
                *fault = other;
                Ok(())
            }
        }
    })
}

#[cfg(not(test))]
pub(super) fn inject_restore_file_fault(_label: &'static str) -> AppResult<()> {
    Ok(())
}

impl RestoreFileSnapshots {
    pub(super) fn capture(paths: &[(&'static str, &Path)]) -> AppResult<Self> {
        let root = loop {
            let sequence = RESTORE_SNAPSHOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let candidate = std::env::temp_dir().join(format!(
                "ccsm-restore-snapshot-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        };
        let mut files = Vec::with_capacity(paths.len());
        for (index, (label, path)) in paths.iter().enumerate() {
            let capture = (|| -> AppResult<RestoreFileSnapshot> {
                match fs::symlink_metadata(path) {
                    Ok(metadata) => {
                        if path_safety::metadata_is_link_or_reparse(&metadata)
                            || !metadata.is_file()
                        {
                            return Err(AppError::Path(format!(
                                "{label} 不是普通文件，拒绝创建还原快照: {}",
                                path.to_string_lossy()
                            )));
                        }
                        let before = atomic_file::fingerprint(path)?;
                        let snapshot_path = root.join(format!("{index}.snapshot"));
                        // A read-only stream copy avoids source metadata write access. The
                        // before/after fingerprints still reject concurrent mutations.
                        let mut source = File::open(path)?;
                        let mut destination = File::create(&snapshot_path)?;
                        std::io::copy(&mut source, &mut destination)?;
                        destination.sync_all()?;
                        drop(destination);
                        let snapshot_fingerprint = atomic_file::fingerprint(&snapshot_path)?;
                        let after = atomic_file::fingerprint(path)?;
                        if before != after || before != snapshot_fingerprint {
                            return Err(AppError::Other(format!(
                                "{label} 在创建还原快照期间发生变化: {}",
                                path.to_string_lossy()
                            )));
                        }
                        Ok(RestoreFileSnapshot {
                            label,
                            path: path.to_path_buf(),
                            snapshot_path: Some(snapshot_path),
                            original_fingerprint: Some(before),
                            mutation_started: false,
                            mutation_committed: false,
                            expected_after: None,
                        })
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        Ok(RestoreFileSnapshot {
                            label,
                            path: path.to_path_buf(),
                            snapshot_path: None,
                            original_fingerprint: None,
                            mutation_started: false,
                            mutation_committed: false,
                            expected_after: None,
                        })
                    }
                    Err(error) => Err(error.into()),
                }
            })()
            .map_err(|error| {
                AppError::Other(format!(
                    "创建 {label} 快照失败 {}: {error}",
                    path.to_string_lossy()
                ))
            });
            match capture {
                Ok(snapshot) => files.push(snapshot),
                Err(error) => {
                    return match fs::remove_dir_all(&root) {
                        Ok(()) => Err(error),
                        Err(cleanup_error) => Err(AppError::Other(format!(
                            "{error}；清理未完成还原快照失败 {}: {cleanup_error}",
                            root.to_string_lossy()
                        ))),
                    };
                }
            }
        }
        Ok(Self { root, files })
    }

    pub(super) fn start(&mut self, label: &'static str) -> AppResult<()> {
        let snapshot = self
            .files
            .iter_mut()
            .find(|snapshot| snapshot.label == label)
            .ok_or_else(|| AppError::Other(format!("缺少 {label} 的还原快照")))?;
        snapshot.mutation_started = true;
        Ok(())
    }

    pub(super) fn was_present(&self, label: &'static str) -> AppResult<bool> {
        self.files
            .iter()
            .find(|snapshot| snapshot.label == label)
            .map(|snapshot| snapshot.original_fingerprint.is_some())
            .ok_or_else(|| AppError::Other(format!("缺少 {label} 的还原快照")))
    }

    pub(super) fn finish(&mut self, label: &'static str) -> AppResult<()> {
        let snapshot = self
            .files
            .iter_mut()
            .find(|snapshot| snapshot.label == label)
            .ok_or_else(|| AppError::Other(format!("缺少 {label} 的还原快照")))?;
        // The write has returned success. Mark it committed before observing the final
        // fingerprint so an observation failure cannot silently drop it from rollback.
        snapshot.mutation_committed = true;
        snapshot.expected_after =
            Some(current_regular_fingerprint(&snapshot.path, snapshot.label)?);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn mark_committed_without_observation(
        &mut self,
        label: &'static str,
    ) -> AppResult<()> {
        let snapshot = self
            .files
            .iter_mut()
            .find(|snapshot| snapshot.label == label)
            .ok_or_else(|| AppError::Other(format!("缺少 {label} 的还原快照")))?;
        snapshot.mutation_committed = true;
        snapshot.expected_after = None;
        Ok(())
    }

    pub(super) fn record_failure(
        &mut self,
        label: &'static str,
        error: &AppError,
    ) -> AppResult<()> {
        let snapshot = self
            .files
            .iter_mut()
            .find(|snapshot| snapshot.label == label)
            .ok_or_else(|| AppError::Other(format!("缺少 {label} 的还原快照")))?;
        if error.atomic_write_not_committed() {
            return Ok(());
        }
        snapshot.mutation_committed = true;
        snapshot.expected_after =
            Some(current_regular_fingerprint(&snapshot.path, snapshot.label)?);
        Ok(())
    }

    pub(super) fn compensate_except(&self, excluded_labels: &[&str]) -> Vec<String> {
        let mut errors = Vec::new();
        for snapshot in self.files.iter().rev().filter(|snapshot| {
            snapshot.mutation_started
                && snapshot.mutation_committed
                && !excluded_labels.contains(&snapshot.label)
        }) {
            if let Err(error) = restore_file_snapshot(snapshot) {
                errors.push(format!("补偿 {} 失败: {error}", snapshot.label));
            }
        }
        errors
    }

    pub(super) fn cleanup(&self) -> AppResult<()> {
        fs::remove_dir_all(&self.root).map_err(|error| {
            AppError::Other(format!(
                "清理还原快照失败 {}: {error}",
                self.root.to_string_lossy()
            ))
        })
    }
}

fn current_regular_fingerprint(
    path: &Path,
    label: &str,
) -> AppResult<Option<atomic_file::FileFingerprint>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if path_safety::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
                return Err(AppError::Path(format!(
                    "{label} 在补偿前不再是普通文件: {}",
                    path.to_string_lossy()
                )));
            }
            Ok(Some(atomic_file::fingerprint(path)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn restore_file_snapshot(snapshot: &RestoreFileSnapshot) -> AppResult<()> {
    let current = current_regular_fingerprint(&snapshot.path, snapshot.label)?;
    if current == snapshot.original_fingerprint {
        return Ok(());
    }
    let expected_after = snapshot.expected_after.as_ref().ok_or_else(|| {
        AppError::Other(format!(
            "无法确认本次写入后的文件状态，拒绝盲目覆盖: {}",
            snapshot.path.to_string_lossy()
        ))
    })?;
    if &current != expected_after {
        return Err(AppError::Other(format!(
            "文件在还原失败后又发生变化，拒绝覆盖并发数据: {}",
            snapshot.path.to_string_lossy()
        )));
    }

    match (
        snapshot.original_fingerprint.as_ref(),
        snapshot.snapshot_path.as_deref(),
        current.as_ref(),
    ) {
        (Some(_), Some(snapshot_path), Some(current)) => {
            atomic_file::replace_with_writer_if_unchanged(&snapshot.path, current, |destination| {
                let mut source = File::open(snapshot_path)?;
                std::io::copy(&mut source, destination)?;
                Ok(())
            })
        }
        (Some(_), Some(snapshot_path), None) => {
            if let Some(parent) = snapshot.path.parent() {
                fs::create_dir_all(parent)?;
            }
            atomic_file::create_with_writer_if_absent(&snapshot.path, |destination| {
                let mut source = File::open(snapshot_path)?;
                std::io::copy(&mut source, destination)?;
                Ok(())
            })
        }
        (None, None, Some(current)) => {
            atomic_file::remove_file_if_unchanged(&snapshot.path, current, "还原补偿目标")
        }
        (None, None, None) => Ok(()),
        _ => Err(AppError::Other(format!(
            "{} 的还原快照结构不完整",
            snapshot.label
        ))),
    }
}

pub(super) fn restore_failure_message(
    primary: impl std::fmt::Display,
    rollback_error: Option<rusqlite::Error>,
    compensation_errors: Vec<String>,
    cleanup_error: Option<AppError>,
) -> String {
    let final_state_uncertain = rollback_error.is_some() || !compensation_errors.is_empty();
    let mut details = vec![primary.to_string()];
    if let Some(error) = rollback_error {
        details.push(format!("回滚 SQLite 事务失败: {error}"));
    }
    details.extend(compensation_errors);
    if let Some(error) = cleanup_error {
        details.push(error.to_string());
    }
    if final_state_uncertain {
        details.push("部分回滚或补偿失败，最终状态不确定，请检查上述文件与数据库".into());
    }
    details.join("；")
}
