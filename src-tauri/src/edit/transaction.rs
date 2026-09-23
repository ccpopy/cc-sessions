//! Persist the intended change before replacing a rollout. Recovery never overwrites history.
use super::*;
use crate::models::EditPendingOperation;

pub(super) fn lines_hash(lines: &[String], trailing_newline: bool) -> String {
    let mut hash = Sha256::new();
    for (i, line) in lines.iter().enumerate() {
        hash.update(line.as_bytes());
        if i + 1 < lines.len() || trailing_newline {
            hash.update(b"\n");
        }
    }
    hex::encode(hash.finalize())
}

pub(super) fn snapshot(path: &Path, loaded: &LoadedFile) -> AppResult<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    atomic_file::create_with_writer_if_absent(path, |file| {
        for (i, line) in loaded.lines.iter().enumerate() {
            file.write_all(line.as_bytes())?;
            if i + 1 < loaded.lines.len() || loaded.trailing_newline {
                file.write_all(b"\n")?;
            }
        }
        Ok(())
    })
}

pub(super) fn commit(
    ctx: &OpContext,
    entry: &JournalEntry,
    lines: &[String],
    trailing_newline: bool,
) -> AppResult<Option<String>> {
    let pending = ctx.dir.join("pending-operation.json");
    atomic_file::create_with_writer_if_absent(&pending, |file| {
        serde_json::to_writer(file, entry)?;
        Ok(())
    })?;
    if let Err(error) = write_lines(&ctx.path, lines, trailing_newline, &ctx.loaded.hash) {
        if error.atomic_write_not_committed() {
            fs::remove_file(&pending)?;
            return Err(error);
        }
        return Err(AppError::Other(format!("[EDIT_RECOVERY_REQUIRED] 操作 {} 提交状态待核对：{error}。请打开编辑历史核对，不要重复执行。", entry.op_id)));
    }
    #[cfg(test)]
    if FAIL_AFTER_ROLLOUT.with(|flag| flag.replace(false)) {
        return Ok(Some(
            "会话已写入，编辑日志尚未提交；请核对提交状态。".into(),
        ));
    }
    if let Err(error) = append_journal(&ctx.dir, entry) {
        return Ok(Some(format!(
            "会话已写入，编辑日志提交失败：{error}。操作清单已保留，请核对提交状态。"
        )));
    }
    if let Err(error) = fs::remove_file(&pending) {
        return Ok(Some(format!(
            "会话和编辑日志已保存，操作清单清理失败：{error}。请核对提交状态。"
        )));
    }
    Ok(None)
}

fn pending(dir: &Path) -> AppResult<Option<JournalEntry>> {
    match fs::read(dir.join("pending-operation.json")) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn pending_summary(dir: &Path, path: &Path) -> AppResult<Option<EditPendingOperation>> {
    let Some(entry) = pending(dir)? else {
        return Ok(None);
    };
    let hash = atomic_file::fingerprint(path)?.sha256_hex();
    let same_path = Path::new(&entry.rollout_path).canonicalize()? == path.canonicalize()?;
    let status = if same_path && hash == entry.after_hash {
        "committed_pending_journal"
    } else if same_path && hash == entry.before_hash {
        "not_committed"
    } else {
        "conflict"
    };
    Ok(Some(EditPendingOperation {
        op_id: entry.op_id,
        status: status.into(),
        description: entry.description,
        can_reconcile: status != "conflict",
    }))
}

pub(super) fn reconcile(
    provider: &str,
    path: &Path,
    id: &str,
    dir: &Path,
    expected: Option<&str>,
) -> AppResult<()> {
    let loaded = load_file(path)?;
    safety::check_revision(path, &loaded, expected)?;
    let Some(entry) = pending(dir)? else {
        return Ok(());
    };
    if entry.provider != provider
        || entry.session_id != id
        || Path::new(&entry.rollout_path).canonicalize()? != path.canonicalize()?
    {
        return Err(AppError::Other(
            "[EDIT_IDENTITY] 操作清单与目标会话不一致".into(),
        ));
    }
    if loaded.hash == entry.after_hash {
        append_journal(dir, &entry)?;
    } else if loaded.hash != entry.before_hash {
        return Err(AppError::Other(
            "[EDIT_CONFLICT] 未完成操作后文件又有变化，已保留操作清单与快照；请在独立副本中核对。"
                .into(),
        ));
    }
    fs::remove_file(dir.join("pending-operation.json"))?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    pub(super) static FAIL_AFTER_ROLLOUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
