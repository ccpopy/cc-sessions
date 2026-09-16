//! Cross-store mutation compensation shared by Codex write workflows.
//!
//! A Codex operation can touch rollout/index/family files, SQLite, and Desktop's private
//! project-state JSON. This module keeps the file and project-state compensations independent of
//! any one business workflow so move/import/convert/repair/delete can share one rollback model.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::atomic_file;
use crate::error::{AppError, AppResult};

#[derive(Debug)]
struct DiskSnapshot {
    path: PathBuf,
    fingerprint: atomic_file::FileFingerprint,
    permissions: fs::Permissions,
    retain: bool,
}

impl DiskSnapshot {
    fn cleanup(&mut self) -> AppResult<()> {
        self.retain = true;
        match atomic_file::remove_staged_file_if_unchanged(
            &self.path,
            &self.fingerprint,
            "补偿快照",
        ) {
            Ok(()) => Ok(()),
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for DiskSnapshot {
    fn drop(&mut self) {
        if !self.retain && !std::thread::panicking() {
            if let Err(error) = fs::remove_file(&self.path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("清理补偿快照失败 {}: {error}", self.path.display());
                }
            }
        }
    }
}

#[derive(Debug)]
struct FileMutationSnapshot {
    path: PathBuf,
    contents: Option<DiskSnapshot>,
}

impl FileMutationSnapshot {
    fn fingerprint(&self) -> Option<atomic_file::FileFingerprint> {
        self.contents
            .as_ref()
            .map(|snapshot| snapshot.fingerprint.clone())
    }

    fn capture(path: &Path) -> AppResult<Self> {
        let _measurement = crate::operation_metrics::Measurement::start("compensation_snapshot");
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.is_file()
                    && !crate::path_safety::metadata_is_link_or_reparse(&metadata) =>
            {
                metadata
            }
            Ok(_) => {
                return Err(AppError::Path(format!(
                    "待修改路径不是普通文件: {}",
                    path.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path: path.to_path_buf(),
                    contents: None,
                });
            }
            Err(error) => return Err(error.into()),
        };
        let mut name = path
            .file_name()
            .ok_or_else(|| AppError::Path("快照目标缺少文件名".into()))?
            .to_os_string();
        name.push(".ccsm-compensation");
        let (snapshot_path, file) = atomic_file::create_unique_temp(&path.with_file_name(name))?;
        let mut writer = atomic_file::AtomicWriter::new(file);
        let mut snapshot = DiskSnapshot {
            path: snapshot_path,
            fingerprint: writer.fingerprint(),
            permissions: metadata.permissions(),
            retain: false,
        };
        let length = std::io::copy(&mut fs::File::open(path)?, &mut writer)?;
        writer.flush()?;
        writer.sync_all()?;
        crate::operation_metrics::record(|c| {
            c.read_bytes += length;
            c.snapshot_bytes += length;
        });
        snapshot.fingerprint = writer.fingerprint();
        drop(writer);
        if atomic_file::fingerprint(path)? != snapshot.fingerprint {
            return Err(AppError::AtomicWriteConflict(format!(
                "文件在创建补偿快照期间发生变化，已拒绝修改: {}",
                path.display()
            )));
        }
        Ok(Self {
            path: path.to_path_buf(),
            contents: Some(snapshot),
        })
    }

    fn into_compensation(
        mut self,
        written: Option<atomic_file::FileFingerprint>,
    ) -> AppResult<Option<MutationCompensation>> {
        if let Some(expected_current) = written {
            return Ok(Some(MutationCompensation::RestoreFile {
                path: self.path,
                contents: self.contents,
                expected_current,
            }));
        }
        let current = match fs::symlink_metadata(&self.path) {
            Ok(metadata)
                if metadata.is_file()
                    && !crate::path_safety::metadata_is_link_or_reparse(&metadata) =>
            {
                atomic_file::fingerprint(&self.path).map(Some)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Ok(_) => Err(AppError::Path("修改后的路径不是普通文件".into())),
            Err(error) => Err(error.into()),
        };
        if matches!(&current, Ok(current) if current == &self.fingerprint()) {
            return Ok(None);
        }
        let recovery = self
            .contents
            .as_mut()
            .map(|snapshot| {
                snapshot.retain = true;
                format!("；原文件补偿快照保留在 {}", snapshot.path.display())
            })
            .unwrap_or_default();
        Err(AppError::Other(format!(
            "文件缺少本次写入凭据，拒绝猜测补偿状态: {}{recovery}{}",
            self.path.display(),
            current
                .err()
                .map(|error| format!("；{error}"))
                .unwrap_or_default()
        )))
    }
}

#[derive(Debug)]
enum MutationCompensation {
    RestoreProjectState(crate::codex_projects::StateMutationReceipt),
    RestoreStagedFile {
        original: PathBuf,
        staged: PathBuf,
        expected_staged: atomic_file::FileFingerprint,
    },
    RestoreFile {
        path: PathBuf,
        contents: Option<DiskSnapshot>,
        expected_current: atomic_file::FileFingerprint,
    },
    UndoMove {
        original: PathBuf,
        current: PathBuf,
        expected_current: atomic_file::FileFingerprint,
    },
}

impl MutationCompensation {
    fn apply(self) -> AppResult<()> {
        match self {
            Self::RestoreProjectState(receipt) => receipt.compensate(),
            Self::RestoreStagedFile {
                original,
                staged,
                expected_staged,
            } => {
                let metadata = fs::symlink_metadata(&staged)?;
                if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata)
                {
                    return Err(AppError::Path(format!(
                        "补偿删除的暂存源不是普通文件或属于链接/junction: {}",
                        staged.to_string_lossy()
                    )));
                }
                if atomic_file::fingerprint(&staged)? != expected_staged {
                    return Err(AppError::Other(format!(
                        "补偿删除前暂存文件已发生变化，拒绝恢复: {}",
                        staged.to_string_lossy()
                    )));
                }
                atomic_file::move_file_if_absent(&staged, &original)?;
                Ok(())
            }
            Self::RestoreFile {
                path,
                mut contents,
                expected_current,
            } => {
                let result = (|| {
                    let metadata = fs::symlink_metadata(&path)?;
                    if !metadata.is_file()
                        || crate::path_safety::metadata_is_link_or_reparse(&metadata)
                    {
                        return Err(AppError::Path(format!(
                            "补偿目标不是普通文件或属于链接/junction: {}",
                            path.display()
                        )));
                    }
                    if let Some(snapshot) = &contents {
                        atomic_file::replace_with_writer_if_unchanged(
                            &path,
                            &expected_current,
                            |file| {
                                let metadata = fs::symlink_metadata(&snapshot.path)?;
                                if !metadata.is_file()
                                    || crate::path_safety::metadata_is_link_or_reparse(&metadata)
                                {
                                    return Err(AppError::Path("补偿快照不是普通文件".into()));
                                }
                                std::io::copy(&mut fs::File::open(&snapshot.path)?, file)?;
                                if file.fingerprint() != snapshot.fingerprint {
                                    return Err(AppError::Other(
                                        "补偿快照已发生变化，拒绝恢复".into(),
                                    ));
                                }
                                file.set_permissions(snapshot.permissions.clone())?;
                                Ok(())
                            },
                        )?;
                    } else {
                        atomic_file::remove_file_if_unchanged(
                            &path,
                            &expected_current,
                            "补偿新建文件",
                        )?;
                    }
                    if let Some(snapshot) = &mut contents {
                        snapshot.cleanup()?;
                    }
                    Ok(())
                })();
                result.map_err(|error: AppError| {
                    if let Some(snapshot) = &mut contents {
                        snapshot.retain = true;
                        AppError::Other(format!(
                            "{error}; 补偿快照保留在 {}",
                            snapshot.path.display()
                        ))
                    } else {
                        error
                    }
                })
            }
            Self::UndoMove {
                original,
                current,
                expected_current,
            } => {
                let metadata = fs::symlink_metadata(&current)?;
                if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata)
                {
                    return Err(AppError::Path(format!(
                        "补偿移动源不是普通文件或属于链接/junction: {}",
                        current.to_string_lossy()
                    )));
                }
                let current_fingerprint = atomic_file::fingerprint(&current)?;
                if current_fingerprint != expected_current {
                    return Err(AppError::Other(format!(
                        "补偿移动前文件已再次变化，拒绝移动: {}",
                        current.to_string_lossy()
                    )));
                }
                if let Some(parent) = original.parent() {
                    fs::create_dir_all(parent)?;
                }
                atomic_file::move_file_if_absent(&current, &original)?;
                Ok(())
            }
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct MutationJournal {
    compensations: Vec<MutationCompensation>,
    staged_deletions: Vec<(PathBuf, atomic_file::FileFingerprint)>,
}

impl MutationJournal {
    pub(crate) fn register_project_state_receipt(
        &mut self,
        receipt: crate::codex_projects::StateMutationReceipt,
    ) {
        self.compensations
            .push(MutationCompensation::RestoreProjectState(receipt));
    }

    pub(crate) fn mutate_file<T>(
        &mut self,
        path: &Path,
        mutation: impl FnOnce() -> AppResult<T>,
    ) -> AppResult<T> {
        let snapshot = FileMutationSnapshot::capture(path)?;
        let receipt = atomic_file::ReceiptScope::begin(path, snapshot.fingerprint())?;
        let mutation_result = mutation();
        let written = receipt.written();
        drop(receipt);
        // Even if the final write failed, earlier successful writes in this callback still need
        // compensation. Only an actual writer receipt establishes ownership of those bytes.
        if written.is_none()
            && mutation_result
                .as_ref()
                .is_err_and(|error| error.atomic_write_not_committed())
        {
            return mutation_result;
        }
        match snapshot.into_compensation(written) {
            Ok(Some(compensation)) => {
                self.compensations.push(compensation);
                mutation_result
            }
            Ok(None) => mutation_result,
            Err(error) => match mutation_result {
                Ok(_) => Err(error),
                Err(primary) => Err(AppError::Other(format!("{primary}; {error}"))),
            },
        }
    }

    /// Stage an ordinary file by same-directory rename. The original path disappears atomically,
    /// while rollback can restore the exact file without a read/delete race.
    pub(crate) fn remove_file(&mut self, path: &Path) -> AppResult<()> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "待删除路径不是普通文件或属于链接/junction: {}",
                path.to_string_lossy()
            )));
        }
        let original_fingerprint = atomic_file::fingerprint(path)?;
        let staged = unique_delete_stage(path)?;
        atomic_file::move_file_if_absent(path, &staged)?;
        let expected_staged = match staged_regular_fingerprint(&staged) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                return match atomic_file::move_file_if_absent(&staged, path) {
                    Ok(()) => Err(error),
                    Err(restore_error) => Err(AppError::Other(format!(
                        "删除暂存后读取指纹失败: {error}; 立即恢复也失败 {} -> {}: {restore_error}",
                        staged.to_string_lossy(),
                        path.to_string_lossy()
                    ))),
                };
            }
        };
        if expected_staged != original_fingerprint {
            let conflict = AppError::AtomicWriteConflict(format!(
                "文件在删除暂存期间发生变化，已拒绝提交删除: {}",
                path.to_string_lossy()
            ));
            return match atomic_file::move_file_if_absent(&staged, path) {
                Ok(()) => Err(conflict),
                Err(restore_error) => Err(AppError::Other(format!(
                    "{conflict}; 恢复原路径也失败 {} -> {}: {restore_error}",
                    staged.to_string_lossy(),
                    path.to_string_lossy()
                ))),
            };
        }
        self.compensations
            .push(MutationCompensation::RestoreStagedFile {
                original: path.to_path_buf(),
                staged: staged.clone(),
                expected_staged: expected_staged.clone(),
            });
        self.staged_deletions.push((staged, expected_staged));
        Ok(())
    }

    /// Permanently remove files staged by `remove_file` after the surrounding SQLite commit.
    pub(crate) fn finalize(mut self) -> AppResult<()> {
        let mut errors = Vec::new();
        for compensation in &mut self.compensations {
            if let MutationCompensation::RestoreFile {
                contents: Some(snapshot),
                ..
            } = compensation
            {
                if let Err(error) = snapshot.cleanup() {
                    errors.push(error.to_string());
                }
            }
        }
        for (staged, expected) in self.staged_deletions.drain(..) {
            let cleanup =
                atomic_file::remove_staged_file_if_unchanged(&staged, &expected, "删除暂存文件");
            match cleanup {
                Ok(()) => {}
                Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => errors.push(error.to_string()),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(AppError::Other(format!(
                "操作已提交，但清理补偿暂存文件失败: {}",
                errors.join(" | ")
            )))
        }
    }

    pub(crate) fn compensate_without_transaction(self, primary_error: AppError) -> AppError {
        self.compensate(primary_error)
    }

    pub(crate) fn move_file(&mut self, original: &Path, current: &Path) -> AppResult<()> {
        let expected_current = atomic_file::fingerprint(original)?;
        atomic_file::move_file_if_absent(original, current)?;
        self.compensations.push(MutationCompensation::UndoMove {
            original: original.to_path_buf(),
            current: current.to_path_buf(),
            expected_current: expected_current.clone(),
        });
        if atomic_file::fingerprint(current)? != expected_current {
            return Err(AppError::AtomicWriteConflict(format!(
                "文件在移动期间发生变化: {}",
                current.display()
            )));
        }
        Ok(())
    }

    fn compensate(self, primary_error: AppError) -> AppError {
        let mut compensation_errors = Vec::new();
        for compensation in self.compensations.into_iter().rev() {
            if let Err(error) = compensation.apply() {
                compensation_errors.push(error.to_string());
            }
        }
        if compensation_errors.is_empty() {
            primary_error
        } else {
            AppError::Other(format!(
                "{primary_error}; 补偿失败: {}",
                compensation_errors.join(" | ")
            ))
        }
    }
}

