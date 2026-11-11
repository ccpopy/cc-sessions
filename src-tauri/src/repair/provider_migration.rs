//! Same-ID provider migration through Core. No edits to historical bytes or Desktop projects.
use super::*;
use crate::codex_app_server::ForkedThread;
use crate::models::ProviderDirs;
use sha2::{Digest, Sha256};

#[path = "provider_migration/paginated_compat.rs"]
mod paginated_compat;

fn current_path(codex: &Path, id: &str, hint: Option<&Path>) -> AppResult<PathBuf> {
    let states = read_thread_states_for_ids(codex, [id])?;
    let raw = states.get(id).and_then(|s| s.rollout_path.as_deref());
    let path = raw
        .map(|p| paths::host_path_from_codex_record(codex, p))
        .or_else(|| hint.map(Path::to_path_buf))
        .ok_or_else(|| {
            AppError::NotFound(format!("会话 {id} 缺少当前 rollout 路径；请先修复会话索引"))
        })?;
    let path = if path.is_absolute() {
        path
    } else {
        codex.join(path)
    };
    let path = crate::rollout_io::resolve(&path)?;
    crate::path_safety::validate_descendant(
        &paths::sessions_dir(codex),
        &path,
        crate::path_safety::EntryKind::File,
        false,
        "迁移源 rollout",
    )?;
    if read_rollout_identity(&path)?
        .as_ref()
        .map(|m| m.id.as_str())
        != Some(id)
    {
        return Err(AppError::Other(format!(
            "迁移源 rollout 身份与会话 {id} 不一致"
        )));
    }
    Ok(path)
}

