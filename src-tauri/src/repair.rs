//! 修复 / 诊断：
//!
//! - `diagnose_codex_state`：扫描 rollout、session_index、threads 三边差集
//! - `repair_session_index`：从 rollout 批量重建 session_index.jsonl
//! - `rebuild_threads_table`：从 rollout 批量 upsert state_5.sqlite 的 threads 表
//! - `clone_session_for_provider`：把会话"克隆到当前 provider"（三种策略）
//! - `batch_clone_for_current_provider`：对所有 provider 不匹配的家族做批量克隆

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::family;
use crate::models::{
    BranchStatus, BranchSyncReport, BranchSyncState, CloneReport, DiagnosticReport, Family,
    FamilyBranch, ForkSessionReport, GuiVisibilityFixReport, GuiVisibilityIssue,
    GuiVisibilityReport, HistoryOrphanReport, HistoryPruneReport, IndexRepairReport,
    OrphanPruneReport, ProjectConfigIssue, ProjectConfigRepairItem, ProjectConfigRepairReport,
    ProjectConfigReport, ProviderInfo, SwitchStrategy, SyncBranchReport, ThreadsRebuildReport,
};

/// Codex CLI 的内建默认 provider（与官方文档一致）。
/// 未在 config.toml 里显式写 model_provider 时，Codex 自己就按 "openai" 处理；
/// ChatGPT OAuth 登录与 OpenAI API key 场景都是这个值。
pub(crate) const DEFAULT_PROVIDER: &str = "openai";
const DEFAULT_THREAD_SOURCE: &str = "cli";
const DEFAULT_SANDBOX_POLICY: &str = "read-only";
const DEFAULT_APPROVAL_MODE: &str = "on-request";
const DEFAULT_MEMORY_MODE: &str = "enabled";
use crate::paths;
use crate::state_db;

// ========================= 读当前 provider =========================

/// 给其他模块使用的导出版本（只返回 provider，不返回 exists）。
/// 返回值会落到 Codex 默认值 `openai`，便于下游按照"生效 provider"比较。
pub(crate) fn read_current_provider_export(codex_dir: &Path) -> Option<String> {
    Some(effective_current_provider(codex_dir))
}

/// 显式读取 config.toml 顶层的 `model_provider`，仅当字段存在时才返回 Some。
fn read_explicit_provider(codex_dir: &Path) -> (Option<String>, bool) {
    let p = paths::config_toml_path(codex_dir);
    if !p.is_file() {
        return (None, false);
    }
    let raw = match fs::read_to_string(&p) {
        Ok(v) => v,
        Err(_) => return (None, true),
    };
    // 严格 TOML：只取顶层 `model_provider`，避免 `[model_providers.xxx]` 子表误匹配。
    match raw.parse::<toml::Value>() {
        Ok(toml::Value::Table(tbl)) => {
            let v = tbl
                .get("model_provider")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            (v, true)
        }
        _ => (None, true),
    }
}

/// 返回 Codex 实际生效的 provider：显式值优先，否则默认 `openai`。
fn effective_current_provider(codex_dir: &Path) -> String {
    read_explicit_provider(codex_dir)
        .0
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_string())
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn get_provider_info(codex_dir: String) -> AppResult<ProviderInfo> {
    let p = PathBuf::from(&codex_dir);
    let cfg = paths::config_toml_path(&p);
    let (explicit, exists) = read_explicit_provider(&p);
    let is_explicit = explicit.is_some();
    let current = explicit.or_else(|| Some(DEFAULT_PROVIDER.to_string()));
    Ok(ProviderInfo {
        current,
        is_explicit,
        config_path: cfg.to_string_lossy().into_owned(),
        exists,
    })
}

// ========================= 项目级 Codex 配置诊断 =========================

const PROJECT_CODEX_CONFIG_RELPATH: [&str; 2] = [".codex", "config.toml"];
const MULTI_AGENT_V2_SECTION: &str = "features.multi_agent_v2";
const DEFAULT_WAIT_TIMEOUT_KEY: &str = "default_wait_timeout_ms";
const MIN_WAIT_TIMEOUT_KEY: &str = "min_wait_timeout_ms";
const MAX_WAIT_TIMEOUT_KEY: &str = "max_wait_timeout_ms";

#[derive(Debug, Clone)]
struct ProjectConfigCandidate {
    project_cwd: PathBuf,
    config_path: PathBuf,
    session_ids: BTreeSet<String>,
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn diagnose_project_configs(codex_dir: String) -> AppResult<ProjectConfigReport> {
    let codex = PathBuf::from(&codex_dir);
    let (scanned_projects, candidates) = collect_project_config_candidates(&codex)?;
    let mut issues = Vec::new();

    for candidate in candidates.values() {
        match diagnose_project_config_candidate(candidate) {
            Ok(Some(issue)) => issues.push(issue),
            Ok(None) => {}
            Err(err) => issues.push(project_config_issue(
                candidate,
                None,
                None,
                None,
                None,
                false,
                format!("读取或解析项目 config.toml 失败：{err}"),
            )),
        }
    }

    let repairable_count = issues.iter().filter(|issue| issue.repairable).count() as u32;
    Ok(ProjectConfigReport {
        scanned_projects,
        config_files: candidates.len() as u32,
        issue_count: issues.len() as u32,
        repairable_count,
        issues,
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn repair_project_configs(
    codex_dir: String,
    dry_run: bool,
) -> AppResult<ProjectConfigRepairReport> {
    let report = diagnose_project_configs(codex_dir)?;
    let mut items = Vec::new();
    let mut errors = Vec::new();

    for issue in report.issues.iter().filter(|issue| issue.repairable) {
        let Some(next_default) = issue.suggested_default_wait_timeout_ms else {
            errors.push(format!(
                "{}: repairable=true 但缺少建议 default_wait_timeout_ms",
                issue.config_path
            ));
            continue;
        };
        let changed = if dry_run {
            issue.current_default_wait_timeout_ms != Some(next_default)
        } else {
            match upsert_project_default_wait_timeout(Path::new(&issue.config_path), next_default) {
                Ok(changed) => changed,
                Err(err) => {
                    errors.push(format!("{}: {err}", issue.config_path));
                    continue;
                }
            }
        };
        items.push(ProjectConfigRepairItem {
            project_cwd: issue.project_cwd.clone(),
            config_path: issue.config_path.clone(),
            changed,
            dry_run,
            old_default_wait_timeout_ms: issue.current_default_wait_timeout_ms,
            new_default_wait_timeout_ms: next_default,
        });
    }

    let repaired_count = items.iter().filter(|item| item.changed).count() as u32;
    Ok(ProjectConfigRepairReport {
        scanned_projects: report.scanned_projects,
        config_files: report.config_files,
        issue_count: report.issue_count,
        repaired_count,
        dry_run,
        items,
        errors,
    })
}

fn collect_project_config_candidates(
    codex: &Path,
) -> AppResult<(u32, BTreeMap<PathBuf, ProjectConfigCandidate>)> {
    let mut projects: BTreeSet<PathBuf> = BTreeSet::new();
    let mut candidates: BTreeMap<PathBuf, ProjectConfigCandidate> = BTreeMap::new();

    for rollout_path in family::scan_rollouts(codex) {
        let Some(brief) = read_rollout_brief(codex, &rollout_path)? else {
            continue;
        };
        let Some(cwd) = brief.cwd.as_deref().map(normalize_project_cwd) else {
            continue;
        };
        if cwd.as_os_str().is_empty() {
            continue;
        }

        projects.insert(cwd.clone());
        let config_path = cwd
            .join(PROJECT_CODEX_CONFIG_RELPATH[0])
            .join(PROJECT_CODEX_CONFIG_RELPATH[1]);
        if !config_path.is_file() {
            continue;
        }

        candidates
            .entry(config_path.clone())
            .and_modify(|candidate| {
                candidate.session_ids.insert(brief.id.clone());
            })
            .or_insert_with(|| {
                let mut session_ids = BTreeSet::new();
                session_ids.insert(brief.id);
                ProjectConfigCandidate {
                    project_cwd: cwd,
                    config_path,
                    session_ids,
                }
            });
    }

    Ok((projects.len() as u32, candidates))
}

fn normalize_project_cwd(raw: &str) -> PathBuf {
    PathBuf::from(paths::strip_verbatim(raw.trim()))
}

fn diagnose_project_config_candidate(
    candidate: &ProjectConfigCandidate,
) -> AppResult<Option<ProjectConfigIssue>> {
    let raw = fs::read_to_string(&candidate.config_path)?;
    let parsed = raw.parse::<toml::Value>().map_err(|err| {
        AppError::Other(format!(
            "config.toml 不是有效 TOML，Codex 恢复会话会直接失败：{err}"
        ))
    })?;

    let Some(table) = parsed.as_table() else {
        return Ok(Some(project_config_issue(
            candidate,
            None,
            None,
            None,
            None,
            false,
            "config.toml 顶层不是 TOML 表，Codex 无法按配置文件读取".to_string(),
        )));
    };
    let Some(features) = table.get("features").and_then(|v| v.as_table()) else {
        return Ok(None);
    };
    let Some(multi_agent) = features.get("multi_agent_v2").and_then(|v| v.as_table()) else {
        return Ok(None);
    };

    let min_wait = read_timeout_value(multi_agent, MIN_WAIT_TIMEOUT_KEY).map_err(|msg| {
        AppError::Other(format!(
            "features.multi_agent_v2.{MIN_WAIT_TIMEOUT_KEY}: {msg}"
        ))
    })?;
    let default_wait =
        read_timeout_value(multi_agent, DEFAULT_WAIT_TIMEOUT_KEY).map_err(|msg| {
            AppError::Other(format!(
                "features.multi_agent_v2.{DEFAULT_WAIT_TIMEOUT_KEY}: {msg}"
            ))
        })?;
    let max_wait = read_timeout_value(multi_agent, MAX_WAIT_TIMEOUT_KEY).map_err(|msg| {
        AppError::Other(format!(
            "features.multi_agent_v2.{MAX_WAIT_TIMEOUT_KEY}: {msg}"
        ))
    })?;

    if let (Some(min), Some(max)) = (min_wait, max_wait) {
        if min > max {
            return Ok(Some(project_config_issue(
                candidate,
                min_wait,
                default_wait,
                max_wait,
                None,
                false,
                format!(
                    "{MIN_WAIT_TIMEOUT_KEY}={min} 大于 {MAX_WAIT_TIMEOUT_KEY}={max}，需要人工决定修改哪个边界值"
                ),
            )));
        }
    }

    let suggestion = suggested_default_wait_timeout(min_wait, default_wait, max_wait);
    let Some(next_default) = suggestion else {
        return Ok(None);
    };

    if let Some(max) = max_wait {
        if next_default > max {
            return Ok(Some(project_config_issue(
                candidate,
                min_wait,
                default_wait,
                max_wait,
                None,
                false,
                format!(
                    "建议 default_wait_timeout_ms={next_default} 会超过 {MAX_WAIT_TIMEOUT_KEY}={max}，需要人工调整边界值"
                ),
            )));
        }
    }

    let message = project_config_issue_message(min_wait, default_wait, max_wait, next_default);
    Ok(Some(project_config_issue(
        candidate,
        min_wait,
        default_wait,
        max_wait,
        Some(next_default),
        true,
        message,
    )))
}

fn read_timeout_value(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<Option<u64>, String> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let Some(n) = value.as_integer() else {
        return Err("必须是非负整数毫秒值".to_string());
    };
    if n < 0 {
        return Err("不能是负数".to_string());
    }
    Ok(Some(n as u64))
}

fn suggested_default_wait_timeout(
    min_wait: Option<u64>,
    default_wait: Option<u64>,
    max_wait: Option<u64>,
) -> Option<u64> {
    match default_wait {
        Some(default) => {
            if let Some(min) = min_wait {
                if default < min {
                    return Some(min);
                }
            }
            if let Some(max) = max_wait {
                if default > max {
                    return Some(max);
                }
            }
            None
        }
        None => min_wait.or(max_wait),
    }
}

fn project_config_issue_message(
    min_wait: Option<u64>,
    default_wait: Option<u64>,
    max_wait: Option<u64>,
    next_default: u64,
) -> String {
    match default_wait {
        None => {
            if min_wait.is_some() && max_wait.is_some() {
                format!(
                    "{MIN_WAIT_TIMEOUT_KEY} 或 {MAX_WAIT_TIMEOUT_KEY} 已显式设置，但缺少 {DEFAULT_WAIT_TIMEOUT_KEY}；将补为 {next_default}"
                )
            } else if min_wait.is_some() {
                format!(
                    "{MIN_WAIT_TIMEOUT_KEY} 已显式设置，但缺少 {DEFAULT_WAIT_TIMEOUT_KEY}；新版 Codex 会用内置默认值参与校验，可能小于最小值。将补为 {next_default}"
                )
            } else {
                format!(
                    "{MAX_WAIT_TIMEOUT_KEY} 已显式设置，但缺少 {DEFAULT_WAIT_TIMEOUT_KEY}；新版 Codex 会用内置默认值参与校验，可能大于最大值。将补为 {next_default}"
                )
            }
        }
        Some(default) if min_wait.is_some_and(|min| default < min) => format!(
            "{DEFAULT_WAIT_TIMEOUT_KEY}={default} 小于 {MIN_WAIT_TIMEOUT_KEY}={}；将改为 {next_default}",
            min_wait.unwrap()
        ),
        Some(default) if max_wait.is_some_and(|max| default > max) => format!(
            "{DEFAULT_WAIT_TIMEOUT_KEY}={default} 大于 {MAX_WAIT_TIMEOUT_KEY}={}；将改为 {next_default}",
            max_wait.unwrap()
        ),
        _ => format!("{DEFAULT_WAIT_TIMEOUT_KEY} 将改为 {next_default}"),
    }
}

fn project_config_issue(
    candidate: &ProjectConfigCandidate,
    min_wait: Option<u64>,
    default_wait: Option<u64>,
    max_wait: Option<u64>,
    suggested_default: Option<u64>,
    repairable: bool,
    message: String,
) -> ProjectConfigIssue {
    let session_ids: Vec<String> = candidate.session_ids.iter().cloned().collect();
    ProjectConfigIssue {
        project_cwd: candidate.project_cwd.to_string_lossy().into_owned(),
        config_path: candidate.config_path.to_string_lossy().into_owned(),
        session_count: session_ids.len() as u32,
        session_ids,
        current_min_wait_timeout_ms: min_wait,
        current_default_wait_timeout_ms: default_wait,
        current_max_wait_timeout_ms: max_wait,
        suggested_default_wait_timeout_ms: suggested_default,
        repairable,
        message,
    }
}

fn upsert_project_default_wait_timeout(config_path: &Path, value: u64) -> AppResult<bool> {
    let raw = fs::read_to_string(config_path)?;
    raw.parse::<toml::Value>()
        .map_err(|err| AppError::Other(format!("config.toml 不是有效 TOML：{err}")))?;

    let newline = if raw.contains("\r\n") { "\r\n" } else { "\n" };
    let had_final_newline = raw.ends_with('\n');
    let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
    let Some((section_start, section_end)) =
        find_toml_section_range(&lines, MULTI_AGENT_V2_SECTION)
    else {
        return Err(AppError::Other(format!(
            "未找到 [{MULTI_AGENT_V2_SECTION}] 配置段"
        )));
    };

    for line in lines.iter_mut().take(section_end).skip(section_start + 1) {
        if is_toml_key_assignment(line, DEFAULT_WAIT_TIMEOUT_KEY) {
            let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
            let next_line = format!("{indent}{DEFAULT_WAIT_TIMEOUT_KEY} = {value}");
            if *line == next_line {
                return Ok(false);
            }
            *line = next_line;
            return write_valid_project_config(config_path, raw, lines, newline, had_final_newline);
        }
    }

    let insert_after =
        find_key_line_in_range(&lines, section_start + 1, section_end, MIN_WAIT_TIMEOUT_KEY)
            .or_else(|| {
                find_key_line_in_range(&lines, section_start + 1, section_end, MAX_WAIT_TIMEOUT_KEY)
            })
            .unwrap_or(section_start);
    lines.insert(
        insert_after + 1,
        format!("{DEFAULT_WAIT_TIMEOUT_KEY} = {value}"),
    );
    write_valid_project_config(config_path, raw, lines, newline, had_final_newline)
}

fn write_valid_project_config(
    config_path: &Path,
    old_raw: String,
    lines: Vec<String>,
    newline: &str,
    final_newline: bool,
) -> AppResult<bool> {
    let mut next_raw = lines.join(newline);
    if final_newline {
        next_raw.push_str(newline);
    }
    next_raw
        .parse::<toml::Value>()
        .map_err(|err| AppError::Other(format!("修改后的 config.toml 不是有效 TOML：{err}")))?;
    if next_raw == old_raw {
        return Ok(false);
    }
    fs::write(config_path, next_raw)?;
    Ok(true)
}

fn find_toml_section_range(lines: &[String], section: &str) -> Option<(usize, usize)> {
    let header = format!("[{section}]");
    let start = lines
        .iter()
        .position(|line| line.trim() == header.as_str())?;
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(idx, line)| {
            let trimmed = line.trim_start();
            (trimmed.starts_with('[') && !trimmed.starts_with("[[")).then_some(idx)
        })
        .unwrap_or(lines.len());
    Some((start, end))
}

fn find_key_line_in_range(lines: &[String], start: usize, end: usize, key: &str) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .take(end)
        .skip(start)
        .find_map(|(idx, line)| is_toml_key_assignment(line, key).then_some(idx))
}

fn is_toml_key_assignment(line: &str, key: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || !trimmed.starts_with(key) {
        return false;
    }
    let rest = &trimmed[key.len()..];
    rest.trim_start().starts_with('=')
}

// ========================= 诊断 =========================

pub(crate) struct RolloutBrief {
    pub(crate) path: PathBuf,
    pub(crate) relpath: PathBuf,
    pub(crate) id: String,
    pub(crate) model_provider: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) sandbox_policy: Option<String>,
    pub(crate) approval_mode: Option<String>,
    pub(crate) memory_mode: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) first_user_message: String,
    pub(crate) tokens_used: i64,
    pub(crate) updated_at_ms: i64,
    pub(crate) created_at_ms: i64,
}

