#[cfg(feature = "desktop")]
pub mod app_update;
pub mod atomic_file;
pub mod backup;
pub mod bundle;
pub mod claude_sessions;
#[cfg(feature = "desktop")]
pub mod commands;
pub mod content_search;
pub mod convert;
pub mod edit;
pub mod error;
pub mod family;
pub mod fs_ops;
pub mod history;
pub mod logs_db;
pub mod markdown_export;
pub mod models;
pub mod path_safety;
pub mod paths;
pub mod provenance;
pub mod repair;
pub mod rollout;
pub mod sessions;
pub mod settings;
pub mod state_db;
pub mod stats;
pub mod webui;

#[cfg(feature = "desktop")]
use tauri::Manager;

#[cfg(feature = "desktop")]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_fs::init())
        .manage(std::sync::Arc::new(family::FamilyLock::default()))
        .setup(|_app| {
            app_update::cleanup_stale_update_dirs();
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
            app_update::check_app_update,
            app_update::install_app_update,
            settings::default_codex_dir,
            settings::default_claude_dir,
            settings::validate_codex_dir,
            settings::validate_claude_dir,
            fs_ops::reveal_cwd,
            fs_ops::open_latest_release_page,
            fs_ops::copy_resume_command,
            commands::list_sessions,
            commands::group_sessions_by_project,
            commands::search_sessions,
            commands::start_content_search,
            commands::content_search_status,
            commands::active_content_search,
            commands::cancel_content_search,
            commands::set_archived,
            commands::rename_session,
            commands::move_session_cwd,
            commands::delete_session,
            commands::delete_sessions,
            commands::preview_session_head,
            commands::preview_session_range,
            commands::preview_session_user_prompts,
            commands::preview_session_meta,
            commands::plan_session_event_deletion,
            commands::edit_session_event_text,
            commands::delete_session_events,
            commands::undo_last_session_edit,
            commands::restore_session_edit_snapshot,
            commands::session_edit_history,
            commands::export_session_markdown,
            commands::create_backup,
            commands::list_backups,
            commands::open_backup,
            commands::restore_session,
            commands::restore_all,
            commands::delete_backup,
            commands::verify_backup,
            commands::stats_kpi,
            commands::stats_timeseries,
            commands::stats_by_project,
            commands::stats_by_model,
            commands::stats_heatmap,
            commands::read_preview_image,
            commands::get_provider_info,
            commands::diagnose_project_configs,
            commands::repair_project_configs,
            commands::diagnose_codex_state,
            commands::repair_session_index,
            commands::rebuild_threads_table,
            commands::prune_orphan_entries,
            commands::diagnose_claude_history_orphans,
            commands::prune_claude_history_orphans,
            commands::diagnose_claude_gui_visibility,
            commands::repair_claude_gui_visibility,
            commands::clone_session_for_provider,
            commands::convert_session_provider,
            commands::fork_session_at_event,
            commands::duplicate_session,
            commands::get_provider_sync_plan,
            commands::batch_clone_for_current_provider,
            commands::rollback_family_active,
            commands::delete_family_branch,
            commands::get_family_branch_sync_states,
            commands::sync_branch_into_active,
            commands::sync_active_into_branch,
            commands::get_family_store,
            commands::verify_family_integrity,
            commands::get_session_family_overlay,
            commands::export_session_bundles,
            commands::export_all_bundles,
            commands::list_bundles,
            commands::verify_bundles,
            commands::import_session_bundles,
            commands::pack_bundles_zip,
            commands::unpack_zip,
            commands::unpack_zip_to_temp,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
