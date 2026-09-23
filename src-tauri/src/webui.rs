use std::env;
use std::fs;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use url::Url;

use crate::error::{AppError, AppResult};
use crate::models::{
    ArchiveOrigin, ImportMode, ProjectPathMapping, SetArchiveOriginReport, Settings, SwitchStrategy,
};
use crate::{
    backup, bundle, claude_memory, content_search, convert, edit, family, fs_ops, markdown_export,
    repair, rollout, sessions, settings, stats,
};

const WEBUI_TOKEN_HEADER: &str = "X-CC-Sessions-Webui-Token";
const WEBUI_SETTINGS_ENV: &str = "CC_SESSIONS_WEBUI_SETTINGS";
const WEBUI_SETTINGS_FILE: &str = "cc-sessions-webui-settings.json";
const WEBUI_PORTABLE_MARKER: &str = "cc-sessions.portable";
const WEBUI_CONFIG_DIR: &str = "cc-sessions";

#[derive(Debug, Clone)]
pub struct WebuiConfig {
    pub host: String,
    pub port: u16,
    pub default_provider: Option<String>,
    pub codex_dir: String,
    pub codex_dir_explicit: bool,
    pub claude_dir: String,
    pub claude_dir_explicit: bool,
    pub opencode_dir: String,
    pub opencode_dir_explicit: bool,
    pub cursor_dir: String,
    pub cursor_dir_explicit: bool,
}

struct WebuiState {
    settings: Mutex<Settings>,
    settings_file: PathBuf,
    family_lock: Arc<family::FamilyLock>,
    dist_dir: PathBuf,
    api_token: String,
    default_provider: String,
}

pub fn run(config: WebuiConfig) -> AppResult<()> {
    validate_host(&config.host)?;
    let dist_dir = resolve_dist_dir()?;
    let settings_file = resolve_settings_file()?;
    let settings_exists = settings_file.exists();
    let mut initial_settings = if settings_exists {
        settings::read_settings_file(&settings_file)?
    } else {
        Settings::default()
    };
    if !settings_exists || config.codex_dir_explicit {
        initial_settings.codex_dir = config.codex_dir;
    }
    if !settings_exists || config.claude_dir_explicit {
        initial_settings.claude_dir = config.claude_dir;
    }
    if !settings_exists || config.opencode_dir_explicit {
        initial_settings.opencode_dir = config.opencode_dir;
    }
    if !settings_exists || config.cursor_dir_explicit {
        initial_settings.cursor_dir = config.cursor_dir;
    }
    if !settings_exists
        || config.codex_dir_explicit
        || config.claude_dir_explicit
        || config.opencode_dir_explicit
        || config.cursor_dir_explicit
    {
        settings::write_settings_file(&settings_file, &initial_settings)?;
    }

    let state = Arc::new(WebuiState {
        settings: Mutex::new(initial_settings),
        settings_file,
        family_lock: Arc::new(family::FamilyLock::default()),
        dist_dir,
        api_token: generate_api_token()?,
        default_provider: config
            .default_provider
            .unwrap_or_else(|| "codex".to_string()),
    });

    let addr = if config.host.contains(':') {
        format!("[{}]:{}", config.host, config.port)
    } else {
        format!("{}:{}", config.host, config.port)
    };
    let server =
        Server::http(&addr).map_err(|err| AppError::Other(format!("启动 Web UI 失败: {err}")))?;
    let listen_addr = server.server_addr().to_string();
    let display_host = if config.host == "0.0.0.0" {
        "localhost"
    } else {
        config.host.as_str()
    };
    println!(
        "CC Sessions Web UI 已启动: http://{}:{}",
        display_host, config.port
    );
    println!("监听地址: {listen_addr}");
    println!("前端资源: {}", state.dist_dir.to_string_lossy());
    println!("配置文件: {}", state.settings_file.to_string_lossy());
    println!("默认 provider: {}", state.default_provider);
    if config.host == "0.0.0.0" {
        println!("注意: 当前绑定 0.0.0.0，局域网内其他设备可能可以访问此服务。");
    } else {
        println!("WSL2 中启动时，Windows 宿主机通常也可以访问同一个 localhost 端口。");
    }
    println!("按 Ctrl+C 停止服务。");

    let (sender, receiver) = std::sync::mpsc::sync_channel::<Request>(8);
    let receiver = Arc::new(Mutex::new(receiver));
    for _ in 0..4 {
        let receiver = Arc::clone(&receiver);
        let state = Arc::clone(&state);
        std::thread::spawn(move || loop {
            let request = receiver
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .recv();
            let Ok(request) = request else {
                break;
            };
            if let Err(err) = handle_request(request, Arc::clone(&state)) {
                eprintln!("webui request failed: {err}");
            }
        });
    }
    for request in server.incoming_requests() {
        // Keep existing job polling/cancellation available even when all I/O workers are busy.
        if matches!(
            request.url(),
            "/api/invoke/provider_sync_status"
                | "/api/invoke/active_provider_sync"
                | "/api/invoke/content_search_status"
                | "/api/invoke/active_content_search"
                | "/api/invoke/cancel_content_search"
                | "/api/invoke/app_version"
        ) {
            if let Err(err) = handle_request(request, Arc::clone(&state)) {
                eprintln!("webui control request failed: {err}");
            }
        } else if let Err(error) = sender.try_send(request) {
            let request = match error {
                std::sync::mpsc::TrySendError::Full(request)
                | std::sync::mpsc::TrySendError::Disconnected(request) => request,
            };
            let _ = request.respond(json_error_response(
                StatusCode(503),
                "任务队列已满，请稍后重试",
            ));
        }
    }
    Ok(())
}

