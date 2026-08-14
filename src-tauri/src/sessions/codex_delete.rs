//! Atomic removal of the physical records that make a Codex thread visible.
//!
//! The parent `sessions` module resolves user-facing session/family targets. This module owns the
//! lower-level compensated mutation across SQLite, rollout/index/family files, and Desktop's
//! private project state.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;

use crate::error::{AppError, AppResult};
use crate::family;
use crate::models::{DeleteResult, FamilyStore};
use crate::{paths, state_db};

pub(crate) struct CodexDeleteOutcome {
    pub(crate) result: DeleteResult,
    pub(crate) structurally_removed: bool,
}

pub(crate) fn delete_codex_artifacts(codex_dir: &Path, id: &str) -> AppResult<CodexDeleteOutcome> {
    let mut outcomes = delete_codex_artifacts_batch(codex_dir, &[id.to_string()])?;
    outcomes
        .pop()
        .ok_or_else(|| AppError::Other("Codex 删除未返回结果".to_string()))
}

/// Delete all Core and Desktop-visible state for several Codex threads as one compensated unit.
fn delete_codex_artifacts_batch(
    codex_dir: &Path,
    ids: &[String],
) -> AppResult<Vec<CodexDeleteOutcome>> {
    delete_codex_artifacts_batch_with_family_store(codex_dir, ids, None)
}

pub(super) fn delete_codex_artifacts_batch_with_family_store(
    codex_dir: &Path,
    ids: &[String],
    family_store: Option<&FamilyStore>,
) -> AppResult<Vec<CodexDeleteOutcome>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut unique_ids = Vec::with_capacity(ids.len());
    let mut seen_ids = HashSet::with_capacity(ids.len());
    for id in ids {
        super::validate_delete_id(id)?;
        if seen_ids.insert(id.clone()) {
            unique_ids.push(id.clone());
        }
    }

    // This preflight deliberately precedes a read/write SQLite open: opening SQLite may update
    // connection metadata, which would violate the zero-change contract for malformed state.
    crate::codex_projects::preflight_thread_project_state_cleanup(codex_dir, &unique_ids)?;
    if crate::codex_projects::desktop_state_initialized(codex_dir)? {
        crate::codex_projects::ensure_desktop_not_running(codex_dir)?;
    }

    struct DeletePreparation {
        id: String,
        rollout_files: Vec<PathBuf>,
        rollout_path: Option<String>,
    }

    let state = state_db::open(codex_dir)?;
    let transaction =
        rusqlite::Transaction::new_unchecked(&state, rusqlite::TransactionBehavior::Immediate)?;
    let mut preparations = Vec::with_capacity(unique_ids.len());
    for id in &unique_ids {
        let rollout_path: Option<String> = transaction
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        let mut rollout_files = super::rollout_files_by_id(codex_dir, id)?;
        if let Some(raw_path) = rollout_path.as_deref() {
            let db_path = PathBuf::from(paths::strip_verbatim(
                &paths::host_path_string_from_codex_record(codex_dir, raw_path),
            ));
            if db_path.is_file() {
                super::validate_codex_rollout_path(codex_dir, &db_path, id)?;
                rollout_files.push(db_path);
            }
        }
        let mut canonical_files = Vec::with_capacity(rollout_files.len());
        for path in rollout_files {
            super::validate_codex_rollout_path(codex_dir, &path, id)?;
            let canonical = path.canonicalize()?;
            if !canonical_files.contains(&canonical) {
                canonical_files.push(canonical);
            }
        }
        preparations.push(DeletePreparation {
            id: id.clone(),
            rollout_path: canonical_files
                .first()
                .map(|path| path.to_string_lossy().into_owned()),
            rollout_files: canonical_files,
        });
    }

    let logs_attached = attach_logs_database_for_delete(codex_dir, &transaction)?;
    let index_path = paths::session_index_path(codex_dir);
    let mut journal = crate::mutation_journal::MutationJournal::default();
    let operation = (|| -> AppResult<Vec<CodexDeleteOutcome>> {
        let mut outcomes = Vec::with_capacity(preparations.len());
        for prepared in &preparations {
            let rows = transaction.execute("DELETE FROM threads WHERE id = ?", [&prepared.id])?;
            let rows_logs = if logs_attached {
                transaction.execute(
                    "DELETE FROM delete_logs.logs WHERE thread_id = ?",
                    [&prepared.id],
                )?
            } else {
                0
            };

            for rollout in &prepared.rollout_files {
                journal.remove_file(rollout)?;
            }
            if index_path.exists() {
                journal.mutate_file(&index_path, || {
                    super::filter_index_file(&index_path, &prepared.id)
                })?;
            }

            // C5：删除会话后同步归档来源账本，经 journal 纳入同一补偿（M3 一致）。
            // 无记录时 remove 是 no-op 且不写盘，不会因此创建账本文件。
            journal.mutate_file(&paths::archive_ledger_path(codex_dir), || {
                crate::archive_ledger::remove(codex_dir, &prepared.id)
            })?;

            outcomes.push(CodexDeleteOutcome {
                result: DeleteResult {
                    id: prepared.id.clone(),
                    rollout_path: prepared.rollout_path.clone(),
                    threads_rows_deleted: rows as u32,
                    logs_rows_deleted: rows_logs as u32,
                    history_rows_deleted: 0,
                    rollout_deleted: !prepared.rollout_files.is_empty(),
                    rollout_missing: prepared.rollout_files.is_empty(),
                    sidecar_deleted: false,
                    tasks_deleted: false,
                    file_history_deleted: false,
                    shared_data_preserved: false,
                    ok: true,
                    error: None,
                },
                structurally_removed: true,
            });
        }

        // Keep this after every Core file mutation so late Desktop/CAS failures exercise the same
        // compensation path as other write failures.
        if let Some(receipt) =
            crate::codex_projects::clear_thread_project_states_with_receipt(codex_dir, &unique_ids)?
        {
            journal.register_project_state_receipt(receipt);
        }
        if let Some(family_store) = family_store {
            let family_path = paths::family_store_path(codex_dir);
            journal.mutate_file(&family_path, || family::save(codex_dir, family_store))?;
        }
        Ok(outcomes)
    })();

    let outcomes = match operation {
        Ok(outcomes) => {
            crate::mutation_journal::commit_transaction_with_compensation(transaction, journal)?;
            outcomes
        }
        Err(error) => {
            return Err(
                crate::mutation_journal::rollback_transaction_with_compensation(
                    transaction,
                    journal,
                    error,
                ),
            );
        }
    };

    // Empty date directories are cosmetic and deliberately outside the transaction. Failure to
    // remove one cannot make the conversation visible again, so it must not downgrade deletion.
    for prepared in &preparations {
        for rollout in &prepared.rollout_files {
            cleanup_empty_rollout_ancestors_best_effort(codex_dir, rollout);
        }
    }

    Ok(outcomes)
}

