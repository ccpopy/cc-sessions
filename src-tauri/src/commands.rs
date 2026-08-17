//! 桌面端 Tauri 命令的异步包装层。
//!
//! Tauri 2 的同步命令在主线程上执行，任何耗时 IO（扫描 rollout、读写 SQLite、
//! 备份/导入导出）都会冻结整个窗口。这里统一把业务逻辑丢进 `spawn_blocking`
//! 线程池，命令本身保持 async，主线程只做参数转发。
//!
//! 业务实现仍在各自模块（`*_with_lock` / 同名同步函数），CLI 与 WebUI 继续
//! 直接调用同步版本；本模块仅在 desktop feature 下编译。

use std::path::PathBuf;
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::family::*;
use crate::fs_ops::*;
use crate::models::*;

type SharedLock<'a> = tauri::State<'a, Arc<FamilyLock>>;

async fn run_blocking<T, F>(task: F) -> AppResult<T>
where
    F: FnOnce() -> AppResult<T> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(task)
        .await
        .map_err(|error| AppError::Other(format!("后台任务执行失败: {error}")))?
}

#[tauri::command]
pub fn start_content_search(
    provider: String,
    codex_dir: String,
    claude_dir: String,
    opencode_dir: String,
    query: String,
    rollout_paths: Vec<String>,
) -> AppResult<ContentSearchStart> {
    crate::content_search::start_content_search(
        provider,
        codex_dir,
        claude_dir,
        opencode_dir,
        query,
        rollout_paths,
    )
}

#[tauri::command]
pub fn content_search_status(job_id: u64) -> AppResult<ContentSearchStatus> {
    crate::content_search::content_search_status(job_id)
}

#[tauri::command]
pub fn active_content_search() -> AppResult<Option<ContentSearchStart>> {
    crate::content_search::active_content_search()
}

#[tauri::command]
pub fn cancel_content_search(job_id: u64) -> AppResult<()> {
    crate::content_search::cancel_content_search(job_id)
}

#[tauri::command]
pub async fn create_backup(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    backup_dir: String,
    ids: Vec<String>,
    targets: Option<Vec<BundleExportTarget>>,
    name: Option<String>,
    note: Option<String>,
) -> AppResult<BackupSummary> {
    run_blocking(move || {
        crate::backup::create_backup_with_opencode(
            provider,
            codex_dir,
            claude_dir,
            opencode_dir,
            backup_dir,
            ids,
            targets,
            name,
            note,
        )
    })
    .await
}

#[tauri::command]
pub async fn list_backups(
    backup_dir: String,
    provider: Option<String>,
) -> AppResult<Vec<BackupSummary>> {
    run_blocking(move || crate::backup::list_backups(backup_dir, provider)).await
}

#[tauri::command]
pub async fn open_backup(backup_dir: String, backup_path: String) -> AppResult<BackupDetail> {
    run_blocking(move || crate::backup::open_backup(backup_dir, backup_path)).await
}

#[tauri::command]
pub async fn delete_backup(backup_dir: String, backup_path: String) -> AppResult<()> {
    run_blocking(move || crate::backup::delete_backup(backup_dir, backup_path)).await
}

#[tauri::command]
pub async fn verify_backup(backup_dir: String, backup_path: String) -> AppResult<VerifyReport> {
    run_blocking(move || crate::backup::verify_backup(backup_dir, backup_path)).await
}

