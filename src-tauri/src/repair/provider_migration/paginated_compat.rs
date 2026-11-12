//! Narrow compatibility bridge for the independently tested Codex 0.167.1.
//! Magpie's paginated-row observation informed this independently written code.
//! Unlike a permanent DB-only retag, Core must resume and persist its own settings.
use super::*;

fn eligible(error: &AppError, from: &str, to: &str) -> bool {
    matches!(error, AppError::NativeProviderMismatch {
        method, native_version, model_provider, current_model_provider, ..
    } if method == "thread/resume"
        && native_version.as_deref() == Some("0.167.1")
        && model_provider == from && current_model_provider == to && from != to)
}

pub(super) fn prepare(
    codex: &Path,
    id: &str,
    path: &Path,
    from: &str,
    to: &str,
    error: &AppError,
    history: &HistoryStamp,
    metadata: &MetadataStamp,
) -> AppResult<bool> {
    if !eligible(error, from, to) {
        return Ok(false);
    }
    let physical_meta = crate::family::read_physical_session_meta(path)?;
    if physical_meta["payload"]["history_mode"] != "paginated" {
        // Legacy metadata is read from the rollout, not solely from this row.
        // Do not rewrite historical headers or guess at newer formats here.
        return Ok(false);
    }
    let state = state_db::open_ro(codex)?;
    if !threads_table_columns(&state)?
        .iter()
        .any(|c| c == "history_mode")
    {
        return Ok(false);
    }
    drop(state);
    // This also holds Core's coordination lock, including when no per-thread lock
    // file exists yet. A quiet but loaded thread must not be modified.
    let _writer = crate::rollout_io::writer_guard(path)?;
    verify_locked_identity(path, id)?;
    if history_stamp(path, None)? != *history
        || !metadata.matches(&metadata_stamp(codex, id, path)?)
        || read_rollout_brief(codex, path)?
            .and_then(|b| b.model_provider)
            .as_deref()
            != Some(from)
    {
        return Err(AppError::SessionBusy(id.into()));
    }
    compare_and_set(codex, id, path, from, to)?;
    Ok(true)
}

fn verify_locked_identity(path: &Path, id: &str) -> AppResult<()> {
    let meta = crate::family::read_physical_session_meta(path)?;
    if meta["payload"]["id"].as_str() != Some(id) || meta["payload"]["history_mode"] != "paginated"
    {
        return Err(AppError::Other(
            "持锁后的会话身份或历史格式发生变化，未更新服务商".into(),
        ));
    }
    Ok(())
}

fn compare_and_set(codex: &Path, id: &str, path: &Path, from: &str, to: &str) -> AppResult<()> {
    let mut state = state_db::open(codex)?;
    let transaction = state.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (mode, provider, recorded): (String, String, String) = transaction.query_row(
        "SELECT history_mode, model_provider, rollout_path FROM threads WHERE id=?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let recorded_path = paths::host_path_from_codex_record(codex, &recorded);
    let recorded_path = if recorded_path.is_absolute() {
        recorded_path
    } else {
        codex.join(recorded_path)
    };
    if mode != "paginated"
        || provider != from
        || !crate::rollout_io::same_file(&recorded_path, path)?
    {
        return Err(AppError::Other(
            "会话状态在兼容迁移前发生变化，尚未更新服务商".into(),
        ));
    }
    let changed = transaction.execute(
        "UPDATE threads SET model_provider=?1 WHERE id=?2 AND model_provider=?3 AND history_mode='paginated' AND rollout_path=?4",
        rusqlite::params![to, id, from, recorded],
    )?;
    if changed != 1 {
        return Err(AppError::Other(
            "兼容迁移未匹配唯一的原会话，尚未提交".into(),
        ));
    }
    transaction.commit()?;
    Ok(())
}

/// If Core rejected the resumed thread without persisting settings or history,
/// undo only our selected row. Never restore a whole database over another writer.
pub(super) fn rollback_unchanged(
    codex: &Path,
    id: &str,
    from: &str,
    to: &str,
    history: &HistoryStamp,
    metadata: &MetadataStamp,
) -> AppResult<()> {
    let path = current_path(codex, id, None)?;
    let _writer = crate::rollout_io::writer_guard(&path)?;
    verify_locked_identity(&path, id)?;
    if history_stamp(&path, None)? != *history
        || !metadata.matches(&metadata_stamp(codex, id, &path)?)
        || read_rollout_brief(codex, &path)?
            .and_then(|b| b.model_provider)
            .as_deref()
            != Some(from)
    {
        return Err(AppError::Other(
            "原生迁移已经写入设置或历史，未自动回退服务商".into(),
        ));
    }
    compare_and_set(codex, id, &path, to, from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_allows_first_title_but_protects_existing_title_and_name() {
        let mut before = MetadataStamp {
            rollout: vec![],
            database: BTreeMap::from([
                ("title".into(), Some(String::new())),
                ("name".into(), Some("user title".into())),
            ]),
            first_meta_sha256: Some("same".into()),
            owned_settings: Some(BTreeMap::new()),
        };
        let mut after = before.clone();
        after
            .database
            .insert("title".into(), Some("inherited preview".into()));
        assert!(before.matches(&after));
        after
            .database
            .insert("name".into(), Some("unexpected".into()));
        assert!(!before.matches(&after));
        after
            .database
            .insert("name".into(), Some("user title".into()));
        before
            .database
            .insert("title".into(), Some("existing preview".into()));
        assert!(!before.matches(&after));
    }

    #[test]
    fn fallback_requires_exact_version_and_structured_routing_error() {
        for (version, saved, current, expected) in [
            ("0.167.1", "source", "target", true),
            ("0.167.2", "source", "target", false),
            ("0.167.0-alpha.4.1", "source", "target", false),
            ("0.167.1-alpha.1", "source", "target", false),
            ("0.167.1", "other", "target", false),
            ("0.167.1", "source", "other", false),
        ] {
            let error = AppError::NativeProviderMismatch {
                method: "thread/resume".into(),
                message: "mismatch".into(),
                native_version: Some(version.into()),
                model_provider: saved.into(),
                current_model_provider: current.into(),
            };
            assert_eq!(eligible(&error, "source", "target"), expected);
        }
        assert!(!eligible(
            &AppError::Other("modelProviderMismatch".into()),
            "source",
            "target"
        ));
    }
}