fn verify_native(
    thread: &ForkedThread,
    id: &str,
    path: &Path,
    provider: Option<&str>,
) -> AppResult<()> {
    if thread.id != id
        || thread
            .path
            .as_ref()
            .map(|p| crate::rollout_io::same_file(p, path))
            .transpose()?
            != Some(true)
        || provider.is_some_and(|p| thread.model_provider.as_deref() != Some(p))
    {
        return Err(AppError::Other(
            "原生读取的会话身份、路径或服务商与迁移目标不一致".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct HistoryStamp {
    records: usize,
    sha256: String,
}

fn history_stamp(path: &Path, limit: Option<usize>) -> AppResult<HistoryStamp> {
    let history = crate::logical_history::read_view(path, None)?;
    let mut hash = Sha256::new();
    let mut records = 0;
    for value in history.records() {
        // Only native settings snapshots may be added by resume. All other records,
        // including media, tool calls, failed turns and inherited history, must survive.
        if value["type"] == "session_meta"
            || (value["type"] == "event_msg"
                && value["payload"]["type"] == "thread_settings_applied")
        {
            continue;
        }
        if limit == Some(records) {
            break;
        }
        hash.update(serde_json::to_vec(value)?);
        hash.update(b"\n");
        records += 1;
    }
    Ok(HistoryStamp {
        records,
        sha256: hex::encode(hash.finalize()),
    })
}

// Resume may add settings records, which history_stamp intentionally excludes.
// Check routing-independent metadata separately, including columns present on older clients.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct MetadataStamp {
    rollout: Vec<Option<String>>,
    database: BTreeMap<String, Option<String>>,
    #[serde(default)]
    first_meta_sha256: Option<String>,
    #[serde(default)]
    owned_settings: Option<BTreeMap<String, Value>>,
}

impl MetadataStamp {
    fn matches(&self, current: &Self) -> bool {
        let mut current = current.clone();
        // A newly forked thread can have no title yet. Core derives its first title
        // from inherited history on resume; no existing title is being replaced.
        // Explicit names and every previously nonempty title remain protected.
        if self
            .database
            .get("title")
            .is_some_and(|title| title.as_deref() == Some(""))
        {
            current.database.insert("title".into(), Some(String::new()));
        }
        // Older receipts did not capture these additional fields.
        if self.first_meta_sha256.is_none() {
            current.first_meta_sha256 = None;
        }
        if self.owned_settings.is_none() {
            current.owned_settings = None;
        }
        &current == self
    }
}

fn metadata_stamp(codex: &Path, id: &str, path: &Path) -> AppResult<MetadataStamp> {
    let brief = read_rollout_brief(codex, path)?
        .ok_or_else(|| AppError::Other("迁移源 rollout 缺少元数据".into()))?;
    let state = state_db::open_ro(codex)?;
    let columns = threads_table_columns(&state)?;
    let mut database = BTreeMap::new();
    for column in [
        "cwd",
        "source",
        "sandbox_policy",
        "approval_mode",
        "memory_mode",
        "title",
        "name",
        "archived",
        "archived_at",
        "created_at",
        // Core may normalize created_at_ms from the rollout header on resume.
        // Preserve the stable creation second instead of rejecting that rounding.
    ] {
        if columns.iter().any(|name| name == column) {
            let value = state.query_row(
                &format!("SELECT CAST({column} AS TEXT) FROM threads WHERE id=?1"),
                [id],
                |row| row.get::<_, Option<String>>(0),
            )?;
            database.insert(column.into(), value);
        }
    }
    let physical_meta = crate::family::read_physical_session_meta(path)?;
    let first_meta_sha256 = hex::encode(Sha256::digest(serde_json::to_vec(&physical_meta)?));
    let history = crate::logical_history::read_view(path, None)?;
    let mut header_id = None;
    let mut owned_settings = BTreeMap::new();
    for record in history.records() {
        let payload = &record["payload"];
        if record["type"] == "session_meta" {
            header_id = payload["id"].as_str();
        } else if record["type"] == "event_msg"
            && payload["type"] == "thread_settings_applied"
            && payload["thread_id"].as_str().or(header_id) == Some(id)
        {
            // Each event is a settings snapshot. Compare only these known routing-
            // independent fields, including nested permission objects, not model choices.
            owned_settings.clear();
            for field in [
                "cwd",
                "runtime_workspace_roots",
                "approval_policy",
                "approvals_reviewer",
                "permission_profile",
                "active_permission_profile",
                "sandbox_policy",
                "memory_mode",
            ] {
                if let Some(value) = payload["thread_settings"].get(field) {
                    owned_settings.insert(field.into(), value.clone());
                }
            }
        }
    }
    Ok(MetadataStamp {
        first_meta_sha256: Some(first_meta_sha256),
        owned_settings: Some(owned_settings),
        rollout: vec![
            brief.cwd,
            brief.source,
            brief.sandbox_policy,
            brief.approval_mode,
            brief.memory_mode,
        ],
        database,
    })
}

fn receipt(
    backup: &Path,
    id: &str,
    from: &str,
    to: &str,
    status: &str,
    history: &HistoryStamp,
    metadata: Option<&MetadataStamp>,
) -> AppResult<()> {
    atomic_file::overwrite_with_writer(&backup.join("provider-migration.json"), |file| {
        serde_json::to_writer_pretty(
            file,
            &serde_json::json!({
                "thread_id": id, "from_provider": from, "to_provider": to, "status": status,
                "history": history, "metadata": metadata
            }),
        )?;
        Ok(())
    })
}

/// A killed process can leave a durable prepared receipt. Reconcile only this thread's
/// receipts before another write; appended turns are allowed when the original prefix survives.
fn reconcile_interrupted(codex: &Path, id: &str, path: &Path, provider: &str) -> AppResult<()> {
    let root = codex.join("backups/provider-migrations");
    if !root.is_dir() {
        return Ok(());
    }
    let prefix = format!("migration-{id}-");
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let backup = entry.path();
        let record_path = backup.join("provider-migration.json");
        if !record_path.is_file() {
            continue;
        }
        crate::path_safety::validate_descendant(
            &root,
            &record_path,
            crate::path_safety::EntryKind::File,
            false,
            "迁移恢复记录",
        )?;
        let record: Value = serde_json::from_slice(&fs::read(&record_path)?)?;
        if record["thread_id"].as_str() != Some(id)
            || !matches!(
                record["status"].as_str(),
                Some("prepared" | "needs_recovery")
            )
        {
            continue;
        }
        let expected: HistoryStamp = serde_json::from_value(record["history"].clone())?;
        let metadata: Option<MetadataStamp> = record
            .get("metadata")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value)
            .transpose()?;
        let from = record["from_provider"]
            .as_str()
            .ok_or_else(|| AppError::Other("迁移恢复记录缺少原服务商".into()))?;
        let to = record["to_provider"]
            .as_str()
            .ok_or_else(|| AppError::Other("迁移恢复记录缺少目标服务商".into()))?;
        let persisted = read_rollout_brief(codex, path)?.and_then(|b| b.model_provider);
        if history_stamp(path, Some(expected.records))? != expected
            || persisted.as_deref() != Some(provider)
            || metadata.as_ref().is_some_and(|expected| {
                let Ok(current) = metadata_stamp(codex, id, path) else {
                    return true;
                };
                !expected.matches(&current)
            })
            || (provider != from && provider != to)
        {
            return Err(AppError::Other(format!("[needs_recovery] 会话 {id} 上次迁移尚未核对完成，原历史、服务商或会话元数据与恢复记录不一致；请先核对迁移前备份：{}", backup.display())));
        }
        receipt(
            &backup,
            id,
            from,
            to,
            if provider == to {
                "completed"
            } else {
                "failed_unchanged"
            },
            &expected,
            metadata.as_ref(),
        )?;
    }
    Ok(())
}

fn sync_local_metadata(codex: &Path, id: &str, provider: &str, path: &Path) -> AppResult<()> {
    if !read_session_index_ids(codex)?.contains(id) {
        let state = state_db::open_ro(codex)?;
        let title: String =
            state.query_row("SELECT title FROM threads WHERE id=?1", [id], |row| {
                row.get(0)
            })?;
        append_index_line(codex, id, &title, path)?;
    }
    let mut store = family::load(codex)?;
    if let Some(family_id) = store.index.get(id).cloned() {
        if let Some(branch) = store
            .families
            .get_mut(&family_id)
            .and_then(|f| f.chain.iter_mut().find(|b| b.id == id))
        {
            let relative = paths::rollout_relpath(&path.to_string_lossy(), codex)
                .to_string_lossy()
                .into_owned();
            if branch.provider != provider || branch.rollout_relpath != relative {
                branch.provider = provider.into();
                branch.rollout_relpath = relative;
                family::save(codex, &store)?;
            }
        }
    }
    Ok(())
}

pub(super) fn migrate(
    codex: &Path,
    id: &str,
    target: Option<String>,
    dry_run: bool,
    hint: Option<&Path>,
    native: &mut dyn ProviderThreadForker,
    observed: Option<ProviderSyncActivity>,
) -> AppResult<CloneReport> {
    let provider = target.unwrap_or(effective_current_provider(codex)?);
    let provider = provider.trim();
    if provider.is_empty() {
        return Err(AppError::Other("目标 provider 不能为空".into()));
    }
    let source = current_path(codex, id, hint)?;
    let mut report = CloneReport {
        source_id: id.into(),
        new_id: Some(id.into()),
        new_rollout_path: Some(source.to_string_lossy().into_owned()),
        new_provider: provider.into(),
        backup_path: None,
        desktop_restart_required: false,
        ok: false,
        skipped_reason: None,
        error: None,
    };
    if dry_run {
        report.ok = true;
        report.skipped_reason = Some("预览：备份后原地迁移，保留会话 ID 和历史，不新增分支".into());
        return Ok(report);
    }

    let before = native.read_provider_thread(id)?;
    verify_native(&before, id, &source, None)?;
    let states = read_thread_states_for_ids(codex, [id])?;
    let original_provider = states
        .get(id)
        .and_then(|state| state.model_provider.as_deref())
        .ok_or_else(|| AppError::Other("原生读取未返回当前服务商，尚未修改会话".into()))?;
    reconcile_interrupted(codex, id, &source, original_provider)?;
    // A DB-only retag can be undone by index repair. Require durable rollout agreement,
    // while allowing legacy thread/read to continue reporting the creation provider.
    let persisted_provider = read_rollout_brief(codex, &source)?.and_then(|b| b.model_provider);
    if original_provider == provider
        && persisted_provider.as_deref() == Some(provider)
        && read_session_index_ids(codex)?.contains(id)
    {
        sync_local_metadata(codex, id, provider, &source)?;
        native.close_provider_thread();
        report.ok = true;
        report.skipped_reason = Some("原会话已使用目标服务商".into());
        return Ok(report);
    }
    let guard = match observed {
        Some(ProviderSyncActivity::Observed(guard)) => {
            guard.ensure_unchanged()?;
            guard
        }
        _ => crate::codex_activity::SessionActivityGuard::observe(
            codex,
            vec![(id.into(), vec![source.clone()])],
        )?,
    };
    let history_before = history_stamp(&source, None)?;
    let metadata_before = metadata_stamp(codex, id, &source)?;
    let backup = crate::backup::create_backup_with_dirs(
        Some("codex".into()),
        ProviderDirs::new(codex.to_string_lossy().into_owned()),
        codex
            .join("backups/provider-migrations")
            .to_string_lossy()
            .into_owned(),
        vec![id.into()],
        None,
        Some(format!("migration-{id}-{}", new_session_id())),
        Some(format!(
            "原地迁移服务商：{original_provider} → {provider}；会话 {id}"
        )),
    )?;
    let backup_path = Path::new(&backup.path);
    report.backup_path = Some(backup.path.clone());
    let prepared = (|| -> AppResult<()> {
        receipt(
            backup_path,
            id,
            original_provider,
            provider,
            "prepared",
            &history_before,
            Some(&metadata_before),
        )?;
        guard.ensure_unchanged()?;
        if history_stamp(&source, None)? != history_before
            || !metadata_before.matches(&metadata_stamp(codex, id, &source)?)
        {
            return Err(AppError::SessionBusy(id.into()));
        }
        Ok(())
    })();
    if let Err(error) = prepared {
        report.error = Some(format!("尚未执行迁移：{error}；备份：{}", backup.path));
        return Ok(report);
    }

    let mut resumed = native.resume_provider_thread(id, provider);
    let mut compatibility_staged = false;
    if let Err(error) = &resumed {
        // This client must relinquish any native state before acquiring Core's
        // publication lock. Only the verified routing mismatch can stage a row.
        native.close_provider_thread();
        match paginated_compat::prepare(
            codex,
            id,
            &source,
            original_provider,
            provider,
            error,
            &history_before,
            &metadata_before,
        ) {
            Ok(true) => {
                compatibility_staged = true;
                resumed = native.resume_provider_thread(id, provider);
            }
            Ok(false) => {}
            Err(error) => resumed = Err(error),
        }
    }
    // Close our writer before independently reopening; do not keep a routing service running.
    native.close_provider_thread();
    let verification = (|| -> AppResult<PathBuf> {
        let path = current_path(codex, id, None)?;
        if let Ok(result) = &resumed {
            verify_native(result, id, &path, Some(provider))?;
        }
        let reopened = native.read_provider_thread(id)?;
        // Legacy thread/read can expose the creation provider even after a successful
        // resume. Verify identity here, and current provider against persisted settings.
        verify_native(&reopened, id, &path, None)?;
        let states = read_thread_states_for_ids(codex, [id])?;
        if states.get(id).and_then(|s| s.model_provider.as_deref()) != Some(provider) {
            return Err(AppError::Other(
                "原生迁移后的 threads 服务商尚未持久化".into(),
            ));
        }
        if read_rollout_brief(codex, &path)?
            .and_then(|b| b.model_provider)
            .as_deref()
            != Some(provider)
        {
            return Err(AppError::Other("原生历史未保存目标服务商设置".into()));
        }
        if !metadata_before.matches(&metadata_stamp(codex, id, &path)?) {
            return Err(AppError::Other(
                "迁移期间会话工作目录、权限或元数据发生变化".into(),
            ));
        }
        if history_stamp(&path, None)? != history_before {
            return Err(AppError::Other(
                "迁移期间历史记录发生变化，未自动覆盖或回滚新记录".into(),
            ));
        }
        Ok(path)
    })();
    native.close_provider_thread();
    match verification {
        Ok(path) => {
            // Update an existing branch label only. A plain thread never gains a family here.
            report.new_rollout_path = Some(path.to_string_lossy().into_owned());
            let finalized = (|| -> AppResult<()> {
                sync_local_metadata(codex, id, provider, &path)?;
                receipt(
                    backup_path,
                    id,
                    original_provider,
                    provider,
                    "completed",
                    &history_before,
                    Some(&metadata_before),
                )
            })();
            match finalized {
                Ok(()) => report.ok = true,
                Err(error) => {
                    report.error = Some(format!(
                        "原会话已迁移，但本地迁移记录保存失败：{error}；备份：{}",
                        backup.path
                    ))
                }
            }
        }
        Err(check_error) => {
            if compatibility_staged {
                // If anything has changed, this refuses the rollback and the
                // durable receipt below remains needs_recovery with its backup.
                let _ = paginated_compat::rollback_unchanged(
                    codex,
                    id,
                    original_provider,
                    provider,
                    &history_before,
                    &metadata_before,
                );
            }
            let unchanged = current_path(codex, id, None).ok().is_some_and(|path| {
                history_stamp(&path, None).ok().as_ref() == Some(&history_before)
                    && metadata_stamp(codex, id, &path)
                        .ok()
                        .is_some_and(|current| metadata_before.matches(&current))
                    && read_thread_states_for_ids(codex, [id])
                        .ok()
                        .and_then(|s| s.get(id).and_then(|s| s.model_provider.clone()))
                        .as_deref()
                        == Some(original_provider)
                    && read_rollout_brief(codex, &path)
                        .ok()
                        .flatten()
                        .and_then(|b| b.model_provider)
                        .as_deref()
                        == Some(original_provider)
            });
            let status = if unchanged {
                "failed_unchanged"
            } else {
                "needs_recovery"
            };
            let cause = resumed.err().unwrap_or(check_error);
            let action = if unchanged {
                if cause.to_string().contains("active writer") {
                    "未修改历史。请在 Codex App 中关闭这条会话后重试；若仍被占用，请退出 Codex 后重试。"
                } else {
                    "原服务商和历史保持不变；解决上述原因后可重新迁移。"
                }
            } else {
                "请停止该会话写入后核对；需要恢复时可从迁移前备份恢复。"
            };
            report.error = Some(format!(
                "[{status}] {cause}；{action} 迁移前备份：{}",
                backup.path
            ));
            if let Err(error) = receipt(
                backup_path,
                id,
                original_provider,
                provider,
                status,
                &history_before,
                Some(&metadata_before),
            ) {
                report.error = Some(format!(
                    "{} 迁移状态写入失败：{error}",
                    report.error.unwrap_or_default()
                ));
            }
        }
    }
    Ok(report)
}