pub(crate) fn read_rollout_brief(codex_dir: &Path, path: &Path) -> AppResult<Option<RolloutBrief>> {
    let f = fs::File::open(path)?;
    let reader = BufReader::new(f);
    let mut id: Option<String> = None;
    let mut model_provider: Option<String> = None;
    let mut source: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut sandbox_policy: Option<String> = None;
    let mut approval_mode: Option<String> = None;
    let mut memory_mode: Option<String> = None;
    let mut model: Option<String> = None;
    let mut reasoning_effort: Option<String> = None;
    let mut first_user: Option<String> = None;
    let mut tokens_used: i64 = 0;
    let mut created_ms: i64 = 0;
    let mut last_ms: i64 = 0;
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(x) => x,
            Err(_) => continue,
        };
        // 时间戳（顶层可能是 ISO8601 字符串）
        if let Some(ts) = v.get("timestamp").and_then(|x| x.as_str()) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                let ms = dt.timestamp_millis();
                if created_ms == 0 {
                    created_ms = ms;
                }
                last_ms = ms;
            }
        }
        let outer_type = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        if let Some(total) = crate::rollout::token_total_from_value(&v) {
            tokens_used = total;
        }
        match outer_type {
            "session_meta" => {
                let payload = v.get("payload");
                id = id.or_else(|| {
                    payload
                        .and_then(|p| p.get("id"))
                        .and_then(|x| x.as_str())
                        .map(String::from)
                });
                model_provider = model_provider.or_else(|| {
                    payload
                        .and_then(|p| p.get("model_provider"))
                        .and_then(|x| x.as_str())
                        .map(String::from)
                });
                source =
                    source.or_else(|| payload.and_then(|p| metadata_string_field(p, "source")));
                cwd = cwd.or_else(|| {
                    payload
                        .and_then(|p| p.get("cwd"))
                        .and_then(|x| x.as_str())
                        .map(String::from)
                });
                memory_mode = memory_mode
                    .or_else(|| payload.and_then(|p| metadata_string_field(p, "memory_mode")));
            }
            "turn_context" => {
                let payload = v.get("payload").unwrap_or(&v);
                if cwd.is_none() {
                    cwd = payload
                        .get("cwd")
                        .and_then(|x| x.as_str())
                        .map(String::from);
                }
                sandbox_policy =
                    sandbox_policy.or_else(|| metadata_string_field(payload, "sandbox_policy"));
                approval_mode = approval_mode
                    .or_else(|| metadata_string_field(payload, "approval_policy"))
                    .or_else(|| metadata_string_field(payload, "approval_mode"));
                model = model.or_else(|| {
                    payload
                        .get("model")
                        .and_then(|x| x.as_str())
                        .map(String::from)
                });
                reasoning_effort = reasoning_effort
                    .or_else(|| metadata_string_field(payload, "effort"))
                    .or_else(|| metadata_string_field(payload, "reasoning_effort"));
            }
            "event_msg" if first_user.is_none() => {
                let payload = v.get("payload");
                let pt = payload
                    .and_then(|p| p.get("type"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                if pt == "user_message" {
                    first_user = payload
                        .and_then(user_message_preview)
                        .map(|text| text.chars().take(200).collect());
                }
            }
            _ => {}
        }
        let _ = i;
    }
    let id = match id {
        Some(x) => x,
        None => return Ok(None), // 没有有效 session_meta.id 直接跳过
    };
    let relpath = path
        .strip_prefix(codex_dir)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| path.file_name().map(PathBuf::from).unwrap_or_default());
    Ok(Some(RolloutBrief {
        path: path.to_path_buf(),
        relpath,
        id,
        model_provider: Some(model_provider.unwrap_or_else(|| DEFAULT_PROVIDER.to_string())),
        source,
        cwd,
        sandbox_policy,
        approval_mode,
        memory_mode,
        model,
        reasoning_effort,
        first_user_message: first_user.unwrap_or_default(),
        tokens_used,
        updated_at_ms: last_ms,
        created_at_ms: created_ms,
    }))
}

const USER_MESSAGE_BEGIN: &str = "## My request for Codex:";
const IMAGE_ONLY_USER_MESSAGE_PLACEHOLDER: &str = "[Image]";

fn user_message_preview(payload: &Value) -> Option<String> {
    let message = payload
        .get("message")
        .and_then(|x| x.as_str())
        .map(strip_user_message_prefix)
        .unwrap_or("")
        .trim();
    if !message.is_empty() {
        return Some(message.to_string());
    }

    let has_remote_image = payload
        .get("images")
        .and_then(|x| x.as_array())
        .is_some_and(|items| !items.is_empty());
    let has_local_image = payload
        .get("local_images")
        .and_then(|x| x.as_array())
        .is_some_and(|items| !items.is_empty());
    if has_remote_image || has_local_image {
        return Some(IMAGE_ONLY_USER_MESSAGE_PLACEHOLDER.to_string());
    }

    None
}

fn strip_user_message_prefix(text: &str) -> &str {
    match text.find(USER_MESSAGE_BEGIN) {
        Some(idx) => text[idx + USER_MESSAGE_BEGIN.len()..].trim(),
        None => text.trim(),
    }
}

fn metadata_string_field(payload: &Value, field: &str) -> Option<String> {
    payload.get(field).and_then(metadata_string_value)
}

fn metadata_string_value(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) if s.trim().is_empty() => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

pub(crate) fn is_desktop_visible_source(source: Option<&str>) -> bool {
    matches!(source, Some("cli" | "vscode"))
}

pub(crate) fn is_subagent_source(source: Option<&str>) -> bool {
    let Some(source) = source.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    if source.eq_ignore_ascii_case("subagent") {
        return true;
    }
    serde_json::from_str::<Value>(source)
        .ok()
        .is_some_and(|v| v.get("subagent").is_some())
}

fn desktop_visible_source(payload: &Value) -> String {
    let source = metadata_string_field(payload, "source").or_else(|| {
        metadata_string_field(payload, "originator").and_then(|originator| {
            let normalized = originator.to_ascii_lowercase();
            if normalized.contains("vscode") {
                Some("vscode".to_string())
            } else if normalized.contains("cli") || normalized.contains("codex") {
                Some(DEFAULT_THREAD_SOURCE.to_string())
            } else {
                None
            }
        })
    });

    if let Some(source) = source {
        if is_subagent_source(Some(source.as_str())) {
            return source;
        }
        if is_desktop_visible_source(Some(source.as_str())) {
            return source;
        }
    }
    DEFAULT_THREAD_SOURCE.to_string()
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn diagnose_codex_state(codex_dir: String) -> AppResult<DiagnosticReport> {
    let codex = PathBuf::from(&codex_dir);

    // 1) 扫 sessions/。这里的 rollout_count 只统计 active 会话，和官方 thread/list
    // archived=false 的默认语义保持一致。
    let rollouts = family::scan_rollouts(&codex);
    let mut rollout_ids: Vec<String> = Vec::new();
    for p in &rollouts {
        if let Ok(Some(b)) = read_rollout_brief(&codex, p) {
            rollout_ids.push(b.id);
        }
    }
    rollout_ids.sort();
    rollout_ids.dedup();
    let rollout_count = rollout_ids.len() as u32;

    // 2) archived_sessions/
    let archived_rollouts = family::scan_archived_rollouts(&codex);
    let mut archived_ids: Vec<String> = Vec::new();
    let archived_count = archived_rollouts.len() as u32;
    for p in &archived_rollouts {
        if let Ok(Some(b)) = read_rollout_brief(&codex, p) {
            archived_ids.push(b.id);
        }
    }
    archived_ids.sort();
    archived_ids.dedup();

    // 3) session_index.jsonl
    let index_path = paths::session_index_path(&codex);
    let mut index_ids: Vec<String> = Vec::new();
    if index_path.is_file() {
        let f = fs::File::open(&index_path)?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                    index_ids.push(id.to_string());
                }
            }
        }
    }
    index_ids.sort();
    index_ids.dedup();

    // 4) threads 表
    let mut threads_ids: Vec<String> = Vec::new();
    let mut threads_active_ids: Vec<String> = Vec::new();
    let mut threads_archived_ids: Vec<String> = Vec::new();
    if paths::state_db_path(&codex).is_file() {
        let conn = state_db::open_ro(&codex)?;
        let mut stmt = conn.prepare("SELECT id, COALESCE(archived,0) FROM threads")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0))
        })?;
        for r in rows.flatten() {
            let (id, archived) = r;
            if archived {
                threads_archived_ids.push(id.clone());
            } else {
                threads_active_ids.push(id.clone());
            }
            threads_ids.push(id);
        }
    }
    threads_ids.sort();
    threads_ids.dedup();
    threads_active_ids.sort();
    threads_active_ids.dedup();
    threads_archived_ids.sort();
    threads_archived_ids.dedup();

    // 5) 差集
    let rs: BTreeSet<&String> = rollout_ids.iter().collect();
    let ars: BTreeSet<&String> = archived_ids.iter().collect();
    let all_rs: BTreeSet<&String> = rs.union(&ars).copied().collect();
    let is_: BTreeSet<&String> = index_ids.iter().collect();
    let ts: BTreeSet<&String> = threads_ids.iter().collect();

    let missing_in_index: Vec<String> = rs.difference(&is_).map(|s| (*s).clone()).collect();
    let missing_in_threads: Vec<String> = rs.difference(&ts).map(|s| (*s).clone()).collect();
    let orphan_in_index: Vec<String> = is_.difference(&rs).map(|s| (*s).clone()).collect();
    let orphan_in_threads: Vec<String> = ts.difference(&all_rs).map(|s| (*s).clone()).collect();

    // 6) provider mismatch —— 与 batch_clone 共用实现。
    // config.toml 没显式写 model_provider 时 Codex 默认 "openai"，这里也按默认值比较。
    let cur_provider = effective_current_provider(&codex);
    let mismatch = list_mismatched_session_ids(&codex, &cur_provider)?.len() as u32;

    Ok(DiagnosticReport {
        rollout_count,
        archived_rollout_count: archived_count,
        index_count: index_ids.len() as u32,
        threads_count: threads_ids.len() as u32,
        threads_active_count: threads_active_ids.len() as u32,
        threads_archived_count: threads_archived_ids.len() as u32,
        rollout_ids,
        index_ids,
        threads_ids,
        missing_in_index,
        missing_in_threads,
        orphan_in_index,
        orphan_in_threads,
        current_provider: Some(cur_provider),
        provider_mismatched_families: mismatch,
    })
}

// ========================= 重建 session_index.jsonl =========================

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn repair_session_index(codex_dir: String, dry_run: bool) -> AppResult<IndexRepairReport> {
    let codex = PathBuf::from(&codex_dir);
    let rollouts = family::scan_rollouts(&codex);
    let mut written = 0u32;
    let mut salvaged = 0u32;
    let mut errors: Vec<String> = Vec::new();

    let mut entries: Vec<Value> = Vec::with_capacity(rollouts.len());
    for p in &rollouts {
        match read_rollout_brief(&codex, p) {
            Ok(Some(b)) => {
                let updated = if b.updated_at_ms > 0 {
                    b.updated_at_ms
                } else if b.created_at_ms > 0 {
                    b.created_at_ms
                } else {
                    0
                };
                let abs = b.path.to_string_lossy().into_owned();
                entries.push(serde_json::json!({
                    "id": b.id,
                    "thread_name": b.first_user_message.clone(),
                    "rollout_path": abs,
                    "updated_at": updated,
                }));
                written += 1;
            }
            Ok(None) => {
                // 没有 session_meta → 尝试从文件名救援
                if let Some(id) = salvage_id_from_filename(p) {
                    let md = fs::metadata(p).ok();
                    let mtime_ms = md
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    entries.push(serde_json::json!({
                        "id": id,
                        "thread_name": "",
                        "rollout_path": p.to_string_lossy(),
                        "updated_at": mtime_ms,
                    }));
                    salvaged += 1;
                }
            }
            Err(e) => {
                errors.push(format!("{}: {}", p.to_string_lossy(), e));
            }
        }
    }

    if !dry_run {
        let out_path = paths::session_index_path(&codex);
        let tmp = out_path.with_extension("jsonl.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            for e in &entries {
                writeln!(f, "{}", serde_json::to_string(e)?)?;
            }
            f.sync_all().ok();
        }
        fs::rename(&tmp, &out_path)?;
    }

    Ok(IndexRepairReport {
        scanned: rollouts.len() as u32,
        written,
        salvaged,
        dry_run,
        errors,
    })
}