#[tauri::command]
pub async fn restore_session(
    provider: Option<String>,
    backup_dir: String,
    backup_path: String,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    id: String,
    backup_rollout_relpath: Option<String>,
    overwrite: bool,
    lock: SharedLock<'_>,
) -> AppResult<RestoreResult> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::backup::restore_session_with_lock(
            provider,
            backup_dir,
            backup_path,
            codex_dir,
            claude_dir,
            opencode_dir,
            id,
            backup_rollout_relpath,
            overwrite,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn restore_all(
    provider: Option<String>,
    backup_dir: String,
    backup_path: String,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    overwrite: bool,
    lock: SharedLock<'_>,
) -> AppResult<Vec<RestoreResult>> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::backup::restore_all_with_lock(
            provider,
            backup_dir,
            backup_path,
            codex_dir,
            claude_dir,
            opencode_dir,
            overwrite,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn export_session_bundles(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    out_dir: String,
    ids: Vec<String>,
    targets: Option<Vec<BundleExportTarget>>,
    machine_label: Option<String>,
    export_group: Option<String>,
) -> AppResult<Vec<ExportReport>> {
    run_blocking(move || {
        crate::bundle::export_session_bundles_with_opencode(
            provider,
            codex_dir,
            claude_dir,
            opencode_dir,
            out_dir,
            ids,
            targets,
            machine_label,
            export_group,
        )
    })
    .await
}

#[tauri::command]
pub async fn export_all_bundles(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    out_dir: String,
    machine_label: Option<String>,
    export_group: Option<String>,
    active_only: bool,
) -> AppResult<Vec<ExportReport>> {
    run_blocking(move || {
        crate::bundle::export_all_bundles_with_opencode(
            provider,
            codex_dir,
            claude_dir,
            opencode_dir,
            out_dir,
            machine_label,
            export_group,
            active_only,
        )
    })
    .await
}

#[tauri::command]
pub async fn list_bundles(
    src_dir: String,
    provider: Option<String>,
) -> AppResult<Vec<BundleListItem>> {
    run_blocking(move || crate::bundle::list_bundles(src_dir, provider)).await
}

#[tauri::command]
pub async fn verify_bundles(
    src_dir: String,
    provider: Option<String>,
) -> AppResult<Vec<BundleListItem>> {
    run_blocking(move || crate::bundle::verify_bundles(src_dir, provider)).await
}

#[tauri::command]
pub async fn import_session_bundles(
    provider: Option<String>,
    src_dir: String,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    mode: ImportMode,
    make_visible: bool,
    strict: bool,
    project_mappings: Vec<ProjectPathMapping>,
    lock: SharedLock<'_>,
) -> AppResult<Vec<ImportReport>> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::bundle::import_session_bundles_with_lock(
            provider,
            src_dir,
            codex_dir,
            claude_dir,
            opencode_dir,
            mode,
            make_visible,
            strict,
            project_mappings,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn pack_bundles_zip(src_dir: String, zip_path: String) -> AppResult<ZipReport> {
    run_blocking(move || crate::bundle::pack_bundles_zip(src_dir, zip_path)).await
}

#[tauri::command]
pub async fn unpack_zip(zip_path: String, dst_dir: String) -> AppResult<ZipReport> {
    run_blocking(move || crate::bundle::unpack_zip(zip_path, dst_dir)).await
}

#[tauri::command]
pub async fn unpack_zip_to_temp(zip_path: String) -> AppResult<ZipReport> {
    run_blocking(move || crate::bundle::unpack_zip_to_temp(zip_path)).await
}

#[tauri::command]
pub async fn plan_session_event_deletion(
    provider: String,
    rollout_path: String,
    line_nos: Vec<usize>,
) -> AppResult<DeletePlan> {
    run_blocking(move || crate::edit::plan_session_event_deletion(provider, rollout_path, line_nos))
        .await
}

#[tauri::command]
pub async fn edit_session_event_text(
    provider: String,
    rollout_path: String,
    session_id: String,
    backup_dir: String,
    line_no: usize,
    new_text: String,
    lock: SharedLock<'_>,
) -> AppResult<EditApplyReport> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::edit::edit_session_event_text_with_lock(
            provider,
            rollout_path,
            session_id,
            backup_dir,
            line_no,
            new_text,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn delete_session_events(
    provider: String,
    rollout_path: String,
    session_id: String,
    backup_dir: String,
    line_nos: Vec<usize>,
    lock: SharedLock<'_>,
) -> AppResult<EditApplyReport> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::edit::delete_session_events_with_lock(
            provider,
            rollout_path,
            session_id,
            backup_dir,
            line_nos,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn undo_last_session_edit(
    provider: String,
    rollout_path: String,
    session_id: String,
    backup_dir: String,
    lock: SharedLock<'_>,
) -> AppResult<EditApplyReport> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::edit::undo_last_session_edit_with_lock(
            provider,
            rollout_path,
            session_id,
            backup_dir,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn restore_session_edit_snapshot(
    provider: String,
    rollout_path: String,
    session_id: String,
    backup_dir: String,
    snapshot_name: String,
    lock: SharedLock<'_>,
) -> AppResult<EditApplyReport> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::edit::restore_session_edit_snapshot_with_lock(
            provider,
            rollout_path,
            session_id,
            backup_dir,
            snapshot_name,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn session_edit_history(
    provider: String,
    rollout_path: String,
    session_id: String,
    backup_dir: String,
) -> AppResult<EditHistory> {
    run_blocking(move || {
        crate::edit::session_edit_history(provider, rollout_path, session_id, backup_dir)
    })
    .await
}

#[tauri::command]
pub async fn get_family_store(codex_dir: String, lock: SharedLock<'_>) -> AppResult<FamilyStore> {
    let lock = lock.inner().clone();
    run_blocking(move || crate::family::get_family_store_with_lock(codex_dir, &lock)).await
}

#[tauri::command]
pub async fn verify_family_integrity(
    codex_dir: String,
    lock: SharedLock<'_>,
) -> AppResult<FamilyIntegrityReport> {
    let lock = lock.inner().clone();
    run_blocking(move || crate::family::verify_family_integrity_with_lock(codex_dir, &lock)).await
}

#[tauri::command]
pub async fn get_session_family_overlay(
    codex_dir: String,
    lock: SharedLock<'_>,
) -> AppResult<Vec<FamilyOverlay>> {
    let lock = lock.inner().clone();
    run_blocking(move || crate::family::get_session_family_overlay_with_lock(codex_dir, &lock))
        .await
}

#[tauri::command]
pub async fn read_preview_image(path: String) -> AppResult<PreviewImage> {
    run_blocking(move || crate::fs_ops::read_preview_image(path)).await
}

#[tauri::command]
pub async fn export_session_markdown(
    provider: Option<String>,
    rollout_path: String,
    out_path: Option<String>,
    header: MarkdownExportHeader,
    options: MarkdownExportOptions,
) -> AppResult<MarkdownExportReport> {
    run_blocking(move || {
        crate::markdown_export::export_session_markdown(
            provider,
            rollout_path,
            out_path,
            header,
            options,
        )
    })
    .await
}

#[tauri::command]
pub async fn get_provider_info(codex_dir: String) -> AppResult<ProviderInfo> {
    run_blocking(move || crate::repair::get_provider_info(codex_dir)).await
}

#[tauri::command]
pub async fn diagnose_project_configs(codex_dir: String) -> AppResult<ProjectConfigReport> {
    run_blocking(move || crate::repair::diagnose_project_configs(codex_dir)).await
}

#[tauri::command]
pub async fn repair_project_configs(
    codex_dir: String,
    dry_run: bool,
) -> AppResult<ProjectConfigRepairReport> {
    run_blocking(move || crate::repair::repair_project_configs(codex_dir, dry_run)).await
}

#[tauri::command]
pub async fn diagnose_codex_state(codex_dir: String) -> AppResult<DiagnosticReport> {
    run_blocking(move || crate::repair::diagnose_codex_state(codex_dir)).await
}

#[tauri::command]
pub async fn repair_session_index(
    codex_dir: String,
    dry_run: bool,
) -> AppResult<IndexRepairReport> {
    run_blocking(move || crate::repair::repair_session_index(codex_dir, dry_run)).await
}

#[tauri::command]
pub async fn prune_orphan_entries(
    codex_dir: String,
    prune_index: bool,
    prune_threads: bool,
    prune_family: bool,
    prune_subagents: bool,
    dry_run: bool,
    lock: SharedLock<'_>,
) -> AppResult<OrphanPruneReport> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::repair::prune_orphan_entries_with_lock(
            codex_dir,
            prune_index,
            prune_threads,
            prune_family,
            prune_subagents,
            dry_run,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn diagnose_claude_history_orphans(claude_dir: String) -> AppResult<HistoryOrphanReport> {
    run_blocking(move || crate::repair::diagnose_claude_history_orphans(claude_dir)).await
}

#[tauri::command]
pub async fn backfill_archive_origins(
    codex_dir: String,
    dry_run: bool,
    lock: SharedLock<'_>,
) -> AppResult<ArchiveOriginBackfillReport> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::repair::backfill_archive_origins_with_lock(codex_dir, dry_run, &lock)
    })
    .await
}

#[tauri::command]
pub async fn get_archive_ledger(codex_dir: String) -> AppResult<Vec<ArchiveLedgerEntry>> {
    run_blocking(move || crate::archive_ledger::entries(&PathBuf::from(codex_dir))).await
}

/// 用户显式指定归档来源（前端"来源未知"徽标下拉）：强制覆盖 ledger 记录（绕 D13），
/// 并同步 family 分支字段（若该会话属于某 family）。
#[tauri::command]
pub async fn set_archive_origin(
    codex_dir: String,
    session_id: String,
    origin: ArchiveOrigin,
    lock: SharedLock<'_>,
) -> AppResult<SetArchiveOriginReport> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::family::with_lock(&lock, |_g| {
            crate::archive_ledger::set_archive_origin(
                &PathBuf::from(&codex_dir),
                &session_id,
                origin.clone(),
            )?;
            let family_synced = crate::family::set_archive_origin_for_session(
                &PathBuf::from(&codex_dir),
                &session_id,
                origin.clone(),
            )?;
            Ok(SetArchiveOriginReport {
                session_id,
                origin,
                family_synced,
            })
        })
    })
    .await
}

#[tauri::command]
pub async fn prune_claude_history_orphans(
    claude_dir: String,
    dry_run: bool,
) -> AppResult<HistoryPruneReport> {
    run_blocking(move || crate::repair::prune_claude_history_orphans(claude_dir, dry_run)).await
}

#[tauri::command]
pub async fn diagnose_claude_gui_visibility(claude_dir: String) -> AppResult<GuiVisibilityReport> {
    run_blocking(move || crate::repair::diagnose_claude_gui_visibility(claude_dir)).await
}

#[tauri::command]
pub async fn repair_claude_gui_visibility(
    claude_dir: String,
    dry_run: bool,
    session_ids: Option<Vec<String>>,
) -> AppResult<GuiVisibilityFixReport> {
    run_blocking(move || {
        crate::repair::repair_claude_gui_visibility(claude_dir, dry_run, session_ids)
    })
    .await
}

#[tauri::command]
pub async fn rebuild_threads_table(
    codex_dir: String,
    dry_run: bool,
) -> AppResult<ThreadsRebuildReport> {
    run_blocking(move || crate::repair::rebuild_threads_table(codex_dir, dry_run)).await
}

#[tauri::command]
pub async fn fork_session_at_event(
    codex_dir: String,
    session_id: String,
    rollout_path: String,
    event_index: usize,
    lock: SharedLock<'_>,
) -> AppResult<ForkSessionReport> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::repair::fork_session_at_event_with_lock(
            codex_dir,
            session_id,
            rollout_path,
            event_index,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn duplicate_session(
    codex_dir: String,
    session_id: String,
    rollout_path: String,
    lock: SharedLock<'_>,
) -> AppResult<DuplicateSessionReport> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::repair::duplicate_session_with_lock(codex_dir, session_id, rollout_path, &lock)
    })
    .await
}

