use std::env;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use url::Url;

use crate::error::{AppError, AppResult};
use crate::models::{ImportMode, ProjectPathMapping, Settings, SwitchStrategy};
use crate::{
    backup, bundle, convert, edit, family, fs_ops, markdown_export, repair, rollout, sessions,
    settings, stats,
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
}

struct WebuiState {
    settings: Mutex<Settings>,
    settings_file: PathBuf,
    family_lock: family::FamilyLock,
    dist_dir: PathBuf,
    api_token: String,
    default_provider: String,
}

pub fn run(config: WebuiConfig) -> AppResult<()> {
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
    if !settings_exists || config.codex_dir_explicit || config.claude_dir_explicit {
        settings::write_settings_file(&settings_file, &initial_settings)?;
    }

    let state = Arc::new(WebuiState {
        settings: Mutex::new(initial_settings),
        settings_file,
        family_lock: family::FamilyLock::default(),
        dist_dir,
        api_token: generate_api_token()?,
        default_provider: config
            .default_provider
            .unwrap_or_else(|| "codex".to_string()),
    });

    let addr = format!("{}:{}", config.host, config.port);
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

    for request in server.incoming_requests() {
        let state = Arc::clone(&state);
        if let Err(err) = handle_request(request, state) {
            eprintln!("webui request failed: {err}");
        }
    }
    Ok(())
}