// ========================= 清理残留（orphan） =========================
//
// 与 `repair_session_index`/`rebuild_threads_table` 不同：此命令**只删除**
// 指向已消失 rollout 的孤儿行（session_index.jsonl 里多出来的 id、threads
// 表里多出来的 id），不会从 rollout 重建。适合只想"把残留清干净"的场景。
#[cfg_attr(feature = "desktop", tauri::command)]
pub fn prune_orphan_entries(
    codex_dir: String,
    prune_index: bool,
    prune_threads: bool,
    dry_run: bool,
) -> AppResult<OrphanPruneReport> {
    let codex = PathBuf::from(&codex_dir);

    // active rollout 用于 session_index；active + archived rollout 用于 threads。
    let rollouts = family::scan_rollouts(&codex);
    let mut rollout_ids: BTreeSet<String> = BTreeSet::new();
    for p in &rollouts {
        if let Ok(Some(b)) = read_rollout_brief(&codex, p) {
            rollout_ids.insert(b.id);
        }
    }
    let mut all_rollout_ids = rollout_ids.clone();
    for p in family::scan_archived_rollouts(&codex) {
        if let Ok(Some(b)) = read_rollout_brief(&codex, &p) {
            all_rollout_ids.insert(b.id);
        }
    }

    let mut index_removed = 0u32;
    let mut threads_removed = 0u32;

    if prune_index {
        let index_path = paths::session_index_path(&codex);
        if index_path.is_file() {
            let mut kept_lines: Vec<String> = Vec::new();
            let f = fs::File::open(&index_path)?;
            for line in BufReader::new(f).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let keep = match serde_json::from_str::<Value>(&line) {
                    Ok(v) => v
                        .get("id")
                        .and_then(|x| x.as_str())
                        .map(|id| rollout_ids.contains(id))
                        .unwrap_or(true),
                    Err(_) => true,
                };
                if keep {
                    kept_lines.push(line);
                } else {
                    index_removed += 1;
                }
            }
            if !dry_run && index_removed > 0 {
                let tmp = index_path.with_extension("jsonl.tmp");
                {
                    let mut f = fs::File::create(&tmp)?;
                    for l in &kept_lines {
                        writeln!(f, "{}", l)?;
                    }
                    f.sync_all().ok();
                }
                fs::rename(&tmp, &index_path)?;
            }
        }
    }

    if prune_threads && paths::state_db_path(&codex).is_file() {
        let conn = state_db::open(&codex)?;
        let orphan_ids: Vec<String> = {
            let mut stmt = conn.prepare("SELECT id FROM threads")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.flatten()
                .filter(|id| !all_rollout_ids.contains(id))
                .collect()
        };
        threads_removed = orphan_ids.len() as u32;
        if !dry_run && !orphan_ids.is_empty() {
            let tx = conn.unchecked_transaction()?;
            for id in &orphan_ids {
                tx.execute("DELETE FROM threads WHERE id = ?", [id])?;
            }
            tx.commit()?;
        }
    }

    Ok(OrphanPruneReport {
        index_removed,
        threads_removed,
        dry_run,
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn diagnose_claude_history_orphans(claude_dir: String) -> AppResult<HistoryOrphanReport> {
    let (history_path, session_ids) = claude_history_context(claude_dir)?;

    let mut history_rows = 0u32;
    let mut linked_rows = 0u32;
    let mut orphan_rows = 0u32;
    let mut untracked_rows = 0u32;
    let mut orphan_ids = BTreeSet::new();

    if history_path.is_file() {
        let file = fs::File::open(&history_path)?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            history_rows += 1;
            match crate::history::line_session_id(&line) {
                Some(id) if session_ids.contains(&id) => linked_rows += 1,
                Some(id) => {
                    orphan_rows += 1;
                    orphan_ids.insert(id);
                }
                None => untracked_rows += 1,
            }
        }
    }

    Ok(HistoryOrphanReport {
        provider: "claude".to_string(),
        history_path: history_path.to_string_lossy().into_owned(),
        session_count: session_ids.len() as u32,
        history_rows,
        linked_rows,
        orphan_rows,
        untracked_rows,
        orphan_session_ids: orphan_ids.into_iter().collect(),
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn prune_claude_history_orphans(
    claude_dir: String,
    dry_run: bool,
) -> AppResult<HistoryPruneReport> {
    let (history_path, session_ids) = claude_history_context(claude_dir)?;

    let mut removed_rows = 0u32;
    let mut orphan_ids = BTreeSet::new();
    let mut kept_lines = Vec::new();

    if history_path.is_file() {
        let file = fs::File::open(&history_path)?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if let Some(id) = crate::history::line_session_id(&line) {
                if !session_ids.contains(&id) {
                    removed_rows += 1;
                    orphan_ids.insert(id);
                    continue;
                }
            }
            kept_lines.push(line);
        }

        if !dry_run && removed_rows > 0 {
            let tmp = history_path.with_extension("jsonl.tmp");
            {
                let mut file = fs::File::create(&tmp)?;
                for line in &kept_lines {
                    writeln!(file, "{}", line)?;
                }
                file.sync_all().ok();
            }
            fs::rename(&tmp, &history_path)?;
        }
    }

    Ok(HistoryPruneReport {
        provider: "claude".to_string(),
        history_path: history_path.to_string_lossy().into_owned(),
        removed_rows,
        dry_run,
        orphan_session_ids: orphan_ids.into_iter().collect(),
    })
}

fn claude_history_context(claude_dir: String) -> AppResult<(PathBuf, BTreeSet<String>)> {
    let claude = PathBuf::from(claude_dir);
    let session_ids = crate::claude_sessions::scan_sessions(&claude)?
        .into_iter()
        .map(|session| session.id)
        .collect::<BTreeSet<_>>();
    Ok((paths::history_path(&claude), session_ids))
}

// ========================= Claude GUI 会话列表可见性 =========================
//
// Claude Code 的 VS Code 插件（GUI）构建"历史会话"列表时，只读取每个
// projects/<项目>/<uuid>.jsonl 的头部与尾部各 64KB 窗口，并按
// customTitle → aiTitle → lastPrompt → summary → 头部窗口内首条用户消息
// 的顺序推导标题；推导不出标题的会话会被直接从列表里丢弃（CLI 的
// `claude --resume <id>` 不受影响，因为它按 id 读取完整文件）。
//
// 走中转 provider 时 AI 标题/summary 生成经常失败，而长会话 compact 后
// resume 的文件头部往往被 compact summary（isCompactSummary，被跳过）和
// 工具输出占满，导致标题链全部落空 →"CLI 里有完整对话，GUI 不显示"。
//
// 修复方式与插件自身的"重命名"完全一致：在 jsonl 末尾追加一条
// `{"type":"custom-title","sessionId":...,"customTitle":...}` 记录。

const GUI_WINDOW_BYTES: u64 = 65536;

/// 读取文件头部/尾部各 64KB 窗口（与 VS Code 插件的读取方式一致）。
fn gui_read_windows(path: &Path) -> AppResult<Option<(String, String, u64)>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path)?;
    let size = file.metadata()?.len();
    if size == 0 {
        return Ok(None);
    }
    let head_len = size.min(GUI_WINDOW_BYTES) as usize;
    let mut head_buf = vec![0u8; head_len];
    file.read_exact(&mut head_buf)?;
    let head = String::from_utf8_lossy(&head_buf).into_owned();
    let tail = if size > GUI_WINDOW_BYTES {
        let mut tail_buf = vec![0u8; GUI_WINDOW_BYTES as usize];
        file.seek(SeekFrom::Start(size - GUI_WINDOW_BYTES))?;
        file.read_exact(&mut tail_buf)?;
        String::from_utf8_lossy(&tail_buf).into_owned()
    } else {
        head.clone()
    };
    Ok(Some((head, tail, size)))
}

/// 插件的字符串字段提取：取文本中最后一次出现的 `"key":"value"`（含转义处理）。
fn gui_last_string_field(text: &str, key: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut best: Option<String> = None;
    let mut best_idx: isize = -1;
    for pat in [format!("\"{key}\":\""), format!("\"{key}\": \"")] {
        let pat_bytes = pat.as_bytes();
        let mut from = 0usize;
        while let Some(rel) = find_subslice(&bytes[from..], pat_bytes) {
            let at = from + rel;
            let start = at + pat_bytes.len();
            let mut i = start;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    if at as isize > best_idx {
                        let raw = String::from_utf8_lossy(&bytes[start..i]).into_owned();
                        best = Some(gui_unescape(&raw));
                        best_idx = at as isize;
                    }
                    break;
                }
                i += 1;
            }
            from = i + 1;
            if from >= bytes.len() {
                break;
            }
        }
    }
    best
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn gui_unescape(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    serde_json::from_str::<String>(&format!("\"{raw}\"")).unwrap_or_else(|_| raw.to_string())
}

/// 插件 `a6e`：以类 XML 标签或 "[Request interrupted by user...]" 开头的文本不作为标题。
fn gui_title_skipped(text: &str) -> bool {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix('<') {
        let mut chars = rest.chars();
        if let Some(first) = chars.next() {
            if first.is_ascii_lowercase() {
                let after = &rest[1..];
                let name_len = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                    .unwrap_or(after.len());
                if let Some(next) = after[name_len..].chars().next() {
                    if next.is_whitespace() || next == '>' {
                        return true;
                    }
                }
            }
        }
    }
    if let Some(rest) = text.strip_prefix("[Request interrupted by user") {
        if rest.contains(']') {
            return true;
        }
    }
    false
}

fn gui_tag_capture<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