#[tauri::command]
pub async fn convert_session_provider(
    codex_dir: String,
    claude_dir: String,
    source_provider: String,
    rollout_path: String,
    conversion_mode: Option<String>,
    lock: SharedLock<'_>,
) -> AppResult<ConvertReport> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::convert::convert_session_with_lock(
            codex_dir,
            claude_dir,
            source_provider,
            rollout_path,
            conversion_mode,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn clone_session_for_provider(
    codex_dir: String,
    session_id: String,
    target_provider: Option<String>,
    strategy: SwitchStrategy,
    dry_run: bool,
    lock: SharedLock<'_>,
) -> AppResult<CloneReport> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::repair::clone_session_for_provider_with_lock(
            codex_dir,
            session_id,
            target_provider,
            strategy,
            dry_run,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn get_provider_sync_plan(
    codex_dir: String,
    lock: SharedLock<'_>,
) -> AppResult<Vec<String>> {
    let lock = lock.inner().clone();
    run_blocking(move || crate::repair::get_provider_sync_plan_with_lock(codex_dir, &lock)).await
}

#[tauri::command]
pub async fn batch_clone_for_current_provider(
    codex_dir: String,
    strategy: SwitchStrategy,
    dry_run: bool,
    lock: SharedLock<'_>,
) -> AppResult<Vec<CloneReport>> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::repair::batch_clone_for_current_provider_with_lock(
            codex_dir, strategy, dry_run, &lock,
        )
    })
    .await
}