fn handle_request(mut request: Request, state: Arc<WebuiState>) -> AppResult<()> {
    if !local_request_allowed(
        header_value(&request, "Host"),
        header_value(&request, "Origin"),
        header_value(&request, "Sec-Fetch-Site"),
    ) {
        return request
            .respond(json_error_response(
                StatusCode(403),
                "只允许同源的本地 Web UI 请求",
            ))
            .map_err(AppError::Io);
    }
    let method = request.method().clone();
    let url = request_url(&request)?;
    let path = url.path().to_string();

    let result = if method == Method::Post {
        if let Some(command) = path.strip_prefix("/api/invoke/") {
            if !api_token_matches(&request, &state.api_token) {
                return request
                    .respond(json_error_response(
                        StatusCode(403),
                        "Web UI API token 缺失或无效",
                    ))
                    .map_err(AppError::Io);
            }
            let args = match read_json_body(&mut request) {
                Ok(args) => args,
                Err(error) => {
                    return request
                        .respond(json_error_response(StatusCode(400), &error.to_string()))
                        .map_err(AppError::Io)
                }
            };
            respond_result_json(request, dispatch_invoke(&state, command, args))
        } else {
            request.respond(text_response(
                StatusCode(404),
                "text/plain; charset=utf-8",
                "not found",
            ))
        }
    } else if method == Method::Get || method == Method::Head {
        serve_static(request, &state, &path, method == Method::Head)
    } else {
        request.respond(text_response(
            StatusCode(405),
            "text/plain; charset=utf-8",
            "method not allowed",
        ))
    };
    result.map_err(AppError::Io)
}