/// 插件 `Aie`：从一条 user 记录提取标题候选。
fn gui_user_record_title(value: &Value, command_fallback: &mut String) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    if value.get("isMeta").and_then(Value::as_bool) == Some(true)
        || value.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let message = value.get("message")?;
    if message.is_null() {
        return None;
    }
    let mut texts: Vec<String> = Vec::new();
    match message.get("content") {
        Some(Value::String(s)) => texts.push(s.clone()),
        Some(Value::Array(items)) => {
            for item in items {
                let Some(obj) = item.as_object() else {
                    continue;
                };
                match obj.get("type").and_then(Value::as_str) {
                    Some("tool_result") => return None,
                    Some("text") => {
                        if let Some(text) = obj.get("text").and_then(Value::as_str) {
                            texts.push(text.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    for text in texts {
        let line = text.replace('\n', " ").trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(cmd) = gui_tag_capture(&line, "<command-name>", "</command-name>") {
            if command_fallback.is_empty() {
                *command_fallback = cmd.to_string();
            }
            continue;
        }
        if let Some(bash) = gui_tag_capture(&line, "<bash-input>", "</bash-input>") {
            return Some(format!("! {}", bash.trim()));
        }
        if gui_title_skipped(&line) {
            continue;
        }
        let truncated: String = if line.chars().count() > 200 {
            format!("{}…", line.chars().take(200).collect::<String>().trim_end())
        } else {
            line
        };
        return Some(truncated);
    }
    None
}

/// 插件 `jie`：在头部窗口内逐行寻找首条可作标题的用户消息。
fn gui_head_title(head: &str) -> Option<String> {
    let mut command_fallback = String::new();
    for line in head.split('\n') {
        if !line.contains("\"type\":\"user\"") && !line.contains("\"type\": \"user\"") {
            continue;
        }
        if line.contains("\"tool_result\"") {
            continue;
        }
        if line.contains("\"isMeta\":true") || line.contains("\"isMeta\": true") {
            continue;
        }
        if line.contains("\"isCompactSummary\":true") || line.contains("\"isCompactSummary\": true")
        {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(title) = gui_user_record_title(&value, &mut command_fallback) {
            return Some(title);
        }
    }
    if command_fallback.is_empty() {
        None
    } else {
        Some(command_fallback)
    }
}

/// 复刻插件 fetchSessions 的标题推导链；返回 None 即该会话在 GUI 列表中不可见。
fn gui_visible_title(head: &str, tail: &str) -> Option<String> {
    let named = gui_last_string_field(tail, "customTitle")
        .or_else(|| gui_last_string_field(head, "customTitle"))
        .or_else(|| gui_last_string_field(tail, "aiTitle"))
        .or_else(|| gui_last_string_field(head, "aiTitle"));
    if let Some(title) = named.filter(|t| !t.is_empty()) {
        return Some(title);
    }
    if let Some(title) = gui_last_string_field(tail, "lastPrompt").filter(|t| !t.is_empty()) {
        return Some(title);
    }
    if let Some(title) = gui_last_string_field(tail, "summary").filter(|t| !t.is_empty()) {
        return Some(title);
    }
    gui_head_title(head).filter(|t| !t.is_empty())
}

fn is_session_uuid(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, b) in bytes.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if *b != b'-' {
                    return false;
                }
            }
            _ => {
                if !b.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

fn first_line_is_sidechain(head: &str) -> bool {
    let first = head.split('\n').next().unwrap_or(head);
    first.contains("\"isSidechain\":true") || first.contains("\"isSidechain\": true")
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn diagnose_claude_gui_visibility(claude_dir: String) -> AppResult<GuiVisibilityReport> {
    let claude = PathBuf::from(&claude_dir);
    let projects_root = paths::claude_projects_dir(&claude);

    let mut scanned = 0u32;
    let mut visible = 0u32;
    let mut sidechain = 0u32;
    let mut empty = 0u32;
    let mut unfixable = 0u32;
    let mut invisible_paths: Vec<(PathBuf, String, String)> = Vec::new();

    if projects_root.is_dir() {
        for project in fs::read_dir(&projects_root)? {
            let project = project?;
            let project_path = project.path();
            if !project_path.is_dir() {
                continue;
            }
            let project_dir = project.file_name().to_string_lossy().into_owned();
            for entry in fs::read_dir(&project_path)? {
                let entry = entry?;
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(stem) = name.strip_suffix(".jsonl") else {
                    continue;
                };
                if !is_session_uuid(stem) {
                    continue;
                }
                scanned += 1;
                let Some((head, tail, _size)) = gui_read_windows(&path)? else {
                    empty += 1;
                    continue;
                };
                if first_line_is_sidechain(&head) {
                    sidechain += 1;
                    continue;
                }
                if gui_visible_title(&head, &tail).is_some() {
                    visible += 1;
                    continue;
                }
                invisible_paths.push((path, project_dir.clone(), stem.to_string()));
            }
        }
    }

    let mut issues = Vec::new();
    if !invisible_paths.is_empty() {
        let summaries: HashMap<String, crate::models::SessionSummary> =
            crate::claude_sessions::scan_sessions(&claude)?
                .into_iter()
                .map(|s| (s.rollout_path.clone(), s))
                .collect();
        for (path, project_dir, stem) in invisible_paths {
            let key = path.to_string_lossy().into_owned();
            let Some(summary) = summaries.get(&key) else {
                unfixable += 1;
                continue;
            };
            // 标题必须来自会话内容（用户消息 / 标题记录），而不是 id 或目录名兜底，
            // 否则补写出来的是没有意义的占位标题。
            let content_derived = !summary.first_user_message.is_empty()
                || (summary.title != summary.id && summary.title != summary.cwd_display);
            if !content_derived || summary.title.is_empty() {
                unfixable += 1;
                continue;
            }
            issues.push(GuiVisibilityIssue {
                session_id: if summary.id.is_empty() {
                    stem
                } else {
                    summary.id.clone()
                },
                path: key,
                project_dir,
                cwd: summary.cwd.clone(),
                proposed_title: summary.title.clone(),
                updated_at: summary.updated_at,
                file_size: summary.rollout_bytes,
            });
        }
    }
    issues.sort_by_key(|issue| std::cmp::Reverse(issue.updated_at));

    Ok(GuiVisibilityReport {
        provider: "claude".to_string(),
        projects_root: projects_root.to_string_lossy().into_owned(),
        scanned_sessions: scanned,
        visible_sessions: visible,
        sidechain_sessions: sidechain,
        empty_sessions: empty,
        unfixable_sessions: unfixable,
        issues,
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn repair_claude_gui_visibility(
    claude_dir: String,
    dry_run: bool,
    session_ids: Option<Vec<String>>,
) -> AppResult<GuiVisibilityFixReport> {
    let report = diagnose_claude_gui_visibility(claude_dir)?;
    let filter: Option<BTreeSet<String>> = session_ids.map(|ids| ids.into_iter().collect());

    let mut fixed = 0u32;
    let mut skipped = 0u32;
    let mut fixed_ids = Vec::new();
    let mut errors = Vec::new();

    for issue in &report.issues {
        if let Some(filter) = &filter {
            if !filter.contains(&issue.session_id) {
                skipped += 1;
                continue;
            }
        }
        if !dry_run {
            if let Err(err) = append_custom_title(
                Path::new(&issue.path),
                &issue.session_id,
                &issue.proposed_title,
            ) {
                errors.push(format!("{}: {}", issue.session_id, err));
                continue;
            }
        }
        fixed += 1;
        fixed_ids.push(issue.session_id.clone());
    }

    Ok(GuiVisibilityFixReport {
        provider: "claude".to_string(),
        fixed,
        skipped,
        dry_run,
        fixed_session_ids: fixed_ids,
        errors,
    })
}

/// 与 VS Code 插件"重命名会话"的写入格式一致：
/// 在 jsonl 末尾追加一行 `{"type":"custom-title","sessionId":...,"customTitle":...}`。
fn append_custom_title(path: &Path, session_id: &str, title: &str) -> AppResult<()> {
    use std::io::{Read, Seek, SeekFrom};
    let needs_newline = {
        let mut file = fs::File::open(path)?;
        let size = file.metadata()?.len();
        if size == 0 {
            false
        } else {
            file.seek(SeekFrom::Start(size - 1))?;
            let mut last = [0u8; 1];
            file.read_exact(&mut last)?;
            last[0] != b'\n'
        }
    };
    let record = serde_json::json!({
        "type": "custom-title",
        "sessionId": session_id,
        "customTitle": title,
    });
    let mut file = fs::OpenOptions::new().append(true).open(path)?;
    if needs_newline {
        file.write_all(b"\n")?;
    }
    file.write_all(record.to_string().as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all().ok();
    Ok(())
}

fn salvage_id_from_filename(p: &Path) -> Option<String> {
    // 形如 rollout-2024-10-01T12-34-56-<uuid>.jsonl
    let stem = p.file_stem()?.to_string_lossy().into_owned();
    let parts: Vec<&str> = stem.rsplitn(2, '-').collect();
    if parts.len() != 2 {
        return None;
    }
    let candidate = parts[0];
    // 简单校验：非空且包含连字符/字母数字
    if candidate.len() >= 8 && candidate.chars().any(|c| c.is_ascii_alphabetic()) {
        Some(candidate.to_string())
    } else {
        None
    }
}

// ========================= 重建 threads 表 =========================

/// columns in threads（和 backup.rs 保持一致以便互操作）
const THREADS_COLS: &[&str] = &[
    "id",
    "rollout_path",
    "created_at",
    "updated_at",
    "source",
    "model_provider",
    "cwd",
    "title",
    "sandbox_policy",
    "approval_mode",
    "tokens_used",
    "has_user_event",
    "archived",
    "archived_at",
    "git_sha",
    "git_branch",
    "git_origin_url",
    "cli_version",
    "first_user_message",
    "agent_nickname",
    "agent_role",
    "memory_mode",
    "model",
    "reasoning_effort",
    "agent_path",
    "created_at_ms",
    "updated_at_ms",
];

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn rebuild_threads_table(codex_dir: String, dry_run: bool) -> AppResult<ThreadsRebuildReport> {
    let codex = PathBuf::from(&codex_dir);
    let active_rollouts = family::scan_rollouts(&codex);
    let archived_rollouts = family::scan_archived_rollouts(&codex);
    let mut scanned = 0u32;
    let mut upserted = 0u32;
    let mut skipped = 0u32;
    let mut errors: Vec<String> = Vec::new();

    if !paths::state_db_path(&codex).is_file() {
        return Err(AppError::InvalidCodexDir(format!(
            "state_5.sqlite 不存在: {}",
            paths::state_db_path(&codex).to_string_lossy()
        )));
    }

    let state = state_db::open(&codex)?;

    for (p, archived) in active_rollouts
        .iter()
        .map(|p| (p, false))
        .chain(archived_rollouts.iter().map(|p| (p, true)))
    {
        scanned += 1;
        if dry_run {
            match thread_values_from_rollout(&codex, p, archived) {
                Ok(Some(_)) => upserted += 1,
                Ok(None) => skipped += 1,
                Err(e) => {
                    errors.push(format!("{}: {}", p.to_string_lossy(), e));
                    skipped += 1;
                }
            }
            continue;
        }

        match upsert_thread_from_rollout(&codex, &state, p, archived) {
            Ok(true) => upserted += 1,
            Ok(false) => skipped += 1,
            Err(e) => {
                errors.push(format!("{}: {}", p.to_string_lossy(), e));
                skipped += 1;
            }
        }
    }

    Ok(ThreadsRebuildReport {
        scanned,
        upserted,
        skipped,
        dry_run,
        errors,
    })
}

fn ensure_state_db_exists(codex: &Path) -> AppResult<()> {
    let path = paths::state_db_path(codex);
    if path.is_file() {
        return Ok(());
    }
    Err(AppError::InvalidCodexDir(format!(
        "state_5.sqlite 不存在，无法同步会话可见性: {}",
        path.to_string_lossy()
    )))
}

fn threads_upsert_sql() -> String {
    let placeholders = (0..THREADS_COLS.len())
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let cols_sql = THREADS_COLS.join(",");
    let update_sql = THREADS_COLS
        .iter()
        .filter(|c| **c != "id")
        .map(|c| format!("{c}=excluded.{c}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("INSERT INTO threads ({cols_sql}) VALUES ({placeholders}) ON CONFLICT(id) DO UPDATE SET {update_sql}")
}

fn thread_values_from_rollout(
    codex: &Path,
    rollout: &Path,
    archived: bool,
) -> AppResult<Option<Vec<Value>>> {
    let brief = match read_rollout_brief(codex, rollout)? {
        Some(b) => b,
        None => return Ok(None),
    };
    let meta = family::read_session_meta(rollout)?;
    let payload = meta.get("payload").cloned().unwrap_or(Value::Null);
    let title = brief
        .first_user_message
        .chars()
        .take(80)
        .collect::<String>();
    let updated = if brief.updated_at_ms > 0 {
        brief.updated_at_ms
    } else {
        chrono::Utc::now().timestamp_millis()
    };
    let created = if brief.created_at_ms > 0 {
        brief.created_at_ms
    } else {
        updated
    };
    let archived_at = if archived {
        fs::metadata(rollout)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or_else(|| chrono::Utc::now().timestamp())
    } else {
        0
    };

    Ok(Some(
        THREADS_COLS
            .iter()
            .map(|name| match *name {
                "id" => Value::String(brief.id.clone()),
                "rollout_path" => Value::String(brief.path.to_string_lossy().into_owned()),
                "created_at" => Value::from(created / 1000),
                "updated_at" => Value::from(updated / 1000),
                "created_at_ms" => Value::from(created),
                "updated_at_ms" => Value::from(updated),
                "cwd" => Value::String(
                    metadata_string_field(&payload, "cwd")
                        .or_else(|| brief.cwd.clone())
                        .unwrap_or_default(),
                ),
                "source" => Value::String(desktop_visible_source(&payload)),
                "model_provider" => Value::String(
                    metadata_string_field(&payload, "model_provider")
                        .or_else(|| brief.model_provider.clone())
                        .unwrap_or_else(|| DEFAULT_PROVIDER.to_string()),
                ),
                "sandbox_policy" => Value::String(
                    brief
                        .sandbox_policy
                        .clone()
                        .unwrap_or_else(|| DEFAULT_SANDBOX_POLICY.to_string()),
                ),
                "approval_mode" => Value::String(
                    brief
                        .approval_mode
                        .clone()
                        .unwrap_or_else(|| DEFAULT_APPROVAL_MODE.to_string()),
                ),
                "memory_mode" => Value::String(
                    brief
                        .memory_mode
                        .clone()
                        .unwrap_or_else(|| DEFAULT_MEMORY_MODE.to_string()),
                ),
                "title" => Value::String(title.clone()),
                "first_user_message" => Value::String(brief.first_user_message.clone()),
                "has_user_event" => Value::from(1i64),
                "archived" => Value::from(if archived { 1i64 } else { 0i64 }),
                "archived_at" if archived => Value::from(archived_at),
                "archived_at" => Value::Null,
                "tokens_used" => Value::from(brief.tokens_used),
                "cli_version" => Value::String(
                    metadata_string_field(&payload, "cli_version").unwrap_or_default(),
                ),
                "model" => brief
                    .model
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
                "reasoning_effort" => brief
                    .reasoning_effort
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
                _ => payload.get(*name).cloned().unwrap_or(Value::Null),
            })
            .collect(),
    ))
}

fn bind_thread_values(values: &[Value]) -> Vec<Box<dyn rusqlite::ToSql>> {
    values
        .iter()
        .map(|v| match v {
            Value::Null => Box::new(Option::<String>::None) as Box<dyn rusqlite::ToSql>,
            Value::Bool(b) => Box::new(if *b { 1i64 } else { 0i64 }),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Box::new(i) as Box<dyn rusqlite::ToSql>
                } else if let Some(f) = n.as_f64() {
                    Box::new(f) as Box<dyn rusqlite::ToSql>
                } else {
                    Box::new(n.to_string()) as Box<dyn rusqlite::ToSql>
                }
            }
            Value::String(s) => Box::new(s.clone()) as Box<dyn rusqlite::ToSql>,
            other => Box::new(other.to_string()) as Box<dyn rusqlite::ToSql>,
        })
        .collect()
}

pub(crate) fn upsert_thread_from_rollout(
    codex: &Path,
    state: &rusqlite::Connection,
    rollout: &Path,
    archived: bool,
) -> AppResult<bool> {
    let values = match thread_values_from_rollout(codex, rollout, archived)? {
        Some(values) => values,
        None => return Ok(false),
    };
    let sql = threads_upsert_sql();
    let mut stmt = state.prepare(&sql)?;
    let boxed = bind_thread_values(&values);
    let refs: Vec<&dyn rusqlite::ToSql> = boxed.iter().map(|b| b.as_ref()).collect();
    stmt.execute(refs.as_slice())?;
    Ok(true)
}

fn sync_thread_from_rollout(
    codex: &Path,
    state: &rusqlite::Connection,
    rollout: &Path,
) -> AppResult<()> {
    if upsert_thread_from_rollout(codex, state, rollout, false)? {
        return Ok(());
    }
    Err(AppError::InvalidCodexDir(format!(
        "rollout 缺少有效 session_meta.id，无法同步 threads: {}",
        rollout.to_string_lossy()
    )))
}

fn require_thread_row(state: &rusqlite::Connection, id: &str) -> AppResult<()> {
    match state.query_row("SELECT 1 FROM threads WHERE id = ?", [id], |_| Ok(())) {
        Ok(()) => Ok(()),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            Err(AppError::NotFound(format!("threads 中未找到 id: {}", id)))
        }
        Err(e) => Err(e.into()),
    }
}

fn mark_thread_archived(
    state: &rusqlite::Connection,
    id: &str,
    archived_rollout_path: &Path,
) -> AppResult<()> {
    let now = chrono::Utc::now().timestamp();
    let rows = state.execute(
        "UPDATE threads SET archived = 1, archived_at = ?, rollout_path = ? WHERE id = ?",
        rusqlite::params![now, archived_rollout_path.to_string_lossy(), id],
    )?;
    if rows == 0 {
        return Err(AppError::NotFound(format!("threads 中未找到 id: {}", id)));
    }
    Ok(())
}

/// 把会话的 cwd 注册进 `.codex-global-state.json` 的三个数组：
/// - `electron-saved-workspace-roots`（已知项目根，父目录已覆盖则跳过）
/// - `active-workspace-roots`（Codex App 侧栏当前显示的项目筛选集）
/// - `project-order`（侧栏项目展示顺序）
///
/// 三者缺一，官方 App 在"按项目分组"模式下都可能漏显新会话。文件不存在或非 JSON 对象时静默返回。
fn ensure_workspace_root_registered(codex: &Path, cwd: &str) -> AppResult<()> {
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return Ok(());
    }
    let path = paths::codex_global_state_json_path(codex);
    if !path.is_file() {
        return Ok(());
    }
    let raw = match fs::read_to_string(&path) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let mut root: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let obj = match root.as_object_mut() {
        Some(o) => o,
        None => return Ok(()),
    };

    let mut changed = false;
    // electron-saved-workspace-roots：若已有条目是 cwd 的前缀，则视为已覆盖。
    let saved_covered = workspace_root_covered(obj, "electron-saved-workspace-roots", cwd);
    if !saved_covered {
        append_string_to_array(obj, "electron-saved-workspace-roots", cwd);
        changed = true;
    }
    // active-workspace-roots：严格包含 cwd 才算命中，避免被父目录吞掉。
    if !array_contains(obj, "active-workspace-roots", cwd) {
        append_string_to_array(obj, "active-workspace-roots", cwd);
        changed = true;
    }
    // project-order：同上，保证侧栏顺序里能看到。
    if !array_contains(obj, "project-order", cwd) {
        append_string_to_array(obj, "project-order", cwd);
        changed = true;
    }
    if !changed {
        return Ok(());
    }

    let tmp = path.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(serde_json::to_string(&root)?.as_bytes())?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn workspace_root_covered(obj: &serde_json::Map<String, Value>, key: &str, cwd: &str) -> bool {
    let arr = match obj.get(key).and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return false,
    };
    let cwd_norm = normalize_path_for_compare(cwd);
    for item in arr {
        if let Some(s) = item.as_str() {
            let item_norm = normalize_path_for_compare(s);
            if item_norm == cwd_norm {
                return true;
            }
            // 父目录覆盖：cwd 以 item + 分隔符开头
            let with_sep = format!("{}/", item_norm.trim_end_matches('/'));
            if cwd_norm.starts_with(&with_sep) {
                return true;
            }
        }
    }
    false
}

fn array_contains(obj: &serde_json::Map<String, Value>, key: &str, cwd: &str) -> bool {
    let arr = match obj.get(key).and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return false,
    };
    let cwd_norm = normalize_path_for_compare(cwd);
    arr.iter().any(|item| {
        item.as_str()
            .map(|s| normalize_path_for_compare(s) == cwd_norm)
            .unwrap_or(false)
    })
}

fn append_string_to_array(obj: &mut serde_json::Map<String, Value>, key: &str, cwd: &str) {
    let entry = obj
        .entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(arr) = entry.as_array_mut() {
        arr.push(Value::String(cwd.to_string()));
    } else {
        *entry = Value::Array(vec![Value::String(cwd.to_string())]);
    }
}

fn normalize_path_for_compare(s: &str) -> String {
    // Windows 路径比较：剥离 `\\?\` 前缀，统一为正斜杠，忽略大小写和尾随分隔符。
    let stripped = paths::strip_verbatim(s);
    let unified = stripped.replace('\\', "/");
    let trimmed = unified.trim_end_matches('/').to_string();
    if cfg!(windows) {
        trimmed.to_ascii_lowercase()
    } else {
        trimmed
    }
}

fn remove_index_line(codex: &Path, id: &str) -> AppResult<()> {
    let path = paths::session_index_path(codex);
    if !path.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(&path)?;
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut out = fs::File::create(&tmp)?;
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let keep = match serde_json::from_str::<Value>(line) {
                Ok(v) => {
                    v.get("id").and_then(|x| x.as_str()) != Some(id)
                        && v.get("session_id").and_then(|x| x.as_str()) != Some(id)
                }
                Err(_) => true,
            };
            if keep {
                writeln!(out, "{}", line)?;
            }
        }
        out.sync_all().ok();
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug, Clone)]
struct ThreadRepairState {
    model_provider: Option<String>,
    source: Option<String>,
    archived: bool,
}

fn read_thread_state_map(codex: &Path) -> AppResult<BTreeMap<String, ThreadRepairState>> {
    let mut out = BTreeMap::new();
    if !paths::state_db_path(codex).is_file() {
        return Ok(out);
    }
    let conn = state_db::open_ro(codex)?;
    let mut stmt =
        conn.prepare("SELECT id, model_provider, source, COALESCE(archived,0) FROM threads")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            ThreadRepairState {
                model_provider: r.get::<_, Option<String>>(1)?,
                source: r.get::<_, Option<String>>(2)?,
                archived: r.get::<_, i64>(3)? != 0,
            },
        ))
    })?;
    for row in rows {
        let (id, state) = row?;
        out.insert(id, state);
    }
    Ok(out)
}

fn thread_state_matches_active_provider(
    states: &BTreeMap<String, ThreadRepairState>,
    id: &str,
    expected: &str,
) -> bool {
    matches!(
        states.get(id),
        Some(state)
            if state.model_provider.as_deref() == Some(expected)
                && (is_desktop_visible_source(state.source.as_deref())
                    || is_subagent_source(state.source.as_deref()))
                && !state.archived
    )
}

fn thread_state_is_subagent(states: &BTreeMap<String, ThreadRepairState>, id: &str) -> bool {
    states
        .get(id)
        .is_some_and(|state| is_subagent_source(state.source.as_deref()))
}

// ========================= Provider 克隆 =========================

fn new_session_id() -> String {
    // 与 codex protocol::ThreadId::new() 等价：UUIDv7（毫秒时间序 + 随机）
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut bytes = [0u8; 16];
    bytes[0] = ((ms >> 40) & 0xFF) as u8;
    bytes[1] = ((ms >> 32) & 0xFF) as u8;
    bytes[2] = ((ms >> 24) & 0xFF) as u8;
    bytes[3] = ((ms >> 16) & 0xFF) as u8;
    bytes[4] = ((ms >> 8) & 0xFF) as u8;
    bytes[5] = (ms & 0xFF) as u8;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let rnd =
        nanos ^ ((std::process::id() as u128).rotate_left(17)) ^ ((ms as u128).rotate_left(37));
    for (i, b) in rnd.to_le_bytes().iter().enumerate().take(10) {
        bytes[6 + i] = *b;
    }
    bytes[6] = (bytes[6] & 0x0F) | 0x70; // version 7
    bytes[8] = (bytes[8] & 0x3F) | 0x80; // RFC4122 variant
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// 与 codex 原生 recorder 一致：sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl
/// 文件名时间戳与 UUID 一一对应；调用方传入新生成的 UUIDv7 与对应时间。
fn build_clone_path(codex_dir: &Path, new_id: &str, ts: &chrono::DateTime<chrono::Utc>) -> PathBuf {
    let dir = codex_dir
        .join("sessions")
        .join(ts.format("%Y").to_string())
        .join(ts.format("%m").to_string())
        .join(ts.format("%d").to_string());
    let stem = format!("rollout-{}-{}", ts.format("%Y-%m-%dT%H-%M-%S"), new_id);
    dir.join(format!("{}.jsonl", stem))
}

/// 验证生成的文件名能被 codex 的 parse_timestamp_uuid_from_filename 解析。
fn validate_rollout_filename(path: &Path) -> AppResult<()> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| AppError::Other("rollout 路径缺少文件名".into()))?;
    let rest = stem
        .strip_prefix("rollout-")
        .ok_or_else(|| AppError::Other(format!("rollout 文件名缺少前缀: {}", stem)))?;
    if rest.len() < 37 {
        return Err(AppError::Other(format!(
            "rollout 文件名过短无法解析: {}",
            stem
        )));
    }
    let (ts_part, uuid_part) = rest.split_at(rest.len() - 37);
    if !uuid_part.starts_with('-') {
        return Err(AppError::Other(format!(
            "rollout 文件名 UUID 段格式异常: {}",
            stem
        )));
    }
    let uuid_str = &uuid_part[1..];
    // UUID 必须是合法的 8-4-4-4-12，且只能有 4 个 '-'
    if uuid_str.matches('-').count() != 4 || uuid_str.len() != 36 {
        return Err(AppError::Other(format!(
            "rollout 文件名 UUID 段不合法: {}",
            stem
        )));
    }
    if !uuid_str.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(AppError::Other(format!(
            "rollout 文件名 UUID 段含非法字符: {}",
            stem
        )));
    }
    let ts_str = ts_part.trim_end_matches('-');
    // 期望格式：YYYY-MM-DDTHH-MM-SS（19 个字符）
    if ts_str.len() != 19
        || ts_str.as_bytes()[10] != b'T'
        || ts_str.as_bytes()[4] != b'-'
        || ts_str.as_bytes()[7] != b'-'
        || ts_str.as_bytes()[13] != b'-'
        || ts_str.as_bytes()[16] != b'-'
    {
        return Err(AppError::Other(format!(
            "rollout 文件名时间戳段不符合 codex 解析规则: {}",
            stem
        )));
    }
    Ok(())
}

/// 深拷 rollout 到新 id + 新 provider；返回新文件绝对路径。
fn write_cloned_rollout(
    src_abs: &Path,
    dest_abs: &Path,
    new_id: &str,
    new_provider: &str,
    _source_id: &str,
) -> AppResult<()> {
    if let Some(parent) = dest_abs.parent() {
        fs::create_dir_all(parent)?;
    }
    let src = fs::File::open(src_abs)?;
    let reader = BufReader::new(src);
    let tmp = dest_abs.with_extension("jsonl.tmp");
    {
        let mut out = fs::File::create(&tmp)?;
        let mut meta_rewritten = false;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if !meta_rewritten {
                if let Ok(mut v) = serde_json::from_str::<Value>(&line) {
                    if v.get("type").and_then(|x| x.as_str()) == Some("session_meta") {
                        let now_iso = chrono::Utc::now().to_rfc3339();
                        // 顶层 timestamp 与 payload.timestamp 都对齐到克隆时间，
                        // 与 codex recorder 行为一致；避免与文件名时间戳错位。
                        v["timestamp"] = Value::String(now_iso.clone());
                        if let Some(payload) = v.get_mut("payload").and_then(|p| p.as_object_mut())
                        {
                            payload.insert("id".into(), Value::String(new_id.into()));
                            payload.insert("timestamp".into(), Value::String(now_iso));
                            payload.insert(
                                "model_provider".into(),
                                Value::String(new_provider.into()),
                            );
                            // 不再向 SessionMeta 注入非标字段，血统信息由 family.json 维护。
                            payload.remove("clone_timestamp");
                            payload.remove("cloned_from");
                        }
                        writeln!(out, "{}", serde_json::to_string(&v)?)?;
                        meta_rewritten = true;
                        continue;
                    }
                }
            }
            writeln!(out, "{}", line)?;
        }
        out.sync_all().ok();
    }
    fs::rename(&tmp, dest_abs)?;
    Ok(())
}