#[tauri::command]
pub fn start_provider_sync(
    codex_dir: String,
    strategy: SwitchStrategy,
    dry_run: bool,
    lock: SharedLock<'_>,
) -> AppResult<ProviderSyncStart> {
    crate::provider_sync::start_provider_sync(codex_dir, strategy, dry_run, lock.inner().clone())
}

#[tauri::command]
pub fn provider_sync_status(job_id: u64) -> AppResult<ProviderSyncStatus> {
    crate::provider_sync::provider_sync_status(job_id)
}

#[tauri::command]
pub fn active_provider_sync() -> AppResult<Option<ProviderSyncStart>> {
    crate::provider_sync::active_provider_sync()
}

#[tauri::command]
pub async fn rollback_family_active(
    codex_dir: String,
    family_id: String,
    target_branch_id: String,
    lock: SharedLock<'_>,
) -> AppResult<()> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::repair::rollback_family_active_with_lock(
            codex_dir,
            family_id,
            target_branch_id,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn delete_family_branch(
    codex_dir: String,
    family_id: String,
    branch_id: String,
    lock: SharedLock<'_>,
) -> AppResult<crate::models::DeleteResult> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::repair::delete_family_branch_with_lock(codex_dir, family_id, branch_id, &lock)
    })
    .await
}