fn dispatch_invoke(state: &WebuiState, command: &str, args: Value) -> AppResult<Value> {
    match command {
        "app_version" => to_value(settings::app_version()),
        "get_settings" => {
            let settings = state.settings.lock().unwrap_or_else(|err| err.into_inner());
            to_value(settings.clone())
        }
        "save_settings" => {
            let next: Settings = arg(&args, "settings")?;
            let mut settings = state.settings.lock().unwrap_or_else(|err| err.into_inner());
            settings::write_settings_file(&state.settings_file, &next)?;
            *settings = next;
            to_value(())
        }
        "default_codex_dir" => to_value(settings::default_codex_dir()),
        "default_claude_dir" => to_value(settings::default_claude_dir()),
        "default_opencode_dir" => to_value(settings::default_opencode_dir()),
        "validate_codex_dir" => {
            to_result_value(settings::validate_codex_dir(string_arg(&args, "path")?))
        }
        "validate_claude_dir" => {
            to_result_value(settings::validate_claude_dir(string_arg(&args, "path")?))
        }
        "validate_opencode_dir" => {
            to_result_value(settings::validate_opencode_dir(string_arg(&args, "path")?))
        }
        "validate_cursor_dir" => {
            to_result_value(settings::validate_cursor_dir(string_arg(&args, "path")?))
        }
        "default_cursor_dir" => to_value(settings::default_cursor_dir()),
        "list_sessions" => to_result_value(sessions::list_sessions_with_dirs(
            opt_string_arg(&args, "provider")?,
            provider_dirs_arg(&args)?,
        )),
        "group_sessions_by_project" => {
            to_result_value(sessions::group_sessions_by_project_with_dirs(
                opt_string_arg(&args, "provider")?,
                provider_dirs_arg(&args)?,
            ))
        }
        "search_sessions" => to_result_value(sessions::search_sessions_with_dirs(
            opt_string_arg(&args, "provider")?,
            provider_dirs_arg(&args)?,
            string_arg(&args, "query")?,
        )),
        "start_content_search" => to_result_value(content_search::start_content_search(
            string_arg(&args, "provider")?,
            provider_dirs_arg(&args)?,
            string_arg(&args, "query")?,
            arg(&args, "rolloutPaths")?,
        )),
        "content_search_status" => to_result_value(content_search::content_search_status(
            usize_arg(&args, "jobId")? as u64,
        )),
        "active_content_search" => to_result_value(content_search::active_content_search()),
        "cancel_content_search" => to_result_value(content_search::cancel_content_search(
            usize_arg(&args, "jobId")? as u64,
        )),
        "set_archived" => to_result_value(sessions::set_archived_with_dirs(
            opt_string_arg(&args, "provider")?,
            provider_dirs_arg(&args)?,
            string_arg(&args, "id")?,
            bool_arg(&args, "v")?,
            &state.family_lock,
        )),
        "rename_session" => to_result_value(sessions::rename_session_with_dirs(
            opt_string_arg(&args, "provider")?,
            provider_dirs_arg(&args)?,
            string_arg(&args, "id")?,
            opt_string_arg(&args, "rolloutPath")?,
            string_arg(&args, "title")?,
            &state.family_lock,
        )),
        "move_session_cwd" => {
            to_result_value(sessions::move_session_cwd_with_provider_dirs_and_options(
                opt_string_arg(&args, "provider")?,
                string_arg(&args, "codexDir")?,
                opt_string_arg(&args, "claudeDir")?,
                opt_string_arg(&args, "opencodeDir")?,
                string_arg(&args, "id")?,
                opt_string_arg(&args, "rolloutPath")?,
                string_arg(&args, "targetCwd")?,
                opt_bool_arg(&args, "preserveClaudePathCase")?.unwrap_or(false),
                &state.family_lock,
            ))
        }
        "delete_session" => to_result_value(sessions::delete_session_with_dirs(
            opt_string_arg(&args, "provider")?,
            provider_dirs_arg(&args)?,
            string_arg(&args, "id")?,
            opt_arg(&args, "target")?,
            &state.family_lock,
        )),
        "delete_sessions" => to_result_value(sessions::delete_sessions_with_dirs(
            opt_string_arg(&args, "provider")?,
            provider_dirs_arg(&args)?,
            arg(&args, "ids")?,
            opt_arg(&args, "targets")?,
            &state.family_lock,
        )),
        "preview_session_head" => to_result_value(rollout::preview_session_head(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            usize_arg(&args, "limit")?,
        )),
        "preview_session_range" => to_result_value(rollout::preview_session_range(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            usize_arg(&args, "offset")?,
            usize_arg(&args, "limit")?,
        )),
        "preview_session_page" => to_result_value(rollout::preview_session_page(
            string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            usize_arg(&args, "offset")?,
            usize_arg(&args, "limit")?,
            opt_string_arg(&args, "expectedRevision")?,
        )),
        "preview_session_user_prompts" => to_result_value(rollout::preview_session_user_prompts(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
        )),
        "preview_session_meta" => to_result_value(rollout::preview_session_meta(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
        )),
        "list_claude_memory_projects" => to_result_value(claude_memory::list_projects(string_arg(
            &args,
            "claudeDir",
        )?)),
        "list_claude_memory_files" => to_result_value(claude_memory::list_files(
            string_arg(&args, "claudeDir")?,
            string_arg(&args, "projectKey")?,
        )),
        "read_claude_memory_file" => to_result_value(claude_memory::read_file(
            string_arg(&args, "claudeDir")?,
            string_arg(&args, "projectKey")?,
            string_arg(&args, "fileName")?,
        )),
        "save_claude_memory_file" => to_result_value(claude_memory::save_file(
            string_arg(&args, "claudeDir")?,
            string_arg(&args, "projectKey")?,
            string_arg(&args, "fileName")?,
            string_arg(&args, "content")?,
            opt_string_arg(&args, "expectedSha256")?,
        )),
        "rename_claude_memory_file" => to_result_value(claude_memory::rename_file(
            string_arg(&args, "claudeDir")?,
            string_arg(&args, "projectKey")?,
            string_arg(&args, "fileName")?,
            string_arg(&args, "newFileName")?,
            string_arg(&args, "expectedSha256")?,
        )),
        "delete_claude_memory_file" => to_result_value(claude_memory::delete_file(
            string_arg(&args, "claudeDir")?,
            string_arg(&args, "projectKey")?,
            string_arg(&args, "fileName")?,
            string_arg(&args, "expectedSha256")?,
        )),
        "plan_session_event_deletion" => to_result_value(edit::plan_session_event_deletion(
            string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            arg(&args, "lineNos")?,
            opt_string_arg(&args, "expectedRevision")?,
        )),
        "edit_session_event_text" => to_result_value(edit::edit_session_event_text_with_lock(
            string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            string_arg(&args, "sessionId")?,
            string_arg(&args, "backupDir")?,
            usize_arg(&args, "lineNo")?,
            string_arg(&args, "newText")?,
            opt_string_arg(&args, "expectedRevision")?,
            &state.family_lock,
        )),
        "delete_session_events" => to_result_value(edit::delete_session_events_with_lock(
            string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            string_arg(&args, "sessionId")?,
            string_arg(&args, "backupDir")?,
            arg(&args, "lineNos")?,
            opt_string_arg(&args, "expectedRevision")?,
            &state.family_lock,
        )),
        "undo_last_session_edit" => to_result_value(edit::undo_last_session_edit_with_lock(
            string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            string_arg(&args, "sessionId")?,
            string_arg(&args, "backupDir")?,
            opt_string_arg(&args, "expectedRevision")?,
            &state.family_lock,
        )),
        "restore_session_edit_snapshot" => {
            to_result_value(edit::restore_session_edit_snapshot_with_lock(
                string_arg(&args, "provider")?,
                string_arg(&args, "rolloutPath")?,
                string_arg(&args, "sessionId")?,
                string_arg(&args, "backupDir")?,
                string_arg(&args, "snapshotName")?,
                opt_string_arg(&args, "expectedRevision")?,
                &state.family_lock,
            ))
        }
        "session_edit_history" => to_result_value(edit::session_edit_history(
            string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            string_arg(&args, "sessionId")?,
            string_arg(&args, "backupDir")?,
        )),
        "reconcile_session_edit" => to_result_value(edit::reconcile_session_edit_with_lock(
            string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            string_arg(&args, "sessionId")?,
            string_arg(&args, "backupDir")?,
            opt_string_arg(&args, "expectedRevision")?,
            &state.family_lock,
        )),
        "preview_session_markdown" => to_result_value(markdown_export::preview_session_markdown(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            arg(&args, "header")?,
            arg(&args, "options")?,
            arg(&args, "offset")?,
        )),
        "export_session_markdown" => to_result_value(markdown_export::export_session_markdown(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            opt_string_arg(&args, "outPath")?,
            arg(&args, "header")?,
            arg(&args, "options")?,
        )),
        "create_backup" => to_result_value(backup::create_backup_with_dirs(
            opt_string_arg(&args, "provider")?,
            provider_dirs_arg(&args)?,
            string_arg(&args, "backupDir")?,
            arg(&args, "ids")?,
            opt_arg(&args, "targets")?,
            opt_string_arg(&args, "name")?,
            opt_string_arg(&args, "note")?,
        )),
        "list_backups" => to_result_value(backup::list_backups(
            string_arg(&args, "backupDir")?,
            opt_string_arg(&args, "provider")?,
        )),
        "open_backup" => to_result_value(backup::open_backup(
            string_arg(&args, "backupDir")?,
            string_arg(&args, "backupPath")?,
        )),
        "restore_session" => to_result_value(backup::restore_session_with_lock(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "backupDir")?,
            string_arg(&args, "backupPath")?,
            provider_dirs_arg(&args)?,
            string_arg(&args, "id")?,
            opt_string_arg(&args, "backupRolloutRelpath")?,
            bool_arg(&args, "overwrite")?,
            &state.family_lock,
        )),
        "restore_all" => to_result_value(backup::restore_selected_with_lock(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "backupDir")?,
            string_arg(&args, "backupPath")?,
            provider_dirs_arg(&args)?,
            opt_arg(&args, "targets")?,
            bool_arg(&args, "overwrite")?,
            &state.family_lock,
        )),
        "delete_backup" => to_result_value(backup::delete_backup(
            string_arg(&args, "backupDir")?,
            string_arg(&args, "backupPath")?,
        )),
        "verify_backup" => to_result_value(backup::verify_backup(
            string_arg(&args, "backupDir")?,
            string_arg(&args, "backupPath")?,
        )),
        "stats_snapshot" => to_result_value(stats::stats_snapshot(
            opt_string_arg(&args, "provider")?,
            provider_dirs_arg(&args)?,
            opt_i64_arg(&args, "fromTs")?,
            opt_i64_arg(&args, "toTs")?,
            string_arg(&args, "bucket")?,
            usize_arg(&args, "projectLimit")?,
            arg(&args, "cwdFilter")?,
            bool_arg(&args, "includeArchived")?,
        )),
        "compact_cursor_database" => to_result_value(crate::cursor_mutate::compact_database(
            &provider_dirs_arg(&args)?,
        )),
        "diagnose_cursor_residue" => to_result_value(crate::cursor_mutate::diagnose_residue(
            &provider_dirs_arg(&args)?,
        )),
        "prune_cursor_residue" => to_result_value(crate::cursor_mutate::prune_residue(
            &provider_dirs_arg(&args)?,
            &arg::<Vec<String>>(&args, "kinds")?,
            bool_arg(&args, "dryRun")?,
        )),
        "reveal_cwd" => to_result_value(fs_ops::reveal_cwd(string_arg(&args, "cwd")?)),
        "open_latest_release_page" => to_result_value(fs_ops::open_latest_release_page()),
        "copy_resume_command" => to_result_value(fs_ops::resume_command_text(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "sessionId")?,
            opt_string_arg(&args, "cwd")?,
        )),
        "read_preview_image" => {
            to_result_value(fs_ops::read_preview_image(string_arg(&args, "path")?))
        }
        "get_provider_info" => {
            to_result_value(repair::get_provider_info(string_arg(&args, "codexDir")?))
        }
        "diagnose_project_configs" => to_result_value(repair::diagnose_project_configs(
            string_arg(&args, "codexDir")?,
        )),
        "repair_project_configs" => to_result_value(repair::repair_project_configs(
            string_arg(&args, "codexDir")?,
            bool_arg(&args, "dryRun")?,
        )),
        "diagnose_codex_state" => {
            to_result_value(repair::diagnose_codex_state(string_arg(&args, "codexDir")?))
        }
        "repair_session_index" => to_result_value(repair::repair_session_index(
            string_arg(&args, "codexDir")?,
            bool_arg(&args, "dryRun")?,
        )),
        "rebuild_threads_table" => to_result_value(repair::rebuild_threads_table(
            string_arg(&args, "codexDir")?,
            bool_arg(&args, "dryRun")?,
        )),
        "prune_orphan_entries" => to_result_value(repair::prune_orphan_entries_with_lock(
            string_arg(&args, "codexDir")?,
            bool_arg(&args, "pruneIndex")?,
            bool_arg(&args, "pruneThreads")?,
            bool_arg(&args, "pruneFamily")?,
            bool_arg(&args, "pruneSubagents")?,
            bool_arg(&args, "dryRun")?,
            &state.family_lock,
        )),
        "backfill_archive_origins" => to_result_value(repair::backfill_archive_origins_with_lock(
            string_arg(&args, "codexDir")?,
            bool_arg(&args, "dryRun")?,
            &state.family_lock,
        )),
        "get_archive_ledger" => to_result_value(crate::archive_ledger::entries(Path::new(
            &string_arg(&args, "codexDir")?,
        ))),
        "diagnose_claude_history_orphans" => to_result_value(
            repair::diagnose_claude_history_orphans(string_arg(&args, "claudeDir")?),
        ),
        "prune_claude_history_orphans" => to_result_value(repair::prune_claude_history_orphans(
            string_arg(&args, "claudeDir")?,
            bool_arg(&args, "dryRun")?,
        )),
        "diagnose_claude_gui_visibility" => to_result_value(
            repair::diagnose_claude_gui_visibility(string_arg(&args, "claudeDir")?),
        ),
        "repair_claude_gui_visibility" => to_result_value(repair::repair_claude_gui_visibility(
            string_arg(&args, "claudeDir")?,
            bool_arg(&args, "dryRun")?,
            opt_string_vec_arg(&args, "sessionIds")?,
        )),
        "convert_session_provider" => to_result_value(convert::convert_session_with_target(
            provider_dirs_arg(&args)?,
            string_arg(&args, "sourceProvider")?,
            opt_string_arg(&args, "targetProvider")?,
            string_arg(&args, "rolloutPath")?,
            opt_string_arg(&args, "conversionMode")?,
            &state.family_lock,
        )),
        "clone_session_for_provider" => {
            to_result_value(repair::clone_session_for_provider_with_lock(
                string_arg(&args, "codexDir")?,
                string_arg(&args, "sessionId")?,
                opt_string_arg(&args, "targetProvider")?,
                enum_arg::<SwitchStrategy>(&args, "strategy")?,
                bool_arg(&args, "dryRun")?,
                &state.family_lock,
            ))
        }
        "fork_session_at_event" => to_result_value(repair::fork_session_at_event_with_lock(
            string_arg(&args, "codexDir")?,
            string_arg(&args, "sessionId")?,
            string_arg(&args, "rolloutPath")?,
            usize_arg(&args, "eventIndex")?,
            &state.family_lock,
        )),
        "duplicate_session" => to_result_value(repair::duplicate_session_with_lock(
            string_arg(&args, "codexDir")?,
            string_arg(&args, "sessionId")?,
            string_arg(&args, "rolloutPath")?,
            &state.family_lock,
        )),
        "fork_claude_session_at_event" => {
            to_result_value(crate::claude_fork::fork_session_at_event_with_lock(
                string_arg(&args, "claudeDir")?,
                string_arg(&args, "sessionId")?,
                string_arg(&args, "rolloutPath")?,
                usize_arg(&args, "eventIndex")?,
                string_arg(&args, "messageUuid")?,
                &state.family_lock,
            ))
        }
        "duplicate_claude_session" => {
            to_result_value(crate::claude_fork::duplicate_session_with_lock(
                string_arg(&args, "claudeDir")?,
                string_arg(&args, "sessionId")?,
                string_arg(&args, "rolloutPath")?,
                &state.family_lock,
            ))
        }
        "copy_opencode_session" => to_result_value(crate::opencode_fork::copy_session_with_lock(
            string_arg(&args, "opencodeDir")?,
            string_arg(&args, "sessionId")?,
            string_arg(&args, "rolloutPath")?,
            opt_arg::<crate::models::OpenCodeForkPoint>(&args, "cutoff")?,
            &state.family_lock,
        )),
        "get_provider_sync_plan" => to_result_value(repair::get_provider_sync_plan_with_lock(
            string_arg(&args, "codexDir")?,
            &state.family_lock,
        )),
        "batch_clone_for_current_provider" => {
            to_result_value(repair::batch_clone_for_current_provider_with_lock(
                string_arg(&args, "codexDir")?,
                enum_arg::<SwitchStrategy>(&args, "strategy")?,
                bool_arg(&args, "dryRun")?,
                &state.family_lock,
            ))
        }
        "start_provider_sync" => to_result_value(crate::provider_sync::start_provider_sync(
            string_arg(&args, "codexDir")?,
            enum_arg::<SwitchStrategy>(&args, "strategy")?,
            bool_arg(&args, "dryRun")?,
            Arc::clone(&state.family_lock),
        )),
        "provider_sync_status" => to_result_value(crate::provider_sync::provider_sync_status(
            arg::<u64>(&args, "jobId")?,
        )),
        "active_provider_sync" => to_result_value(crate::provider_sync::active_provider_sync()),
        "set_archive_origin" => to_result_value(webui_set_archive_origin(&state, &args)),
        "rollback_family_active" => to_result_value(repair::rollback_family_active_with_lock(
            string_arg(&args, "codexDir")?,
            string_arg(&args, "familyId")?,
            string_arg(&args, "targetBranchId")?,
            &state.family_lock,
        )),
        "delete_family_branch" => to_result_value(repair::delete_family_branch_with_lock(
            string_arg(&args, "codexDir")?,
            string_arg(&args, "familyId")?,
            string_arg(&args, "branchId")?,
            &state.family_lock,
        )),
        "get_family_branch_sync_states" => {
            to_result_value(repair::get_family_branch_sync_states_with_lock(
                string_arg(&args, "codexDir")?,
                string_arg(&args, "familyId")?,
                &state.family_lock,
            ))
        }
        "sync_branch_into_active" => to_result_value(repair::sync_branch_into_active_with_lock(
            string_arg(&args, "codexDir")?,
            string_arg(&args, "familyId")?,
            string_arg(&args, "sourceBranchId")?,
            &state.family_lock,
        )),
        "sync_active_into_branch" => to_result_value(repair::sync_active_into_branch_with_lock(
            string_arg(&args, "codexDir")?,
            string_arg(&args, "familyId")?,
            string_arg(&args, "targetBranchId")?,
            &state.family_lock,
        )),
        "get_family_store" => to_result_value(family::get_family_store_with_lock(
            string_arg(&args, "codexDir")?,
            &state.family_lock,
        )),
        "verify_family_integrity" => to_result_value(family::verify_family_integrity_with_lock(
            string_arg(&args, "codexDir")?,
            &state.family_lock,
        )),
        "get_session_family_overlay" => {
            to_result_value(family::get_session_family_overlay_with_lock(
                string_arg(&args, "codexDir")?,
                &state.family_lock,
            ))
        }
        "export_session_bundles" => to_result_value(bundle::export_session_bundles_with_dirs(
            opt_string_arg(&args, "provider")?,
            provider_dirs_arg(&args)?,
            string_arg(&args, "outDir")?,
            arg(&args, "ids")?,
            opt_arg(&args, "targets")?,
            opt_string_arg(&args, "machineLabel")?,
            opt_string_arg(&args, "exportGroup")?,
        )),
        "export_all_bundles" => to_result_value(bundle::export_all_bundles_in_range_with_dirs(
            opt_string_arg(&args, "provider")?,
            provider_dirs_arg(&args)?,
            string_arg(&args, "outDir")?,
            opt_string_arg(&args, "machineLabel")?,
            opt_string_arg(&args, "exportGroup")?,
            bool_arg(&args, "activeOnly")?,
            opt_i64_arg(&args, "fromUpdatedAt")?,
            opt_i64_arg(&args, "toUpdatedAt")?,
        )),
        "list_bundles" => to_result_value(bundle::list_bundles(
            string_arg(&args, "srcDir")?,
            opt_string_arg(&args, "provider")?,
        )),
        "verify_bundles" => to_result_value(bundle::verify_bundles(
            string_arg(&args, "srcDir")?,
            opt_string_arg(&args, "provider")?,
        )),
        "import_session_bundles" => to_result_value(bundle::import_session_bundles_with_lock(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "srcDir")?,
            provider_dirs_arg(&args)?,
            enum_arg::<ImportMode>(&args, "mode")?,
            bool_arg(&args, "makeVisible")?,
            bool_arg(&args, "strict")?,
            arg::<Vec<ProjectPathMapping>>(&args, "projectMappings")?,
            opt_arg::<Vec<String>>(&args, "bundleDirs")?,
            &state.family_lock,
        )),
        "pack_bundles_zip" => to_result_value(bundle::pack_bundles_zip(
            string_arg(&args, "srcDir")?,
            string_arg(&args, "zipPath")?,
        )),
        "unpack_zip" => to_result_value(bundle::unpack_zip(
            string_arg(&args, "zipPath")?,
            string_arg(&args, "dstDir")?,
        )),
        "unpack_zip_to_temp" => {
            to_result_value(bundle::unpack_zip_to_temp(string_arg(&args, "zipPath")?))
        }
        other => Err(AppError::Other(format!("Web UI 不支持的命令: {other}"))),
    }
}