fn staged_regular_fingerprint(path: &Path) -> AppResult<atomic_file::FileFingerprint> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        return Err(AppError::Path(format!(
            "删除暂存路径不是普通文件或属于链接/junction: {}",
            path.to_string_lossy()
        )));
    }
    atomic_file::fingerprint(path)
}

fn unique_delete_stage(path: &Path) -> AppResult<PathBuf> {
    let parent = path.parent().ok_or_else(|| {
        AppError::Path(format!("待删除文件缺少父目录: {}", path.to_string_lossy()))
    })?;
    let name = path.file_name().ok_or_else(|| {
        AppError::Path(format!("待删除文件缺少文件名: {}", path.to_string_lossy()))
    })?;
    for sequence in 0u32.. {
        let mut staged_name = name.to_os_string();
        staged_name.push(format!(
            ".{}.{}.ccsm-delete-stage",
            std::process::id(),
            sequence
        ));
        let staged = parent.join(staged_name);
        match fs::symlink_metadata(&staged) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(staged),
            Ok(_) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!()
}

pub(crate) fn rollback_transaction_with_compensation(
    transaction: rusqlite::Transaction<'_>,
    journal: MutationJournal,
    primary_error: AppError,
) -> AppError {
    let primary_error = match transaction.rollback() {
        Ok(()) => primary_error,
        Err(error) => AppError::Other(format!("{primary_error}; SQLite 事务回滚失败: {error}")),
    };
    journal.compensate(primary_error)
}

pub(crate) fn commit_transaction_with_compensation(
    transaction: rusqlite::Transaction<'_>,
    journal: MutationJournal,
) -> AppResult<()> {
    match transaction.execute_batch("COMMIT") {
        Ok(()) => {
            drop(transaction);
            journal.finalize()
        }
        Err(commit_error) => {
            let primary_error = AppError::Other(format!("提交 SQLite 事务失败: {commit_error}"));
            Err(rollback_transaction_with_compensation(
                transaction,
                journal,
                primary_error,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn round2_receipt_registration_never_rereads_new_output() -> AppResult<()> {
        let (root, path) = temp_file("receipt-read-count")?;
        fs::remove_file(&path)?;
        let mut journal = MutationJournal::default();
        let (result, counters) = crate::operation_metrics::measured(|| {
            journal.mutate_file(&path, || {
                atomic_file::create_with_writer_if_absent(&path, |file| {
                    let chunk = [b'x'; 8192];
                    for _ in 0..128 {
                        file.write_all(&chunk)?;
                    }
                    Ok(())
                })
            })
        });
        result?;
        assert_eq!(counters.read_bytes, 0);
        assert_eq!(counters.hash_bytes, 1024 * 1024);
        assert_eq!(fs::metadata(&path)?.len(), 1024 * 1024);
        journal.finalize()?;
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn round2_disk_snapshot_starts_private_and_restores_permissions() -> AppResult<()> {
        use std::os::unix::fs::PermissionsExt;
        let (root, path) = temp_file("snapshot-permissions")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;
        let mut journal = MutationJournal::default();
        journal.mutate_file(&path, || {
            atomic_file::overwrite_with_writer(&path, |file| {
                file.write_all(b"ours\n")?;
                Ok(())
            })
        })?;
        if let MutationCompensation::RestoreFile {
            contents: Some(snapshot),
            ..
        } = &journal.compensations[0]
        {
            assert_eq!(
                fs::metadata(&snapshot.path)?.permissions().mode() & 0o777,
                0o600
            );
        } else {
            panic!("disk snapshot expected");
        }
        journal.compensate_without_transaction(AppError::Other("later".into()));
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o640);
        assert_eq!(fs::read(&path)?, b"before\n");
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn round2_refreshed_writer_baseline_cannot_include_external_bytes() -> AppResult<()> {
        let (root, path) = temp_file("refreshed-baseline")?;
        let mut journal = MutationJournal::default();
        let error = journal
            .mutate_file(&path, || {
                fs::write(&path, b"external\n")?;
                atomic_file::replace_with_writer_if_unchanged(
                    &path,
                    &atomic_file::fingerprint(&path)?,
                    |file| {
                        file.write_all(b"ours\n")?;
                        Ok(())
                    },
                )
            })
            .unwrap_err();
        assert!(error.atomic_write_not_committed());
        journal.compensate_without_transaction(error);
        assert_eq!(fs::read(&path)?, b"external\n");
        assert_eq!(fs::read_dir(&root)?.count(), 1);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn round2_receipts_cover_multiple_writes_and_late_conflicts() -> AppResult<()> {
        for external in [false, true] {
            let (root, path) = temp_file("multi-write-receipts")?;
            let mut journal = MutationJournal::default();
            let result = journal.mutate_file(&path, || {
                atomic_file::overwrite_with_writer(&path, |file| {
                    file.write_all(b"first\n")?;
                    Ok(())
                })?;
                if external {
                    fs::write(&path, b"external\n")?;
                }
                atomic_file::overwrite_with_writer(&path, |file| {
                    file.write_all(b"second\n")?;
                    Ok(())
                })
            });
            assert_eq!(result.is_err(), external);
            let error = journal.compensate_without_transaction(AppError::Other("later".into()));
            assert_eq!(
                fs::read(&path)?,
                if external {
                    b"external\n".as_slice()
                } else {
                    b"before\n".as_slice()
                }
            );
            assert_eq!(error.to_string().contains("补偿快照保留在"), external);
            assert_eq!(fs::read_dir(&root)?.count(), if external { 2 } else { 1 });
            fs::remove_dir_all(root)?;
        }
        Ok(())
    }

    #[test]
    fn round2_disk_snapshot_lifecycle_and_corruption() -> AppResult<()> {
        for corrupt in [false, true] {
            let (root, path) = temp_file("disk-snapshot")?;
            let mut journal = MutationJournal::default();
            journal.mutate_file(&path, || {
                atomic_file::overwrite_with_writer(&path, |file| {
                    file.write_all(b"ours\n")?;
                    Ok(())
                })
            })?;
            let snapshot = match &journal.compensations[0] {
                MutationCompensation::RestoreFile {
                    contents: Some(snapshot),
                    ..
                } => snapshot.path.clone(),
                _ => panic!("disk snapshot expected"),
            };
            assert_eq!(fs::read(&snapshot)?, b"before\n");
            if corrupt {
                fs::write(&snapshot, b"corrupt\n")?;
                let error = journal.compensate_without_transaction(AppError::Other("later".into()));
                assert!(error.to_string().contains("快照已发生变化"));
                assert!(snapshot.exists());
            } else {
                journal.finalize()?;
                assert!(!snapshot.exists());
            }
            assert_eq!(fs::read(&path)?, b"ours\n");
            fs::remove_dir_all(root)?;
        }
        Ok(())
    }

    #[test]
    fn round2_finalize_preserves_a_replaced_snapshot() -> AppResult<()> {
        let (root, path) = temp_file("snapshot-finalize-conflict")?;
        let mut journal = MutationJournal::default();
        journal.mutate_file(&path, || {
            atomic_file::overwrite_with_writer(&path, |file| {
                file.write_all(b"ours\n")?;
                Ok(())
            })
        })?;
        let snapshot_path = match &journal.compensations[0] {
            MutationCompensation::RestoreFile {
                contents: Some(snapshot),
                ..
            } => snapshot.path.clone(),
            _ => panic!("snapshot expected"),
        };
        fs::write(&snapshot_path, b"external recovery data\n")?;
        assert!(journal.finalize().is_err());
        assert_eq!(fs::read(&snapshot_path)?, b"external recovery data\n");
        assert_eq!(fs::read(&path)?, b"ours\n");
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn round2_unreceipted_write_retains_recovery_material() -> AppResult<()> {
        let (root, path) = temp_file("unreceipted")?;
        let mut journal = MutationJournal::default();
        let error = journal
            .mutate_file(&path, || {
                fs::write(&path, b"unknown\n")?;
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("缺少本次写入凭据"));
        assert!(error.to_string().contains("补偿快照保留在"));
        journal.compensate_without_transaction(error);
        assert_eq!(fs::read(&path)?, b"unknown\n");
        assert_eq!(fs::read_dir(&root)?.count(), 2);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn round2_receipt_never_claims_an_external_post_write_update() -> AppResult<()> {
        let (root, path) = temp_file("receipt-post-write-race")?;
        let mut journal = MutationJournal::default();
        journal.mutate_file(&path, || {
            atomic_file::replace_with_writer_if_unchanged(
                &path,
                &atomic_file::fingerprint(&path)?,
                |file| {
                    file.write_all(b"ours\n")?;
                    Ok(())
                },
            )?;
            fs::write(&path, b"external\n")?;
            Ok(())
        })?;
        let error = journal.compensate_without_transaction(AppError::Other("later failure".into()));
        assert_eq!(
            fs::read(&path)?,
            b"external\n",
            "the writer receipt must describe our bytes, not a later reader's bytes"
        );
        assert!(error.to_string().contains("补偿失败"));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn review_snapshot_reads_and_hashes_two_copies() -> AppResult<()> {
        let path = std::env::temp_dir().join(format!(
            "cc-snapshot-counts-{}",
            crate::repair::new_session_id()
        ));
        fs::write(&path, vec![b'x'; 1024 * 1024])?;
        let (snapshot, counters) =
            crate::operation_metrics::measured(|| FileMutationSnapshot::capture(&path));
        let snapshot = snapshot?;
        assert_eq!(
            fs::metadata(&snapshot.contents.as_ref().unwrap().path)?.len(),
            1024 * 1024
        );
        assert_eq!(counters.snapshot_bytes, 1024 * 1024);
        assert_eq!(counters.read_bytes, 2 * 1024 * 1024);
        assert_eq!(counters.hash_bytes, 2 * 1024 * 1024);
        fs::remove_file(path)?;
        Ok(())
    }
    use super::*;

    fn temp_file(label: &str) -> AppResult<(PathBuf, PathBuf)> {
        let root = std::env::temp_dir().join(format!(
            "cc-session-manager-journal-{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&root)?;
        let path = root.join("state.jsonl");
        fs::write(&path, b"before\n")?;
        Ok((root, path))
    }

    #[test]
    fn concurrent_failure_does_not_claim_or_compensate_the_other_write() -> AppResult<()> {
        let (root, path) = temp_file("failed-concurrent-write")?;
        let mut journal = MutationJournal::default();

        let error = journal
            .mutate_file(&path, || {
                fs::write(&path, b"concurrent\n")?;
                Err::<(), _>(AppError::AtomicWriteConflict(
                    "文件在操作期间发生变化，已拒绝覆盖".to_string(),
                ))
            })
            .expect_err("failed mutation must be reported");
        assert!(error.to_string().contains("发生变化"));

        let compensated = journal.compensate_without_transaction(AppError::Other("later".into()));
        assert!(compensated.to_string().contains("later"));
        assert_eq!(fs::read(&path)?, b"concurrent\n");
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn committed_change_is_compensated_even_when_the_writer_reports_an_error() -> AppResult<()> {
        let (root, path) = temp_file("post-commit-error")?;
        let mut journal = MutationJournal::default();

        let error = journal
            .mutate_file(&path, || {
                atomic_file::overwrite_with_writer(&path, |file| {
                    file.write_all(b"committed\n")?;
                    Ok(())
                })?;
                Err::<(), _>(AppError::Other("cleanup failed after commit".to_string()))
            })
            .expect_err("post-commit failure must be reported");
        assert!(error.to_string().contains("cleanup failed"));

        let compensated = journal.compensate_without_transaction(AppError::Other("later".into()));
        assert!(compensated.to_string().contains("later"));
        assert_eq!(fs::read(&path)?, b"before\n");
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn error_text_alone_never_misclassifies_a_committed_change_as_concurrent() -> AppResult<()> {
        let (root, path) = temp_file("business-error-mentions-conflict")?;
        let mut journal = MutationJournal::default();

        journal
            .mutate_file(&path, || {
                atomic_file::overwrite_with_writer(&path, |file| {
                    file.write_all(b"committed\n")?;
                    Ok(())
                })?;
                Err::<(), _>(AppError::Other(
                    "业务校验失败：文件在操作期间发生变化（仅为引用文本）".to_string(),
                ))
            })
            .expect_err("business failure must be reported");

        journal.compensate_without_transaction(AppError::Other("later".into()));
        assert_eq!(fs::read(&path)?, b"before\n");
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn successful_mutation_is_compensated_when_a_later_step_fails() -> AppResult<()> {
        let (root, path) = temp_file("successful-compensation")?;
        let mut journal = MutationJournal::default();
        journal.mutate_file(&path, || {
            let expected = atomic_file::fingerprint(&path)?;
            atomic_file::replace_with_writer_if_unchanged(&path, &expected, |file| {
                file.write_all(b"after\n")?;
                Ok(())
            })
        })?;

        let compensated = journal.compensate_without_transaction(AppError::Other("later".into()));
        assert!(compensated.to_string().contains("later"));
        assert_eq!(fs::read(&path)?, b"before\n");
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn compensation_preserves_a_concurrently_recreated_new_file() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "cc-session-manager-journal-new-file-race-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&root)?;
        let path = root.join("new.jsonl");
        let mut journal = MutationJournal::default();
        journal.mutate_file(&path, || {
            atomic_file::create_with_writer_if_absent(&path, |file| {
                file.write_all(b"ours\n")?;
                Ok(())
            })
        })?;

        fs::remove_file(&path)?;
        fs::write(&path, b"concurrent\n")?;
        let error = journal.compensate_without_transaction(AppError::Other("later".into()));

        assert!(error.to_string().contains("补偿失败"), "{error}");
        assert_eq!(fs::read(&path)?, b"concurrent\n");
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn staged_delete_restores_the_exact_file_on_rollback() -> AppResult<()> {
        let (root, path) = temp_file("staged-delete-rollback")?;
        let mut journal = MutationJournal::default();

        journal.remove_file(&path)?;
        assert!(!path.exists());

        journal.compensate_without_transaction(AppError::Other("later".into()));
        assert_eq!(fs::read(&path)?, b"before\n");
        let leftovers = fs::read_dir(&root)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("ccsm-delete-stage")
            })
            .count();
        assert_eq!(leftovers, 0);
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn staged_delete_finalize_refuses_a_concurrent_stage_change() -> AppResult<()> {
        let (root, path) = temp_file("staged-delete-concurrent-finalize")?;
        let mut journal = MutationJournal::default();
        journal.remove_file(&path)?;
        let staged = journal.staged_deletions[0].0.clone();
        fs::write(&staged, b"concurrent\n")?;

        let error = journal
            .finalize()
            .expect_err("changed staged bytes must never be permanently deleted");

        assert!(error.to_string().contains("发生变化"), "{error}");
        assert_eq!(fs::read(&staged)?, b"concurrent\n");
        assert!(!path.exists());
        fs::remove_dir_all(root).ok();
        Ok(())
    }
}
