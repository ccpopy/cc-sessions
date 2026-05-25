pub mod backup;
pub mod bundle;
pub mod claude_sessions;
pub mod error;
pub mod family;
pub mod fs_ops;
pub mod gemini_sessions;
pub mod history;
pub mod logs_db;
pub mod models;
pub mod paths;
pub mod repair;
pub mod rollout;
pub mod sessions;
pub mod settings;
pub mod state_db;
pub mod stats;

#[cfg(feature = "desktop")]
#[cfg(debug_assertions)]
use tauri::Manager;

#[cfg(feature = "desktop")]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_fs::init())
        .manage(family::FamilyLock::default())
        .setup(|_app| {
            #[cfg(debug_assertions)]
            {
                if let Some(win) = _app.get_webview_window("main") {
                    win.open_devtools();
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            settings::get_settings,
            settings::save_settings,
            settings::app_version,
            settings::default_codex_dir,
            settings::default_claude_dir,
            settings::default_gemini_dir,
            settings::validate_codex_dir,
            settings::validate_claude_dir,
            settings::validate_gemini_dir,
            sessions::list_sessions,
            sessions::group_sessions_by_project,
            sessions::search_sessions,
            sessions::set_archived,
            sessions::delete_session,
            sessions::delete_sessions,
            rollout::preview_session_head,
            rollout::preview_session_range,
            rollout::preview_session_meta,
            backup::create_backup,
            backup::list_backups,
            backup::open_backup,
            backup::restore_session,
            backup::restore_all,
            backup::delete_backup,
            backup::verify_backup,
            stats::stats_kpi,
            stats::stats_timeseries,
            stats::stats_by_project,
            stats::stats_by_model,
            stats::stats_heatmap,
            fs_ops::reveal_cwd,
            fs_ops::open_latest_release_page,
            fs_ops::copy_resume_command,
            repair::get_provider_info,
            repair::diagnose_project_configs,
            repair::repair_project_configs,
            repair::diagnose_codex_state,
            repair::repair_session_index,
            repair::rebuild_threads_table,
            repair::prune_orphan_entries,
            repair::diagnose_claude_history_orphans,
            repair::prune_claude_history_orphans,
            gemini_sessions::diagnose_gemini_orphans,
            gemini_sessions::prune_gemini_orphans,
            repair::clone_session_for_provider,
            repair::fork_session_at_event,
            repair::batch_clone_for_current_provider,
            repair::rollback_family_active,
            repair::delete_family_branch,
            repair::get_family_branch_sync_states,
            repair::sync_branch_into_active,
            repair::sync_active_into_branch,
            family::get_family_store,
            family::verify_family_integrity,
            family::get_session_family_overlay,
            bundle::export_session_bundles,
            bundle::export_all_bundles,
            bundle::list_bundles,
            bundle::verify_bundles,
            bundle::import_session_bundles,
            bundle::pack_bundles_zip,
            bundle::unpack_zip,
            bundle::unpack_zip_to_temp,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