fn serve_static(
    request: Request,
    state: &WebuiState,
    request_path: &str,
    head_only: bool,
) -> std::io::Result<()> {
    let dist_dir = &state.dist_dir;
    let Some(path) = static_path(dist_dir, request_path) else {
        return request.respond(text_response(
            StatusCode(400),
            "text/plain; charset=utf-8",
            "bad request",
        ));
    };
    let final_path = if path.is_file() {
        path
    } else if should_fallback_to_spa(request_path) {
        dist_dir.join("index.html")
    } else {
        return request.respond(text_response(
            StatusCode(404),
            "text/plain; charset=utf-8",
            "not found",
        ));
    };
    let final_path = final_path.canonicalize()?;
    if !final_path.starts_with(dist_dir.canonicalize()?) {
        return request.respond(text_response(
            StatusCode(403),
            "text/plain; charset=utf-8",
            "asset outside trusted root",
        ));
    }
    let content_type = content_type(&final_path);
    let body = if head_only {
        Vec::new()
    } else if is_index_html(&final_path) {
        inject_runtime_config(fs::read_to_string(&final_path)?, state)?.into_bytes()
    } else {
        fs::read(&final_path)?
    };
    let csp = format!("default-src 'self'; script-src 'self' 'nonce-{}'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'", state.api_token);
    request.respond(
        binary_response(StatusCode(200), content_type, body)
            .with_header(Header::from_bytes("Content-Security-Policy", csp).unwrap()),
    )
}

