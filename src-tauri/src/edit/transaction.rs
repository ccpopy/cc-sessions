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

pub(super) fn save_projection_snapshot(
    dir: &Path,
    name: &str,
    history: &paginated::HistoryImage,
) -> AppResult<()> {
    atomic_file::create_with_writer_if_absent(&dir.join(format!("{name}.history.json")), |file| {
        serde_json::to_writer(file, history)?;
        Ok(())
    })
}

pub(super) fn commit(
    ctx: &OpContext,
    entry: &JournalEntry,
    lines: &[String],
    trailing_newline: bool,
) -> AppResult<Option<String>> {
    let _writer_guard = if entry.history.is_some() {
        Some(paginated::writer_guard(&ctx.path)?)
    } else {
        None
    };
    if entry.history.is_some() {
        // A descendant may have been created after planning. Recheck affected
        // history while native publication is excluded by its coordination lock.
        paginated::validate_change(
            &ctx.path,
            &ctx.loaded,
            &paginated::from_lines(lines, trailing_newline),
        )?;
    }
    let db = entry
        .history
        .as_ref()
        .map(|history| history.begin(&entry.session_id))
        .transpose()?;
    if let (Some(db), Some(history)) = (&db, &entry.history) {
        paginated::projection::replace(db, &entry.session_id, &history.after)?;
    }
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
    if let Some(db) = &db {
        if let Err(error) = db.execute_batch("COMMIT") {
            return Ok(Some(format!(
                "会话已写入，原生历史提交未完成：{error}；请核对提交状态。"
            )));
        }
    }
    #[cfg(test)]
    if FAIL_AFTER_PROJECTION.with(|flag| flag.replace(false)) {
        return Ok(Some(
            "投影已提交，编辑日志尚未保存；请核对提交状态。".into(),
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
    let projection_status = entry
        .history
        .as_ref()
        .map(|history| -> AppResult<(bool, bool)> {
            validate_history_path(history, path)?;
            let db = paginated::projection::open(&history.path, false)?;
            let current = paginated::projection::capture(&db, &entry.session_id)?;
            Ok((current == history.before, history.can_reconcile(&current)))
        })
        .transpose();
    let status = if projection_status.is_err()
        || projection_status
            .as_ref()
            .is_ok_and(|state| state.is_some_and(|(before, after)| !before && !after))
    {
        "conflict"
    } else if same_path && hash == entry.after_hash {
        "committed_pending_journal"
    } else if same_path
        && hash == entry.before_hash
        && projection_status
            .ok()
            .flatten()
            .is_none_or(|(before, _)| before)
    {
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
    if let Some(history) = &entry.history {
        validate_history_path(history, path)?;
    }
    if loaded.hash == entry.after_hash {
        if let Some(history) = &entry.history {
            let _guard = paginated::writer_guard(path)?;
            let db = paginated::projection::open(&history.path, true)?;
            db.execute_batch("BEGIN IMMEDIATE")?;
            let current = paginated::projection::capture(&db, id)?;
            if history.can_reconcile(&current) {
                paginated::projection::replace(&db, id, &history.after)?;
            } else {
                return Err(AppError::Other(
                    "[EDIT_CONFLICT] 原生历史已有外部修改，保留操作清单；未执行覆盖".into(),
                ));
            }
            db.execute_batch("COMMIT")?;
        }
        append_journal(dir, &entry)?;
    } else if loaded.hash == entry.before_hash {
        if let Some(history) = &entry.history {
            let db = paginated::projection::open(&history.path, false)?;
            if paginated::projection::capture(&db, id)? != history.before {
                return Err(AppError::Other(
                    "[EDIT_CONFLICT] 日志尚未提交但原生投影已有变化，已保留操作清单".into(),
                ));
            }
        }
    } else {
        return Err(AppError::Other(
            "[EDIT_CONFLICT] 未完成操作后文件又有变化，已保留操作清单与快照；请在独立副本中核对。"
                .into(),
        ));
    }
    fs::remove_file(dir.join("pending-operation.json"))?;
    Ok(())
}

fn validate_history_path(history: &paginated::HistoryChange, rollout: &Path) -> AppResult<()> {
    if history.path.canonicalize()? != paginated::projection::path(rollout)?.canonicalize()? {
        return Err(AppError::Other(
            "[EDIT_IDENTITY] 历史投影路径不属于目标数据根".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    pub(super) static FAIL_AFTER_ROLLOUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(super) static FAIL_AFTER_PROJECTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(super) static FAIL_ROLLOUT_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
