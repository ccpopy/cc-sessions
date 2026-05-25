use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use cc_session_manager_lib::error::AppError;
use cc_session_manager_lib::models::{
    BackupSummary, BundleListItem, ImportMode, ProjectGroup, SessionSummary, Settings,
    SwitchStrategy,
};
use cc_session_manager_lib::{
    backup, bundle, family, fs_ops, paths, repair, rollout, sessions, settings, stats,
};
use serde::Serialize;

#[path = "../cli_menu.rs"]
mod menu;

type CliResult<T> = Result<T, CliError>;

#[derive(Debug)]
struct CliError(String);

impl CliError {
    fn message(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for CliError {}

impl From<AppError> for CliError {
    fn from(value: AppError) -> Self {
        Self(value.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(value: serde_json::Error) -> Self {
        Self(value.to_string())
    }
}

impl From<std::io::Error> for CliError {
    fn from(value: std::io::Error) -> Self {
        Self(value.to_string())
    }
}

struct CliContext {
    json: bool,
    provider: Option<String>,
    codex_dir: String,
    claude_dir: String,
    gemini_dir: String,
    family_lock: family::FamilyLock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionSort {
    Time,
    Size,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewMode {
    Conversation,
    ConversationAndReasoning,
    All,
}

fn main() {
    if let Err(err) = run_cli() {
        eprintln!("错误: {err}");
        std::process::exit(1);
    }
}

fn run_cli() -> CliResult<()> {
    let mut args: Vec<String> = env::args().skip(1).collect();

    let help = take_flag(&mut args, "-h") || take_flag(&mut args, "--help");
    let json = take_flag(&mut args, "--json");
    let provider = take_value(&mut args, "--provider")?;
    let codex_dir = take_value(&mut args, "--codex-dir")?
        .unwrap_or_else(|| paths::default_codex_dir().to_string_lossy().into_owned());
    let claude_dir = take_value(&mut args, "--claude-dir")?
        .unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned());
    let gemini_dir = take_value(&mut args, "--gemini-dir")?
        .unwrap_or_else(|| paths::default_gemini_dir().to_string_lossy().into_owned());

    if help {
        print_help();
        return Ok(());
    }

    let ctx = CliContext {
        json,
        provider,
        codex_dir,
        claude_dir,
        gemini_dir,
        family_lock: family::FamilyLock::default(),
    };

    let Some(command) = pop_command(&mut args) else {
        return menu::run(
            ctx.provider.clone(),
            ctx.codex_dir.clone(),
            ctx.claude_dir.clone(),
            ctx.gemini_dir.clone(),
        )
        .map_err(CliError::message);
    };

    match command.as_str() {
        "menu" => {
            ensure_no_args(&args)?;
            menu::run(
                ctx.provider.clone(),
                ctx.codex_dir.clone(),
                ctx.claude_dir.clone(),
                ctx.gemini_dir.clone(),
            )
            .map_err(CliError::message)
        }
        "version" => output(&ctx, &settings::app_version(), |version| {
            println!("{version}");
        }),
        "list" => cmd_list(&ctx, args),
        "search" => cmd_search(&ctx, args),
        "projects" => cmd_projects(&ctx, args),
        "preview" => cmd_preview(&ctx, args),
        "meta" => cmd_meta(&ctx, args),
        "resume-command" => cmd_resume_command(&ctx, args),
        "stats" => cmd_stats(&ctx, args),
        "backup" => cmd_backup(&ctx, args),
        "bundle" => cmd_bundle(&ctx, args),
        "repair" => cmd_repair(&ctx, args),
        "family" => cmd_family(&ctx, args),
        "settings" => cmd_settings(&ctx, args),
        other => Err(CliError::message(format!("未知命令: {other}"))),
    }
}

fn print_help() {
    println!(
        r#"cc-sessions - CC Sessions 命令行版本

用法:
  cc-sessions [全局选项] <命令> [命令选项]

不带命令会进入交互菜单，这是日常使用的推荐入口。

全局选项:
  --json                    输出 JSON
  --provider <codex|claude|gemini|all>
  --codex-dir <路径>         默认读取 ~/.codex
  --claude-dir <路径>        默认读取 ~/.claude
  --gemini-dir <路径>        默认读取 ~/.gemini
  -h, --help                显示帮助

常用命令:
  list [--archived] [--limit N] [--sort time|size]
  search <关键词> [--sort time|size]
  projects [--archived]
  preview <rollout路径> [--offset N] [--limit N] [--mode conversation|reasoning|all]
  meta <rollout路径>
  resume-command <session-id>
  stats <kpi|projects|models|timeseries|heatmap>
  backup <create|list|open|verify|delete|restore|restore-all>
  bundle <export|export-all|list|verify|import|pack|unpack>
  repair <provider-info|project-configs|diagnose|index|threads|prune|claude-history|clone|batch-clone|fork>
  family <store|verify|overlay|rollback|delete-branch|sync-states|sync-into-active|sync-active-into>
  settings <defaults|read|validate>
  menu

示例:
  cc-sessions
  cc-sessions menu
  cc-sessions list --limit 20 --sort size
  cc-sessions --provider claude search "hello"
  cc-sessions --provider gemini list --limit 20
  cc-sessions preview ~/.codex/sessions/.../rollout-xxx.jsonl --mode all --limit 40
  cc-sessions repair diagnose --json
  cc-sessions backup create --backup-dir ./backups --id <session-id> --name first-backup
"#
    );
}

fn cmd_list(ctx: &CliContext, mut args: Vec<String>) -> CliResult<()> {
    let include_archived = take_flag(&mut args, "--archived");
    let limit = take_usize(&mut args, "--limit")?.unwrap_or(usize::MAX);
    let sort = parse_session_sort(take_value(&mut args, "--sort")?)?;
    ensure_no_args(&args)?;

    let mut list = load_sessions(ctx, session_provider(ctx)?)?;
    if !include_archived {
        list.retain(|session| !session.archived);
    }
    sort_sessions(&mut list, sort);
    list.truncate(limit);
    output(ctx, &list, print_sessions)
}

fn cmd_search(ctx: &CliContext, mut args: Vec<String>) -> CliResult<()> {
    let include_archived = take_flag(&mut args, "--archived");
    let sort = parse_session_sort(take_value(&mut args, "--sort")?)?;
    let query = take_value(&mut args, "--query")?.unwrap_or_else(|| args.join(" "));
    if query.trim().is_empty() {
        return Err(CliError::message("search 需要关键词"));
    }
    args.clear();

    let provider = session_provider(ctx)?;
    let mut hits = if provider == "all" {
        let mut codex_hits = sessions::search_sessions(
            Some("codex".to_string()),
            ctx.codex_dir.clone(),
            Some(provider_aux_dir(ctx, "codex")),
            query.clone(),
        )?;
        codex_hits.extend(sessions::search_sessions(
            Some("claude".to_string()),
            ctx.codex_dir.clone(),
            Some(provider_aux_dir(ctx, "claude")),
            query.clone(),
        )?);
        codex_hits.extend(sessions::search_sessions(
            Some("gemini".to_string()),
            ctx.codex_dir.clone(),
            Some(provider_aux_dir(ctx, "gemini")),
            query,
        )?);
        codex_hits
    } else {
        sessions::search_sessions(
            Some(provider.clone()),
            ctx.codex_dir.clone(),
            Some(provider_aux_dir(ctx, &provider)),
            query,
        )?
    };
    if !include_archived {
        hits.retain(|session| !session.archived);
    }
    sort_sessions(&mut hits, sort);
    output(ctx, &hits, print_sessions)
}

fn cmd_projects(ctx: &CliContext, mut args: Vec<String>) -> CliResult<()> {
    let include_archived = take_flag(&mut args, "--archived");
    ensure_no_args(&args)?;

    let list = load_sessions(ctx, session_provider(ctx)?)?;
    let groups = group_projects(list, include_archived);
    output(ctx, &groups, print_project_groups)
}

fn cmd_preview(ctx: &CliContext, mut args: Vec<String>) -> CliResult<()> {
    let offset = take_usize(&mut args, "--offset")?.unwrap_or(0);
    let limit = take_usize(&mut args, "--limit")?.unwrap_or(40);
    let mode = parse_preview_mode(take_value(&mut args, "--mode")?)?;
    let path = take_value(&mut args, "--path")?.or_else(|| pop_command(&mut args));
    ensure_no_args(&args)?;
    let path = required(path, "preview 需要 rollout 路径")?;
    let events = collect_preview_events(concrete_provider(ctx)?, path, offset, limit, mode)?;
    output(ctx, &events, |events| {
        for event in events {
            println!(
                "{}\t{}\t{}\t{}",
                event.index,
                event.role,
                event.kind,
                event.text_summary.replace('\n', " ")
            );
        }
    })
}

fn collect_preview_events(
    provider: String,
    path: String,
    offset: usize,
    limit: usize,
    mode: PreviewMode,
) -> CliResult<Vec<cc_session_manager_lib::models::PreviewEvent>> {
    if mode == PreviewMode::All {
        return Ok(rollout::preview_session_range(
            Some(provider),
            path,
            offset,
            limit,
        )?);
    }

    let mut raw_offset = offset;
    let mut selected = Vec::with_capacity(limit);
    let batch = 100usize;
    loop {
        let events = rollout::preview_session_range(
            Some(provider.clone()),
            path.clone(),
            raw_offset,
            batch,
        )?;
        let fetched = events.len();
        if fetched == 0 {
            break;
        }
        for event in events {
            if preview_event_visible(&event, mode) {
                selected.push(event);
                if selected.len() >= limit {
                    return Ok(selected);
                }
            }
        }
        raw_offset += fetched;
        if fetched < batch {
            break;
        }
    }
    Ok(selected)
}

fn preview_event_visible(
    event: &cc_session_manager_lib::models::PreviewEvent,
    mode: PreviewMode,
) -> bool {
    match mode {
        PreviewMode::Conversation => rollout::preview_event_is_conversation(event),
        PreviewMode::ConversationAndReasoning => {
            rollout::preview_event_is_conversation_or_reasoning(event)
        }
        PreviewMode::All => true,
    }
}

fn cmd_meta(ctx: &CliContext, mut args: Vec<String>) -> CliResult<()> {
    let path = take_value(&mut args, "--path")?.or_else(|| pop_command(&mut args));
    ensure_no_args(&args)?;
    let path = required(path, "meta 需要 rollout 路径")?;
    let meta = rollout::preview_session_meta(Some(concrete_provider(ctx)?), path)?;
    output(ctx, &meta, |meta| {
        println!("id\t{}", meta.id.as_deref().unwrap_or(""));
        println!("cwd\t{}", meta.cwd.as_deref().unwrap_or(""));
        println!("timestamp\t{}", meta.timestamp.as_deref().unwrap_or(""));
        println!("source\t{}", meta.source.as_deref().unwrap_or(""));
        println!(
            "model_provider\t{}",
            meta.model_provider.as_deref().unwrap_or("")
        );
    })
}

fn cmd_resume_command(ctx: &CliContext, mut args: Vec<String>) -> CliResult<()> {
    let id = take_value(&mut args, "--id")?.or_else(|| pop_command(&mut args));
    ensure_no_args(&args)?;
    let id = required(id, "resume-command 需要 session id")?;
    let provider = concrete_provider(ctx)?;
    let command = if provider == "gemini" {
        load_sessions(ctx, provider.clone())?
            .into_iter()
            .find(|session| session.id == id)
            .map(|session| session.resume_command)
            .unwrap_or_else(|| format!("gemini --resume {id}"))
    } else {
        fs_ops::resume_command_text(Some(provider), id)?
    };
    output(ctx, &command, |command| println!("{command}"))
}

fn cmd_stats(ctx: &CliContext, mut args: Vec<String>) -> CliResult<()> {
    let Some(subcommand) = pop_command(&mut args) else {
        return Err(CliError::message("stats 需要子命令"));
    };
    let provider = ctx.provider.clone().unwrap_or_else(|| "all".to_string());
    let from_ts = take_i64(&mut args, "--from-ts")?;
    let to_ts = take_i64(&mut args, "--to-ts")?;
    let cwd_filter = take_values(&mut args, "--cwd")?;
    let include_archived = take_flag(&mut args, "--include-archived");
    let provider_dir = provider_aux_dir(ctx, &provider);

    match subcommand.as_str() {
        "kpi" => {
            ensure_no_args(&args)?;
            let data = stats::stats_kpi(
                Some(provider.clone()),
                ctx.codex_dir.clone(),
                Some(provider_dir.clone()),
                from_ts,
                to_ts,
                cwd_filter,
                include_archived,
            )?;
            output(ctx, &data, |data| {
                println!("sessions_total\t{}", data.sessions_total);
                println!("tokens_total\t{}", data.tokens_total);
                println!("active_projects\t{}", data.active_projects);
                println!("avg_tokens_per_session\t{:.2}", data.avg_tokens_per_session);
            })
        }
        "projects" => {
            let limit = take_usize(&mut args, "--limit")?.unwrap_or(20);
            ensure_no_args(&args)?;
            let data = stats::stats_by_project(
                Some(provider.clone()),
                ctx.codex_dir.clone(),
                Some(provider_dir.clone()),
                from_ts,
                to_ts,
                limit,
                cwd_filter,
                include_archived,
            )?;
            output(ctx, &data, |items| {
                println!("provider\tsessions\ttokens\tcwd");
                for item in items {
                    println!(
                        "{}\t{}\t{}\t{}",
                        item.provider.as_deref().unwrap_or(""),
                        item.sessions,
                        item.tokens,
                        item.cwd
                    );
                }
            })
        }
        "models" => {
            ensure_no_args(&args)?;
            let data = stats::stats_by_model(
                Some(provider.clone()),
                ctx.codex_dir.clone(),
                Some(provider_dir.clone()),
                from_ts,
                to_ts,
                cwd_filter,
                include_archived,
            )?;
            output(ctx, &data, |items| {
                println!("provider\tsessions\ttokens\tmodel\treasoning_effort");
                for item in items {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        item.provider.as_deref().unwrap_or(""),
                        item.sessions,
                        item.tokens,
                        item.model,
                        item.reasoning_effort.as_deref().unwrap_or("")
                    );
                }
            })
        }
        "timeseries" => {
            let bucket = take_value(&mut args, "--bucket")?.unwrap_or_else(|| "day".to_string());
            ensure_no_args(&args)?;
            let data = stats::stats_timeseries(
                Some(provider.clone()),
                ctx.codex_dir.clone(),
                Some(provider_dir.clone()),
                from_ts,
                to_ts,
                bucket,
                cwd_filter,
                include_archived,
            )?;
            output(ctx, &data, |items| {
                println!("bucket_start\tsessions\ttokens");
                for item in items {
                    println!("{}\t{}\t{}", item.bucket_start, item.sessions, item.tokens);
                }
            })
        }
        "heatmap" => {
            ensure_no_args(&args)?;
            let data = stats::stats_heatmap(
                Some(provider.clone()),
                ctx.codex_dir.clone(),
                Some(provider_dir.clone()),
                from_ts,
                to_ts,
                cwd_filter,
                include_archived,
            )?;
            output(ctx, &data, |grid| {
                for row in grid {
                    println!(
                        "{}",
                        row.iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join("\t")
                    );
                }
            })
        }
        other => Err(CliError::message(format!("未知 stats 子命令: {other}"))),
    }
}

fn cmd_backup(ctx: &CliContext, mut args: Vec<String>) -> CliResult<()> {
    let Some(subcommand) = pop_command(&mut args) else {
        return Err(CliError::message("backup 需要子命令"));
    };
    match subcommand.as_str() {
        "create" => {
            let backup_dir = backup_dir_or_default(&mut args)?;
            let ids = require_ids(&mut args)?;
            let name = take_value(&mut args, "--name")?;
            let note = take_value(&mut args, "--note")?;
            ensure_no_args(&args)?;
            let provider = concrete_provider(ctx)?;
            let summary = backup::create_backup(
                Some(provider.clone()),
                ctx.codex_dir.clone(),
                Some(provider_aux_dir(ctx, &provider)),
                backup_dir,
                ids,
                name,
                note,
            )?;
            output(ctx, &summary, print_backup_summary)
        }
        "list" => {
            let backup_dir = backup_dir_or_default(&mut args)?;
            ensure_no_args(&args)?;
            let summaries = backup::list_backups(backup_dir, Some(concrete_provider(ctx)?))?;
            output(ctx, &summaries, print_backup_summaries)
        }
        "open" => {
            let backup_path = backup_path_arg(&mut args)?;
            ensure_no_args(&args)?;
            let detail = backup::open_backup(backup_path)?;
            output(ctx, &detail, |detail| {
                print_backup_summary(&detail.summary);
                println!("sessions");
                for session in &detail.manifest.sessions {
                    println!(
                        "{}\t{}\t{}\thistory={}\t{}",
                        session.provider.as_deref().unwrap_or(""),
                        session.id,
                        session.bytes_rollout,
                        session.history_rows,
                        session.title
                    );
                }
            })
        }
        "verify" => {
            let backup_path = backup_path_arg(&mut args)?;
            ensure_no_args(&args)?;
            let report = backup::verify_backup(backup_path)?;
            output(ctx, &report, |report| {
                println!("all_ok\t{}", report.all_ok);
                for item in &report.items {
                    println!(
                        "{}\t{}\tmissing={}",
                        item.id,
                        if item.ok { "ok" } else { "bad" },
                        item.missing
                    );
                }
            })
        }
        "delete" => {
            let backup_path = backup_path_arg(&mut args)?;
            ensure_no_args(&args)?;
            backup::delete_backup(backup_path.clone())?;
            output(ctx, &backup_path, |path| println!("deleted\t{path}"))
        }
        "restore" => {
            let backup_path = backup_path_arg(&mut args)?;
            let id = required(take_value(&mut args, "--id")?, "restore 需要 --id")?;
            let overwrite = take_flag(&mut args, "--overwrite");
            ensure_no_args(&args)?;
            let provider = concrete_provider(ctx)?;
            let result = backup::restore_session(
                Some(provider.clone()),
                backup_path,
                ctx.codex_dir.clone(),
                Some(provider_aux_dir(ctx, &provider)),
                id,
                overwrite,
            )?;
            output(ctx, &result, |result| {
                println!(
                    "{}\tok={}\thistory_appended={}",
                    result.id, result.ok, result.history_appended
                );
                if let Some(error) = &result.error {
                    println!("error\t{error}");
                }
            })
        }
        "restore-all" => {
            let backup_path = backup_path_arg(&mut args)?;
            let overwrite = take_flag(&mut args, "--overwrite");
            ensure_no_args(&args)?;
            let provider = concrete_provider(ctx)?;
            let results = backup::restore_all(
                Some(provider.clone()),
                backup_path,
                ctx.codex_dir.clone(),
                Some(provider_aux_dir(ctx, &provider)),
                overwrite,
            )?;
            output(ctx, &results, |items| {
                for item in items {
                    println!(
                        "{}\tok={}\thistory_appended={}",
                        item.id, item.ok, item.history_appended
                    );
                    if let Some(error) = &item.error {
                        println!("{}\terror={}", item.id, error);
                    }
                }
            })
        }
        other => Err(CliError::message(format!("未知 backup 子命令: {other}"))),
    }
}

fn cmd_bundle(ctx: &CliContext, mut args: Vec<String>) -> CliResult<()> {
    let Some(subcommand) = pop_command(&mut args) else {
        return Err(CliError::message("bundle 需要子命令"));
    };
    match subcommand.as_str() {
        "export" => {
            let out_dir = required(take_value(&mut args, "--out-dir")?, "export 需要 --out-dir")?;
            let ids = require_ids(&mut args)?;
            let machine_label = take_value(&mut args, "--machine-label")?;
            let export_group = take_value(&mut args, "--export-group")?;
            ensure_no_args(&args)?;
            let provider = concrete_provider(ctx)?;
            let reports = bundle::export_session_bundles(
                Some(provider.clone()),
                ctx.codex_dir.clone(),
                Some(provider_aux_dir(ctx, &provider)),
                out_dir,
                ids,
                machine_label,
                export_group,
            )?;
            output(ctx, &reports, print_export_reports)
        }
        "export-all" => {
            let out_dir = required(
                take_value(&mut args, "--out-dir")?,
                "export-all 需要 --out-dir",
            )?;
            let machine_label = take_value(&mut args, "--machine-label")?;
            let export_group = take_value(&mut args, "--export-group")?;
            let active_only = take_flag(&mut args, "--active-only");
            ensure_no_args(&args)?;
            let provider = concrete_provider(ctx)?;
            let reports = bundle::export_all_bundles(
                Some(provider.clone()),
                ctx.codex_dir.clone(),
                Some(provider_aux_dir(ctx, &provider)),
                out_dir,
                machine_label,
                export_group,
                active_only,
            )?;
            output(ctx, &reports, print_export_reports)
        }
        "list" => {
            let src_dir = required(take_value(&mut args, "--src-dir")?, "list 需要 --src-dir")?;
            ensure_no_args(&args)?;
            let items = bundle::list_bundles(src_dir, Some(concrete_provider(ctx)?))?;
            output(ctx, &items, print_bundle_items)
        }
        "verify" => {
            let src_dir = required(take_value(&mut args, "--src-dir")?, "verify 需要 --src-dir")?;
            ensure_no_args(&args)?;
            let items = bundle::verify_bundles(src_dir, Some(concrete_provider(ctx)?))?;
            output(ctx, &items, print_bundle_items)
        }
        "import" => {
            let src_dir = required(take_value(&mut args, "--src-dir")?, "import 需要 --src-dir")?;
            let mode = parse_import_mode(take_value(&mut args, "--mode")?)?;
            let make_visible = take_flag(&mut args, "--make-visible");
            let strict = take_flag(&mut args, "--strict");
            ensure_no_args(&args)?;
            let provider = concrete_provider(ctx)?;
            let reports = bundle::import_session_bundles(
                Some(provider.clone()),
                src_dir,
                ctx.codex_dir.clone(),
                Some(provider_aux_dir(ctx, &provider)),
                mode,
                make_visible,
                strict,
                Vec::new(),
            )?;
            output(ctx, &reports, |reports| {
                for report in reports {
                    println!(
                        "{}\tok={}\tverified={}\t{}",
                        report.session_id,
                        report.ok,
                        report.verified,
                        report.skipped_reason.as_deref().unwrap_or("")
                    );
                    if let Some(error) = &report.error {
                        println!("{}\terror={}", report.session_id, error);
                    }
                }
            })
        }
        "pack" => {
            let src_dir = required(take_value(&mut args, "--src-dir")?, "pack 需要 --src-dir")?;
            let zip_path = required(take_value(&mut args, "--zip-path")?, "pack 需要 --zip-path")?;
            ensure_no_args(&args)?;
            let report = bundle::pack_bundles_zip(src_dir, zip_path)?;
            output(ctx, &report, |report| {
                println!(
                    "{}\tfiles={}\tbytes={}",
                    report.path, report.files, report.bytes
                );
            })
        }
        "unpack" => {
            let zip_path = required(
                take_value(&mut args, "--zip-path")?,
                "unpack 需要 --zip-path",
            )?;
            let dst_dir = required(take_value(&mut args, "--dst-dir")?, "unpack 需要 --dst-dir")?;
            ensure_no_args(&args)?;
            let report = bundle::unpack_zip(zip_path, dst_dir)?;
            output(ctx, &report, |report| {
                println!(
                    "{}\tfiles={}\tbytes={}",
                    report.path, report.files, report.bytes
                );
            })
        }
        other => Err(CliError::message(format!("未知 bundle 子命令: {other}"))),
    }
}

fn cmd_repair(ctx: &CliContext, mut args: Vec<String>) -> CliResult<()> {
    let Some(subcommand) = pop_command(&mut args) else {
        return Err(CliError::message("repair 需要子命令"));
    };
    match subcommand.as_str() {
        "provider-info" => {
            ensure_no_args(&args)?;
            let info = repair::get_provider_info(ctx.codex_dir.clone())?;
            output(ctx, &info, |info| {
                println!("current\t{}", info.current.as_deref().unwrap_or(""));
                println!("is_explicit\t{}", info.is_explicit);
                println!("config_path\t{}", info.config_path);
                println!("exists\t{}", info.exists);
            })
        }
        "project-configs" => {
            let fix = take_flag(&mut args, "--fix");
            let dry_run = take_flag(&mut args, "--dry-run");
            ensure_no_args(&args)?;
            if fix {
                let report = repair::repair_project_configs(ctx.codex_dir.clone(), dry_run)?;
                output(ctx, &report, |report| {
                    println!("scanned_projects\t{}", report.scanned_projects);
                    println!("config_files\t{}", report.config_files);
                    println!("issue_count\t{}", report.issue_count);
                    println!("repaired_count\t{}", report.repaired_count);
                    println!("dry_run\t{}", report.dry_run);
                    for item in &report.items {
                        println!(
                            "fixed\t{}\told={}\tnew={}\tchanged={}",
                            item.config_path,
                            item.old_default_wait_timeout_ms
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "(missing)".to_string()),
                            item.new_default_wait_timeout_ms,
                            item.changed
                        );
                    }
                    for error in &report.errors {
                        println!("error\t{error}");
                    }
                })
            } else {
                let report = repair::diagnose_project_configs(ctx.codex_dir.clone())?;
                output(ctx, &report, |report| {
                    println!("scanned_projects\t{}", report.scanned_projects);
                    println!("config_files\t{}", report.config_files);
                    println!("issue_count\t{}", report.issue_count);
                    println!("repairable_count\t{}", report.repairable_count);
                    for issue in &report.issues {
                        println!(
                            "issue\t{}\tsessions={}\trepairable={}\t{}",
                            issue.config_path, issue.session_count, issue.repairable, issue.message
                        );
                    }
                })
            }
        }
        "diagnose" => {
            ensure_no_args(&args)?;
            let report = repair::diagnose_codex_state(ctx.codex_dir.clone())?;
            output(ctx, &report, |report| {
                println!("rollout_count\t{}", report.rollout_count);
                println!("archived_rollout_count\t{}", report.archived_rollout_count);
                println!("index_count\t{}", report.index_count);
                println!("threads_count\t{}", report.threads_count);
                println!("missing_in_index\t{}", report.missing_in_index.len());
                println!("missing_in_threads\t{}", report.missing_in_threads.len());
                println!("orphan_in_index\t{}", report.orphan_in_index.len());
                println!("orphan_in_threads\t{}", report.orphan_in_threads.len());
                println!(
                    "provider_mismatched_families\t{}",
                    report.provider_mismatched_families
                );
            })
        }
        "index" => {
            let dry_run = take_flag(&mut args, "--dry-run");
            ensure_no_args(&args)?;
            let report = repair::repair_session_index(ctx.codex_dir.clone(), dry_run)?;
            output(ctx, &report, |report| {
                println!(
                    "scanned={}\twritten={}\tsalvaged={}\tdry_run={}",
                    report.scanned, report.written, report.salvaged, report.dry_run
                );
                for error in &report.errors {
                    println!("error\t{error}");
                }
            })
        }
        "threads" => {
            let dry_run = take_flag(&mut args, "--dry-run");
            ensure_no_args(&args)?;
            let report = repair::rebuild_threads_table(ctx.codex_dir.clone(), dry_run)?;
            output(ctx, &report, |report| {
                println!(
                    "scanned={}\tupserted={}\tskipped={}\tdry_run={}",
                    report.scanned, report.upserted, report.skipped, report.dry_run
                );
                for error in &report.errors {
                    println!("error\t{error}");
                }
            })
        }
        "prune" => {
            let prune_index = take_flag(&mut args, "--index");
            let prune_threads = take_flag(&mut args, "--threads");
            let dry_run = take_flag(&mut args, "--dry-run");
            ensure_no_args(&args)?;
            if !prune_index && !prune_threads {
                return Err(CliError::message("prune 需要显式指定 --index 或 --threads"));
            }
            let report = repair::prune_orphan_entries(
                ctx.codex_dir.clone(),
                prune_index,
                prune_threads,
                dry_run,
            )?;
            output(ctx, &report, |report| {
                println!(
                    "index_removed={}\tthreads_removed={}\tdry_run={}",
                    report.index_removed, report.threads_removed, report.dry_run
                );
            })
        }
        "claude-history" => {
            let prune = take_flag(&mut args, "--prune");
            let dry_run = take_flag(&mut args, "--dry-run");
            ensure_no_args(&args)?;
            if prune {
                let report = repair::prune_claude_history_orphans(ctx.claude_dir.clone(), dry_run)?;
                output(ctx, &report, |report| {
                    println!(
                        "removed_rows={}\tdry_run={}\thistory_path={}",
                        report.removed_rows, report.dry_run, report.history_path
                    );
                    for id in &report.orphan_session_ids {
                        println!("orphan_session_id\t{id}");
                    }
                })
            } else {
                let report = repair::diagnose_claude_history_orphans(ctx.claude_dir.clone())?;
                output(ctx, &report, |report| {
                    println!("history_path\t{}", report.history_path);
                    println!("session_count\t{}", report.session_count);
                    println!("history_rows\t{}", report.history_rows);
                    println!("linked_rows\t{}", report.linked_rows);
                    println!("orphan_rows\t{}", report.orphan_rows);
                    println!("untracked_rows\t{}", report.untracked_rows);
                    for id in &report.orphan_session_ids {
                        println!("orphan_session_id\t{id}");
                    }
                })
            }
        }
        "clone" => {
            let id = required(take_value(&mut args, "--id")?, "clone 需要 --id")?;
            let target_provider = take_value(&mut args, "--target-provider")?;
            let strategy = parse_switch_strategy(take_value(&mut args, "--strategy")?)?;
            let dry_run = take_flag(&mut args, "--dry-run");
            ensure_no_args(&args)?;
            let report = repair::clone_session_for_provider_with_lock(
                ctx.codex_dir.clone(),
                id,
                target_provider,
                strategy,
                dry_run,
                &ctx.family_lock,
            )?;
            output(ctx, &report, |report| {
                println!(
                    "{}\tok={}\tnew_id={}\t{}",
                    report.source_id,
                    report.ok,
                    report.new_id.as_deref().unwrap_or(""),
                    report.skipped_reason.as_deref().unwrap_or("")
                );
                if let Some(error) = &report.error {
                    println!("error\t{error}");
                }
            })
        }
        "batch-clone" => {
            let strategy = parse_switch_strategy(take_value(&mut args, "--strategy")?)?;
            let dry_run = take_flag(&mut args, "--dry-run");
            ensure_no_args(&args)?;
            let reports = repair::batch_clone_for_current_provider_with_lock(
                ctx.codex_dir.clone(),
                strategy,
                dry_run,
                &ctx.family_lock,
            )?;
            output(ctx, &reports, |reports| {
                for report in reports {
                    println!(
                        "{}\tok={}\tnew_id={}\t{}",
                        report.source_id,
                        report.ok,
                        report.new_id.as_deref().unwrap_or(""),
                        report.skipped_reason.as_deref().unwrap_or("")
                    );
                    if let Some(error) = &report.error {
                        println!("{}\terror={}", report.source_id, error);
                    }
                }
            })
        }
        "fork" => {
            let id = required(take_value(&mut args, "--id")?, "fork 需要 --id")?;
            let rollout_path = required(
                take_value(&mut args, "--rollout-path")?,
                "fork 需要 --rollout-path",
            )?;
            let event_index = required(
                take_usize(&mut args, "--event-index")?,
                "fork 需要 --event-index",
            )?;
            ensure_no_args(&args)?;
            let report = repair::fork_session_at_event_with_lock(
                ctx.codex_dir.clone(),
                id,
                rollout_path,
                event_index,
                &ctx.family_lock,
            )?;
            output(ctx, &report, |report| {
                println!(
                    "{}\tnew_id={}\tline={}\t{}",
                    report.source_id, report.new_id, report.event_index, report.cut_summary
                );
            })
        }
        other => Err(CliError::message(format!("未知 repair 子命令: {other}"))),
    }
}

fn cmd_family(ctx: &CliContext, mut args: Vec<String>) -> CliResult<()> {
    let Some(subcommand) = pop_command(&mut args) else {
        return Err(CliError::message("family 需要子命令"));
    };
    match subcommand.as_str() {
        "store" => {
            ensure_no_args(&args)?;
            let store =
                family::get_family_store_with_lock(ctx.codex_dir.clone(), &ctx.family_lock)?;
            output(ctx, &store, |store| {
                println!("families\t{}", store.families.len());
                println!("branches\t{}", store.index.len());
            })
        }
        "verify" => {
            ensure_no_args(&args)?;
            let report =
                family::verify_family_integrity_with_lock(ctx.codex_dir.clone(), &ctx.family_lock)?;
            output(ctx, &report, |report| {
                println!("all_ok\t{}", report.all_ok);
                for item in &report.items {
                    println!(
                        "{}\t{}\t{}",
                        item.family_id,
                        item.branch_id,
                        if item.ok { "ok" } else { "bad" }
                    );
                }
            })
        }
        "overlay" => {
            ensure_no_args(&args)?;
            let overlay = family::get_session_family_overlay_with_lock(
                ctx.codex_dir.clone(),
                &ctx.family_lock,
            )?;
            output(ctx, &overlay, |items| {
                println!("session_id\tprovider\tfamily_id\tbranches\tactive\tclone_state");
                for item in items {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        item.session_id,
                        item.provider.as_deref().unwrap_or(""),
                        item.family_id.as_deref().unwrap_or(""),
                        item.branch_count,
                        item.is_active_branch,
                        item.clone_state
                    );
                }
            })
        }
        "rollback" => {
            let family_id = required(
                take_value(&mut args, "--family-id")?,
                "rollback 需要 --family-id",
            )?;
            let branch_id = required(
                take_value(&mut args, "--branch-id")?,
                "rollback 需要 --branch-id",
            )?;
            ensure_no_args(&args)?;
            repair::rollback_family_active_with_lock(
                ctx.codex_dir.clone(),
                family_id.clone(),
                branch_id.clone(),
                &ctx.family_lock,
            )?;
            output(ctx, &(family_id, branch_id), |(family_id, branch_id)| {
                println!("active\t{}\t{}", family_id, branch_id);
            })
        }
        "delete-branch" => {
            let family_id = required(
                take_value(&mut args, "--family-id")?,
                "delete-branch 需要 --family-id",
            )?;
            let branch_id = required(
                take_value(&mut args, "--branch-id")?,
                "delete-branch 需要 --branch-id",
            )?;
            ensure_no_args(&args)?;
            let result = repair::delete_family_branch_with_lock(
                ctx.codex_dir.clone(),
                family_id,
                branch_id,
                &ctx.family_lock,
            )?;
            output(ctx, &result, |result| {
                println!("{}\tok={}", result.id, result.ok);
                if let Some(error) = &result.error {
                    println!("error\t{error}");
                }
            })
        }
        "sync-states" => {
            let family_id = required(
                take_value(&mut args, "--family-id")?,
                "sync-states 需要 --family-id",
            )?;
            ensure_no_args(&args)?;
            let states = repair::get_family_branch_sync_states_with_lock(
                ctx.codex_dir.clone(),
                family_id,
                &ctx.family_lock,
            )?;
            output(ctx, &states, |states| {
                println!("branch_id\trelation\tto_active\tto_branch\terror");
                for state in states {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        state.branch_id,
                        state.relation,
                        state.appendable_lines_to_active,
                        state.appendable_lines_to_branch,
                        state.error.as_deref().unwrap_or("")
                    );
                }
            })
        }
        "sync-into-active" => {
            let family_id = required(
                take_value(&mut args, "--family-id")?,
                "sync-into-active 需要 --family-id",
            )?;
            let source_branch_id = required(
                take_value(&mut args, "--source-branch-id")?,
                "sync-into-active 需要 --source-branch-id",
            )?;
            ensure_no_args(&args)?;
            let report = repair::sync_branch_into_active_with_lock(
                ctx.codex_dir.clone(),
                family_id,
                source_branch_id,
                &ctx.family_lock,
            )?;
            output(ctx, &report, |report| {
                println!(
                    "active={}\tsource={}\tappended={}\ttotal={}",
                    report.active_id, report.source_id, report.appended_lines, report.total_lines
                );
            })
        }
        "sync-active-into" => {
            let family_id = required(
                take_value(&mut args, "--family-id")?,
                "sync-active-into 需要 --family-id",
            )?;
            let target_branch_id = required(
                take_value(&mut args, "--target-branch-id")?,
                "sync-active-into 需要 --target-branch-id",
            )?;
            ensure_no_args(&args)?;
            let report = repair::sync_active_into_branch_with_lock(
                ctx.codex_dir.clone(),
                family_id,
                target_branch_id,
                &ctx.family_lock,
            )?;
            output(ctx, &report, |report| {
                println!(
                    "source={}\ttarget={}\tappended={}\ttotal={}",
                    report.source_id, report.target_id, report.appended_lines, report.total_lines
                );
            })
        }
        other => Err(CliError::message(format!("未知 family 子命令: {other}"))),
    }
}

fn cmd_settings(ctx: &CliContext, mut args: Vec<String>) -> CliResult<()> {
    let Some(subcommand) = pop_command(&mut args) else {
        return Err(CliError::message("settings 需要子命令"));
    };
    match subcommand.as_str() {
        "defaults" => {
            ensure_no_args(&args)?;
            let defaults = Settings::default();
            output(ctx, &defaults, |settings| {
                println!("codex_dir\t{}", settings.codex_dir);
                println!("claude_dir\t{}", settings.claude_dir);
                println!("gemini_dir\t{}", settings.gemini_dir);
                println!("backup_dir\t{}", settings.backup_dir);
                println!("refresh_interval_ms\t{}", settings.refresh_interval_ms);
            })
        }
        "read" => {
            let file = required(take_value(&mut args, "--file")?, "read 需要 --file")?;
            ensure_no_args(&args)?;
            let settings = settings::read_settings_file(Path::new(&file))?;
            output(ctx, &settings, |settings| {
                println!("codex_dir\t{}", settings.codex_dir);
                println!("claude_dir\t{}", settings.claude_dir);
                println!("gemini_dir\t{}", settings.gemini_dir);
                println!("backup_dir\t{}", settings.backup_dir);
                println!("refresh_interval_ms\t{}", settings.refresh_interval_ms);
            })
        }
        "validate" => {
            ensure_no_args(&args)?;
            let codex = settings::validate_codex_dir(ctx.codex_dir.clone())?;
            let claude = settings::validate_claude_dir(ctx.claude_dir.clone())?;
            let gemini = settings::validate_gemini_dir(ctx.gemini_dir.clone())?;
            let report = HashMap::from([
                ("codex", serde_json::to_value(codex)?),
                ("claude", serde_json::to_value(claude)?),
                ("gemini", serde_json::to_value(gemini)?),
            ]);
            output(ctx, &report, |report| {
                for (name, value) in report {
                    println!("{name}\t{value}");
                }
            })
        }
        other => Err(CliError::message(format!("未知 settings 子命令: {other}"))),
    }
}

fn load_sessions(ctx: &CliContext, provider: String) -> CliResult<Vec<SessionSummary>> {
    match provider.as_str() {
        "codex" | "claude" | "gemini" => list_provider_sessions(ctx, &provider),
        "all" => {
            let mut list = list_provider_sessions(ctx, "codex")?;
            list.extend(list_provider_sessions(ctx, "claude")?);
            list.extend(list_provider_sessions(ctx, "gemini")?);
            list.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
            Ok(list)
        }
        other => Err(CliError::message(format!("不支持的 provider: {other}"))),
    }
}

fn list_provider_sessions(ctx: &CliContext, provider: &str) -> CliResult<Vec<SessionSummary>> {
    Ok(sessions::list_sessions(
        Some(provider.to_string()),
        ctx.codex_dir.clone(),
        Some(provider_aux_dir(ctx, provider)),
    )?)
}

fn provider_aux_dir(ctx: &CliContext, provider: &str) -> String {
    if provider == "gemini" {
        ctx.gemini_dir.clone()
    } else {
        ctx.claude_dir.clone()
    }
}

fn sort_sessions(list: &mut [SessionSummary], sort: SessionSort) {
    match sort {
        SessionSort::Time => {
            list.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        }
        SessionSort::Size => {
            list.sort_by(|a, b| {
                a.tokens_used
                    .cmp(&b.tokens_used)
                    .then_with(|| a.rollout_bytes.cmp(&b.rollout_bytes))
                    .then_with(|| b.updated_at.cmp(&a.updated_at))
                    .then_with(|| a.id.cmp(&b.id))
            });
        }
    }
}

fn group_projects(list: Vec<SessionSummary>, include_archived: bool) -> Vec<ProjectGroup> {
    let mut groups: HashMap<String, ProjectGroup> = HashMap::new();
    for session in list {
        if !include_archived && session.archived {
            continue;
        }
        let entry = groups.entry(session.cwd.clone()).or_insert(ProjectGroup {
            cwd: session.cwd.clone(),
            cwd_display: session.cwd_display.clone(),
            sessions: Vec::new(),
            latest_updated_at: 0,
            total_tokens: 0,
        });
        entry.latest_updated_at = entry.latest_updated_at.max(session.updated_at);
        entry.total_tokens += session.tokens_used;
        entry.sessions.push(session);
    }
    let mut out: Vec<ProjectGroup> = groups.into_values().collect();
    out.sort_by_key(|group| std::cmp::Reverse(group.latest_updated_at));
    out
}

fn session_provider(ctx: &CliContext) -> CliResult<String> {
    let provider = ctx.provider.clone().unwrap_or_else(|| "codex".to_string());
    match provider.as_str() {
        "codex" | "claude" | "gemini" | "all" => Ok(provider),
        other => Err(CliError::message(format!("不支持的 provider: {other}"))),
    }
}

fn concrete_provider(ctx: &CliContext) -> CliResult<String> {
    let provider = session_provider(ctx)?;
    if provider == "all" {
        Err(CliError::message(
            "此命令只支持 --provider codex、--provider claude 或 --provider gemini",
        ))
    } else {
        Ok(provider)
    }
}

fn backup_dir_or_default(args: &mut Vec<String>) -> CliResult<String> {
    Ok(take_value(args, "--backup-dir")?
        .unwrap_or_else(|| paths::default_backup_dir().to_string_lossy().into_owned()))
}

fn backup_path_arg(args: &mut Vec<String>) -> CliResult<String> {
    let path = take_value(args, "--backup-path")?.or_else(|| pop_command(args));
    required(path, "需要 --backup-path 或位置参数")
}

fn parse_switch_strategy(value: Option<String>) -> CliResult<SwitchStrategy> {
    match value.as_deref().unwrap_or("continuous") {
        "continuous" => Ok(SwitchStrategy::Continuous),
        "scatter" => Ok(SwitchStrategy::Scatter),
        "follow" => Ok(SwitchStrategy::Follow),
        other => Err(CliError::message(format!(
            "不支持的 strategy: {other}，可用值: continuous, scatter, follow"
        ))),
    }
}

fn parse_session_sort(value: Option<String>) -> CliResult<SessionSort> {
    match value.as_deref().unwrap_or("time") {
        "time" => Ok(SessionSort::Time),
        "size" => Ok(SessionSort::Size),
        other => Err(CliError::message(format!(
            "不支持的 sort: {other}，可用值: time, size"
        ))),
    }
}

fn parse_preview_mode(value: Option<String>) -> CliResult<PreviewMode> {
    match value.as_deref().unwrap_or("conversation") {
        "conversation" => Ok(PreviewMode::Conversation),
        "reasoning" | "conversation-and-reasoning" | "conversation_and_reasoning" => {
            Ok(PreviewMode::ConversationAndReasoning)
        }
        "all" => Ok(PreviewMode::All),
        other => Err(CliError::message(format!(
            "不支持的 preview mode: {other}，可用值: conversation, reasoning, all"
        ))),
    }
}

fn parse_import_mode(value: Option<String>) -> CliResult<ImportMode> {
    match value.as_deref().unwrap_or("skip") {
        "skip" => Ok(ImportMode::Skip),
        "overwrite" => Ok(ImportMode::Overwrite),
        "keep-local" | "keep_local" => Ok(ImportMode::KeepLocal),
        other => Err(CliError::message(format!(
            "不支持的 import mode: {other}，可用值: skip, overwrite, keep-local"
        ))),
    }
}

fn print_sessions(sessions: &Vec<SessionSummary>) {
    println!("updated_at\tprovider\tarchived\ttokens\tbytes\tid\tcwd\ttitle");
    for session in sessions {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            session.updated_at,
            session.provider,
            session.archived,
            session.tokens_used,
            session.rollout_bytes,
            session.id,
            session.cwd,
            compact(&session.title, 80)
        );
    }
}