fn should_fallback_to_spa(request_path: &str) -> bool {
    let trimmed = request_path.trim_start_matches('/');
    trimmed.is_empty() || !trimmed.rsplit('/').next().unwrap_or("").contains('.')
}

fn static_path(dist_dir: &Path, request_path: &str) -> Option<PathBuf> {
    let trimmed = request_path.trim_start_matches('/');
    if trimmed.is_empty() {
        return Some(dist_dir.join("index.html"));
    }
    let mut out = dist_dir.to_path_buf();
    for segment in trimmed.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.contains('\\') {
            return None;
        }
        out.push(segment);
    }
    Some(out)
}

fn inject_runtime_config(mut html: String, state: &WebuiState) -> std::io::Result<String> {
    html = html.replace(
        "<script>",
        &format!("<script nonce=\"{}\">", state.api_token),
    );
    let config = json!({
        "apiToken": &state.api_token,
        "defaultProvider": &state.default_provider,
    });
    let script = format!(
        "<script nonce=\"{}\">window.__CC_SESSIONS_WEBUI__ = {};</script>\n",
        state.api_token,
        serde_json::to_string(&config)
            .expect("runtime config is serializable")
            .replace('<', "\\u003c")
    );
    let Some(pos) = html.find("</head>") else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Web UI index.html 缺少 </head>，无法注入运行时配置",
        ));
    };
    html.insert_str(pos, &script);
    Ok(html)
}