#[derive(Debug, Clone)]
struct StablePrefixLine {
    physical_index: usize,
    raw_line: String,
    value: Value,
}

#[derive(Debug, Clone)]
struct StableCutInfo {
    role: String,
    kind: String,
    summary: String,
}

#[derive(Debug, Clone)]
struct StablePrefix {
    lines: Vec<StablePrefixLine>,
    cut: StableCutInfo,
}

fn stable_cut_event(raw: &Value) -> Option<StableCutInfo> {
    let outer_type = raw.get("type").and_then(|x| x.as_str()).unwrap_or("");
    let payload = raw.get("payload").unwrap_or(raw);
    let payload_type = payload.get("type").and_then(|x| x.as_str()).unwrap_or("");

    match (outer_type, payload_type) {
        ("event_msg", "user_message") => Some(StableCutInfo {
            role: "user".to_string(),
            kind: "user_message".to_string(),
            summary: payload
                .get("message")
                .and_then(|x| x.as_str())
                .map(strip_user_message_prefix)
                .map(|s| trim_flat(s, 120))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| IMAGE_ONLY_USER_MESSAGE_PLACEHOLDER.to_string()),
        }),
        ("event_msg", "agent_message") => Some(StableCutInfo {
            role: "assistant".to_string(),
            kind: "agent_message".to_string(),
            summary: payload
                .get("message")
                .and_then(|x| x.as_str())
                .map(|s| trim_flat(s, 120))
                .unwrap_or_default(),
        }),
        ("response_item", "message") => {
            let role = payload.get("role").and_then(|x| x.as_str()).unwrap_or("");
            if !matches!(role, "user" | "assistant") {
                return None;
            }
            let summary = flatten_message_content(payload.get("content"))
                .map(|s| trim_flat(&s, 120))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    if message_has_image_content(payload.get("content")) {
                        IMAGE_ONLY_USER_MESSAGE_PLACEHOLDER.to_string()
                    } else {
                        String::new()
                    }
                });
            Some(StableCutInfo {
                role: role.to_string(),
                kind: "message".to_string(),
                summary,
            })
        }
        _ => None,
    }
}

fn flatten_message_content(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(items)) => {
            let text = items
                .iter()
                .filter_map(|item| {
                    item.get("text")
                        .and_then(|x| x.as_str())
                        .or_else(|| item.as_str())
                        .map(String::from)
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.trim().is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    }
}

fn message_has_image_content(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Array(items)) => items.iter().any(|item| {
            item.get("type")
                .and_then(|x| x.as_str())
                .is_some_and(|t| t.contains("image"))
                || item.get("image_url").is_some()
                || item.get("image").is_some()
        }),
        _ => false,
    }
}

fn trim_flat(text: &str, max_chars: usize) -> String {
    let flat = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect::<String>();
    let flat = flat.trim();
    if flat.chars().count() <= max_chars {
        return flat.to_string();
    }
    let mut out = flat.chars().take(max_chars).collect::<String>();
    out.push('…');
    out
}

fn collect_stable_prefix(src_abs: &Path, event_index: usize) -> AppResult<StablePrefix> {
    let src = fs::File::open(src_abs)?;
    let reader = BufReader::new(src);
    let mut lines: Vec<StablePrefixLine> = Vec::new();
    let mut cut: Option<StableCutInfo> = None;

    for (physical_index, line) in reader.lines().enumerate() {
        if physical_index > event_index {
            break;
        }
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value = serde_json::from_str::<Value>(&line).map_err(|err| {
            AppError::Other(format!(
                "无法创建回溯分支：目标节点之前第 {} 行不是有效 JSONL: {}",
                physical_index + 1,
                err
            ))
        })?;
        if physical_index == event_index {
            cut = Some(stable_cut_event(&value).ok_or_else(|| {
                AppError::Other(format!(
                    "只能从稳定对话节点创建分支；第 {} 行不是用户或助手消息节点",
                    physical_index + 1
                ))
            })?);
        }
        lines.push(StablePrefixLine {
            physical_index,
            raw_line: line,
            value,
        });
    }

    let cut = cut.ok_or_else(|| {
        AppError::Other(format!(
            "未找到 index={} 对应的事件行；目标可能是空行或超出 rollout 范围",
            event_index
        ))
    })?;
    let first = lines
        .first()
        .ok_or_else(|| AppError::Other("无法创建回溯分支：目标节点之前没有任何有效事件".into()))?;
    if first.value.get("type").and_then(|x| x.as_str()) != Some("session_meta") {
        return Err(AppError::Other(format!(
            "无法创建回溯分支：第一个有效事件必须是 session_meta，实际位于第 {} 行",
            first.physical_index + 1
        )));
    }

    Ok(StablePrefix { lines, cut })
}

fn write_forked_rollout_prefix(
    prefix: &StablePrefix,
    dest_abs: &Path,
    new_id: &str,
    provider: &str,
) -> AppResult<u64> {
    if let Some(parent) = dest_abs.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dest_abs.with_extension("jsonl.tmp");
    {
        let mut out = fs::File::create(&tmp)?;
        for (idx, item) in prefix.lines.iter().enumerate() {
            if idx == 0 {
                let mut value = item.value.clone();
                let now_iso = chrono::Utc::now().to_rfc3339();
                value["timestamp"] = Value::String(now_iso.clone());
                let payload = value
                    .get_mut("payload")
                    .and_then(|p| p.as_object_mut())
                    .ok_or_else(|| {
                        AppError::Other("session_meta.payload 缺失，无法重写新分支 id".into())
                    })?;
                payload.insert("id".into(), Value::String(new_id.to_string()));
                payload.insert("timestamp".into(), Value::String(now_iso));
                payload.insert("model_provider".into(), Value::String(provider.to_string()));
                payload.remove("clone_timestamp");
                payload.remove("cloned_from");
                writeln!(out, "{}", serde_json::to_string(&value)?)?;
            } else {
                writeln!(out, "{}", item.raw_line)?;
            }
        }
        out.sync_all().ok();
    }
    fs::rename(&tmp, dest_abs)?;
    Ok(prefix.lines.len() as u64)
}

fn resolve_fork_source_rollout(
    codex: &Path,
    session_id: &str,
    rollout_path: &str,
) -> AppResult<(PathBuf, RolloutBrief)> {
    let supplied = paths::host_path_from_codex_record(codex, rollout_path);
    let source = if supplied.is_absolute() {
        supplied
    } else {
        codex.join(supplied)
    };
    let source_abs = source.canonicalize().map_err(|err| {
        AppError::NotFound(format!(
            "源 rollout 不存在或无法访问: {} ({})",
            source.to_string_lossy(),
            err
        ))
    })?;
    let sessions_dir = codex.join("sessions").canonicalize().map_err(|err| {
        AppError::InvalidCodexDir(format!(
            "Codex sessions 目录不存在或无法访问: {} ({})",
            codex.join("sessions").to_string_lossy(),
            err
        ))
    })?;
    if !source_abs.starts_with(&sessions_dir) {
        return Err(AppError::Other(format!(
            "只能从 active sessions/ 下的 rollout 创建回溯分支: {}",
            source_abs.to_string_lossy()
        )));
    }
    let brief = read_rollout_brief(codex, &source_abs)?.ok_or_else(|| {
        AppError::Other(format!(
            "源 rollout 缺少有效 session_meta.id: {}",
            source_abs.to_string_lossy()
        ))
    })?;
    if brief.id != session_id {
        return Err(AppError::Other(format!(
            "源 rollout id 与会话不一致：期望 {}，实际 {}",
            session_id, brief.id
        )));
    }
    Ok((source_abs, brief))
}

pub fn fork_session_at_event_with_lock(
    codex_dir: String,
    session_id: String,
    rollout_path: String,
    event_index: usize,
    lock: &family::FamilyLock,
) -> AppResult<ForkSessionReport> {
    family::with_lock(lock, |_g| {
        fork_session_at_event_locked(codex_dir, session_id, rollout_path, event_index)
    })
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn fork_session_at_event(
    codex_dir: String,
    session_id: String,
    rollout_path: String,
    event_index: usize,
    lock: tauri::State<'_, family::FamilyLock>,
) -> AppResult<ForkSessionReport> {
    fork_session_at_event_with_lock(
        codex_dir,
        session_id,
        rollout_path,
        event_index,
        lock.inner(),
    )
}

fn fork_session_at_event_locked(
    codex_dir: String,
    session_id: String,
    rollout_path: String,
    event_index: usize,
) -> AppResult<ForkSessionReport> {
    let codex = PathBuf::from(&codex_dir);
    let codex = codex.canonicalize().unwrap_or(codex);
    let (source_abs, source_brief) =
        resolve_fork_source_rollout(&codex, &session_id, &rollout_path)?;
    let prefix = collect_stable_prefix(&source_abs, event_index)?;
    let provider = source_brief
        .model_provider
        .clone()
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_string());

    let mut store = family::load(&codex)?;
    let family_id = family::ensure_family_for(
        &mut store,
        &session_id,
        &provider,
        &source_brief.relpath.to_string_lossy(),
        &source_brief.first_user_message,
    );
    let family_snapshot = store
        .families
        .get(&family_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("family not found: {}", family_id)))?;
    if family_snapshot.active_id != session_id {
        return Err(AppError::Other(format!(
            "只能从当前 active 分支创建回溯分支；当前 active={}，请求源={}",
            family_snapshot.active_id, session_id
        )));
    }
    let active_branch = family_snapshot
        .chain
        .iter()
        .find(|b| b.id == session_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("branch not found: {}", session_id)))?;
    let active_abs = codex
        .join(paths::checked_relative_path(
            &active_branch.rollout_relpath,
        )?)
        .canonicalize()?;
    if active_abs != source_abs {
        return Err(AppError::Other(format!(
            "请求的 rollout 不是当前 active 分支文件：{}",
            source_abs.to_string_lossy()
        )));
    }

    ensure_state_db_exists(&codex)?;
    let state = state_db::open(&codex)?;
    let new_id = new_session_id();
    let now = chrono::Utc::now();
    let new_abs = build_clone_path(&codex, &new_id, &now);
    validate_rollout_filename(&new_abs)?;
    let fallback_rel = PathBuf::from(format!(
        "sessions/{}/{}/{}/{}",
        now.format("%Y"),
        now.format("%m"),
        now.format("%d"),
        new_abs
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("rollout-{}.jsonl", new_id))
    ));
    let new_rel = new_abs
        .strip_prefix(&codex)
        .map(|p| p.to_path_buf())
        .unwrap_or(fallback_rel);

    let included_lines = write_forked_rollout_prefix(&prefix, &new_abs, &new_id, &provider)?;
    sync_thread_from_rollout(&codex, &state, &new_abs)?;
    sync_thread_from_rollout(&codex, &state, &source_abs)?;

    family::archive_with_integrity(&mut store, &codex, &family_id, &active_branch.id)?;
    let archived_dir = paths::archived_sessions_dir(&codex);
    fs::create_dir_all(&archived_dir)?;
    let archived_dest = archived_dir.join(source_abs.file_name().unwrap_or_default());
    fs::rename(&source_abs, &archived_dest)?;
    mark_thread_archived(&state, &active_branch.id, &archived_dest)?;
    remove_index_line(&codex, &active_branch.id)?;

    let new_brief = read_rollout_brief(&codex, &new_abs)?.ok_or_else(|| {
        AppError::Other(format!(
            "新分支 rollout 缺少有效 session_meta.id: {}",
            new_abs.to_string_lossy()
        ))
    })?;
    let thread_name = if new_brief.first_user_message.is_empty() {
        source_brief.first_user_message.clone()
    } else {
        new_brief.first_user_message
    };
    let new_branch = FamilyBranch {
        id: new_id.clone(),
        provider: provider.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        status: BranchStatus::Active,
        rollout_relpath: new_rel.to_string_lossy().into_owned(),
        sha256: None,
        line_count: None,
        note: Some(format!(
            "forked_from:{}@line:{}",
            active_branch.id, event_index
        )),
    };
    family::append_branch(&mut store, &family_id, new_branch)?;
    append_index_line(&codex, &new_id, &thread_name, &new_abs)?;
    family::save(&codex, &store)?;

    Ok(ForkSessionReport {
        source_id: session_id,
        new_id,
        new_rollout_path: new_abs.to_string_lossy().into_owned(),
        event_index,
        included_lines,
        cut_role: prefix.cut.role,
        cut_kind: prefix.cut.kind,
        cut_summary: prefix.cut.summary,
    })
}

/// 把一个会话克隆到指定 provider（或当前 provider）。
pub fn clone_session_for_provider_with_lock(
    codex_dir: String,
    session_id: String,
    target_provider: Option<String>,
    strategy: SwitchStrategy,
    dry_run: bool,
    lock: &family::FamilyLock,
) -> AppResult<CloneReport> {
    family::with_lock(lock, |_g| {
        clone_session_for_provider_locked(codex_dir, session_id, target_provider, strategy, dry_run)
    })
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn clone_session_for_provider(
    codex_dir: String,
    session_id: String,
    target_provider: Option<String>,
    strategy: SwitchStrategy,
    dry_run: bool,
    lock: tauri::State<'_, family::FamilyLock>,
) -> AppResult<CloneReport> {
    clone_session_for_provider_with_lock(
        codex_dir,
        session_id,
        target_provider,
        strategy,
        dry_run,
        lock.inner(),
    )
}

fn clone_session_for_provider_locked(
    codex_dir: String,
    session_id: String,
    target_provider: Option<String>,
    strategy: SwitchStrategy,
    dry_run: bool,
) -> AppResult<CloneReport> {
    let codex = PathBuf::from(&codex_dir);
    let provider = target_provider.unwrap_or_else(|| effective_current_provider(&codex));

    let mut report = CloneReport {
        source_id: session_id.clone(),
        new_id: None,
        new_rollout_path: None,
        new_provider: provider.clone(),
        ok: false,
        skipped_reason: None,
        error: None,
    };

    // 加载 family store
    let mut store = family::load(&codex)?;
    // 从 sessions/ 找到 session_id 对应文件
    let rollouts = family::scan_rollouts(&codex);
    let mut src_brief: Option<RolloutBrief> = None;
    for p in &rollouts {
        let Some(b) = read_rollout_brief(&codex, p)? else {
            continue;
        };
        if b.id == session_id {
            src_brief = Some(b);
            break;
        }
    }
    let src_brief = match src_brief {
        Some(b) => b,
        None => {
            report.error = Some(format!("未在 sessions/ 中找到 id={}", session_id));
            return Ok(report);
        }
    };

    // 注册/定位家族
    let family_id = family::ensure_family_for(
        &mut store,
        &session_id,
        src_brief.model_provider.as_deref().unwrap_or(""),
        &src_brief.relpath.to_string_lossy(),
        &src_brief.first_user_message,
    );

    // 已在当前 provider 且是 active → 无需克隆
    let active_branch = {
        let f = store.families.get(&family_id).cloned();
        f.and_then(|f| f.chain.into_iter().find(|b| b.id == f.active_id))
    };
    if let Some(b) = active_branch.as_ref() {
        if b.provider == provider {
            if !dry_run {
                ensure_state_db_exists(&codex)?;
                let state = state_db::open(&codex)?;
                sync_thread_from_rollout(&codex, &state, &src_brief.path)?;
                if let Some(cwd) = src_brief.cwd.as_deref() {
                    let _ = ensure_workspace_root_registered(&codex, cwd);
                }
            }
            report.skipped_reason = Some("已修复本地索引可见性".into());
            report.ok = true;
            return Ok(report);
        }
    }

    match strategy {
        SwitchStrategy::Follow => {
            // 直接改 src 文件第一行的 model_provider（不克隆）
            if dry_run {
                report.ok = true;
                report.skipped_reason = Some("dry_run: follow 模式将就地改写 provider".into());
                return Ok(report);
            }
            ensure_state_db_exists(&codex)?;
            let state = state_db::open(&codex)?;
            rewrite_provider_inplace(&src_brief.path, &provider)?;
            sync_thread_from_rollout(&codex, &state, &src_brief.path)?;
            if let Some(cwd) = src_brief.cwd.as_deref() {
                let _ = ensure_workspace_root_registered(&codex, cwd);
            }
            report.new_id = Some(src_brief.id.clone());
            report.new_rollout_path = Some(src_brief.path.to_string_lossy().into_owned());
            report.ok = true;
            // 家族记录：更新当前 active 分支的 provider
            if let Some(f) = store.families.get_mut(&family_id) {
                if let Some(b) = f.chain.iter_mut().find(|b| b.id == f.active_id) {
                    b.provider = provider.clone();
                }
                f.updated_at = chrono::Utc::now().to_rfc3339();
            }
            family::save(&codex, &store)?;
            Ok(report)
        }
        SwitchStrategy::Scatter | SwitchStrategy::Continuous => {
            // 从 active 分支对应的最新 rollout 文件深拷一份（保证内容连续）
            let source_rollout: PathBuf = match active_branch.as_ref() {
                Some(b) => codex.join(paths::checked_relative_path(&b.rollout_relpath)?),
                None => src_brief.path.clone(),
            };
            if !source_rollout.is_file() {
                report.error = Some(format!(
                    "源 rollout 不存在: {}",
                    source_rollout.to_string_lossy()
                ));
                return Ok(report);
            }
            let new_id = new_session_id();
            let now = chrono::Utc::now();
            let new_abs = build_clone_path(&codex, &new_id, &now);
            validate_rollout_filename(&new_abs)?;
            let fallback_rel = PathBuf::from(format!(
                "sessions/{}/{}/{}/{}",
                now.format("%Y"),
                now.format("%m"),
                now.format("%d"),
                new_abs
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("rollout-{}.jsonl", new_id))
            ));
            let new_rel = new_abs
                .strip_prefix(&codex)
                .map(|p| p.to_path_buf())
                .unwrap_or(fallback_rel);

            if dry_run {
                report.ok = true;
                report.new_id = Some(new_id);
                report.new_rollout_path = Some(new_abs.to_string_lossy().into_owned());
                report.skipped_reason = Some("dry_run: 不会写入磁盘".into());
                return Ok(report);
            }
            ensure_state_db_exists(&codex)?;
            let state = state_db::open(&codex)?;

            // 1) 写新文件
            write_cloned_rollout(
                &source_rollout,
                &new_abs,
                &new_id,
                &provider,
                active_branch
                    .as_ref()
                    .map(|b| b.id.as_str())
                    .unwrap_or(&session_id),
            )?;
            sync_thread_from_rollout(&codex, &state, &new_abs)?;
            if let Some(cwd) = src_brief.cwd.as_deref() {
                let _ = ensure_workspace_root_registered(&codex, cwd);
            }

            // 2) 连续模式下归档旧 active（用 active_branch.id）
            if matches!(strategy, SwitchStrategy::Continuous) {
                if let Some(b) = active_branch.as_ref() {
                    let old_rel = paths::checked_relative_path(&b.rollout_relpath)?;
                    let old_abs = codex.join(&old_rel);
                    if !old_abs.is_file() {
                        return Err(AppError::NotFound(format!(
                            "旧 active rollout 不存在，不能归档: {}",
                            old_abs.to_string_lossy()
                        )));
                    }
                    family::archive_with_integrity(&mut store, &codex, &family_id, &b.id)?;
                    require_thread_row(&state, &b.id)?;
                    let archived_dir = paths::archived_sessions_dir(&codex);
                    fs::create_dir_all(&archived_dir)?;
                    let dest = archived_dir.join(old_abs.file_name().unwrap_or_default());
                    fs::rename(&old_abs, &dest)?;
                    mark_thread_archived(&state, &b.id, &dest)?;
                    remove_index_line(&codex, &b.id)?;
                }
            }

            // 3) 追加新分支为 active（Scatter 模式也用同样的结构，只是不归档）
            let cloned_from_id = active_branch
                .as_ref()
                .map(|b| b.id.clone())
                .unwrap_or_else(|| session_id.clone());
            let new_branch = FamilyBranch {
                id: new_id.clone(),
                provider: provider.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
                status: BranchStatus::Active,
                rollout_relpath: new_rel.to_string_lossy().into_owned(),
                sha256: None,
                line_count: None,
                note: Some(format!("cloned_from:{}", cloned_from_id)),
            };
            if matches!(strategy, SwitchStrategy::Scatter) {
                if let Some(f) = store.families.get_mut(&family_id) {
                    for b in f.chain.iter_mut() {
                        if matches!(b.status, BranchStatus::Active) {
                            b.status = BranchStatus::Archived;
                        }
                    }
                    f.chain.push(new_branch);
                    f.active_id = new_id.clone();
                    f.updated_at = chrono::Utc::now().to_rfc3339();
                }
                store.index.insert(new_id.clone(), family_id.clone());
            } else {
                family::append_branch(&mut store, &family_id, new_branch)?;
            }

            // 4) 更新 session_index.jsonl（追加一行）
            append_index_line(&codex, &new_id, &src_brief.first_user_message, &new_abs)?;

            family::save(&codex, &store)?;

            report.new_id = Some(new_id);
            report.new_rollout_path = Some(new_abs.to_string_lossy().into_owned());
            report.ok = true;
            Ok(report)
        }
    }
}