#[tauri::command]
pub async fn get_family_branch_sync_states(
    codex_dir: String,
    family_id: String,
    lock: SharedLock<'_>,
) -> AppResult<Vec<BranchSyncState>> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::repair::get_family_branch_sync_states_with_lock(codex_dir, family_id, &lock)
    })
    .await
}

#[tauri::command]
pub async fn sync_branch_into_active(
    codex_dir: String,
    family_id: String,
    source_branch_id: String,
    lock: SharedLock<'_>,
) -> AppResult<SyncBranchReport> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::repair::sync_branch_into_active_with_lock(
            codex_dir,
            family_id,
            source_branch_id,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn sync_active_into_branch(
    codex_dir: String,
    family_id: String,
    target_branch_id: String,
    lock: SharedLock<'_>,
) -> AppResult<BranchSyncReport> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::repair::sync_active_into_branch_with_lock(
            codex_dir,
            family_id,
            target_branch_id,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn preview_session_head(
    provider: Option<String>,
    rollout_path: String,
    limit: usize,
) -> AppResult<Vec<PreviewEvent>> {
    run_blocking(move || crate::rollout::preview_session_head(provider, rollout_path, limit)).await
}

#[tauri::command]
pub async fn preview_session_range(
    provider: Option<String>,
    rollout_path: String,
    offset: usize,
    limit: usize,
) -> AppResult<Vec<PreviewEvent>> {
    run_blocking(move || {
        crate::rollout::preview_session_range(provider, rollout_path, offset, limit)
    })
    .await
}

#[tauri::command]
pub async fn preview_session_user_prompts(
    provider: Option<String>,
    rollout_path: String,
) -> AppResult<UserPromptList> {
    run_blocking(move || crate::rollout::preview_session_user_prompts(provider, rollout_path)).await
}

#[tauri::command]
pub async fn preview_session_meta(
    provider: Option<String>,
    rollout_path: String,
) -> AppResult<SessionMetaBrief> {
    run_blocking(move || crate::rollout::preview_session_meta(provider, rollout_path)).await
}

#[tauri::command]
pub async fn list_claude_memory_projects(
    claude_dir: String,
) -> AppResult<Vec<ClaudeMemoryProject>> {
    run_blocking(move || crate::claude_memory::list_projects(claude_dir)).await
}

#[tauri::command]
pub async fn list_claude_memory_files(
    claude_dir: String,
    project_key: String,
) -> AppResult<Vec<ClaudeMemoryFile>> {
    run_blocking(move || crate::claude_memory::list_files(claude_dir, project_key)).await
}

#[tauri::command]
pub async fn read_claude_memory_file(
    claude_dir: String,
    project_key: String,
    file_name: String,
) -> AppResult<ClaudeMemoryDocument> {
    run_blocking(move || crate::claude_memory::read_file(claude_dir, project_key, file_name)).await
}

#[tauri::command]
pub async fn save_claude_memory_file(
    claude_dir: String,
    project_key: String,
    file_name: String,
    content: String,
    expected_sha256: Option<String>,
) -> AppResult<ClaudeMemoryDocument> {
    run_blocking(move || {
        crate::claude_memory::save_file(
            claude_dir,
            project_key,
            file_name,
            content,
            expected_sha256,
        )
    })
    .await
}

#[tauri::command]
pub async fn rename_claude_memory_file(
    claude_dir: String,
    project_key: String,
    file_name: String,
    new_file_name: String,
    expected_sha256: String,
) -> AppResult<ClaudeMemoryDocument> {
    run_blocking(move || {
        crate::claude_memory::rename_file(
            claude_dir,
            project_key,
            file_name,
            new_file_name,
            expected_sha256,
        )
    })
    .await
}

#[tauri::command]
pub async fn delete_claude_memory_file(
    claude_dir: String,
    project_key: String,
    file_name: String,
    expected_sha256: String,
) -> AppResult<bool> {
    run_blocking(move || {
        crate::claude_memory::delete_file(claude_dir, project_key, file_name, expected_sha256)
    })
    .await
}

#[tauri::command]
pub async fn list_sessions(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
) -> AppResult<Vec<SessionSummary>> {
    run_blocking(move || {
        crate::sessions::list_sessions_with_opencode(provider, codex_dir, claude_dir, opencode_dir)
    })
    .await
}

#[tauri::command]
pub async fn group_sessions_by_project(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
) -> AppResult<Vec<ProjectGroup>> {
    run_blocking(move || {
        crate::sessions::group_sessions_by_project_with_opencode(
            provider,
            codex_dir,
            claude_dir,
            opencode_dir,
        )
    })
    .await
}

#[tauri::command]
pub async fn search_sessions(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    query: String,
) -> AppResult<Vec<SessionSummary>> {
    run_blocking(move || {
        crate::sessions::search_sessions_with_opencode(
            provider,
            codex_dir,
            claude_dir,
            opencode_dir,
            query,
        )
    })
    .await
}

#[tauri::command]
pub async fn set_archived(
    provider: Option<String>,
    codex_dir: String,
    opencode_dir: Option<String>,
    id: String,
    v: bool,
    lock: SharedLock<'_>,
) -> AppResult<()> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::sessions::set_archived_with_provider_dir(
            provider,
            codex_dir,
            opencode_dir,
            id,
            v,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn delete_session(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    id: String,
    target: Option<DeleteTarget>,
    lock: SharedLock<'_>,
) -> AppResult<DeleteResult> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::sessions::delete_session_with_provider_dir(
            provider,
            codex_dir,
            claude_dir,
            opencode_dir,
            id,
            target,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn delete_sessions(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    ids: Vec<String>,
    targets: Option<Vec<DeleteTarget>>,
    lock: SharedLock<'_>,
) -> AppResult<Vec<DeleteResult>> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::sessions::delete_sessions_with_provider_dir(
            provider,
            codex_dir,
            claude_dir,
            opencode_dir,
            ids,
            targets,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn rename_session(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    id: String,
    rollout_path: Option<String>,
    title: String,
    lock: SharedLock<'_>,
) -> AppResult<u32> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::sessions::rename_session_with_provider_dirs(
            provider,
            codex_dir,
            claude_dir,
            opencode_dir,
            id,
            rollout_path,
            title,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn move_session_cwd(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    id: String,
    rollout_path: Option<String>,
    target_cwd: String,
    lock: SharedLock<'_>,
) -> AppResult<MoveSessionCwdReport> {
    let lock = lock.inner().clone();
    run_blocking(move || {
        crate::sessions::move_session_cwd_with_provider_dirs(
            provider,
            codex_dir,
            claude_dir,
            opencode_dir,
            id,
            rollout_path,
            target_cwd,
            &lock,
        )
    })
    .await
}

#[tauri::command]
pub async fn stats_snapshot(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    from_ts: Option<i64>,
    to_ts: Option<i64>,
    bucket: String,
    project_limit: usize,
    cwd_filter: Vec<String>,
    include_archived: bool,
) -> AppResult<StatsSnapshot> {
    run_blocking(move || {
        crate::stats::stats_snapshot(
            provider,
            codex_dir,
            claude_dir,
            opencode_dir,
            from_ts,
            to_ts,
            bucket,
            project_limit,
            cwd_filter,
            include_archived,
        )
    })
    .await
}