fn is_index_html(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("index.html"))
}

fn resolve_dist_dir() -> AppResult<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(raw) = env::var("CC_SESSIONS_WEBUI_DIST") {
        if !raw.trim().is_empty() {
            let candidate = PathBuf::from(raw);
            if !candidate.join("index.html").is_file() {
                return Err(AppError::Path(
                    "显式设置的 CC_SESSIONS_WEBUI_DIST 不包含 index.html".into(),
                ));
            }
            return Ok(candidate.canonicalize()?);
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("dist"));
            candidates.push(dir.join("webui"));
        }
    }
    #[cfg(debug_assertions)]
    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../dist"));

    for candidate in candidates {
        if candidate.join("index.html").is_file() {
            return Ok(candidate.canonicalize()?);
        }
    }

    Err(AppError::Other(
        "找不到 Web UI 前端构建产物。请把 dist 放在 cc-sessions 可执行文件旁；开发时可用 CC_SESSIONS_WEBUI_DIST 显式指定受信任的构建目录。".into(),
    ))
}

fn resolve_settings_file() -> AppResult<PathBuf> {
    if let Ok(raw) = env::var(WEBUI_SETTINGS_ENV) {
        if !raw.trim().is_empty() {
            return Ok(PathBuf::from(raw));
        }
    }
    let exe = env::current_exe()
        .map_err(|err| AppError::Other(format!("无法确定 cc-sessions 可执行文件路径: {err}")))?;
    let exe_dir = exe
        .parent()
        .ok_or_else(|| AppError::Other("无法确定 cc-sessions 可执行文件目录".to_string()))?;
    if exe_dir.join(WEBUI_PORTABLE_MARKER).is_file() {
        return Ok(exe_dir.join(WEBUI_SETTINGS_FILE));
    }
    let config_dir = dirs::config_dir()
        .ok_or_else(|| AppError::Other("无法确定系统用户配置目录".to_string()))?;
    Ok(config_dir.join(WEBUI_CONFIG_DIR).join(WEBUI_SETTINGS_FILE))
}

fn generate_api_token() -> AppResult<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|err| AppError::Other(format!("生成 Web UI API token 失败: {err}")))?;
    Ok(hex::encode(bytes))
}

fn api_token_matches(request: &Request, expected: &str) -> bool {
    header_value(request, WEBUI_TOKEN_HEADER)
        .is_some_and(|actual| constant_time_eq(actual.as_bytes(), expected.as_bytes()))
}

fn header_value<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

fn request_url(request: &Request) -> AppResult<Url> {
    Url::parse(&format!("http://localhost{}", request.url()))
        .map_err(|err| AppError::Other(format!("无效请求 URL: {err}")))
}

fn read_json_body(request: &mut Request) -> AppResult<Value> {
    const MAX_BODY: u64 = 8 * 1024 * 1024;
    if request
        .body_length()
        .is_some_and(|length| length as u64 > MAX_BODY)
    {
        return Err(AppError::Other("Web UI 请求体不能超过 8 MiB".into()));
    }
    let mut body = String::new();
    request
        .as_reader()
        .take(MAX_BODY + 1)
        .read_to_string(&mut body)?;
    if body.len() as u64 > MAX_BODY {
        return Err(AppError::Other("Web UI 请求体不能超过 8 MiB".into()));
    }
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&body).map_err(AppError::Serde)
}

fn respond_result_json(request: Request, result: AppResult<Value>) -> std::io::Result<()> {
    match result {
        Ok(value) => respond_json(request, value),
        Err(err) => {
            let body = json!({ "error": err.to_string() }).to_string();
            request.respond(text_response(
                StatusCode(500),
                "application/json; charset=utf-8",
                body,
            ))
        }
    }
}

fn json_error_response(status: StatusCode, message: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    text_response(
        status,
        "application/json; charset=utf-8",
        json!({ "error": message }).to_string(),
    )
}

fn respond_json<T: Serialize>(request: Request, value: T) -> std::io::Result<()> {
    let body = match serde_json::to_string(&value) {
        Ok(body) => body,
        Err(err) => {
            return request.respond(json_error_response(
                StatusCode(500),
                &format!("序列化响应失败: {err}"),
            ));
        }
    };
    request.respond(text_response(
        StatusCode(200),
        "application/json; charset=utf-8",
        body,
    ))
}

fn to_result_value<T: Serialize>(result: AppResult<T>) -> AppResult<Value> {
    serde_json::to_value(result?).map_err(AppError::Serde)
}

fn to_value<T: Serialize>(value: T) -> AppResult<Value> {
    serde_json::to_value(value).map_err(AppError::Serde)
}