fn handle_request(mut request: Request, state: Arc<WebuiState>) -> AppResult<()> {
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
            let args = read_json_body(&mut request)?;
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
            settings::write_settings_file(&state.settings_file, &next)?;
            let mut settings = state.settings.lock().unwrap_or_else(|err| err.into_inner());
            *settings = next;
            to_value(())
        }
        "default_codex_dir" => to_value(settings::default_codex_dir()),
        "default_claude_dir" => to_value(settings::default_claude_dir()),
        "validate_codex_dir" => {
            to_result_value(settings::validate_codex_dir(string_arg(&args, "path")?))
        }
        "validate_claude_dir" => {
            to_result_value(settings::validate_claude_dir(string_arg(&args, "path")?))
        }
        "list_sessions" => to_result_value(sessions::list_sessions(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
        )),
        "group_sessions_by_project" => to_result_value(sessions::group_sessions_by_project(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
        )),
        "search_sessions" => to_result_value(sessions::search_sessions(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
            string_arg(&args, "query")?,
        )),
        "set_archived" => to_result_value(sessions::set_archived_with_lock(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            string_arg(&args, "id")?,
            bool_arg(&args, "v")?,
            &state.family_lock,
        )),
        "rename_session" => to_result_value(sessions::rename_session_with_lock(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            string_arg(&args, "id")?,
            string_arg(&args, "title")?,
            &state.family_lock,
        )),
        "move_session_cwd" => to_result_value(sessions::move_session_cwd_with_lock(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            string_arg(&args, "id")?,
            string_arg(&args, "targetCwd")?,
            &state.family_lock,
        )),
        "delete_session" => to_result_value(sessions::delete_session_with_lock(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
            string_arg(&args, "id")?,
            opt_arg(&args, "target")?,
            &state.family_lock,
        )),
        "delete_sessions" => to_result_value(sessions::delete_sessions_with_lock(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
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
        "preview_session_user_prompts" => to_result_value(rollout::preview_session_user_prompts(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
        )),
        "preview_session_meta" => to_result_value(rollout::preview_session_meta(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
        )),
        "plan_session_event_deletion" => to_result_value(edit::plan_session_event_deletion(
            string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            arg(&args, "lineNos")?,
        )),
        "edit_session_event_text" => to_result_value(edit::edit_session_event_text_with_lock(
            string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            string_arg(&args, "sessionId")?,
            string_arg(&args, "backupDir")?,
            usize_arg(&args, "lineNo")?,
            string_arg(&args, "newText")?,
            &state.family_lock,
        )),
        "delete_session_events" => to_result_value(edit::delete_session_events_with_lock(
            string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            string_arg(&args, "sessionId")?,
            string_arg(&args, "backupDir")?,
            arg(&args, "lineNos")?,
            &state.family_lock,
        )),
        "undo_last_session_edit" => to_result_value(edit::undo_last_session_edit_with_lock(
            string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            string_arg(&args, "sessionId")?,
            string_arg(&args, "backupDir")?,
            &state.family_lock,
        )),
        "restore_session_edit_snapshot" => {
            to_result_value(edit::restore_session_edit_snapshot_with_lock(
                string_arg(&args, "provider")?,
                string_arg(&args, "rolloutPath")?,
                string_arg(&args, "sessionId")?,
                string_arg(&args, "backupDir")?,
                string_arg(&args, "snapshotName")?,
                &state.family_lock,
            ))
        }
        "session_edit_history" => to_result_value(edit::session_edit_history(
            string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            string_arg(&args, "sessionId")?,
            string_arg(&args, "backupDir")?,
        )),
        "export_session_markdown" => to_result_value(markdown_export::export_session_markdown(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "rolloutPath")?,
            opt_string_arg(&args, "outPath")?,
            arg(&args, "header")?,
            arg(&args, "options")?,
        )),
        "create_backup" => to_result_value(backup::create_backup(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
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
        "restore_session" => to_result_value(backup::restore_session(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "backupDir")?,
            string_arg(&args, "backupPath")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
            string_arg(&args, "id")?,
            opt_string_arg(&args, "backupRolloutRelpath")?,
            bool_arg(&args, "overwrite")?,
        )),
        "restore_all" => to_result_value(backup::restore_all(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "backupDir")?,
            string_arg(&args, "backupPath")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
            bool_arg(&args, "overwrite")?,
        )),
        "delete_backup" => to_result_value(backup::delete_backup(
            string_arg(&args, "backupDir")?,
            string_arg(&args, "backupPath")?,
        )),
        "verify_backup" => to_result_value(backup::verify_backup(
            string_arg(&args, "backupDir")?,
            string_arg(&args, "backupPath")?,
        )),
        "stats_kpi" => to_result_value(stats::stats_kpi(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
            opt_i64_arg(&args, "fromTs")?,
            opt_i64_arg(&args, "toTs")?,
            arg(&args, "cwdFilter")?,
            bool_arg(&args, "includeArchived")?,
        )),
        "stats_timeseries" => to_result_value(stats::stats_timeseries(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
            opt_i64_arg(&args, "fromTs")?,
            opt_i64_arg(&args, "toTs")?,
            string_arg(&args, "bucket")?,
            arg(&args, "cwdFilter")?,
            bool_arg(&args, "includeArchived")?,
        )),
        "stats_by_project" => to_result_value(stats::stats_by_project(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
            opt_i64_arg(&args, "fromTs")?,
            opt_i64_arg(&args, "toTs")?,
            usize_arg(&args, "limit")?,
            arg(&args, "cwdFilter")?,
            bool_arg(&args, "includeArchived")?,
        )),
        "stats_by_model" => to_result_value(stats::stats_by_model(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
            opt_i64_arg(&args, "fromTs")?,
            opt_i64_arg(&args, "toTs")?,
            arg(&args, "cwdFilter")?,
            bool_arg(&args, "includeArchived")?,
        )),
        "stats_heatmap" => to_result_value(stats::stats_heatmap(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
            opt_i64_arg(&args, "fromTs")?,
            opt_i64_arg(&args, "toTs")?,
            arg(&args, "cwdFilter")?,
            bool_arg(&args, "includeArchived")?,
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
        "prune_orphan_entries" => to_result_value(repair::prune_orphan_entries(
            string_arg(&args, "codexDir")?,
            bool_arg(&args, "pruneIndex")?,
            bool_arg(&args, "pruneThreads")?,
            bool_arg(&args, "dryRun")?,
        )),
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
        "convert_session_provider" => to_result_value(convert::convert_session_with_lock(
            string_arg(&args, "codexDir")?,
            string_arg(&args, "claudeDir")?,
            string_arg(&args, "sourceProvider")?,
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
        "export_session_bundles" => to_result_value(bundle::export_session_bundles(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
            string_arg(&args, "outDir")?,
            arg(&args, "ids")?,
            opt_arg(&args, "targets")?,
            opt_string_arg(&args, "machineLabel")?,
            opt_string_arg(&args, "exportGroup")?,
        )),
        "export_all_bundles" => to_result_value(bundle::export_all_bundles(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
            string_arg(&args, "outDir")?,
            opt_string_arg(&args, "machineLabel")?,
            opt_string_arg(&args, "exportGroup")?,
            bool_arg(&args, "activeOnly")?,
        )),
        "list_bundles" => to_result_value(bundle::list_bundles(
            string_arg(&args, "srcDir")?,
            opt_string_arg(&args, "provider")?,
        )),
        "verify_bundles" => to_result_value(bundle::verify_bundles(
            string_arg(&args, "srcDir")?,
            opt_string_arg(&args, "provider")?,
        )),
        "import_session_bundles" => to_result_value(bundle::import_session_bundles(
            opt_string_arg(&args, "provider")?,
            string_arg(&args, "srcDir")?,
            string_arg(&args, "codexDir")?,
            opt_string_arg(&args, "claudeDir")?,
            enum_arg::<ImportMode>(&args, "mode")?,
            bool_arg(&args, "makeVisible")?,
            bool_arg(&args, "strict")?,
            arg::<Vec<ProjectPathMapping>>(&args, "projectMappings")?,
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
    let content_type = content_type(&final_path);
    let body = if head_only {
        Vec::new()
    } else if is_index_html(&final_path) {
        inject_runtime_config(fs::read_to_string(&final_path)?, state)?.into_bytes()
    } else {
        fs::read(&final_path)?
    };
    request.respond(binary_response(StatusCode(200), content_type, body))
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
    let config = json!({
        "apiToken": &state.api_token,
        "defaultProvider": &state.default_provider,
    });
    let script = format!(
        "<script>window.__CC_SESSIONS_WEBUI__ = {};</script>\n",
        serde_json::to_string(&config).expect("runtime config is serializable")
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
            candidates.push(PathBuf::from(raw));
        }
    }
    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd.join("dist"));
        candidates.push(cwd.join("webui"));
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("dist"));
            candidates.push(dir.join("webui"));
        }
    }

    for candidate in candidates {
        if candidate.join("index.html").is_file() {
            return Ok(candidate);
        }
    }

    Err(AppError::Other(
        "找不到 Web UI 前端构建产物。请先在项目根目录运行 npm run build，或把 dist 目录放在 cc-sessions 可执行文件旁。".into(),
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
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
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

fn bool_arg(args: &Value, name: &str) -> AppResult<bool> {
    arg(args, name)
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
    host.parse::<IpAddr>()
        .map(|_| ())
        .map_err(|_| AppError::Other(format!("无效 host: {host}")))
}