fn print_project_groups(groups: &Vec<ProjectGroup>) {
    println!("sessions\ttokens\tupdated_at\tcwd");
    for group in groups {
        println!(
            "{}\t{}\t{}\t{}",
            group.sessions.len(),
            group.total_tokens,
            group.latest_updated_at,
            group.cwd
        );
    }
}

fn print_backup_summaries(items: &Vec<BackupSummary>) {
    println!("created_at\tprovider\tsessions\tbytes\tname\tpath");
    for item in items {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            item.created_at,
            item.provider.as_deref().unwrap_or(""),
            item.sessions_count,
            item.total_bytes,
            item.name,
            item.path
        );
    }
}

fn print_backup_summary(summary: &BackupSummary) {
    println!("name\t{}", summary.name);
    println!("path\t{}", summary.path);
    println!("provider\t{}", summary.provider.as_deref().unwrap_or(""));
    println!("created_at\t{}", summary.created_at);
    println!("sessions_count\t{}", summary.sessions_count);
    println!("total_bytes\t{}", summary.total_bytes);
}

fn print_export_reports(reports: &Vec<cc_session_manager_lib::models::ExportReport>) {
    for report in reports {
        println!(
            "{}\tok={}\t{}",
            report.session_id,
            report.ok,
            report.bundle_path.as_deref().unwrap_or("")
        );
        if let Some(reason) = &report.skipped_reason {
            println!("{}\tskipped={}", report.session_id, reason);
        }
        if let Some(error) = &report.error {
            println!("{}\terror={}", report.session_id, error);
        }
    }
}