fn rewrite_provider_inplace(path: &Path, new_provider: &str) -> AppResult<()> {
    let raw = fs::read_to_string(path)?;
    let mut rewritten = false;
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if !rewritten {
                if let Ok(mut v) = serde_json::from_str::<Value>(line) {
                    if v.get("type").and_then(|x| x.as_str()) == Some("session_meta") {
                        if let Some(payload) = v.get_mut("payload").and_then(|p| p.as_object_mut())
                        {
                            payload.insert(
                                "model_provider".into(),
                                Value::String(new_provider.into()),
                            );
                        }
                        writeln!(f, "{}", serde_json::to_string(&v)?)?;
                        rewritten = true;
                        continue;
                    }
                }
            }
            writeln!(f, "{}", line)?;
        }
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

pub(crate) fn append_index_line(
    codex: &Path,
    id: &str,
    thread_name: &str,
    _rollout_abs: &Path,
) -> AppResult<()> {
    let index_path = paths::session_index_path(codex);
    // 与 codex 原生 SessionIndexEntry 对齐：{ id, thread_name, updated_at: RFC3339 }
    // 不再写 rollout_path（codex 不识别），不再用毫秒数字（codex 期望 String）。
    let entry = serde_json::json!({
        "id": id,
        "thread_name": thread_name,
        "updated_at": chrono::Utc::now().to_rfc3339(),
    });
    let entry_line = serde_json::to_string(&entry)?;

    let mut lines: Vec<String> = Vec::new();
    let mut replaced = false;
    if index_path.is_file() {
        let f = fs::File::open(&index_path)?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            let is_match = match serde_json::from_str::<Value>(&line) {
                Ok(v) => {
                    v.get("id").and_then(|x| x.as_str()) == Some(id)
                        || v.get("session_id").and_then(|x| x.as_str()) == Some(id)
                }
                Err(_) => false,
            };
            if is_match {
                if !replaced {
                    lines.push(entry_line.clone());
                    replaced = true;
                }
            } else if !line.trim().is_empty() {
                lines.push(line);
            }
        }
    }
    if !replaced {
        lines.push(entry_line);
    }

    let tmp = index_path.with_extension("jsonl.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        for line in &lines {
            writeln!(f, "{}", line)?;
        }
        f.sync_all().ok();
    }
    fs::rename(&tmp, &index_path)?;
    Ok(())
}

/// 列出"active 分支 provider ≠ target_provider"的 session id（去重，稳定顺序）。
///
/// - 优先读 `session_family.json`（单点真相）
/// - 对尚未进入 family store 的历史会话继续扫描 rollout，避免部分迁移状态漏处理
/// - 已在 target_provider 下存在 clone（同家族有匹配 provider 的分支）的不计入
fn list_mismatched_session_ids(codex: &Path, target_provider: &str) -> AppResult<Vec<String>> {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut family_managed_ids: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    let thread_states = read_thread_state_map(codex)?;

    let store = family::load(codex)?;
    family_managed_ids.extend(store.index.keys().cloned());
    for f in store.families.values() {
        family_managed_ids.extend(f.chain.iter().map(|b| b.id.clone()));
        if let Some(active) = f.chain.iter().find(|b| b.id == f.active_id) {
            if thread_state_is_subagent(&thread_states, &active.id) {
                continue;
            }
            let has_target_branch = f.chain.iter().any(|b| b.provider == target_provider);
            if active.provider != target_provider && has_target_branch {
                continue;
            }
            let state_drift =
                !thread_state_matches_active_provider(&thread_states, &active.id, &active.provider);
            if (active.provider != target_provider || state_drift) && seen.insert(active.id.clone())
            {
                out.push(active.id.clone());
            }
        }
    }

    for p in family::scan_rollouts(codex) {
        let Some(b) = read_rollout_brief(codex, &p)? else {
            continue;
        };
        if family_managed_ids.contains(&b.id) {
            continue;
        }
        if is_subagent_source(b.source.as_deref()) {
            continue;
        }
        let provider = b.model_provider.as_deref().unwrap_or(DEFAULT_PROVIDER);
        let state_drift = !thread_state_matches_active_provider(&thread_states, &b.id, provider);
        if (provider != target_provider || state_drift) && seen.insert(b.id.clone()) {
            out.push(b.id);
        }
    }
    Ok(out)
}

/// 对所有 active 分支 provider ≠ 当前 provider 的家族批量克隆。
pub fn batch_clone_for_current_provider_with_lock(
    codex_dir: String,
    strategy: SwitchStrategy,
    dry_run: bool,
    lock: &family::FamilyLock,
) -> AppResult<Vec<CloneReport>> {
    family::with_lock(lock, |_g| {
        let codex = PathBuf::from(&codex_dir);
        let cur = effective_current_provider(&codex);

        let targets = list_mismatched_session_ids(&codex, &cur)?;

        let mut out: Vec<CloneReport> = Vec::new();
        for id in targets {
            match clone_session_for_provider_locked(
                codex_dir.clone(),
                id.clone(),
                Some(cur.clone()),
                strategy.clone(),
                dry_run,
            ) {
                Ok(r) => out.push(r),
                Err(e) => out.push(CloneReport {
                    source_id: id,
                    new_id: None,
                    new_rollout_path: None,
                    new_provider: cur.clone(),
                    ok: false,
                    skipped_reason: None,
                    error: Some(e.to_string()),
                }),
            }
        }
        Ok(out)
    })
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn batch_clone_for_current_provider(
    codex_dir: String,
    strategy: SwitchStrategy,
    dry_run: bool,
    lock: tauri::State<'_, family::FamilyLock>,
) -> AppResult<Vec<CloneReport>> {
    batch_clone_for_current_provider_with_lock(codex_dir, strategy, dry_run, lock.inner())
}

/// 回滚：把家族的 active 切回某个历史分支（把当前 active 归档，目标分支从归档恢复）。
pub fn rollback_family_active_with_lock(
    codex_dir: String,
    family_id: String,
    target_branch_id: String,
    lock: &family::FamilyLock,
) -> AppResult<()> {
    family::with_lock(lock, |_g| {
        rollback_family_active_locked(codex_dir, family_id, target_branch_id)
    })
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn rollback_family_active(
    codex_dir: String,
    family_id: String,
    target_branch_id: String,
    lock: tauri::State<'_, family::FamilyLock>,
) -> AppResult<()> {
    rollback_family_active_with_lock(codex_dir, family_id, target_branch_id, lock.inner())
}

fn rollback_family_active_locked(
    codex_dir: String,
    family_id: String,
    target_branch_id: String,
) -> AppResult<()> {
    let codex = PathBuf::from(&codex_dir);
    ensure_state_db_exists(&codex)?;
    let state = state_db::open(&codex)?;
    let mut store = family::load(&codex)?;
    let family = store
        .families
        .get(&family_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("family: {}", family_id)))?;
    // 当前 active 归档
    if let Some(cur_active) = family.chain.iter().find(|b| b.id == family.active_id) {
        let cur_rel = paths::checked_relative_path(&cur_active.rollout_relpath)?;
        let abs = codex.join(&cur_rel);
        if !abs.is_file() {
            return Err(AppError::NotFound(format!(
                "当前 active rollout 不存在，不能归档: {}",
                abs.to_string_lossy()
            )));
        }
        family::archive_with_integrity(&mut store, &codex, &family_id, &cur_active.id)?;
        require_thread_row(&state, &cur_active.id)?;
        let archived_dir = paths::archived_sessions_dir(&codex);
        fs::create_dir_all(&archived_dir)?;
        let dest = archived_dir.join(abs.file_name().unwrap_or_default());
        fs::rename(&abs, &dest)?;
        mark_thread_archived(&state, &cur_active.id, &dest)?;
        remove_index_line(&codex, &cur_active.id)?;
    }
    // 目标分支从归档恢复
    if let Some(target) = family.chain.iter().find(|b| b.id == target_branch_id) {
        let target_rel = paths::checked_relative_path(&target.rollout_relpath)?;
        let expected_abs = codex.join(&target_rel);
        if !expected_abs.is_file() {
            let archived = paths::archived_sessions_dir(&codex)
                .join(target_rel.file_name().unwrap_or_default());
            if archived.is_file() {
                if let Some(parent) = expected_abs.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&archived, &expected_abs)?;
            } else {
                return Err(AppError::NotFound(format!(
                    "目标分支 rollout 丢失: {}",
                    expected_abs.to_string_lossy()
                )));
            }
        }
        sync_thread_from_rollout(&codex, &state, &expected_abs)?;
        let brief = read_rollout_brief(&codex, &expected_abs)?;
        let thread_name = brief
            .as_ref()
            .map(|b| b.first_user_message.clone())
            .unwrap_or_default();
        append_index_line(&codex, &target_branch_id, &thread_name, &expected_abs)?;
        if let Some(cwd) = brief.as_ref().and_then(|b| b.cwd.as_deref()) {
            let _ = ensure_workspace_root_registered(&codex, cwd);
        }
    } else {
        return Err(AppError::NotFound(format!(
            "branch not in family {}: {}",
            family_id, target_branch_id
        )));
    }
    family::set_active(&mut store, &family_id, &target_branch_id)?;
    family::save(&codex, &store)?;
    Ok(())
}

/// 删除一个家族分支：清理 family.chain + 复用 sessions::delete_one 的全套清理。
/// 不允许删除 active 分支（必须先切换或回滚）。
pub fn delete_family_branch_with_lock(
    codex_dir: String,
    family_id: String,
    branch_id: String,
    lock: &family::FamilyLock,
) -> AppResult<crate::models::DeleteResult> {
    family::with_lock(lock, |_g| {
        delete_family_branch_locked(codex_dir, family_id, branch_id)
    })
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn delete_family_branch(
    codex_dir: String,
    family_id: String,
    branch_id: String,
    lock: tauri::State<'_, family::FamilyLock>,
) -> AppResult<crate::models::DeleteResult> {
    delete_family_branch_with_lock(codex_dir, family_id, branch_id, lock.inner())
}

fn delete_family_branch_locked(
    codex_dir: String,
    family_id: String,
    branch_id: String,
) -> AppResult<crate::models::DeleteResult> {
    let codex = PathBuf::from(&codex_dir);
    let mut store = family::load(&codex)?;
    let family = store
        .families
        .get(&family_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("family: {}", family_id)))?;
    if family.active_id == branch_id {
        return Err(AppError::Other(
            "不能删除当前 active 分支，请先切换到其他分支".into(),
        ));
    }
    if !family.chain.iter().any(|b| b.id == branch_id) {
        return Err(AppError::NotFound(format!(
            "branch not in family {}: {}",
            family_id, branch_id
        )));
    }

    // 1) 走 sessions::delete_one 把 threads / logs / rollout / session_index 一并清掉
    let result = crate::sessions::delete_one_for_family(&codex, &branch_id)?;

    // 2) 同时检查归档目录里是否有同名文件，一并删除
    let archived_dir = paths::archived_sessions_dir(&codex);
    if archived_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&archived_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name.contains(&branch_id) {
                    let _ = fs::remove_file(&p);
                }
            }
        }
    }

    // 3) 从 family.chain 移除并重保存
    if let Some(f) = store.families.get_mut(&family_id) {
        f.chain.retain(|b| b.id != branch_id);
        f.updated_at = chrono::Utc::now().to_rfc3339();
    }
    store.index.remove(&branch_id);
    family::save(&codex, &store)?;

    Ok(result)
}

/// 读取每个非 active 分支相对当前 active 分支的可同步状态。
/// 比较时跳过第 1 行 session_meta，因为 clone 后 id/provider 不同是正常的。
pub fn get_family_branch_sync_states_with_lock(
    codex_dir: String,
    family_id: String,
    lock: &family::FamilyLock,
) -> AppResult<Vec<BranchSyncState>> {
    family::with_lock(lock, |_g| {
        get_family_branch_sync_states_locked(codex_dir, family_id)
    })
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn get_family_branch_sync_states(
    codex_dir: String,
    family_id: String,
    lock: tauri::State<'_, family::FamilyLock>,
) -> AppResult<Vec<BranchSyncState>> {
    get_family_branch_sync_states_with_lock(codex_dir, family_id, lock.inner())
}

fn get_family_branch_sync_states_locked(
    codex_dir: String,
    family_id: String,
) -> AppResult<Vec<BranchSyncState>> {
    let codex = PathBuf::from(&codex_dir);
    let store = family::load(&codex)?;
    let family = store
        .families
        .get(&family_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("family: {}", family_id)))?;
    let active_branch = family
        .chain
        .iter()
        .find(|b| b.id == family.active_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound("active 分支缺失".into()))?;
    let active_abs = resolve_branch_rollout(&codex, &active_branch)?;
    let active_lines = read_rollout_lines(&active_abs)?;
    if active_lines.is_empty() {
        return Err(AppError::Other("当前 active 分支为空 rollout".into()));
    }

    let mut states = Vec::with_capacity(family.chain.len());
    for branch in family.chain.iter() {
        if branch.id == family.active_id {
            states.push(BranchSyncState {
                branch_id: branch.id.clone(),
                relation: "current".into(),
                active_lines: Some(active_lines.len() as u64),
                branch_lines: Some(active_lines.len() as u64),
                appendable_lines_to_active: 0,
                appendable_lines_to_branch: 0,
                error: None,
            });
            continue;
        }

        let state =
            match resolve_branch_rollout(&codex, branch).and_then(|p| read_rollout_lines(&p)) {
                Ok(branch_lines) if branch_lines.is_empty() => BranchSyncState {
                    branch_id: branch.id.clone(),
                    relation: "missing".into(),
                    active_lines: Some(active_lines.len() as u64),
                    branch_lines: Some(0),
                    appendable_lines_to_active: 0,
                    appendable_lines_to_branch: 0,
                    error: Some("分支为空 rollout".into()),
                },
                Ok(branch_lines) => {
                    let (relation, to_active, to_branch) =
                        compare_rollout_lines(&active_lines, &branch_lines);
                    BranchSyncState {
                        branch_id: branch.id.clone(),
                        relation,
                        active_lines: Some(active_lines.len() as u64),
                        branch_lines: Some(branch_lines.len() as u64),
                        appendable_lines_to_active: to_active,
                        appendable_lines_to_branch: to_branch,
                        error: None,
                    }
                }
                Err(e) => BranchSyncState {
                    branch_id: branch.id.clone(),
                    relation: "missing".into(),
                    active_lines: Some(active_lines.len() as u64),
                    branch_lines: None,
                    appendable_lines_to_active: 0,
                    appendable_lines_to_branch: 0,
                    error: Some(e.to_string()),
                },
            };
        states.push(state);
    }
    Ok(states)
}