fn arg<T: DeserializeOwned>(args: &Value, name: &str) -> AppResult<T> {
    let Some(value) = args.get(name) else {
        return Err(AppError::Other(format!("缺少参数: {name}")));
    };
    serde_json::from_value(value.clone()).map_err(AppError::Serde)
}

fn opt_arg<T: DeserializeOwned>(args: &Value, name: &str) -> AppResult<Option<T>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone()).map_err(AppError::Serde),
    }
}

fn enum_arg<T: DeserializeOwned>(args: &Value, name: &str) -> AppResult<T> {
    arg(args, name)
}

/// 各 provider 目录一次性收齐，新增 provider 时只需要改这一处。
fn provider_dirs_arg(args: &Value) -> AppResult<crate::models::ProviderDirs> {
    Ok(crate::models::ProviderDirs {
        codex_dir: string_arg(args, "codexDir")?,
        claude_dir: opt_string_arg(args, "claudeDir")?,
        opencode_dir: opt_string_arg(args, "opencodeDir")?,
        cursor_dir: opt_string_arg(args, "cursorDir")?,
        cursor_agent_dir: None,
    })
}

fn string_arg(args: &Value, name: &str) -> AppResult<String> {
    arg(args, name)
}

fn opt_string_arg(args: &Value, name: &str) -> AppResult<Option<String>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone()).map_err(AppError::Serde),
    }
}

fn opt_string_vec_arg(args: &Value, name: &str) -> AppResult<Option<Vec<String>>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone()).map_err(AppError::Serde),
    }
}

/// 用户显式指定归档来源：强制覆盖 ledger（绕 D13）并同步 family 分支字段。
fn webui_set_archive_origin(state: &WebuiState, args: &Value) -> AppResult<SetArchiveOriginReport> {
    let codex_dir = string_arg(args, "codexDir")?;
    let session_id = string_arg(args, "sessionId")?;
    let origin = enum_arg::<ArchiveOrigin>(args, "origin")?;
    family::with_lock(&state.family_lock, Path::new(&codex_dir), |_g| {
        crate::archive_ledger::set_archive_origin(
            Path::new(&codex_dir),
            &session_id,
            origin.clone(),
        )?;
        let family_synced = family::set_archive_origin_for_session(
            Path::new(&codex_dir),
            &session_id,
            origin.clone(),
        )?;
        Ok(SetArchiveOriginReport {
            session_id,
            origin,
            family_synced,
        })
    })
}

fn bool_arg(args: &Value, name: &str) -> AppResult<bool> {
    arg(args, name)
}

fn opt_bool_arg(args: &Value, name: &str) -> AppResult<Option<bool>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone()).map_err(AppError::Serde),
    }
}

fn usize_arg(args: &Value, name: &str) -> AppResult<usize> {
    arg(args, name)
}

fn opt_i64_arg(args: &Value, name: &str) -> AppResult<Option<i64>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone()).map_err(AppError::Serde),
    }
}

fn text_response(
    status: StatusCode,
    content_type: &str,
    body: impl Into<String>,
) -> Response<std::io::Cursor<Vec<u8>>> {
    binary_response(status, content_type, body.into().into_bytes())
}

fn binary_response(
    status: StatusCode,
    content_type: &str,
    body: Vec<u8>,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let content_type = Header::from_bytes("Content-Type", content_type)
        .expect("static content-type header is valid");
    let cache =
        Header::from_bytes("Cache-Control", "no-store").expect("static cache header is valid");
    Response::from_data(body)
        .with_status_code(status)
        .with_header(content_type)
        .with_header(cache)
        .with_header(Header::from_bytes("X-Content-Type-Options", "nosniff").unwrap())
        .with_header(Header::from_bytes("X-Frame-Options", "DENY").unwrap())
        .with_header(Header::from_bytes("Referrer-Policy", "no-referrer").unwrap())
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

pub fn validate_host(host: &str) -> AppResult<()> {
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(());
    }
    match host.parse::<IpAddr>() {
        Ok(address) if address.is_loopback() => Ok(()),
        _ => Err(AppError::Other(
            "Web UI 仅支持回环地址（127.0.0.1、::1 或 localhost）；远程访问请使用 SSH 隧道".into(),
        )),
    }
}

