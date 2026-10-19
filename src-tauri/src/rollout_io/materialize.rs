//! Convert a physical representation without changing any logical history bytes.
use super::*;
use crate::atomic_file;
use crate::error::AppError;
use crate::mutation_journal::{self, MutationJournal};
use crate::path_safety::{validate_descendant, EntryKind};
use rusqlite::OptionalExtension;

pub(crate) fn materialize_for_write(path: &Path) -> AppResult<PathBuf> {
    let physical = resolve(path)?;
    let plain = plain_path(&physical);
    let compressed = plain.with_extension("jsonl.zst");
    if !compressed.try_exists()? {
        return Ok(plain);
    }
    let root =
        codex_root(&physical).ok_or_else(|| AppError::Path("无法确定压缩会话的数据根".into()))?;
    validate_descendant(root, &compressed, EntryKind::File, false, "压缩日志源")?;
    validate_descendant(root, &plain, EntryKind::File, true, "解压日志目标")?;
    let _guard = writer_guard(&physical)?;
    let source = atomic_file::fingerprint(&compressed)?;
    let meta = crate::family::read_physical_session_meta(&physical)?;
    let id = meta["payload"]["id"]
        .as_str()
        .ok_or_else(|| AppError::Other("压缩会话缺少身份".into()))?;
    let family_path = crate::paths::family_store_path(root);
    let family_expected = family_path
        .try_exists()?
        .then(|| atomic_file::fingerprint(&family_path))
        .transpose()?;
    let mut store = crate::family::load(root)?;
    let state = crate::paths::state_db_path(root)
        .is_file()
        .then(|| crate::state_db::open(root))
        .transpose()?;
    let transaction = state
        .as_ref()
        .map(|state| {
            rusqlite::Transaction::new_unchecked(state, rusqlite::TransactionBehavior::Immediate)
        })
        .transpose()?;
    let recovery = root
        .join("backups/rollout-materializations")
        .join(format!("{id}-{}", crate::repair::new_session_id()));
    validate_descendant(
        root,
        &recovery,
        EntryKind::Directory,
        true,
        "日志转换恢复目录",
    )?;
    fs::create_dir_all(&recovery)?;
    let original = recovery.join(
        compressed
            .file_name()
            .ok_or_else(|| AppError::Path("压缩日志缺少文件名".into()))?,
    );
    let receipt = recovery.join("materialization.json");
    let write_receipt = |status: &str| {
        atomic_file::overwrite_with_writer(&receipt, |out| {
            serde_json::to_writer_pretty(
                out,
                &serde_json::json!({
                    "status": status, "thread_id": id, "source": compressed, "plain": plain,
                    "source_sha256": source.sha256_hex(), "original": original,
                }),
            )?;
            Ok(())
        })
    };
    write_receipt("prepared")?;
    let mut journal = MutationJournal::default();
    let result = (|| -> AppResult<()> {
        if !plain.try_exists()? {
            let permissions = fs::metadata(&compressed)?.permissions();
            journal.mutate_file(&plain, || {
                atomic_file::create_with_writer_if_absent(&plain, |out| {
                    out.set_permissions(permissions)?;
                    io::copy(&mut reader_exact(&compressed)?, out)?;
                    if atomic_file::fingerprint(&compressed)? != source {
                        return Err(AppError::AtomicWriteConflict(
                            "压缩日志在解压期间变化，已取消操作".into(),
                        ));
                    }
                    inject_fault("before_publish")?;
                    Ok(())
                })
            })?;
        }
        inject_fault("after_publish")?;
        if let Some(tx) = &transaction {
            let recorded: Option<String> = tx
                .query_row("SELECT rollout_path FROM threads WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .optional()?
                .flatten();
            if let Some(recorded) = recorded {
                let recorded_path = crate::paths::host_path_from_codex_record(root, &recorded);
                let recorded_path = if recorded_path.is_absolute() {
                    recorded_path
                } else {
                    root.join(recorded_path)
                };
                if same_file(&recorded_path, &plain).unwrap_or(false) {
                    // Preserve Core's path convention (including Linux paths accessed via WSL).
                    let next = recorded
                        .strip_suffix(".zst")
                        .filter(|_| recorded.ends_with(".jsonl.zst"))
                        .unwrap_or(&recorded);
                    tx.execute(
                        "UPDATE threads SET rollout_path=?1 WHERE id=?2 AND rollout_path=?3",
                        rusqlite::params![next, id, recorded],
                    )?;
                }
            }
        }
        let mut changed = false;
        for family in store.families.values_mut() {
            for branch in &mut family.chain {
                let relative = crate::paths::checked_relative_path(&branch.rollout_relpath)?;
                if same_file(&root.join(&relative), &plain).unwrap_or(false) {
                    let next = crate::paths::rollout_relpath(&plain.to_string_lossy(), root)
                        .to_string_lossy()
                        .into_owned();
                    if branch.rollout_relpath != next {
                        branch.rollout_relpath = next;
                        changed = true;
                    }
                }
            }
        }
        if changed {
            journal.mutate_file(&family_path, || {
                if family_expected.as_ref() != Some(&atomic_file::fingerprint(&family_path)?) {
                    return Err(AppError::AtomicWriteConflict(
                        "分支记录在日志转换期间变化，已取消操作".into(),
                    ));
                }
                crate::family::save(root, &store)
            })?;
        }
        if atomic_file::fingerprint(&compressed)? != source {
            return Err(AppError::AtomicWriteConflict(
                "压缩日志在转换期间变化，已取消操作".into(),
            ));
        }
        journal.move_file(&compressed, &original)?;
        if atomic_file::fingerprint(&original)? != source {
            return Err(AppError::AtomicWriteConflict(
                "压缩日志在保存恢复副本时变化".into(),
            ));
        }
        inject_fault("after_retire")?;
        if compressed.try_exists()? {
            return Err(AppError::AtomicWriteConflict(
                "压缩日志在转换期间被重新创建，已取消操作".into(),
            ));
        }
        Ok(())
    })();
    let result = match (result, transaction) {
        (Ok(()), Some(tx)) => mutation_journal::commit_transaction_with_compensation(tx, journal),
        (Err(error), Some(tx)) => Err(mutation_journal::rollback_transaction_with_compensation(
            tx, journal, error,
        )),
        (Ok(()), None) => journal.finalize(),
        (Err(error), None) => Err(journal.compensate_without_transaction(error)),
    };
    if let Err(error) = result {
        let _ = write_receipt("failed");
        return Err(AppError::Other(format!(
            "{error}；日志转换恢复记录：{}",
            receipt.display()
        )));
    }
    write_receipt("completed")?;
    Ok(plain)
}

#[cfg(test)]
thread_local! {
    static FAULT: std::cell::RefCell<Option<(&'static str, Box<dyn FnOnce() -> AppResult<()>>)>> = const { std::cell::RefCell::new(None) };
}

fn inject_fault(stage: &'static str) -> AppResult<()> {
    #[cfg(test)]
    if let Some(fault) = FAULT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|(name, _)| *name == stage) {
            slot.take()
        } else {
            None
        }
    }) {
        return fault.1();
    }
    let _ = stage;
    Ok(())
}

#[cfg(test)]
pub(super) fn during(stage: &'static str, hook: impl FnOnce() -> AppResult<()> + 'static) {
    FAULT.with(|slot| *slot.borrow_mut() = Some((stage, Box::new(hook))));
}
