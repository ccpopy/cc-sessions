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
struct FileMutationSnapshot {
    path: PathBuf,
    contents: Option<Vec<u8>>,
    fingerprint: Option<atomic_file::FileFingerprint>,
}

impl FileMutationSnapshot {
    fn capture(path: &Path) -> AppResult<Self> {
        match fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.is_file()
                    && !crate::path_safety::metadata_is_link_or_reparse(&metadata) =>
            {
                let before = atomic_file::fingerprint(path)?;
                let contents = fs::read(path)?;
                let contents_fingerprint = atomic_file::fingerprint_bytes(&contents);
                let after = atomic_file::fingerprint(path)?;
                if before != after || before != contents_fingerprint {
                    return Err(AppError::AtomicWriteConflict(format!(
                        "文件在创建补偿快照期间发生变化，已拒绝修改: {}",
                        path.to_string_lossy()
                    )));
                }
                Ok(Self {
                    path: path.to_path_buf(),
                    contents: Some(contents),
                    fingerprint: Some(before),
                })
            }
            Ok(_) => Err(AppError::Path(format!(
                "待修改路径不是普通文件: {}",
                path.to_string_lossy()
            ))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                path: path.to_path_buf(),
                contents: None,
                fingerprint: None,
            }),
            Err(error) => Err(error.into()),
        }
    }

    fn into_compensation(self) -> AppResult<Option<MutationCompensation>> {
        match fs::symlink_metadata(&self.path) {
            Ok(metadata)
                if metadata.is_file()
                    && !crate::path_safety::metadata_is_link_or_reparse(&metadata) =>
            {
                let current = atomic_file::fingerprint(&self.path)?;
                if self.fingerprint.as_ref() == Some(&current) {
                    Ok(None)
                } else {
                    Ok(Some(MutationCompensation::RestoreFile {
                        path: self.path,
                        contents: self.contents,
                        expected_current: current,
                    }))
                }
            }
            Ok(_) => Err(AppError::Path(format!(
                "修改后的路径不是普通文件: {}",
                self.path.to_string_lossy()
            ))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if self.contents.is_none() {
                    Ok(None)
                } else {
                    Err(AppError::Other(format!(
                        "修改后的文件意外消失，无法登记补偿: {}",
                        self.path.to_string_lossy()
                    )))
                }
            }
            Err(error) => Err(error.into()),
        }
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
        contents: Option<Vec<u8>>,
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
                contents,
                expected_current,
            } => {
                let metadata = fs::symlink_metadata(&path)?;
                if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata)
                {
                    return Err(AppError::Path(format!(
                        "补偿目标不是普通文件或属于链接/junction: {}",
                        path.to_string_lossy()
                    )));
                }
                let current = atomic_file::fingerprint(&path)?;
                if current != expected_current {
                    return Err(AppError::Other(format!(
                        "补偿前文件已再次变化，拒绝覆盖: {}",
                        path.to_string_lossy()
                    )));
                }
                if let Some(contents) = contents {
                    atomic_file::replace_with_writer_if_unchanged(
                        &path,
                        &expected_current,
                        |file| {
                            file.write_all(&contents)?;
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
                Ok(())
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
        let mutation_result = mutation();
        let compensation_result = snapshot.into_compensation();
        match (mutation_result, compensation_result) {
            (Ok(value), Ok(Some(compensation))) => {
                self.compensations.push(compensation);
                Ok(value)
            }
            (Ok(value), Ok(None)) => Ok(value),
            (Ok(_), Err(snapshot_error)) => Err(AppError::Other(format!(
                "文件修改已返回成功，但登记补偿状态失败，最终状态不确定: {snapshot_error}"
            ))),
            (Err(mutation_error), Ok(Some(compensation)))
                if !mutation_error.atomic_write_not_committed() =>
            {
                // Some atomic writers can commit the target and then fail while syncing or
                // cleaning a backup. That change still belongs to this workflow and must remain
                // compensatable when a later step rolls back the surrounding operation.
                self.compensations.push(compensation);
                Err(mutation_error)
            }
            (Err(mutation_error), Ok(_)) => {
                // A compare-and-swap/create conflict means the observed bytes may belong to
                // another process. Never register those bytes as this workflow's write.
                Err(mutation_error)
            }
            (Err(mutation_error), Err(snapshot_error)) => {
                if mutation_error.atomic_write_not_committed() {
                    Err(mutation_error)
                } else {
                    Err(AppError::Other(format!(
                        "{mutation_error}; 登记文件补偿失败，最终状态不确定: {snapshot_error}"
                    )))
                }
            }
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
                "操作已提交，但清理删除暂存文件失败: {}",
                errors.join(" | ")
            )))
        }
    }

    pub(crate) fn compensate_without_transaction(self, primary_error: AppError) -> AppError {
        self.compensate(primary_error)
    }

    pub(crate) fn move_file(&mut self, original: &Path, current: &Path) -> AppResult<()> {
        atomic_file::move_file_if_absent(original, current)?;
        match atomic_file::fingerprint(current) {
            Ok(expected_current) => {
                self.compensations.push(MutationCompensation::UndoMove {
                    original: original.to_path_buf(),
                    current: current.to_path_buf(),
                    expected_current,
                });
                Ok(())
            }
            Err(error) => match atomic_file::move_file_if_absent(current, original) {
                Ok(()) => Err(error),
                Err(restore_error) => Err(AppError::Other(format!(
                    "移动后读取文件指纹失败: {error}; 立即移回原位置也失败 {} -> {}: {restore_error}",
                    current.to_string_lossy(),
                    original.to_string_lossy()
                ))),
            },
        }
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
                fs::write(&path, b"committed\n")?;
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
                fs::write(&path, b"committed\n")?;
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