fn attach_logs_database_for_delete(
    codex_dir: &Path,
    transaction: &rusqlite::Transaction<'_>,
) -> AppResult<bool> {
    let logs_path = codex_dir.join("logs_2.sqlite");
    let metadata = match fs::symlink_metadata(&logs_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        return Err(AppError::Path(format!(
            "Codex logs 数据库不是普通文件: {}",
            logs_path.to_string_lossy()
        )));
    }
    transaction.execute(
        "ATTACH DATABASE ?1 AS delete_logs",
        [logs_path.to_string_lossy().into_owned()],
    )?;
    let has_logs_table: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM delete_logs.sqlite_schema WHERE type='table' AND name='logs')",
        [],
        |row| row.get(0),
    )?;
    if !has_logs_table {
        return Err(AppError::Other(format!(
            "Codex logs 数据库缺少 logs 表: {}",
            logs_path.to_string_lossy()
        )));
    }
    Ok(true)
}

fn cleanup_empty_rollout_ancestors_best_effort(codex_dir: &Path, rollout: &Path) {
    let sessions_root = paths::sessions_dir(codex_dir);
    let mut current = rollout.parent();
    while let Some(dir) = current {
        if dir == sessions_root || !dir.starts_with(&sessions_root) {
            break;
        }
        match fs::remove_dir(dir) {
            Ok(()) => current = dir.parent(),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
                ) =>
            {
                break;
            }
            Err(_) => break,
        }
    }
}