fn local_request_allowed(
    host: Option<&str>,
    origin: Option<&str>,
    fetch_site: Option<&str>,
) -> bool {
    if fetch_site.is_some_and(|site| site == "cross-site") {
        return false;
    }
    let Some(host) = host else {
        return false;
    };
    let Ok(base) = Url::parse(&format!("http://{host}")) else {
        return false;
    };
    if base
        .host_str()
        .is_none_or(|host| validate_host(host.trim_matches(['[', ']'])).is_err())
        || !base.username().is_empty()
        || base.password().is_some()
        || base.path() != "/"
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return false;
    }
    origin.is_none_or(|origin| Url::parse(origin).is_ok_and(|url| url.origin() == base.origin()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn webui_message_edits_require_the_version_returned_by_preview() -> AppResult<()> {
        let root = temp_codex_dir("webui-edit-revision");
        fs::create_dir_all(&root)?;
        let rollout = root.join("rollout.jsonl");
        let before = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"unique\"}]}}\n"
        );
        fs::write(&rollout, before)?;
        let state = test_state(&root);
        let page = dispatch_invoke(
            &state,
            "preview_session_page",
            json!({"provider":"codex","rolloutPath":rollout,"offset":0,"limit":20}),
        )?;
        let mut args = json!({"provider":"codex","rolloutPath":rollout,"sessionId":"s","backupDir":root.join("backup"),"lineNos":[1]});
        assert!(dispatch_invoke(&state, "delete_session_events", args.clone()).is_err());
        assert_eq!(fs::read(&rollout)?, before.as_bytes());
        args["expectedRevision"] = page["capability"]["revision"].clone();
        let plan = dispatch_invoke(&state, "plan_session_event_deletion", args.clone())?;
        assert_eq!(plan["revision"], args["expectedRevision"]);
        let report = dispatch_invoke(&state, "delete_session_events", args.clone())?;
        assert_eq!(report["status"], "committed_unverified");
        assert_eq!(report["deleted_lines"], 1);
        let committed = fs::read(&rollout)?;
        assert!(
            dispatch_invoke(&state, "delete_session_events", args).is_err(),
            "超时后重试旧请求不能再次删除"
        );
        assert_eq!(fs::read(&rollout)?, committed);
        fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn review_webui_checks_host_origin_and_runtime_nonce() -> AppResult<()> {
        assert!(local_request_allowed(
            Some("127.0.0.1:17888"),
            Some("http://127.0.0.1:17888"),
            Some("same-origin")
        ));
        assert!(local_request_allowed(Some("localhost:18000"), None, None)); // SSH forwarding port
        assert!(local_request_allowed(
            Some("[::1]:17888"),
            Some("http://[::1]:17888"),
            None
        ));
        for (host, origin, site) in [
            ("attacker.example", None, None),
            ("localhost:17888", Some("https://attacker.example"), None),
            ("localhost:17888", Some("http://localhost:9000"), None),
            ("localhost:17888", Some("null"), None),
            ("localhost:17888", None, Some("cross-site")),
        ] {
            assert!(!local_request_allowed(Some(host), origin, site));
        }
        let state = test_state(Path::new("."));
        let html = inject_runtime_config("<head><script>theme()</script></head>".into(), &state)?;
        assert_eq!(html.matches("nonce=\"test-token\"").count(), 2);
        assert!(!html.contains("<script>"));
        Ok(())
    }

    #[test]
    fn review_webui_http_rejects_untrusted_host_and_large_body() -> AppResult<()> {
        use std::io::{Read, Write};
        let root = temp_codex_dir("webui-http-boundaries");
        fs::create_dir_all(root.join("dist"))?;
        fs::write(
            root.join("dist/index.html"),
            "<html><head></head><body>trusted</body></html>",
        )?;
        let state = Arc::new(test_state(&root));
        for (headers, status) in [
            ("GET / HTTP/1.1\r\nHost: attacker.example\r\nConnection: close\r\n\r\n", "403"),
            ("GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", "200"),
            ("POST /api/invoke/app_version HTTP/1.1\r\nHost: localhost\r\nX-CC-Sessions-Webui-Token: test-token\r\nContent-Length: 9000000\r\nConnection: close\r\n\r\n", "400"),
        ] {
            let server = Server::http("127.0.0.1:0").unwrap();
            let address = server.server_addr().to_ip().unwrap();
            let state = state.clone();
            let worker = std::thread::spawn(move || handle_request(server.recv().unwrap(), state));
            let mut client = std::net::TcpStream::connect(address)?;
            client.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
            client.write_all(headers.as_bytes())?;
            if status == "400" { client.write_all(&vec![b' '; 9_000_000])?; }
            let mut response = String::new();
            client.read_to_string(&mut response)?;
            worker.join().unwrap()?;
            assert!(response.starts_with(&format!("HTTP/1.1 {status}")), "{response}");
            if status != "200" { assert!(!response.contains("apiToken")); }
            else { assert!(response.contains("Content-Security-Policy:")); }
        }
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn review_webui_refuses_non_loopback_listeners() {
        for host in ["0.0.0.0", "::", "192.168.1.10", "8.8.8.8"] {
            assert!(
                validate_host(host).is_err(),
                "{host} must not expose management APIs"
            );
        }
        for host in ["localhost", "127.0.0.1", "::1"] {
            assert!(validate_host(host).is_ok());
        }
    }

    use super::*;

    fn temp_codex_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cc-session-manager-{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ))
    }

    fn test_state(root: &Path) -> WebuiState {
        WebuiState {
            settings: Mutex::new(Settings::default()),
            settings_file: root.join("settings.json"),
            family_lock: Arc::new(family::FamilyLock::default()),
            dist_dir: root.join("dist"),
            api_token: "test-token".to_string(),
            default_provider: "codex".to_string(),
        }
    }

    #[test]
    fn webui_dispatches_archive_ledger_read_and_backfill_commands() -> AppResult<()> {
        let codex = temp_codex_dir("webui-archive-commands");
        fs::create_dir_all(&codex)?;
        let state = test_state(&codex);
        let args = json!({ "codexDir": codex.to_string_lossy() });

        let entries = dispatch_invoke(&state, "get_archive_ledger", args.clone())?;
        assert_eq!(entries, json!([]));
        let report = dispatch_invoke(
            &state,
            "backfill_archive_origins",
            json!({ "codexDir": codex.to_string_lossy(), "dryRun": true }),
        )?;
        assert_eq!(report["scanned"], 0);
        assert_eq!(report["dry_run"], true);

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn webui_markdown_export_includes_the_full_local_timestamp() -> AppResult<()> {
        let codex = temp_codex_dir("webui-markdown-timestamp");
        fs::create_dir_all(&codex)?;
        let rollout = codex.join("rollout.jsonl");
        let timestamp = "2026-08-27T12:34:56Z";
        fs::write(
            &rollout,
            format!(
                r#"{{"timestamp":"{timestamp}","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"WebUI 导出"}}]}}}}"#
            ),
        )?;
        let state = test_state(&codex);

        let report = dispatch_invoke(
            &state,
            "export_session_markdown",
            json!({
                "provider": "codex",
                "rolloutPath": rollout.to_string_lossy(),
                "outPath": null,
                "header": {
                    "title": "WebUI 导出",
                    "session_id": "test-session",
                    "provider": "codex"
                },
                "options": {
                    "include_front_matter": false,
                    "include_reasoning": false,
                    "include_tools": false,
                    "ai_handoff_preamble": false
                }
            }),
        )?;
        let expected = chrono::DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let markdown = report["markdown"].as_str().unwrap_or_default();

        assert_eq!(report["message_count"], 1);
        assert!(
            markdown.contains(&format!("## 👤 User · {expected}")),
            "WebUI 导出缺少完整日期时间: {markdown}"
        );

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn optional_bool_arguments_remain_backward_compatible() -> AppResult<()> {
        assert_eq!(opt_bool_arg(&json!({}), "preserveClaudePathCase")?, None);
        assert_eq!(
            opt_bool_arg(
                &json!({"preserveClaudePathCase": true}),
                "preserveClaudePathCase"
            )?,
            Some(true)
        );
        assert!(opt_bool_arg(
            &json!({"preserveClaudePathCase": "true"}),
            "preserveClaudePathCase"
        )
        .is_err());
        Ok(())
    }
}
