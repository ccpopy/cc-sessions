//! Durable commit intent for one OpenCode session; reconciliation never rewrites the database.
use super::*;
use crate::models::EditPendingOperation;

const PENDING: &str = "pending-operation.json";

#[derive(Serialize, Deserialize)]
struct Pending {
    version: u32,
    status: String,
    entry: JournalEntry,
}

fn read(dir: &Path) -> AppResult<Option<Pending>> {
    match fs::read(dir.join(PENDING)) {
        Ok(bytes) => {
            let pending: Pending = serde_json::from_slice(&bytes)?;
            if pending.version != 1 {
                return Err(AppError::Other("[EDIT_RECOVERY] 操作清单版本不支持".into()));
            }
            Ok(Some(pending))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn ensure_clear(dir: &Path) -> AppResult<()> {
    if dir.join(PENDING).try_exists()? {
        return Err(AppError::Other(
            "[EDIT_RECOVERY] 有尚未核对的 OpenCode 提交，请在编辑历史中核对后再修改".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
fn process_checkpoint(phase: &str) {
    if std::env::var("CC_TEST_OPENCODE_EXIT").as_deref() == Ok(phase) {
        std::process::exit(73);
    }
}

pub(super) fn commit(
    tx: Transaction<'_>,
    dir: &Path,
    journal: &mut JournalFile,
    entry: JournalEntry,
) -> AppResult<Option<String>> {
    let mut pending = Pending {
        version: 1,
        status: "prepared".into(),
        entry,
    };
    write_json_absent(&dir.join(PENDING), &pending).map_err(|error| {
        if error.atomic_write_committed() {
            AppError::Other(format!(
                "[EDIT_RECOVERY] 操作清单已写入但清理未完成；数据库未提交：{error}"
            ))
        } else {
            error
        }
    })?;
    #[cfg(test)]
    process_checkpoint("prepared");
    if let Err(error) = tx.commit() {
        return Err(AppError::Other(format!(
            "[EDIT_RECOVERY] OpenCode 数据库提交结果待核对，操作清单已保留：{error}"
        )));
    }
    #[cfg(test)]
    process_checkpoint("committed");
    pending.status = "committed".into();
    let finish = (|| -> AppResult<()> {
        replace_json(&dir.join(PENDING), &pending)?;
        append_entry(dir, journal, pending.entry)?;
        #[cfg(test)]
        process_checkpoint("journaled");
        fs::remove_file(dir.join(PENDING))?;
        Ok(())
    })();
    Ok(finish.err().map(|error| format!("[EDIT_RECOVERY] 本地修改已提交，编辑记录或清理未完成：{error}。请核对提交状态，不要重复操作。")))
}

fn state(dir: &Path, current: &SessionSnapshot, entry: &JournalEntry) -> AppResult<&'static str> {
    let (database, session) = resolve_context(&entry.rollout_path, &entry.session_id)?;
    if session != current.session_id
        || database.canonicalize()? != Path::new(&current.database_path).canonicalize()?
    {
        return Ok("conflict");
    }
    for (relative, hash) in [
        (&entry.before_snapshot, &entry.before_hash),
        (&entry.after_snapshot, &entry.after_hash),
    ] {
        let snapshot = read_snapshot(&safe_relative_snapshot_path(dir, relative)?)?;
        if snapshot.session_id != session
            || snapshot.hash != *hash
            || Path::new(&snapshot.database_path).canonicalize()? != database.canonicalize()?
        {
            return Ok("conflict");
        }
    }
    let journal = read_journal(dir)?;
    if journal
        .entries
        .iter()
        .any(|e| e.op_id == entry.op_id && e != entry)
    {
        return Ok("conflict");
    }
    if current.hash == entry.after_hash {
        Ok("committed_pending_journal")
    } else if current.hash == entry.before_hash
        && !journal.entries.iter().any(|e| e.op_id == entry.op_id)
    {
        Ok("not_committed")
    } else {
        Ok("conflict")
    }
}

pub(super) fn summary(
    dir: &Path,
    current: &SessionSnapshot,
) -> AppResult<Option<EditPendingOperation>> {
    let Some(pending) = read(dir)? else {
        return Ok(None);
    };
    let status = state(dir, current, &pending.entry)?;
    Ok(Some(EditPendingOperation {
        op_id: pending.entry.op_id,
        description: pending.entry.description,
        status: status.into(),
        can_reconcile: status != "conflict",
    }))
}

pub fn reconcile(locator: &str, id: &str, backup: &str, expected: Option<&str>) -> AppResult<()> {
    let (database, session) = resolve_context(locator, id)?;
    let mut connection = open_writable(&database)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = load_snapshot(&tx, &database, &session)?;
    check_revision(&current, expected)?;
    let dir = edit_dir(backup, id);
    let Some(pending) = read(&dir)? else {
        return Ok(());
    };
    match state(&dir, &current, &pending.entry)? {
        "not_committed" => (),
        "committed_pending_journal" => append_entry(&dir, &mut read_journal(&dir)?, pending.entry)?,
        _ => {
            return Err(AppError::Other(
                "[EDIT_CONFLICT] OpenCode 已有外部变更或恢复身份不匹配，保留操作清单；未覆盖数据库"
                    .into(),
            ))
        }
    }
    fs::remove_file(dir.join(PENDING))?;
    Ok(())
}