/// 把某个非 active 分支的新增内容安全合并到当前 active 分支。
/// 场景：克隆 / 修复后继续在旧分支（如 archived 的 custom）上追加了新消息，
/// 希望这部分增量也能在当前 provider 的 active 分支里可见。
/// 策略：仅当源分支是 active 分支的"行级前缀超集"时允许合并。
pub fn sync_branch_into_active_with_lock(
    codex_dir: String,
    family_id: String,
    source_branch_id: String,
    lock: &family::FamilyLock,
) -> AppResult<SyncBranchReport> {
    family::with_lock(lock, |_g| {
        sync_branch_into_active_locked(codex_dir, family_id, source_branch_id)
    })
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn sync_branch_into_active(
    codex_dir: String,
    family_id: String,
    source_branch_id: String,
    lock: tauri::State<'_, family::FamilyLock>,
) -> AppResult<SyncBranchReport> {
    sync_branch_into_active_with_lock(codex_dir, family_id, source_branch_id, lock.inner())
}

fn sync_branch_into_active_locked(
    codex_dir: String,
    family_id: String,
    source_branch_id: String,
) -> AppResult<SyncBranchReport> {
    let active_id = active_branch_id(&codex_dir, &family_id)?;
    if active_id == source_branch_id {
        return Err(AppError::Other("源分支即为当前 active，无需同步".into()));
    }
    let r = append_branch_extras_locked(codex_dir, family_id, source_branch_id, active_id.clone())?;
    Ok(SyncBranchReport {
        active_id,
        source_id: r.source_id,
        appended_lines: r.appended_lines,
        total_lines: r.total_lines,
    })
}

/// 把当前 active 分支新增内容同步到某个历史分支。
/// 场景：当前 provider 继续对话后，历史 provider 分支落后；同步后再切回该 provider
/// 时也能带上当前分支的新增上下文。
pub fn sync_active_into_branch_with_lock(
    codex_dir: String,
    family_id: String,
    target_branch_id: String,
    lock: &family::FamilyLock,
) -> AppResult<BranchSyncReport> {
    family::with_lock(lock, |_g| {
        let active_id = active_branch_id(&codex_dir, &family_id)?;
        if active_id == target_branch_id {
            return Err(AppError::Other("目标分支即为当前 active，无需同步".into()));
        }
        append_branch_extras_locked(codex_dir, family_id, active_id, target_branch_id)
    })
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn sync_active_into_branch(
    codex_dir: String,
    family_id: String,
    target_branch_id: String,
    lock: tauri::State<'_, family::FamilyLock>,
) -> AppResult<BranchSyncReport> {
    sync_active_into_branch_with_lock(codex_dir, family_id, target_branch_id, lock.inner())
}

fn active_branch_id(codex_dir: &str, family_id: &str) -> AppResult<String> {
    let codex = PathBuf::from(codex_dir);
    let store = family::load(&codex)?;
    let family = store
        .families
        .get(family_id)
        .ok_or_else(|| AppError::NotFound(format!("family: {}", family_id)))?;
    Ok(family.active_id.clone())
}

fn append_branch_extras_locked(
    codex_dir: String,
    family_id: String,
    source_branch_id: String,
    target_branch_id: String,
) -> AppResult<BranchSyncReport> {
    let codex = PathBuf::from(&codex_dir);
    let mut store = family::load(&codex)?;
    let family = store
        .families
        .get(&family_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("family: {}", family_id)))?;
    if source_branch_id == target_branch_id {
        return Err(AppError::Other("源分支和目标分支相同，无需同步".into()));
    }
    let source_branch = family
        .chain
        .iter()
        .find(|b| b.id == source_branch_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("branch: {}", source_branch_id)))?;
    let target_branch = family
        .chain
        .iter()
        .find(|b| b.id == target_branch_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("branch: {}", target_branch_id)))?;

    let source_abs = resolve_branch_rollout(&codex, &source_branch)?;
    let target_abs = resolve_branch_rollout(&codex, &target_branch)?;
    let source_lines = read_rollout_lines(&source_abs)?;
    let target_lines = read_rollout_lines(&target_abs)?;

    validate_source_has_target_prefix(&source_lines, &target_lines)?;

    // 取过滤掉克隆痕迹行后的"可比较 body"；写入时也按这个口径来，
    // 避免把 source 里的 trace 又传染给 target。
    let source_body = comparable_body(&source_lines);
    let target_body = comparable_body(&target_lines);
    let extras: Vec<String> = source_body[target_body.len()..]
        .iter()
        .map(|s| (*s).clone())
        .collect();
    let appended = extras.len() as u32;

    let tmp = target_abs.with_extension("jsonl.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        // 保留目标的 session_meta（首行），后续 body 一律按过滤口径重写
        if let Some(first) = target_lines.first() {
            writeln!(f, "{}", first)?;
        }
        for line in target_body.iter() {
            writeln!(f, "{}", line)?;
        }
        for line in extras.iter() {
            writeln!(f, "{}", line)?;
        }
        f.sync_all().ok();
    }
    fs::rename(&tmp, &target_abs)?;

    if target_branch.id == family.active_id {
        ensure_state_db_exists(&codex)?;
        let state = state_db::open(&codex)?;
        sync_thread_from_rollout(&codex, &state, &target_abs)?;
        let brief = read_rollout_brief(&codex, &target_abs)?;
        let thread_name = brief
            .as_ref()
            .map(|b| b.first_user_message.clone())
            .unwrap_or_default();
        append_index_line(&codex, &target_branch.id, &thread_name, &target_abs)?;
        if let Some(cwd) = brief.as_ref().and_then(|b| b.cwd.as_deref()) {
            let _ = ensure_workspace_root_registered(&codex, cwd);
        }
    }

    if let Some(f) = store.families.get_mut(&family_id) {
        if let Some(b) = f.chain.iter_mut().find(|b| b.id == target_branch_id) {
            if target_branch.id == family.active_id {
                b.sha256 = None;
                b.line_count = None;
            } else {
                let (sha, lines) = family::compute_integrity(&target_abs)?;
                b.sha256 = Some(sha);
                b.line_count = Some(lines);
            }
            b.note = Some(format!("synced_from:{}", source_branch_id));
        }
        f.updated_at = chrono::Utc::now().to_rfc3339();
    }
    family::save(&codex, &store)?;

    Ok(BranchSyncReport {
        source_id: source_branch_id,
        target_id: target_branch_id,
        appended_lines: appended,
        total_lines: source_lines.len() as u32,
    })
}

fn resolve_branch_rollout(codex: &Path, branch: &FamilyBranch) -> AppResult<PathBuf> {
    let rel = paths::checked_relative_path(&branch.rollout_relpath)?;
    let main = codex.join(&rel);
    if main.is_file() {
        return Ok(main);
    }
    let archived = paths::archived_sessions_dir(codex).join(rel.file_name().unwrap_or_default());
    if archived.is_file() {
        return Ok(archived);
    }
    Err(AppError::NotFound(format!(
        "分支 rollout 丢失: {}",
        rel.to_string_lossy()
    )))
}

fn read_rollout_lines(path: &Path) -> AppResult<Vec<String>> {
    Ok(BufReader::new(fs::File::open(path)?)
        .lines()
        .collect::<std::io::Result<Vec<_>>>()?)
}

/// 判断一行是否是"克隆痕迹"（本工具早期写入的元事件，对内容比较来说是噪声）。
/// 这类行只在新分支里出现，不应让两份 rollout 被判为分叉。
fn is_clone_trace_line(line: &str) -> bool {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|v| {
            let typ = v.get("type")?.as_str()?.to_string();
            if typ != "event_msg" {
                return None;
            }
            let sub = v.get("payload")?.get("type")?.as_str()?.to_string();
            Some(sub == "session_cloned")
        })
        .unwrap_or(false)
}

/// 取 rollout 的可比较 body：跳过第 1 行 session_meta，过滤已知的克隆痕迹行。
fn comparable_body(lines: &[String]) -> Vec<&String> {
    lines
        .iter()
        .skip(1)
        .filter(|l| !is_clone_trace_line(l))
        .collect()
}

fn compare_rollout_lines(active_lines: &[String], branch_lines: &[String]) -> (String, u32, u32) {
    let active_body = comparable_body(active_lines);
    let branch_body = comparable_body(branch_lines);
    if branch_body == active_body {
        ("same".into(), 0, 0)
    } else if branch_body.len() > active_body.len() && branch_body.starts_with(&active_body[..]) {
        (
            "branch_ahead".into(),
            (branch_body.len() - active_body.len()) as u32,
            0,
        )
    } else if active_body.len() > branch_body.len() && active_body.starts_with(&branch_body[..]) {
        (
            "active_ahead".into(),
            0,
            (active_body.len() - branch_body.len()) as u32,
        )
    } else {
        ("diverged".into(), 0, 0)
    }
}