fn print_bundle_items(items: &Vec<BundleListItem>) {
    println!("verified\tprovider\tsession_id\tupdated_at\tbundle_dir");
    for item in items {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            item.verified
                .map(|value| value.to_string())
                .unwrap_or_else(|| "".to_string()),
            item.manifest.provider.as_deref().unwrap_or(""),
            item.manifest.session_id,
            item.manifest.updated_at,
            item.bundle_dir
        );
    }
}

fn output<T: Serialize>(ctx: &CliContext, value: &T, text: impl FnOnce(&T)) -> CliResult<()> {
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        text(value);
    }
    Ok(())
}

fn compact(value: &str, max_chars: usize) -> String {
    let flat = value.replace(['\r', '\n'], " ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let mut out = flat.chars().take(max_chars).collect::<String>();
    out.push_str("...");
    out
}

fn pop_command(args: &mut Vec<String>) -> Option<String> {
    if args.is_empty() {
        None
    } else {
        Some(args.remove(0))
    }
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let mut found = false;
    while let Some(index) = args.iter().position(|arg| arg == flag) {
        args.remove(index);
        found = true;
    }
    found
}

fn take_value(args: &mut Vec<String>, name: &str) -> CliResult<Option<String>> {
    let Some(index) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    if index + 1 >= args.len() {
        return Err(CliError::message(format!("{name} 需要一个值")));
    }
    let value = args.remove(index + 1);
    args.remove(index);
    Ok(Some(value))
}

fn take_values(args: &mut Vec<String>, name: &str) -> CliResult<Vec<String>> {
    let mut out = Vec::new();
    while let Some(value) = take_value(args, name)? {
        out.push(value);
    }
    Ok(out)
}

fn take_usize(args: &mut Vec<String>, name: &str) -> CliResult<Option<usize>> {
    take_value(args, name)?
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| CliError::message(format!("{name} 需要非负整数")))
        })
        .transpose()
}

fn take_i64(args: &mut Vec<String>, name: &str) -> CliResult<Option<i64>> {
    take_value(args, name)?
        .map(|value| {
            value
                .parse::<i64>()
                .map_err(|_| CliError::message(format!("{name} 需要整数时间戳")))
        })
        .transpose()
}

fn require_ids(args: &mut Vec<String>) -> CliResult<Vec<String>> {
    let mut ids = take_values(args, "--id")?;
    if ids.is_empty() {
        ids.extend(args.drain(..));
    }
    if ids.is_empty() {
        return Err(CliError::message("需要至少一个 --id 或位置参数 id"));
    }
    Ok(ids)
}

fn required<T>(value: Option<T>, message: &str) -> CliResult<T> {
    value.ok_or_else(|| CliError::message(message))
}

fn ensure_no_args(args: &[String]) -> CliResult<()> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(CliError::message(format!(
            "无法识别的参数: {}",
            args.join(" ")
        )))
    }
}

#[allow(dead_code)]
fn normalize_path(value: String) -> String {
    PathBuf::from(value).to_string_lossy().into_owned()
}