fn validate_source_has_target_prefix(
    source_lines: &[String],
    target_lines: &[String],
) -> AppResult<()> {
    if source_lines.is_empty() || target_lines.is_empty() {
        return Err(AppError::Other("源或目标分支为空 rollout".into()));
    }
    let source_body = comparable_body(source_lines);
    let target_body = comparable_body(target_lines);
    if source_body.len() <= target_body.len() {
        return Err(AppError::Other(format!(
            "源分支无新增内容（源 {} 行，目标 {} 行；不计 session_meta 与克隆痕迹）",
            source_body.len(),
            target_body.len()
        )));
    }
    if !source_body.starts_with(&target_body[..]) {
        for (i, target_line) in target_body.iter().enumerate() {
            if source_body.get(i) != Some(target_line) {
                return Err(AppError::Other(format!(
                    "两份内容从第 {} 行（不计 session_meta 与克隆痕迹）开始出现冲突，无法安全同步。请先切换分支后人工处理",
                    i + 1
                )));
            }
        }
        return Err(AppError::Other("两份内容已分叉，无法安全同步".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{BranchStatus, Family, FamilyBranch, FamilyStore};
    use std::collections::BTreeMap;

    fn temp_codex_dir(name: &str) -> PathBuf {
        let unique = format!(
            "{}-{}-{}",
            name,
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        );
        std::env::temp_dir().join(unique)
    }

    fn write_rollout_in(codex: &Path, root: &str, id: &str, provider: &str) -> AppResult<()> {
        let rollout_dir = codex.join(root).join("2026").join("04").join("22");
        fs::create_dir_all(&rollout_dir)?;
        let path = rollout_dir.join(format!("rollout-{}.jsonl", id));
        let line = serde_json::json!({
            "timestamp": "2026-04-22T00:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "model_provider": provider,
                "cwd": "F:\\project\\example"
            }
        });
        fs::write(path, format!("{}\n", serde_json::to_string(&line)?))?;
        Ok(())
    }

    fn write_rollout(codex: &Path, id: &str, provider: &str) -> AppResult<()> {
        write_rollout_in(codex, "sessions", id, provider)
    }

    fn write_rollout_with_cwd(codex: &Path, id: &str, cwd: &Path) -> AppResult<()> {
        let rollout_dir = codex.join("sessions").join("2026").join("04").join("22");
        fs::create_dir_all(&rollout_dir)?;
        let path = rollout_dir.join(format!("rollout-{}.jsonl", id));
        let line = serde_json::json!({
            "timestamp": "2026-04-22T00:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "model_provider": DEFAULT_PROVIDER,
                "cwd": cwd.to_string_lossy()
            }
        });
        fs::write(path, format!("{}\n", serde_json::to_string(&line)?))?;
        Ok(())
    }

    fn write_claude_session(claude: &Path, id: &str) -> AppResult<()> {
        let dir = claude.join("projects").join("sample-project");
        fs::create_dir_all(&dir)?;
        let line = serde_json::json!({
            "sessionId": id,
            "cwd": "F:\\project\\example",
            "timestamp": "2026-04-22T00:00:00Z",
            "type": "user",
            "message": {"role": "user", "content": "hello"}
        });
        fs::write(
            dir.join(format!("{id}.jsonl")),
            format!("{}\n", serde_json::to_string(&line)?),
        )?;
        Ok(())
    }

    fn create_minimal_state(codex: &Path) -> AppResult<rusqlite::Connection> {
        fs::create_dir_all(codex)?;
        let conn = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        conn.execute(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                model_provider TEXT,
                source TEXT,
                archived INTEGER
            )",
            [],
        )?;
        Ok(conn)
    }

    fn create_full_state(codex: &Path) -> AppResult<rusqlite::Connection> {
        fs::create_dir_all(codex)?;
        let conn = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        let cols = THREADS_COLS
            .iter()
            .map(|name| {
                if *name == "id" {
                    "id TEXT PRIMARY KEY".to_string()
                } else {
                    format!("{name} TEXT")
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        conn.execute(&format!("CREATE TABLE threads ({cols})"), [])?;
        Ok(conn)
    }

    fn write_conversation_rollout(codex: &Path, id: &str) -> AppResult<PathBuf> {
        let rollout_dir = codex.join("sessions").join("2026").join("04").join("23");
        fs::create_dir_all(&rollout_dir)?;
        let path = rollout_dir.join(format!("rollout-{id}.jsonl"));
        let lines = vec![
            serde_json::json!({
                "timestamp": "2026-04-23T00:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "model_provider": DEFAULT_PROVIDER,
                    "cwd": "F:\\project\\example",
                    "source": DEFAULT_THREAD_SOURCE
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-04-23T00:00:01Z",
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": "First request"
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-04-23T00:00:02Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Stable answer"}]
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-04-23T00:00:03Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "decode_image"
                }
            })
            .to_string(),
            "{not valid json".to_string(),
        ];
        fs::write(&path, format!("{}\n", lines.join("\n")))?;
        Ok(path)
    }

    fn write_token_rollout(codex: &Path, id: &str) -> AppResult<PathBuf> {
        let rollout_dir = codex.join("sessions").join("2026").join("04").join("24");
        fs::create_dir_all(&rollout_dir)?;
        let path = rollout_dir.join(format!("rollout-{id}.jsonl"));
        let lines = vec![
            serde_json::json!({
                "timestamp": "2026-04-24T00:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "model_provider": DEFAULT_PROVIDER,
                    "cwd": "F:\\project\\example",
                    "source": DEFAULT_THREAD_SOURCE
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-04-24T00:00:01Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "total_tokens": 2_468_000
                        }
                    }
                }
            })
            .to_string(),
        ];
        fs::write(&path, format!("{}\n", lines.join("\n")))?;
        Ok(path)
    }

    fn write_index_line(codex: &Path, id: &str) -> AppResult<()> {
        let line = serde_json::json!({
            "id": id,
            "thread_name": "First request",
            "updated_at": "2026-04-23T00:00:02Z"
        });
        fs::write(
            paths::session_index_path(codex),
            format!("{}\n", serde_json::to_string(&line)?),
        )?;
        Ok(())
    }

    #[test]
    fn thread_rebuild_values_include_rollout_token_count() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-repair-token-test");
        let rollout = write_token_rollout(&codex, "token-session")?;

        let values = thread_values_from_rollout(&codex, &rollout, false)?.expect("thread values");
        fs::remove_dir_all(&codex).ok();

        let token_index = THREADS_COLS
            .iter()
            .position(|name| *name == "tokens_used")
            .expect("tokens_used column");
        assert_eq!(values[token_index], Value::from(2_468_000i64));
        Ok(())
    }

    #[test]
    fn fork_session_at_event_copies_only_stable_prefix_and_archives_source() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-fork-test");
        let source_id = "source-session";
        let rollout = write_conversation_rollout(&codex, source_id)?;
        create_full_state(&codex)?;
        {
            let state = state_db::open(&codex)?;
            sync_thread_from_rollout(&codex, &state, &rollout)?;
        }
        write_index_line(&codex, source_id)?;

        let report = fork_session_at_event_locked(
            codex.to_string_lossy().into_owned(),
            source_id.to_string(),
            rollout.to_string_lossy().into_owned(),
            2,
        )?;

        assert_eq!(report.source_id, source_id);
        assert_eq!(report.event_index, 2);
        assert_eq!(report.included_lines, 3);
        assert_eq!(report.cut_role, "assistant");

        let new_path = PathBuf::from(&report.new_rollout_path);
        assert!(new_path.is_file());
        let new_lines = read_rollout_lines(&new_path)?;
        assert_eq!(new_lines.len(), 3);
        assert!(new_lines
            .iter()
            .all(|line| !line.contains("decode_image") && !line.contains("not valid json")));
        let first: Value = serde_json::from_str(&new_lines[0])?;
        assert_eq!(
            first
                .get("payload")
                .and_then(|p| p.get("id"))
                .and_then(|x| x.as_str()),
            Some(report.new_id.as_str())
        );

        assert!(!rollout.exists());
        assert!(paths::archived_sessions_dir(&codex)
            .join(rollout.file_name().unwrap())
            .is_file());
        let store = family::load(&codex)?;
        let family_id = store.index.get(source_id).expect("source family");
        let family = store.families.get(family_id).expect("family");
        assert_eq!(family.active_id, report.new_id);
        assert_eq!(family.chain.len(), 2);
        assert!(family
            .chain
            .iter()
            .any(|b| b.id == source_id && matches!(b.status, BranchStatus::Archived)));
        assert!(family.chain.iter().any(|b| {
            b.id == report.new_id
                && matches!(b.status, BranchStatus::Active)
                && b.note.as_deref() == Some("forked_from:source-session@line:2")
        }));

        let state = state_db::open_ro(&codex)?;
        let old_archived: String = state.query_row(
            "SELECT archived FROM threads WHERE id = ?",
            [source_id],
            |row| row.get(0),
        )?;
        let new_archived: String = state.query_row(
            "SELECT archived FROM threads WHERE id = ?",
            [report.new_id.as_str()],
            |row| row.get(0),
        )?;
        assert_eq!(old_archived, "1");
        assert_eq!(new_archived, "0");

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn fork_session_at_event_rejects_unstable_or_damaged_prefix() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-fork-reject-test");
        let source_id = "source-session";
        let rollout = write_conversation_rollout(&codex, source_id)?;
        create_full_state(&codex)?;
        {
            let state = state_db::open(&codex)?;
            sync_thread_from_rollout(&codex, &state, &rollout)?;
        }

        let err = fork_session_at_event_locked(
            codex.to_string_lossy().into_owned(),
            source_id.to_string(),
            rollout.to_string_lossy().into_owned(),
            3,
        )
        .expect_err("tool call is not a stable cut point");
        assert!(err.to_string().contains("稳定对话节点"));

        let err = fork_session_at_event_locked(
            codex.to_string_lossy().into_owned(),
            source_id.to_string(),
            rollout.to_string_lossy().into_owned(),
            4,
        )
        .expect_err("damaged target line must be rejected");
        assert!(err.to_string().contains("不是有效 JSONL"));
        assert!(rollout.exists());

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn mismatched_scan_includes_unregistered_rollouts_when_family_store_exists() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-repair-test");
        fs::create_dir_all(&codex)?;

        let mut families = BTreeMap::new();
        families.insert(
            "managed-source".to_string(),
            Family {
                family_id: "managed-source".to_string(),
                root_id: "managed-source".to_string(),
                title: "managed".to_string(),
                active_id: "managed-source".to_string(),
                updated_at: "2026-04-22T00:00:00Z".to_string(),
                chain: vec![
                    FamilyBranch {
                        id: "managed-source".to_string(),
                        provider: "anthropic".to_string(),
                        created_at: "2026-04-22T00:00:00Z".to_string(),
                        status: BranchStatus::Active,
                        rollout_relpath: "sessions/2026/04/22/rollout-managed-source.jsonl"
                            .to_string(),
                        sha256: None,
                        line_count: None,
                        note: None,
                    },
                    FamilyBranch {
                        id: "managed-target".to_string(),
                        provider: "openai".to_string(),
                        created_at: "2026-04-22T00:00:00Z".to_string(),
                        status: BranchStatus::Archived,
                        rollout_relpath: "sessions/2026/04/22/rollout-managed-target.jsonl"
                            .to_string(),
                        sha256: None,
                        line_count: None,
                        note: None,
                    },
                ],
            },
        );

        let mut index = BTreeMap::new();
        index.insert("managed-source".to_string(), "managed-source".to_string());
        index.insert("managed-target".to_string(), "managed-source".to_string());
        family::save(
            &codex,
            &FamilyStore {
                version: 1,
                families,
                index,
            },
        )?;

        write_rollout(&codex, "legacy-session", "anthropic")?;

        let targets = list_mismatched_session_ids(&codex, "openai")?;
        fs::remove_dir_all(&codex).ok();

        assert_eq!(targets, vec!["legacy-session".to_string()]);
        Ok(())
    }

    #[test]
    fn mismatched_scan_includes_hidden_source_rows_for_resync() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-hidden-source-test");
        write_rollout(&codex, "hidden-source-session", DEFAULT_PROVIDER)?;
        let conn = create_minimal_state(&codex)?;
        conn.execute(
            "INSERT INTO threads (id, model_provider, source, archived) VALUES (?1, ?2, ?3, 0)",
            (
                "hidden-source-session",
                DEFAULT_PROVIDER,
                "cc-session-manager",
            ),
        )?;

        let targets = list_mismatched_session_ids(&codex, DEFAULT_PROVIDER)?;
        fs::remove_dir_all(&codex).ok();

        assert_eq!(targets, vec!["hidden-source-session".to_string()]);
        Ok(())
    }

    #[test]
    fn diagnostics_do_not_treat_archived_rollouts_as_orphan_threads() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-archived-test");
        write_rollout_in(
            &codex,
            "archived_sessions",
            "archived-session",
            DEFAULT_PROVIDER,
        )?;
        let conn = create_minimal_state(&codex)?;
        conn.execute(
            "INSERT INTO threads (id, model_provider, source, archived) VALUES (?1, ?2, ?3, 1)",
            ("archived-session", DEFAULT_PROVIDER, DEFAULT_THREAD_SOURCE),
        )?;

        let diag = diagnose_codex_state(codex.to_string_lossy().into_owned())?;
        let prune = prune_orphan_entries(codex.to_string_lossy().into_owned(), false, true, true)?;
        fs::remove_dir_all(&codex).ok();

        assert_eq!(diag.archived_rollout_count, 1);
        assert_eq!(diag.threads_count, 1);
        assert_eq!(diag.threads_active_count, 0);
        assert_eq!(diag.threads_archived_count, 1);
        assert!(diag.orphan_in_threads.is_empty());
        assert_eq!(prune.threads_removed, 0);
        Ok(())
    }

    #[test]
    fn project_config_diagnosis_repairs_missing_multi_agent_default() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-project-config-test");
        let project = temp_codex_dir("cc-session-manager-project-config-worktree");
        fs::create_dir_all(project.join(".codex"))?;
        fs::write(
            project.join(".codex").join("config.toml"),
            "[features.multi_agent_v2]\n\
             enabled = true\n\
             max_concurrent_threads_per_session = 6\n\
             min_wait_timeout_ms = 480000\n",
        )?;
        write_rollout_with_cwd(&codex, "project-config-session", &project)?;

        let report = diagnose_project_configs(codex.to_string_lossy().into_owned())?;
        assert_eq!(report.scanned_projects, 1);
        assert_eq!(report.config_files, 1);
        assert_eq!(report.issue_count, 1);
        assert_eq!(report.repairable_count, 1);
        assert_eq!(
            report.issues[0].suggested_default_wait_timeout_ms,
            Some(480000)
        );

        let preview = repair_project_configs(codex.to_string_lossy().into_owned(), true)?;
        assert_eq!(preview.repaired_count, 1);
        let raw_before = fs::read_to_string(project.join(".codex").join("config.toml"))?;
        assert!(!raw_before.contains("default_wait_timeout_ms"));

        let repaired = repair_project_configs(codex.to_string_lossy().into_owned(), false)?;
        assert_eq!(repaired.repaired_count, 1);
        let raw_after = fs::read_to_string(project.join(".codex").join("config.toml"))?;
        assert!(raw_after.contains("default_wait_timeout_ms = 480000"));

        let clean = diagnose_project_configs(codex.to_string_lossy().into_owned())?;
        assert_eq!(clean.issue_count, 0);

        fs::remove_dir_all(&codex).ok();
        fs::remove_dir_all(&project).ok();
        Ok(())
    }

    #[test]
    fn project_config_diagnosis_refuses_to_guess_invalid_timeout_bounds() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-project-config-bounds-test");
        let project = temp_codex_dir("cc-session-manager-project-config-bounds-worktree");
        fs::create_dir_all(project.join(".codex"))?;
        fs::write(
            project.join(".codex").join("config.toml"),
            "[features.multi_agent_v2]\n\
             min_wait_timeout_ms = 480000\n\
             max_wait_timeout_ms = 1000\n",
        )?;
        write_rollout_with_cwd(&codex, "project-config-bounds-session", &project)?;

        let report = diagnose_project_configs(codex.to_string_lossy().into_owned())?;
        assert_eq!(report.issue_count, 1);
        assert_eq!(report.repairable_count, 0);
        assert!(!report.issues[0].repairable);
        assert!(report.issues[0].message.contains("需要人工决定"));

        let repaired = repair_project_configs(codex.to_string_lossy().into_owned(), false)?;
        assert_eq!(repaired.repaired_count, 0);

        fs::remove_dir_all(&codex).ok();
        fs::remove_dir_all(&project).ok();
        Ok(())
    }

    #[test]
    fn claude_history_orphans_are_reported_and_pruned() -> AppResult<()> {
        let claude = temp_codex_dir("cc-session-manager-claude-history-test");
        write_claude_session(&claude, "live-session")?;
        fs::write(
            claude.join("history.jsonl"),
            "{\
                \"sessionId\":\"live-session\",\
                \"display\":\"keep\"\
             }\n\
             {\
                \"sessionId\":\"deleted-session\",\
                \"display\":\"remove\"\
             }\n\
             {\
                \"session_id\":\"deleted-session-2\",\
                \"display\":\"remove too\"\
             }\n\
             not-json\n\
             {\"display\":\"no session id\"}\n",
        )?;

        let report = diagnose_claude_history_orphans(claude.to_string_lossy().into_owned())?;
        assert_eq!(report.session_count, 1);
        assert_eq!(report.history_rows, 5);
        assert_eq!(report.linked_rows, 1);
        assert_eq!(report.orphan_rows, 2);
        assert_eq!(report.untracked_rows, 2);
        assert_eq!(
            report.orphan_session_ids,
            vec![
                "deleted-session".to_string(),
                "deleted-session-2".to_string()
            ]
        );

        let preview = prune_claude_history_orphans(claude.to_string_lossy().into_owned(), true)?;
        assert_eq!(preview.removed_rows, 2);
        assert!(fs::read_to_string(claude.join("history.jsonl"))?.contains("deleted-session"));

        let result = prune_claude_history_orphans(claude.to_string_lossy().into_owned(), false)?;
        assert_eq!(result.removed_rows, 2);
        let history = fs::read_to_string(claude.join("history.jsonl"))?;
        assert!(history.contains("live-session"));
        assert!(!history.contains("deleted-session"));
        assert!(history.contains("not-json"));
        assert!(history.contains("no session id"));

        fs::remove_dir_all(&claude).ok();
        Ok(())
    }

    const GUI_TEST_ID_VISIBLE: &str = "11111111-2222-4333-8444-555555555555";
    const GUI_TEST_ID_HIDDEN: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    const GUI_TEST_ID_SIDECHAIN: &str = "99999999-8888-4777-8666-555555555555";

    /// 构造一个对 VS Code 插件不可见的会话：
    /// 头部 64KB 被超大 meta 行占满，真正的用户消息在窗口之外，且没有任何标题记录。
    fn write_gui_hidden_session(claude: &Path) -> AppResult<PathBuf> {
        let dir = claude.join("projects").join("gui-project");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{GUI_TEST_ID_HIDDEN}.jsonl"));
        let filler = "x".repeat(80_000);
        let meta_line = serde_json::json!({
            "sessionId": GUI_TEST_ID_HIDDEN,
            "cwd": "F:\\project\\example",
            "timestamp": "2026-04-22T00:00:00Z",
            "type": "user",
            "isMeta": true,
            "message": {"role": "user", "content": filler}
        });
        let user_line = serde_json::json!({
            "sessionId": GUI_TEST_ID_HIDDEN,
            "timestamp": "2026-04-22T00:01:00Z",
            "type": "user",
            "message": {"role": "user", "content": "帮我修复 GUI 列表"}
        });
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&meta_line)?,
                serde_json::to_string(&user_line)?
            ),
        )?;
        Ok(path)
    }

    #[test]
    fn gui_visibility_diagnose_and_repair() -> AppResult<()> {
        let claude = temp_codex_dir("cc-session-manager-claude-gui-test");

        // 可见会话：首行即普通用户消息
        write_claude_session(&claude, GUI_TEST_ID_VISIBLE)?;
        // 不可见会话：标题链全部落空
        let hidden_path = write_gui_hidden_session(&claude)?;
        // 子代理会话：首行带 isSidechain，GUI 本就不展示
        let side_dir = claude.join("projects").join("gui-project");
        let side_line = serde_json::json!({
            "sessionId": GUI_TEST_ID_SIDECHAIN,
            "isSidechain": true,
            "timestamp": "2026-04-22T00:00:00Z",
            "type": "user",
            "message": {"role": "user", "content": "subagent"}
        });
        fs::write(
            side_dir.join(format!("{GUI_TEST_ID_SIDECHAIN}.jsonl")),
            format!("{}\n", serde_json::to_string(&side_line)?),
        )?;

        let claude_str = claude.to_string_lossy().into_owned();
        let report = diagnose_claude_gui_visibility(claude_str.clone())?;
        assert_eq!(report.scanned_sessions, 3);
        assert_eq!(report.visible_sessions, 1);
        assert_eq!(report.sidechain_sessions, 1);
        assert_eq!(report.issues.len(), 1);
        let issue = &report.issues[0];
        assert_eq!(issue.session_id, GUI_TEST_ID_HIDDEN);
        assert_eq!(issue.proposed_title, "帮我修复 GUI 列表");

        // dry_run 不写入
        let preview = repair_claude_gui_visibility(claude_str.clone(), true, None)?;
        assert_eq!(preview.fixed, 1);
        assert!(preview.dry_run);
        assert!(!fs::read_to_string(&hidden_path)?.contains("custom-title"));

        // 实际修复：追加 custom-title 记录，且之后诊断不再报告
        let result = repair_claude_gui_visibility(claude_str.clone(), false, None)?;
        assert_eq!(result.fixed, 1);
        assert!(result.errors.is_empty());
        let content = fs::read_to_string(&hidden_path)?;
        let last_line = content.lines().last().unwrap();
        let record: Value = serde_json::from_str(last_line)?;
        assert_eq!(
            record.get("type").and_then(Value::as_str),
            Some("custom-title")
        );
        assert_eq!(
            record.get("customTitle").and_then(Value::as_str),
            Some("帮我修复 GUI 列表")
        );
        assert_eq!(
            record.get("sessionId").and_then(Value::as_str),
            Some(GUI_TEST_ID_HIDDEN)
        );

        let after = diagnose_claude_gui_visibility(claude_str)?;
        assert_eq!(after.issues.len(), 0);
        assert_eq!(after.visible_sessions, 2);

        fs::remove_dir_all(&claude).ok();
        Ok(())
    }

    #[test]
    fn gui_title_extraction_matches_extension_semantics() {
        // ta()：取最后一次出现的值，并处理转义
        let tail = r#"{"type":"custom-title","customTitle":"old"}
{"type":"custom-title","customTitle":"new \"quoted\""}"#;
        assert_eq!(
            gui_last_string_field(tail, "customTitle").as_deref(),
            Some("new \"quoted\"")
        );

        // jie()：跳过 isMeta / tool_result / isCompactSummary / 标签开头文本
        let head = concat!(
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"meta"}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#,
            "\n",
            r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"compact"}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"<local-command-stdout>out</local-command-stdout>"}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"真正的标题"}}"#,
            "\n",
        );
        assert_eq!(gui_head_title(head).as_deref(), Some("真正的标题"));

        // 命令消息只作为兜底标题
        let head_cmd = concat!(
            r#"{"type":"user","message":{"role":"user","content":"<command-message>run</command-message><command-name>/compact</command-name>"}}"#,
            "\n",
        );
        assert_eq!(gui_head_title(head_cmd).as_deref(), Some("/compact"));

        // bash 输入展示为 "! cmd"
        let head_bash = concat!(
            r#"{"type":"user","message":{"role":"user","content":"<bash-input>ls -la</bash-input>"}}"#,
            "\n",
        );
        assert_eq!(gui_head_title(head_bash).as_deref(), Some("! ls -la"));

        // 空标题链 → 不可见
        assert_eq!(gui_visible_title("", ""), None);
        // summary 仅在尾部窗口生效
        assert_eq!(
            gui_visible_title("", r#"{"type":"summary","summary":"总结标题"}"#).as_deref(),
            Some("总结标题")
        );
    }
}

// 保留 BTreeMap / HashMap 以便将来扩展批量聚合
#[allow(dead_code)]
fn _unused() {
    let _: BTreeMap<String, Family> = BTreeMap::new();
    let _: HashMap<String, Vec<String>> = HashMap::new();
}
