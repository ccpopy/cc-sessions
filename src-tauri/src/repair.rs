//! 修复 / 诊断：
//!
//! - `diagnose_codex_state`：扫描 rollout、session_index、threads 三边差集
//! - `repair_session_index`：从 rollout 批量重建 session_index.jsonl
//! - `rebuild_threads_table`：从 rollout 批量 upsert state_5.sqlite 的 threads 表
//! - `clone_session_for_provider`：把会话"克隆到当前 provider"（三种策略）
//! - `batch_clone_for_current_provider`：对所有 provider 不匹配的家族做批量克隆

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::atomic_file;
use crate::error::{ensure_not_cancelled, AppError, AppResult};
use crate::family;
use crate::models::{
    ArchiveOrigin, BranchStatus, BranchSyncReport, BranchSyncState, CloneReport, DiagnosticReport,
    DuplicateSessionReport, Family, FamilyBranch, ForkSessionReport, GuiVisibilityFixReport,
    GuiVisibilityIssue, GuiVisibilityReport, HistoryOrphanReport, HistoryPruneReport,
    IndexRepairReport, OrphanPruneReport, ProjectConfigIssue, ProjectConfigRepairItem,
    ProjectConfigRepairReport, ProjectConfigReport, ProviderInfo, SwitchStrategy, SyncBranchReport,
    ThreadsRebuildReport,
};
use crate::mutation_journal::{
    commit_transaction_with_compensation, rollback_transaction_with_compensation, MutationJournal,
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

fn rewrite_lines_atomically(path: &Path, lines: &[String]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let expected = match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.is_file()
                && !crate::path_safety::metadata_is_link_or_reparse(&metadata) =>
        {
            Some(atomic_file::fingerprint(path)?)
        }
        Ok(_) => {
            return Err(AppError::Path(format!(
                "待重写路径不是普通文件或属于链接/junction: {}",
                path.to_string_lossy()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let writer = |file: &mut fs::File| -> AppResult<()> {
        for line in lines {
            writeln!(file, "{line}")?;
        }
        Ok(())
    };
    if let Some(expected) = expected.as_ref() {
        atomic_file::replace_with_writer_if_unchanged(path, expected, writer)
    } else {
        atomic_file::create_with_writer_if_absent(path, writer)
    }
}

// ========================= 读当前 provider =========================

/// 给其他模块使用的导出版本（只返回 provider，不返回 exists）。
/// 仅在配置缺失或有效配置省略字段时落到 Codex 默认值 `openai`；配置损坏必须上抛。
pub(crate) fn read_current_provider_export(codex_dir: &Path) -> AppResult<String> {
    effective_current_provider(codex_dir)
}

/// 显式读取 config.toml 顶层的 `model_provider`，仅当字段存在时才返回 Some。
fn read_explicit_provider(codex_dir: &Path) -> AppResult<(Option<String>, bool)> {
    let p = paths::config_toml_path(codex_dir);
    let metadata = match fs::metadata(&p) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((None, false)),
        Err(err) => {
            return Err(AppError::Other(format!(
                "读取 Codex provider 配置元数据失败: {} ({err})",
                p.to_string_lossy()
            )))
        }
    };
    if !metadata.is_file() {
        return Err(AppError::Other(format!(
            "Codex provider 配置路径不是文件: {}",
            p.to_string_lossy()
        )));
    }
    let raw = fs::read_to_string(&p).map_err(|err| {
        AppError::Other(format!(
            "读取 Codex provider 配置失败: {} ({err})",
            p.to_string_lossy()
        ))
    })?;
    // 严格 TOML：只取顶层 `model_provider`，避免 `[model_providers.xxx]` 子表误匹配。
    let table = raw.parse::<toml::Table>().map_err(|err| {
        AppError::Other(format!(
            "Codex config.toml 不是有效 TOML: {} ({err})",
            p.to_string_lossy()
        ))
    })?;
    let Some(value) = table.get("model_provider") else {
        return Ok((None, true));
    };
    let provider = value.as_str().ok_or_else(|| {
        AppError::Other(format!(
            "Codex config.toml 的 model_provider 必须是非空字符串: {}",
            p.to_string_lossy()
        ))
    })?;
    let provider = provider.trim();
    if provider.is_empty() {
        return Err(AppError::Other(format!(
            "Codex config.toml 的 model_provider 不能为空: {}",
            p.to_string_lossy()
        )));
    }
    Ok((Some(provider.to_string()), true))
}

/// 返回 Codex 实际生效的 provider：显式值优先，否则默认 `openai`。
pub(crate) fn effective_current_provider(codex_dir: &Path) -> AppResult<String> {
    Ok(read_explicit_provider(codex_dir)?
        .0
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_string()))
}

pub fn get_provider_info(codex_dir: String) -> AppResult<ProviderInfo> {
    let p = PathBuf::from(&codex_dir);
    let cfg = paths::config_toml_path(&p);
    let (explicit, exists) = read_explicit_provider(&p)?;
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

    for rollout_path in family::scan_rollouts(codex)? {
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct RolloutIdentity {
    id: String,
    model_provider: String,
    source: Option<String>,
}

/// 读取 provider / 本地索引诊断所需的最小 rollout 身份信息。
///
/// Codex 的 `session_meta` 位于会话头部；找到首个带有效 id 的记录后立即返回，
/// 避免只为核对 id/provider/source 而解析可能达到数百 MB 的后续对话内容。
fn read_rollout_identity(path: &Path) -> AppResult<Option<RolloutIdentity>> {
    let reader = BufReader::new(fs::File::open(path)?);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let Some(id) = payload
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let model_provider = payload
            .get("model_provider")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| DEFAULT_PROVIDER.to_string());
        return Ok(Some(RolloutIdentity {
            id: id.to_string(),
            model_provider,
            source: metadata_string_field(payload, "source"),
        }));
    }
    Ok(None)
}

fn scan_active_rollout_identities(codex: &Path) -> AppResult<Vec<(PathBuf, RolloutIdentity)>> {
    let mut rollouts = Vec::new();
    for path in family::scan_rollouts(codex)? {
        let Some(identity) = read_rollout_identity(&path)? else {
            continue;
        };
        rollouts.push((path, identity));
    }
    Ok(rollouts)
}

pub(crate) fn read_rollout_brief(codex_dir: &Path, path: &Path) -> AppResult<Option<RolloutBrief>> {
    read_rollout_brief_impl(codex_dir, path, None)
}

pub(crate) fn read_rollout_brief_cancellable(
    codex_dir: &Path,
    path: &Path,
    cancel: &AtomicBool,
) -> AppResult<Option<RolloutBrief>> {
    read_rollout_brief_impl(codex_dir, path, Some(cancel))
}

/// Build one Desktop project-assignment record from rollout metadata.
///
/// Rollouts created under WSL persist Core paths, while the Desktop global state expects the
/// corresponding host path. Empty/missing cwd is deliberately skipped so one incomplete rollout
/// cannot block batch repair of all other threads.
fn project_assignment_record(codex: &Path, brief: &RolloutBrief) -> Option<(String, String)> {
    let cwd = brief.cwd.as_deref()?.trim();
    if cwd.is_empty() {
        return None;
    }
    Some((
        brief.id.clone(),
        paths::host_path_string_from_codex_record(codex, cwd),
    ))
}

fn read_rollout_brief_impl(
    codex_dir: &Path,
    path: &Path,
    cancel: Option<&AtomicBool>,
) -> AppResult<Option<RolloutBrief>> {
    ensure_not_cancelled(cancel)?;
    let f = fs::File::open(path)?;
    let reader = BufReader::new(f);
    let mut id: Option<String> = None;
    let mut model_provider: Option<String> = None;
    let mut source: Option<String> = None;
    let mut cwd_tracker = crate::codex_rollout_cwd::EffectiveCwdTracker::default();
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
        ensure_not_cancelled(cancel)?;
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(x) => x,
            Err(_) => continue,
        };
        cwd_tracker.observe(&v);
        // 时间戳（顶层可能是 ISO8601 字符串）
        if let Some(ts) = v.get("timestamp").and_then(|x| x.as_str()) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                let ms = dt.timestamp_millis();
                if created_ms == 0 {
                    created_ms = ms;
                }
                last_ms = last_ms.max(ms);
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
                memory_mode = memory_mode
                    .or_else(|| payload.and_then(|p| metadata_string_field(p, "memory_mode")));
            }
            "turn_context" => {
                let payload = v.get("payload").unwrap_or(&v);
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
    let cwd = cwd_tracker.effective_for(&id);
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

fn metadata_git_field(payload: &Value, legacy_field: &str, git_field: &str) -> Option<String> {
    payload
        .get("git")
        .and_then(|git| git.get(git_field))
        .and_then(metadata_string_value)
        .or_else(|| metadata_string_field(payload, legacy_field))
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
    if source.split_once(':').is_some_and(|(kind, parent_id)| {
        kind.eq_ignore_ascii_case("parent") && !parent_id.trim().is_empty()
    }) {
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

pub fn diagnose_codex_state(codex_dir: String) -> AppResult<DiagnosticReport> {
    let codex = PathBuf::from(&codex_dir);

    // 1) 扫 sessions/。这里的 rollout_count 只统计 active 会话，和官方 thread/list
    // archived=false 的默认语义保持一致。
    let rollouts = family::scan_rollouts(&codex)?;
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
    let archived_rollouts = family::scan_archived_rollouts(&codex)?;
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
    let cur_provider = effective_current_provider(&codex)?;
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

pub fn repair_session_index(codex_dir: String, dry_run: bool) -> AppResult<IndexRepairReport> {
    let codex = PathBuf::from(&codex_dir);
    if !dry_run {
        crate::codex_projects::ensure_desktop_not_running(&codex)?;
    }
    let rollouts = family::scan_rollouts(&codex)?;
    let mut written = 0u32;
    let mut salvaged = 0u32;
    let mut errors: Vec<String> = Vec::new();
    let mut project_assignments = Vec::new();

    let mut entries: Vec<Value> = Vec::with_capacity(rollouts.len());
    for p in &rollouts {
        match read_rollout_brief(&codex, p) {
            Ok(Some(b)) => {
                if let Some(record) = project_assignment_record(&codex, &b) {
                    project_assignments.push(record);
                } else {
                    errors.push(format!(
                        "{}: rollout 缺少有效 cwd，已跳过 Codex Desktop 项目归属修复",
                        p.to_string_lossy()
                    ));
                }
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
        crate::codex_projects::validate_missing_thread_project_assignment_records(
            &codex,
            &project_assignments,
        )?;
        let out_path = paths::session_index_path(&codex);
        let lines = entries
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?;
        let mut journal = MutationJournal::default();
        let operation = (|| -> AppResult<()> {
            journal.mutate_file(&out_path, || rewrite_lines_atomically(&out_path, &lines))?;
            if let Some(receipt) =
                crate::codex_projects::sync_missing_thread_project_assignment_records_with_receipt(
                    &codex,
                    &project_assignments,
                )?
            {
                journal.register_project_state_receipt(receipt);
            }
            Ok(())
        })();
        if let Err(error) = operation {
            return Err(journal.compensate_without_transaction(error));
        }
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
// 表里多出来的 id，以及 session_family.json 中可安全确认的孤儿 family/分支），
// 不会从 rollout 重建。适合只想"把残留清干净"的场景。
pub fn prune_orphan_entries(
    codex_dir: String,
    prune_index: bool,
    prune_threads: bool,
    dry_run: bool,
) -> AppResult<OrphanPruneReport> {
    let lock = family::FamilyLock::default();
    prune_orphan_entries_with_lock(
        codex_dir,
        prune_index,
        prune_threads,
        prune_index || prune_threads,
        dry_run,
        &lock,
    )
}

pub fn prune_orphan_entries_with_lock(
    codex_dir: String,
    prune_index: bool,
    prune_threads: bool,
    prune_family: bool,
    dry_run: bool,
    lock: &family::FamilyLock,
) -> AppResult<OrphanPruneReport> {
    family::with_lock(lock, |_g| {
        prune_orphan_entries_locked(codex_dir, prune_index, prune_threads, prune_family, dry_run)
    })
}

#[derive(Default)]
struct FamilyOrphanPrunePlan {
    family_ids: Vec<String>,
    branches: Vec<(String, String)>,
    recoveries: Vec<FamilyActiveRecovery>,
    normalizations: Vec<FamilyActiveNormalization>,
    skipped_family_ids: Vec<String>,
}

struct FamilyActiveRecovery {
    family_id: String,
    active_id: String,
    missing_branch_ids: Vec<String>,
}

struct FamilyActiveNormalization {
    family_id: String,
    active_id: String,
}

fn family_branch_rollout_exists(
    codex: &Path,
    branch: &FamilyBranch,
    rollout_ids: &BTreeSet<String>,
) -> Option<bool> {
    if rollout_ids.contains(&branch.id) {
        return Some(true);
    }
    let relative = paths::checked_relative_path(&branch.rollout_relpath).ok()?;
    Some(codex.join(relative).is_file())
}

fn family_metadata_is_consistent(
    store: &crate::models::FamilyStore,
    family_id: &str,
    family_record: &Family,
) -> bool {
    if family_record.family_id != family_id {
        return false;
    }
    let chain_ids = family_record
        .chain
        .iter()
        .map(|branch| branch.id.as_str())
        .collect::<BTreeSet<_>>();
    if store.index.iter().any(|(branch_id, mapped_family_id)| {
        mapped_family_id == family_id && !chain_ids.contains(branch_id.as_str())
    }) {
        return false;
    }
    let Some(branch) = family_record.chain.first() else {
        return true;
    };
    matches!(
        family::resolve_family_id_strict(store, &branch.id),
        Ok(Some(resolved_family_id)) if resolved_family_id == family_id
    )
}

fn family_structure_is_safe_to_repair(
    store: &crate::models::FamilyStore,
    family_id: &str,
    family_record: &Family,
) -> bool {
    if family_record.family_id != family_id || family_record.chain.is_empty() {
        return false;
    }
    let mut chain_ids = BTreeSet::new();
    for branch in &family_record.chain {
        if !chain_ids.insert(branch.id.as_str())
            || store.index.get(&branch.id).map(String::as_str) != Some(family_id)
        {
            return false;
        }
    }
    if store.index.iter().any(|(branch_id, mapped_family_id)| {
        mapped_family_id == family_id && !chain_ids.contains(branch_id.as_str())
    }) {
        return false;
    }
    !store
        .families
        .iter()
        .any(|(other_family_id, other_family)| {
            other_family_id != family_id
                && other_family
                    .chain
                    .iter()
                    .any(|branch| chain_ids.contains(branch.id.as_str()))
        })
}

fn existing_family_branch_ids(
    codex: &Path,
    family_record: &Family,
    rollout_ids: &BTreeSet<String>,
) -> Option<BTreeSet<String>> {
    let mut existing_branch_ids = BTreeSet::new();
    for branch in &family_record.chain {
        if family_branch_rollout_exists(codex, branch, rollout_ids)? {
            existing_branch_ids.insert(branch.id.clone());
        }
    }
    Some(existing_branch_ids)
}

fn plan_family_orphan_prune(
    codex: &Path,
    store: &crate::models::FamilyStore,
    rollout_ids: &BTreeSet<String>,
) -> FamilyOrphanPrunePlan {
    let mut plan = FamilyOrphanPrunePlan::default();

    for (family_id, family_record) in &store.families {
        let metadata_consistent = family_metadata_is_consistent(store, family_id, family_record);
        let Some(existing_branch_ids) =
            existing_family_branch_ids(codex, family_record, rollout_ids)
        else {
            plan.skipped_family_ids.push(family_id.clone());
            continue;
        };

        if existing_branch_ids.is_empty() {
            if metadata_consistent {
                plan.family_ids.push(family_id.clone());
            } else {
                plan.skipped_family_ids.push(family_id.clone());
            }
            continue;
        }

        let active_exists = family_record
            .chain
            .iter()
            .find(|branch| branch.id == family_record.active_id)
            .is_some_and(|branch| existing_branch_ids.contains(&branch.id));
        if !active_exists {
            let existing_active_ids = family_record
                .chain
                .iter()
                .filter(|branch| {
                    existing_branch_ids.contains(&branch.id)
                        && matches!(branch.status, crate::models::BranchStatus::Active)
                })
                .map(|branch| branch.id.clone())
                .collect::<Vec<_>>();
            if existing_active_ids.len() == 1
                && family_structure_is_safe_to_repair(store, family_id, family_record)
            {
                plan.recoveries.push(FamilyActiveRecovery {
                    family_id: family_id.clone(),
                    active_id: existing_active_ids[0].clone(),
                    missing_branch_ids: family_record
                        .chain
                        .iter()
                        .filter(|branch| !existing_branch_ids.contains(&branch.id))
                        .map(|branch| branch.id.clone())
                        .collect(),
                });
                continue;
            }
            plan.skipped_family_ids.push(family_id.clone());
            continue;
        }

        if !metadata_consistent {
            if family_structure_is_safe_to_repair(store, family_id, family_record) {
                plan.normalizations.push(FamilyActiveNormalization {
                    family_id: family_id.clone(),
                    active_id: family_record.active_id.clone(),
                });
            } else {
                plan.skipped_family_ids.push(family_id.clone());
                continue;
            }
        }

        let missing_branch_ids = family_record
            .chain
            .iter()
            .filter(|branch| !existing_branch_ids.contains(&branch.id))
            .map(|branch| branch.id.clone())
            .collect::<Vec<_>>();
        if missing_branch_ids.is_empty() {
            continue;
        }

        plan.branches.extend(
            missing_branch_ids
                .into_iter()
                .map(|branch_id| (family_id.clone(), branch_id)),
        );
    }

    plan
}

fn prune_orphan_entries_locked(
    codex_dir: String,
    prune_index: bool,
    prune_threads: bool,
    prune_family: bool,
    dry_run: bool,
) -> AppResult<OrphanPruneReport> {
    let codex = PathBuf::from(&codex_dir);

    // active rollout 用于 session_index；active + archived rollout 用于 threads。
    let rollouts = family::scan_rollouts(&codex)?;
    let mut rollout_ids: BTreeSet<String> = BTreeSet::new();
    for p in &rollouts {
        if let Ok(Some(b)) = read_rollout_brief(&codex, p) {
            rollout_ids.insert(b.id);
        }
    }
    let mut all_rollout_ids = rollout_ids.clone();
    for p in family::scan_archived_rollouts(&codex)? {
        if let Ok(Some(b)) = read_rollout_brief(&codex, &p) {
            all_rollout_ids.insert(b.id);
        }
    }

    let state_db_exists = paths::state_db_path(&codex).is_file();
    let orphan_ids = if prune_threads && state_db_exists {
        let state = state_db::open_ro(&codex)?;
        let mut stmt = state.prepare("SELECT id FROM threads")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|id| !all_rollout_ids.contains(id))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if !dry_run && prune_threads {
        crate::codex_projects::preflight_thread_project_state_cleanup(&codex, &orphan_ids)?;
        if crate::codex_projects::desktop_state_initialized(&codex)? {
            crate::codex_projects::ensure_desktop_not_running(&codex)?;
        }
    }

    let mut index_removed = 0u32;
    let mut threads_removed = 0u32;
    let mut family_branches_removed = 0u32;
    let mut families_removed = 0u32;
    let mut families_recovered = 0u32;
    let mut families_normalized = 0u32;
    let mut families_skipped = Vec::new();
    let mut changed_family_store = None;
    let mut changed_index = None;

    if prune_family {
        let mut store = family::load(&codex)?;
        let plan = plan_family_orphan_prune(&codex, &store, &all_rollout_ids);
        family_branches_removed = plan.branches.len() as u32;
        families_removed = plan.family_ids.len() as u32;
        families_recovered = plan.recoveries.len() as u32;
        families_normalized = plan.normalizations.len() as u32;
        family_branches_removed += plan
            .recoveries
            .iter()
            .map(|recovery| recovery.missing_branch_ids.len() as u32)
            .sum::<u32>();
        families_skipped = plan.skipped_family_ids;

        if !dry_run
            && (family_branches_removed > 0
                || families_removed > 0
                || families_recovered > 0
                || families_normalized > 0)
        {
            for family_id in &plan.family_ids {
                family::remove_family(&mut store, family_id)?;
            }
            for normalization in &plan.normalizations {
                let family_record = store
                    .families
                    .get_mut(&normalization.family_id)
                    .ok_or_else(|| {
                        AppError::NotFound(format!(
                            "family not found during normalization: {}",
                            normalization.family_id
                        ))
                    })?;
                for branch in &mut family_record.chain {
                    if branch.id == normalization.active_id {
                        branch.status = BranchStatus::Active;
                    } else if matches!(branch.status, BranchStatus::Active) {
                        branch.status = BranchStatus::Archived;
                    }
                }
                family_record.updated_at = chrono::Utc::now().to_rfc3339();
            }
            for (family_id, branch_id) in &plan.branches {
                family::remove_non_active_branch(&mut store, family_id, branch_id)?;
            }
            for recovery in &plan.recoveries {
                {
                    let family_record =
                        store.families.get_mut(&recovery.family_id).ok_or_else(|| {
                            AppError::NotFound(format!(
                                "family not found during recovery: {}",
                                recovery.family_id
                            ))
                        })?;
                    family_record
                        .chain
                        .retain(|branch| !recovery.missing_branch_ids.contains(&branch.id));
                    for branch in &mut family_record.chain {
                        branch.status = if branch.id == recovery.active_id {
                            crate::models::BranchStatus::Active
                        } else {
                            crate::models::BranchStatus::Archived
                        };
                    }
                    family_record.active_id = recovery.active_id.clone();
                    if !family_record
                        .chain
                        .iter()
                        .any(|branch| branch.id == family_record.root_id)
                    {
                        family_record.root_id = recovery.active_id.clone();
                    }
                    family_record.updated_at = chrono::Utc::now().to_rfc3339();
                }
                for branch_id in &recovery.missing_branch_ids {
                    store.index.remove(branch_id);
                }
            }
            changed_family_store = Some(store);
        }
    }

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
                // The actual rewrite is part of the joint transaction below.
                changed_index = Some((index_path, kept_lines));
            }
        }
    }

    if prune_threads && state_db_exists {
        threads_removed = orphan_ids.len() as u32;
    }

    if !dry_run {
        let mut journal = MutationJournal::default();
        let family_path = paths::family_store_path(&codex);
        let state = if prune_threads && state_db_exists {
            Some(state_db::open(&codex)?)
        } else {
            None
        };
        let transaction = match state.as_ref() {
            Some(connection) => Some(rusqlite::Transaction::new_unchecked(
                connection,
                rusqlite::TransactionBehavior::Immediate,
            )?),
            None => None,
        };
        let operation = (|| -> AppResult<()> {
            if let Some(store) = changed_family_store.as_ref() {
                journal.mutate_file(&family_path, || family::save(&codex, store))?;
            }
            if let Some((index_path, kept_lines)) = changed_index.as_ref() {
                journal.mutate_file(index_path, || {
                    rewrite_lines_atomically(index_path, kept_lines)
                })?;
            }
            if prune_threads {
                if let Some(transaction) = transaction.as_ref() {
                    for id in &orphan_ids {
                        transaction.execute("DELETE FROM threads WHERE id = ?", [id])?;
                    }
                }
                if let Some(receipt) =
                    crate::codex_projects::clear_thread_project_states_with_receipt(
                        &codex,
                        &orphan_ids,
                    )?
                {
                    journal.register_project_state_receipt(receipt);
                }
            }
            inject_repair_fault("prune_after_project_state")?;
            Ok(())
        })();
        if let Err(error) = operation {
            return Err(match transaction {
                Some(transaction) => {
                    rollback_transaction_with_compensation(transaction, journal, error)
                }
                None => journal.compensate_without_transaction(error),
            });
        }
        if let Some(transaction) = transaction {
            commit_transaction_with_compensation(transaction, journal)?;
        } else {
            journal.finalize()?;
        }
    }

    Ok(OrphanPruneReport {
        index_removed,
        threads_removed,
        family_branches_removed,
        families_removed,
        families_recovered,
        families_normalized,
        families_skipped,
        dry_run,
    })
}

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
            rewrite_lines_atomically(&history_path, &kept_lines)?;
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
pub(crate) fn append_custom_title(path: &Path, session_id: &str, title: &str) -> AppResult<()> {
    use std::io::{Read, Seek, SeekFrom};
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        return Err(AppError::Path(format!(
            "Claude 会话路径不是普通文件或属于链接/junction: {}",
            path.to_string_lossy()
        )));
    }
    let expected = atomic_file::fingerprint(path)?;
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
    atomic_file::replace_with_writer_if_unchanged(path, &expected, |target| {
        let mut source = fs::File::open(path)?;
        std::io::copy(&mut source, target)?;
        if needs_newline {
            target.write_all(b"\n")?;
        }
        target.write_all(record.to_string().as_bytes())?;
        target.write_all(b"\n")?;
        Ok(())
    })
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

/// threads 表的基础列（旧版 Codex App 的 schema；新版新增列见 THREADS_OPTIONAL_COLS）
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

/// 新版 Codex App（内置 codex-rs 0.144.x 起）通过迁移新增的 threads 列。
/// 官方 App 的会话列表查询带 `preview <> ''` 谓词（对应库内 idx_threads_visible_*
/// 部分索引），preview 为空的行在 App 中不可见，因此目标库存在这些列时必须写入；
/// 旧版库没有这些列，写入前需按实际表结构过滤，避免 INSERT 报未知列错误。
const THREADS_OPTIONAL_COLS: &[&str] = &[
    "preview",
    "thread_source",
    "recency_at",
    "recency_at_ms",
    "history_mode",
    "name",
];

/// threads 表当前实际存在的列名（按建表顺序）。
pub(crate) fn threads_table_columns(state: &rusqlite::Connection) -> AppResult<Vec<String>> {
    let mut stmt = state.prepare("PRAGMA table_info(threads)")?;
    let mut rows = stmt.query([])?;
    let mut cols = Vec::new();
    while let Some(row) = rows.next()? {
        cols.push(row.get::<_, String>(1)?);
    }
    Ok(cols)
}

/// upsert 实际写入的列：已知列（固定 + 可选）与目标表结构的交集。
fn effective_threads_cols(state: &rusqlite::Connection) -> AppResult<Vec<&'static str>> {
    let existing: std::collections::HashSet<String> =
        threads_table_columns(state)?.into_iter().collect();
    let cols: Vec<&'static str> = THREADS_COLS
        .iter()
        .chain(THREADS_OPTIONAL_COLS.iter())
        .copied()
        .filter(|name| existing.contains(*name))
        .collect();
    if !cols.contains(&"id") || !cols.contains(&"rollout_path") {
        return Err(AppError::InvalidCodexDir(
            "threads 表缺少 id/rollout_path 列，无法同步会话".into(),
        ));
    }
    Ok(cols)
}

pub fn rebuild_threads_table(codex_dir: String, dry_run: bool) -> AppResult<ThreadsRebuildReport> {
    let codex = PathBuf::from(&codex_dir);
    if !dry_run {
        crate::codex_projects::ensure_desktop_not_running(&codex)?;
    }
    let active_rollouts = family::scan_rollouts(&codex)?;
    let archived_rollouts = family::scan_archived_rollouts(&codex)?;
    let mut scanned = 0u32;
    let mut upserted = 0u32;
    let mut skipped = 0u32;
    let mut errors: Vec<String> = Vec::new();
    let mut project_assignments = Vec::new();

    if !paths::state_db_path(&codex).is_file() {
        return Err(AppError::InvalidCodexDir(format!(
            "state_5.sqlite 不存在: {}",
            paths::state_db_path(&codex).to_string_lossy()
        )));
    }

    let state = state_db::open(&codex)?;
    let effective_cols = effective_threads_cols(&state)?;
    let mut planned_upserts = Vec::new();

    for (p, archived) in active_rollouts
        .iter()
        .map(|p| (p, false))
        .chain(archived_rollouts.iter().map(|p| (p, true)))
    {
        scanned += 1;
        match thread_values_from_rollout(&codex, p, archived, &effective_cols) {
            Ok(Some(values)) => {
                upserted += 1;
                match read_rollout_brief(&codex, p) {
                    Ok(Some(brief)) => {
                        if let Some(record) = project_assignment_record(&codex, &brief) {
                            project_assignments.push(record);
                        } else {
                            errors.push(format!(
                                "{}: rollout 缺少有效 cwd，已跳过 Codex Desktop 项目归属修复",
                                p.to_string_lossy()
                            ));
                        }
                    }
                    Ok(None) => {}
                    Err(e) => errors.push(format!("{}: {}", p.to_string_lossy(), e)),
                }
                if !dry_run {
                    planned_upserts.push(values);
                }
            }
            Ok(None) => skipped += 1,
            Err(e) => {
                errors.push(format!("{}: {}", p.to_string_lossy(), e));
                skipped += 1;
            }
        }
    }

    if !dry_run {
        crate::codex_projects::validate_missing_thread_project_assignment_records(
            &codex,
            &project_assignments,
        )?;
        let transaction =
            rusqlite::Transaction::new_unchecked(&state, rusqlite::TransactionBehavior::Immediate)?;
        let mut journal = MutationJournal::default();
        let operation = (|| -> AppResult<()> {
            for values in &planned_upserts {
                upsert_thread_values(&transaction, &effective_cols, values)?;
            }
            if let Some(receipt) =
                crate::codex_projects::sync_missing_thread_project_assignment_records_with_receipt(
                    &codex,
                    &project_assignments,
                )?
            {
                journal.register_project_state_receipt(receipt);
            }
            Ok(())
        })();
        if let Err(error) = operation {
            return Err(rollback_transaction_with_compensation(
                transaction,
                journal,
                error,
            ));
        }
        commit_transaction_with_compensation(transaction, journal)?;
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

fn threads_upsert_sql(cols: &[&str]) -> String {
    let placeholders = (0..cols.len()).map(|_| "?").collect::<Vec<_>>().join(",");
    let cols_sql = cols.join(",");
    let update_sql = cols
        .iter()
        .filter(|c| **c != "id")
        .map(|c| match *c {
            // title/name 都不是 rollout 的可靠来源；行已存在时不得用派生值覆盖
            // 非空标题或显式名称，否则官方 App 生成标题/用户重命名会丢失。
            "title" | "name" => format!(
                "{c}=CASE WHEN TRIM(COALESCE(threads.{c},''))='' \
                 THEN excluded.{c} ELSE threads.{c} END"
            ),
            // recency 是 SQLite 独立维护的列表排序水位；重扫旧 rollout 不能倒退它。
            "recency_at" | "recency_at_ms" => format!("{c}=threads.{c}"),
            // 空 preview 不能把原本可见的会话从新版官方列表中隐藏。
            "preview" => {
                "preview=COALESCE(NULLIF(excluded.preview,''), threads.preview)".to_string()
            }
            // 显式 Git 元数据更新只存在数据库中，陈旧 rollout 不得覆盖非空值。
            "git_sha" | "git_branch" | "git_origin_url" => {
                format!("{c}=COALESCE(threads.{c}, excluded.{c})")
            }
            _ => format!("{c}=excluded.{c}"),
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("INSERT INTO threads ({cols_sql}) VALUES ({placeholders}) ON CONFLICT(id) DO UPDATE SET {update_sql}")
}

fn thread_values_from_rollout(
    codex: &Path,
    rollout: &Path,
    archived: bool,
    cols: &[&str],
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
        cols.iter()
            .map(|name| match *name {
                "id" => Value::String(brief.id.clone()),
                "rollout_path" => Value::String(brief.path.to_string_lossy().into_owned()),
                "created_at" => Value::from(created / 1000),
                "updated_at" => Value::from(updated / 1000),
                "created_at_ms" => Value::from(created),
                "updated_at_ms" => Value::from(updated),
                "recency_at" => Value::from(updated / 1000),
                "recency_at_ms" => Value::from(updated),
                "cwd" => Value::String(
                    brief
                        .cwd
                        .clone()
                        .or_else(|| metadata_string_field(&payload, "cwd"))
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
                "git_sha" => metadata_git_field(&payload, "git_sha", "commit_hash")
                    .map(Value::String)
                    .unwrap_or(Value::Null),
                "git_branch" => metadata_git_field(&payload, "git_branch", "branch")
                    .map(Value::String)
                    .unwrap_or(Value::Null),
                "git_origin_url" => {
                    metadata_git_field(&payload, "git_origin_url", "repository_url")
                        .map(Value::String)
                        .unwrap_or(Value::Null)
                }
                "history_mode" => Value::String(
                    metadata_string_field(&payload, "history_mode")
                        .unwrap_or_else(|| "legacy".to_string()),
                ),
                // 官方 name 是可选的用户命名；rollout 本身不保存该值，后续从源 threads 行继承。
                "name" => Value::Null,
                // 官方 App 列表要求 preview 非空才可见；与 App 约定一致取首条用户消息。
                "preview" => Value::String(brief.first_user_message.clone()),
                "thread_source" => {
                    let source = desktop_visible_source(&payload);
                    Value::String(
                        if is_subagent_source(Some(source.as_str())) {
                            "subagent"
                        } else {
                            "user"
                        }
                        .to_string(),
                    )
                }
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

fn upsert_thread_values(
    state: &rusqlite::Connection,
    cols: &[&str],
    values: &[Value],
) -> AppResult<()> {
    let sql = threads_upsert_sql(cols);
    let mut stmt = state.prepare(&sql)?;
    let boxed = bind_thread_values(values);
    let refs: Vec<&dyn rusqlite::ToSql> = boxed.iter().map(|value| value.as_ref()).collect();
    stmt.execute(refs.as_slice())?;
    Ok(())
}

pub(crate) fn upsert_thread_from_rollout(
    codex: &Path,
    state: &rusqlite::Connection,
    rollout: &Path,
    archived: bool,
) -> AppResult<bool> {
    let cols = effective_threads_cols(state)?;
    let values = match thread_values_from_rollout(codex, rollout, archived, &cols)? {
        Some(values) => values,
        None => return Ok(false),
    };
    upsert_thread_values(state, &cols, &values)?;
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

/// 把源会话的 threads.title/name 带到克隆/fork 出的新分支。
///
/// 新 id 的行由 rollout 派生，不带过来的话切换 provider 后侧栏名称会退回
/// “首条消息”。源行对应字段为空时不覆盖新行的派生值。
fn carry_thread_title(
    state: &rusqlite::Connection,
    source_id: &str,
    new_id: &str,
) -> AppResult<()> {
    let has_name = threads_table_columns(state)?
        .iter()
        .any(|column| column == "name");
    let sql = if has_name {
        "UPDATE threads
         SET title = CASE
                 WHEN EXISTS (SELECT 1 FROM threads WHERE id = ?1 AND TRIM(COALESCE(title,'')) <> '')
                 THEN (SELECT title FROM threads WHERE id = ?1)
                 ELSE title
             END,
             name = CASE
                 WHEN EXISTS (SELECT 1 FROM threads WHERE id = ?1 AND TRIM(COALESCE(name,'')) <> '')
                 THEN (SELECT name FROM threads WHERE id = ?1)
                 ELSE name
             END
         WHERE id = ?2"
    } else {
        "UPDATE threads SET title = (SELECT title FROM threads WHERE id = ?1)
         WHERE id = ?2
           AND EXISTS (SELECT 1 FROM threads WHERE id = ?1 AND TRIM(COALESCE(title,'')) <> '')"
    };
    state.execute(sql, rusqlite::params![source_id, new_id])?;
    Ok(())
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

fn remove_index_line(codex: &Path, id: &str) -> AppResult<()> {
    let path = paths::session_index_path(codex);
    if !path.is_file() {
        return Ok(());
    }
    let expected = atomic_file::fingerprint(&path)?;
    let content = fs::read_to_string(&path)?;
    let mut kept = Vec::new();
    let mut removed = false;
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
            kept.push(line);
        } else {
            removed = true;
        }
    }
    if !removed {
        return Ok(());
    }
    atomic_file::replace_with_writer_if_unchanged(&path, &expected, |file| {
        for line in kept {
            writeln!(file, "{line}")?;
        }
        Ok(())
    })?;
    Ok(())
}

#[derive(Debug, Clone)]
struct ThreadRepairState {
    rollout_path: Option<String>,
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
    let mut stmt = conn.prepare(
        "SELECT id, rollout_path, model_provider, source, COALESCE(archived,0) FROM threads",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            ThreadRepairState {
                rollout_path: r.get::<_, Option<String>>(1)?,
                model_provider: r.get::<_, Option<String>>(2)?,
                source: r.get::<_, Option<String>>(3)?,
                archived: r.get::<_, i64>(4)? != 0,
            },
        ))
    })?;
    for row in rows {
        let (id, state) = row?;
        out.insert(id, state);
    }
    Ok(out)
}

pub(crate) fn read_session_index_ids(codex: &Path) -> AppResult<BTreeSet<String>> {
    let path = paths::session_index_path(codex);
    let mut ids = BTreeSet::new();
    if !path.is_file() {
        return Ok(ids);
    }
    for (line_no, line) in BufReader::new(fs::File::open(&path)?).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line).map_err(|error| {
            AppError::Other(format!(
                "session_index.jsonl 第 {} 行损坏: {error}",
                line_no + 1
            ))
        })?;
        let id = value
            .get("id")
            .or_else(|| value.get("session_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                AppError::Other(format!(
                    "session_index.jsonl 第 {} 行缺少有效 id",
                    line_no + 1
                ))
            })?;
        ids.insert(id.to_string());
    }
    Ok(ids)
}

fn rollout_is_usable_provider_session(
    codex: &Path,
    states: &BTreeMap<String, ThreadRepairState>,
    index_ids: &BTreeSet<String>,
    id: &str,
    expected_provider: &str,
    rollout: &Path,
) -> AppResult<bool> {
    let Some(state) = states.get(id) else {
        return Ok(false);
    };
    rollout_record_is_usable_provider(
        codex,
        id,
        expected_provider,
        rollout,
        state.rollout_path.as_deref(),
        state.model_provider.as_deref(),
        state.source.as_deref(),
        state.archived,
        index_ids.contains(id),
    )
}

fn family_branch_is_usable_provider(
    codex: &Path,
    states: &BTreeMap<String, ThreadRepairState>,
    index_ids: &BTreeSet<String>,
    branch: &FamilyBranch,
    expected_provider: &str,
) -> AppResult<bool> {
    let relative = paths::checked_relative_path(&branch.rollout_relpath)?;
    if !relative.starts_with("sessions") {
        return Ok(false);
    }
    rollout_is_usable_provider_session(
        codex,
        states,
        index_ids,
        &branch.id,
        expected_provider,
        &codex.join(relative),
    )
}

pub(crate) fn thread_fields_match_usable_provider(
    provider: Option<&str>,
    source: Option<&str>,
    archived: bool,
    expected: &str,
) -> bool {
    provider == Some(expected) && is_desktop_visible_source(source) && !archived
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn rollout_record_is_usable_provider(
    codex: &Path,
    id: &str,
    expected_provider: &str,
    rollout: &Path,
    recorded_rollout_path: Option<&str>,
    recorded_provider: Option<&str>,
    recorded_source: Option<&str>,
    archived: bool,
    indexed: bool,
) -> AppResult<bool> {
    if !thread_fields_match_usable_provider(
        recorded_provider,
        recorded_source,
        archived,
        expected_provider,
    ) || !indexed
        || !rollout.is_file()
    {
        return Ok(false);
    }
    let Some(recorded_rollout_path) = recorded_rollout_path else {
        return Ok(false);
    };
    let recorded_path = PathBuf::from(paths::strip_verbatim(
        &paths::host_path_string_from_codex_record(codex, recorded_rollout_path),
    ));
    if !recorded_path.is_file() || recorded_path.canonicalize()? != rollout.canonicalize()? {
        return Ok(false);
    }
    let Some(identity) = read_rollout_identity(rollout)? else {
        return Ok(false);
    };
    Ok(identity.id == id && identity.model_provider == expected_provider)
}

fn thread_state_is_subagent(states: &BTreeMap<String, ThreadRepairState>, id: &str) -> bool {
    states
        .get(id)
        .is_some_and(|state| is_subagent_source(state.source.as_deref()))
}

// ========================= Provider 克隆 =========================

pub(crate) fn new_session_id() -> String {
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
pub(crate) fn build_clone_path(
    codex_dir: &Path,
    new_id: &str,
    ts: &chrono::DateTime<chrono::Utc>,
) -> PathBuf {
    let dir = codex_dir
        .join("sessions")
        .join(ts.format("%Y").to_string())
        .join(ts.format("%m").to_string())
        .join(ts.format("%d").to_string());
    let stem = format!("rollout-{}-{}", ts.format("%Y-%m-%dT%H-%M-%S"), new_id);
    dir.join(format!("{}.jsonl", stem))
}

/// 验证生成的文件名能被 codex 的 parse_timestamp_uuid_from_filename 解析。
pub(crate) fn validate_rollout_filename(path: &Path) -> AppResult<()> {
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

fn rewrite_session_meta_identity(
    line: &str,
    source_session_id: &str,
    target_session_id: &str,
    provider: &str,
    timestamp_override: Option<&str>,
) -> AppResult<Option<String>> {
    let Ok(mut value) = serde_json::from_str::<Value>(line) else {
        return Ok(None);
    };
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Ok(None);
    }

    if let Some(timestamp) = timestamp_override {
        value["timestamp"] = Value::String(timestamp.to_string());
    }
    let payload = value
        .get_mut("payload")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| AppError::Other("session_meta 缺少有效 payload".into()))?;
    if payload.get("id").and_then(Value::as_str) != Some(source_session_id) {
        return Ok(None);
    }
    payload.insert("id".into(), Value::String(target_session_id.into()));
    payload.insert("session_id".into(), Value::String(target_session_id.into()));
    payload.insert("model_provider".into(), Value::String(provider.into()));
    if let Some(timestamp) = timestamp_override {
        payload.insert("timestamp".into(), Value::String(timestamp.to_string()));
    }
    // 血统信息由 family store 维护，不向 Codex 原生元数据注入私有字段。
    payload.remove("clone_timestamp");
    payload.remove("cloned_from");
    Ok(Some(serde_json::to_string(&value)?))
}

fn source_session_meta(line: &str, source_session_id: &str) -> AppResult<Option<Value>> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Ok(None);
    };
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Ok(None);
    }
    let payload = value
        .get("payload")
        .and_then(Value::as_object)
        .ok_or_else(|| AppError::Other("session_meta 缺少有效 payload".into()))?;
    if payload.get("id").and_then(Value::as_str) != Some(source_session_id) {
        return Ok(None);
    }
    Ok(Some(value))
}

fn ensure_legacy_history_mode(meta: &Value, operation: &str) -> AppResult<()> {
    let history_mode = meta
        .get("payload")
        .and_then(|payload| payload.get("history_mode"));
    match history_mode {
        None => Ok(()),
        Some(Value::String(mode)) if mode == "legacy" => Ok(()),
        Some(Value::String(mode)) if mode == "paginated" => Err(AppError::Other(format!(
            "{operation}暂不支持 history_mode=paginated；请使用 Codex 官方“在新任务中继续”完成派生"
        ))),
        Some(other) => Err(AppError::Other(format!(
            "{operation}遇到不支持的 history_mode={}；请升级工具或使用 Codex 官方派生功能",
            other
        ))),
    }
}

fn create_rollout_from_source_snapshot(
    src_abs: &Path,
    dest_abs: &Path,
    source_fingerprint: &atomic_file::FileFingerprint,
    writer: impl FnOnce(&mut fs::File) -> AppResult<()>,
) -> AppResult<()> {
    if let Some(parent) = dest_abs.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_file::create_with_writer_if_absent(dest_abs, |out| {
        writer(out)?;
        if atomic_file::fingerprint(src_abs)? != *source_fingerprint {
            return Err(AppError::Other(format!(
                "源 rollout 在复制期间发生变化，已取消新会话创建: {}",
                src_abs.to_string_lossy()
            )));
        }
        Ok(())
    })?;

    let post_commit = atomic_file::fingerprint(src_abs);
    if matches!(post_commit.as_ref(), Ok(current) if current == source_fingerprint) {
        return Ok(());
    }
    let source_detail = post_commit
        .err()
        .map(|error| format!(": {error}"))
        .unwrap_or_default();
    match fs::remove_file(dest_abs) {
        Ok(()) => Err(AppError::Other(format!(
            "源 rollout 在复制提交时发生变化，已移除未登记会话{}: {}",
            source_detail,
            src_abs.to_string_lossy()
        ))),
        Err(error) => Err(AppError::Other(format!(
            "源 rollout 在复制提交时发生变化{}，且未登记会话清理失败 {}: {error}",
            source_detail,
            dest_abs.to_string_lossy()
        ))),
    }
}

/// 深拷 rollout 到新 id + 新 provider；返回新文件绝对路径。
fn write_cloned_rollout(
    src_abs: &Path,
    dest_abs: &Path,
    new_id: &str,
    new_provider: &str,
    source_id: &str,
) -> AppResult<()> {
    let source_fingerprint = atomic_file::fingerprint(src_abs)?;
    create_rollout_from_source_snapshot(src_abs, dest_abs, &source_fingerprint, |out| {
        let reader = BufReader::new(fs::File::open(src_abs)?);
        let clone_timestamp = chrono::Utc::now().to_rfc3339();
        let mut found_session_meta = false;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if !found_session_meta {
                if let Some(meta) = source_session_meta(&line, source_id)? {
                    ensure_legacy_history_mode(&meta, "provider 克隆")?;
                }
            }
            let timestamp_override = (!found_session_meta).then_some(clone_timestamp.as_str());
            if let Some(rewritten) = rewrite_session_meta_identity(
                &line,
                source_id,
                new_id,
                new_provider,
                timestamp_override,
            )? {
                writeln!(out, "{rewritten}")?;
                found_session_meta = true;
                continue;
            }
            writeln!(out, "{}", line)?;
        }
        if !found_session_meta {
            return Err(AppError::Other(format!(
                "源 rollout 缺少有效 session_meta，拒绝创建 provider 分支: {}",
                src_abs.to_string_lossy()
            )));
        }
        Ok(())
    })
}

#[derive(Default)]
struct DuplicateTailState {
    turn_active: bool,
    active_turn_id: Option<String>,
    pending_tool_calls: HashSet<String>,
    unterminated_user_turn: bool,
}

impl DuplicateTailState {
    fn observe(&mut self, value: &Value) {
        let outer_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let payload = value.get("payload").unwrap_or(value);
        let payload_type = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        if outer_type == "event_msg" {
            match payload_type {
                "task_started" | "turn_started" => {
                    self.turn_active = true;
                    self.active_turn_id = payload
                        .get("turn_id")
                        .and_then(Value::as_str)
                        .map(String::from);
                    self.pending_tool_calls.clear();
                    self.unterminated_user_turn = false;
                }
                "task_complete" | "turn_complete" | "turn_aborted" => {
                    self.turn_active = false;
                    self.active_turn_id = None;
                    self.pending_tool_calls.clear();
                    self.unterminated_user_turn = false;
                }
                "user_message" => self.unterminated_user_turn = true,
                _ => {}
            }
            return;
        }
        if outer_type != "response_item" {
            return;
        }

        match payload_type {
            "message" => {
                if let Some(role) = payload.get("role").and_then(Value::as_str) {
                    if role == "user" {
                        self.unterminated_user_turn = true;
                    }
                }
            }
            "function_call" | "custom_tool_call" | "tool_search_call" => {
                if let Some(call_id) = response_item_call_id(payload) {
                    self.pending_tool_calls.insert(call_id);
                }
            }
            "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
                if let Some(call_id) = response_item_call_id(payload) {
                    self.pending_tool_calls.remove(&call_id);
                }
            }
            _ => {}
        }
    }

    fn needs_interruption(&self) -> bool {
        self.turn_active || self.unterminated_user_turn || !self.pending_tool_calls.is_empty()
    }
}

fn response_item_call_id(payload: &Value) -> Option<String> {
    payload
        .get("call_id")
        .or_else(|| payload.get("id"))
        .and_then(Value::as_str)
        .map(String::from)
}

fn build_independent_duplicate_meta(
    mut source_meta: Value,
    new_id: &str,
    provider: &str,
    source_id: &str,
    timestamp: &str,
) -> AppResult<String> {
    ensure_legacy_history_mode(&source_meta, "完整 Fork")?;
    source_meta["timestamp"] = Value::String(timestamp.to_string());
    let payload = source_meta
        .get_mut("payload")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| AppError::Other("session_meta 缺少有效 payload".into()))?;
    let source_is_subagent = payload
        .get("source")
        .and_then(metadata_string_value)
        .as_deref()
        .is_some_and(|source| is_subagent_source(Some(source)));

    payload.insert("id".into(), Value::String(new_id.to_string()));
    payload.insert("session_id".into(), Value::String(new_id.to_string()));
    payload.insert(
        "forked_from_id".into(),
        Value::String(source_id.to_string()),
    );
    payload.insert("timestamp".into(), Value::String(timestamp.to_string()));
    payload.insert("model_provider".into(), Value::String(provider.to_string()));
    payload.insert("thread_source".into(), Value::String("user".to_string()));
    payload.insert("history_mode".into(), Value::String("legacy".to_string()));
    if source_is_subagent || !payload.contains_key("source") {
        payload.insert(
            "source".into(),
            Value::String(DEFAULT_THREAD_SOURCE.to_string()),
        );
    }
    for field in [
        "parent_thread_id",
        "agent_nickname",
        "agent_role",
        "agent_type",
        "agent_path",
        "subagent_history_start_ordinal",
        "history_base",
        "context_window",
        "clone_timestamp",
        "cloned_from",
    ] {
        payload.remove(field);
    }
    Ok(serde_json::to_string(&source_meta)?)
}

fn write_interrupted_duplicate_boundary(
    out: &mut fs::File,
    turn_id: Option<&str>,
) -> AppResult<()> {
    const GUIDANCE: &str = "The user interrupted the previous turn on purpose. Any running unified exec processes may still be running in the background. If any tools/commands were aborted, they may have partially executed.";
    let timestamp = chrono::Utc::now().to_rfc3339();
    let marker = serde_json::json!({
        "timestamp": timestamp,
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": format!("<turn_aborted>\n{GUIDANCE}\n</turn_aborted>")
            }]
        }
    });
    let aborted = serde_json::json!({
        "timestamp": timestamp,
        "type": "event_msg",
        "payload": {
            "type": "turn_aborted",
            "turn_id": turn_id,
            "reason": "interrupted"
        }
    });
    writeln!(out, "{}", serde_json::to_string(&marker)?)?;
    writeln!(out, "{}", serde_json::to_string(&aborted)?)?;
    Ok(())
}

fn write_duplicated_rollout(
    src_abs: &Path,
    dest_abs: &Path,
    new_id: &str,
    provider: &str,
    source_id: &str,
) -> AppResult<()> {
    let source_fingerprint = atomic_file::fingerprint(src_abs)?;
    let source_meta = {
        let reader = BufReader::new(fs::File::open(src_abs)?);
        let mut found = None;
        for line in reader.lines() {
            let line = line?;
            if let Some(meta) = source_session_meta(&line, source_id)? {
                found = Some(meta);
                break;
            }
        }
        found.ok_or_else(|| {
            AppError::Other(format!(
                "源 rollout 缺少当前会话的 session_meta: {}",
                src_abs.to_string_lossy()
            ))
        })?
    };
    let clone_timestamp = chrono::Utc::now().to_rfc3339();
    let canonical = build_independent_duplicate_meta(
        source_meta,
        new_id,
        provider,
        source_id,
        &clone_timestamp,
    )?;

    create_rollout_from_source_snapshot(src_abs, dest_abs, &source_fingerprint, |out| {
        writeln!(out, "{canonical}")?;
        let reader = BufReader::new(fs::File::open(src_abs)?);
        let mut skipped_canonical = false;
        let mut tail = DuplicateTailState::default();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if !skipped_canonical && source_session_meta(&line, source_id)?.is_some() {
                skipped_canonical = true;
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                tail.observe(&value);
            }
            writeln!(out, "{line}")?;
        }
        if !skipped_canonical {
            return Err(AppError::Other(
                "复制期间未找到源会话 canonical meta".into(),
            ));
        }
        if tail.needs_interruption() {
            write_interrupted_duplicate_boundary(out, tail.active_turn_id.as_deref())?;
        }
        Ok(())
    })
}

fn require_unchanged_snapshot(
    path: &Path,
    expected: &atomic_file::FileFingerprint,
    label: &str,
) -> AppResult<()> {
    let current = atomic_file::fingerprint(path)?;
    if &current == expected {
        Ok(())
    } else {
        Err(AppError::Other(format!(
            "{label}在操作期间发生变化，已取消提交: {}",
            path.to_string_lossy()
        )))
    }
}

#[cfg(test)]
#[derive(Debug)]
enum RepairTestFaultAction {
    Error,
    Append(PathBuf),
    CreateAndError(PathBuf),
}

#[cfg(test)]
#[derive(Debug)]
struct RepairTestFault {
    stage: &'static str,
    action: RepairTestFaultAction,
}

#[cfg(test)]
std::thread_local! {
    static REPAIR_TEST_FAULT: std::cell::RefCell<Option<RepairTestFault>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
struct RepairTestFaultGuard;

#[cfg(test)]
impl RepairTestFaultGuard {
    fn error(stage: &'static str) -> Self {
        REPAIR_TEST_FAULT.with(|fault| {
            *fault.borrow_mut() = Some(RepairTestFault {
                stage,
                action: RepairTestFaultAction::Error,
            });
        });
        Self
    }

    fn append(stage: &'static str, path: PathBuf) -> Self {
        REPAIR_TEST_FAULT.with(|fault| {
            *fault.borrow_mut() = Some(RepairTestFault {
                stage,
                action: RepairTestFaultAction::Append(path),
            });
        });
        Self
    }

    fn create_and_error(stage: &'static str, path: PathBuf) -> Self {
        REPAIR_TEST_FAULT.with(|fault| {
            *fault.borrow_mut() = Some(RepairTestFault {
                stage,
                action: RepairTestFaultAction::CreateAndError(path),
            });
        });
        Self
    }
}

#[cfg(test)]
impl Drop for RepairTestFaultGuard {
    fn drop(&mut self) {
        REPAIR_TEST_FAULT.with(|fault| *fault.borrow_mut() = None);
    }
}

#[cfg(test)]
fn inject_repair_fault(stage: &'static str) -> AppResult<()> {
    let fault = REPAIR_TEST_FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if fault.as_ref().is_some_and(|fault| fault.stage == stage) {
            fault.take()
        } else {
            None
        }
    });
    let Some(fault) = fault else {
        return Ok(());
    };
    match fault.action {
        RepairTestFaultAction::Error => {
            Err(AppError::Other(format!("测试故障注入: {}", fault.stage)))
        }
        RepairTestFaultAction::Append(path) => {
            let mut file = fs::OpenOptions::new().append(true).open(&path)?;
            writeln!(
                file,
                "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"test_append\"}}}}"
            )?;
            file.flush()?;
            Ok(())
        }
        RepairTestFaultAction::CreateAndError(path) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, "test compensation conflict")?;
            Err(AppError::Other(format!("测试故障注入: {}", fault.stage)))
        }
    }
}

#[cfg(not(test))]
fn inject_repair_fault(_stage: &'static str) -> AppResult<()> {
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
    atomic_file::create_with_writer_if_absent(dest_abs, |out| {
        for (idx, item) in prefix.lines.iter().enumerate() {
            if idx == 0 {
                let now_iso = chrono::Utc::now().to_rfc3339();
                let source_id = item
                    .value
                    .get("payload")
                    .and_then(|payload| payload.get("id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AppError::Other("session_meta.payload.id 缺失，无法创建新分支".into())
                    })?;
                let canonical = build_independent_duplicate_meta(
                    item.value.clone(),
                    new_id,
                    provider,
                    source_id,
                    &now_iso,
                )?;
                writeln!(out, "{canonical}")?;
            } else {
                writeln!(out, "{}", item.raw_line)?;
            }
        }
        Ok(())
    })?;
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

fn duplicate_session_locked(
    codex_dir: String,
    session_id: String,
    rollout_path: String,
) -> AppResult<DuplicateSessionReport> {
    let codex = PathBuf::from(&codex_dir);
    let codex = codex.canonicalize().unwrap_or(codex);
    crate::codex_projects::ensure_desktop_not_running(&codex)?;

    let source = paths::host_path_from_codex_record(&codex, &rollout_path);
    let source_abs = source.canonicalize().map_err(|error| {
        AppError::NotFound(format!(
            "源 rollout 不存在或无法访问: {} ({error})",
            source.to_string_lossy()
        ))
    })?;
    crate::sessions::validate_codex_rollout_path(&codex, &source_abs, &session_id)?;
    let source_brief = read_rollout_brief(&codex, &source_abs)?.ok_or_else(|| {
        AppError::NotFound(format!(
            "无法读取源 rollout 元数据: {}",
            source_abs.to_string_lossy()
        ))
    })?;
    if source_brief.id != session_id {
        return Err(AppError::Other(format!(
            "源 rollout id 与会话不一致：期望 {}，实际 {}",
            session_id, source_brief.id
        )));
    }

    let provider = source_brief
        .model_provider
        .clone()
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_string());

    let new_id = new_session_id();
    let now = chrono::Utc::now();
    let new_abs = build_clone_path(&codex, &new_id, &now);
    validate_rollout_filename(&new_abs)?;

    ensure_state_db_exists(&codex)?;
    let state = state_db::open(&codex)?;
    let transaction =
        rusqlite::Transaction::new_unchecked(&state, rusqlite::TransactionBehavior::Immediate)?;
    let mut journal = MutationJournal::default();
    let operation = (|| -> AppResult<u64> {
        journal.mutate_file(&new_abs, || {
            write_duplicated_rollout(&source_abs, &new_abs, &new_id, &provider, &session_id)
        })?;
        sync_thread_from_rollout(&codex, &transaction, &new_abs)?;
        carry_thread_title(&transaction, &session_id, &new_id)?;

        let provenance_path = paths::session_provenance_path(&codex);
        journal.mutate_file(&provenance_path, || {
            crate::provenance::copy_conversion_origin(&codex, "codex", &session_id, &new_id)
        })?;

        let new_brief = read_rollout_brief(&codex, &new_abs)?
            .ok_or_else(|| AppError::Other("新 rollout 缺少有效 session_meta.id".into()))?;
        let thread_name = if new_brief.first_user_message.is_empty() {
            source_brief.first_user_message.clone()
        } else {
            new_brief.first_user_message.clone()
        };
        let index_path = paths::session_index_path(&codex);
        journal.mutate_file(&index_path, || {
            append_index_line(&codex, &new_id, &thread_name, &new_abs)
        })?;
        if let Some((thread_id, host_cwd)) = project_assignment_record(&codex, &new_brief) {
            if let Some(receipt) =
                crate::codex_projects::sync_thread_project_assignment_with_receipt(
                    &codex, &thread_id, &host_cwd,
                )?
            {
                journal.register_project_state_receipt(receipt);
            }
        }

        let file = fs::File::open(&new_abs)?;
        Ok(std::io::BufReader::new(file).lines().count() as u64)
    })();
    let total_lines = match operation {
        Ok(total_lines) => {
            commit_transaction_with_compensation(transaction, journal)?;
            total_lines
        }
        Err(error) => {
            return Err(rollback_transaction_with_compensation(
                transaction,
                journal,
                error,
            ));
        }
    };

    Ok(DuplicateSessionReport {
        source_id: session_id,
        new_id,
        new_rollout_path: new_abs.to_string_lossy().into_owned(),
        total_lines,
    })
}

pub fn duplicate_session_with_lock(
    codex_dir: String,
    session_id: String,
    rollout_path: String,
    lock: &family::FamilyLock,
) -> AppResult<DuplicateSessionReport> {
    family::with_lock(lock, |_g| {
        duplicate_session_locked(codex_dir, session_id, rollout_path)
    })
}

fn fork_session_at_event_locked(
    codex_dir: String,
    session_id: String,
    rollout_path: String,
    event_index: usize,
) -> AppResult<ForkSessionReport> {
    let codex = PathBuf::from(&codex_dir);
    let codex = codex.canonicalize().unwrap_or(codex);
    crate::codex_projects::ensure_desktop_not_running(&codex)?;
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

    let archived_dir = paths::archived_sessions_dir(&codex);
    let archived_dest = archived_dir.join(source_abs.file_name().unwrap_or_default());
    if archived_dest.exists() {
        return Err(AppError::Other(format!(
            "源分支归档目标已存在，拒绝覆盖: {}",
            archived_dest.to_string_lossy()
        )));
    }

    let transaction =
        rusqlite::Transaction::new_unchecked(&state, rusqlite::TransactionBehavior::Immediate)?;
    let mut journal = MutationJournal::default();
    let operation = (|| -> AppResult<u64> {
        let included_lines = journal.mutate_file(&new_abs, || {
            write_forked_rollout_prefix(&prefix, &new_abs, &new_id, &provider)
        })?;
        sync_thread_from_rollout(&codex, &transaction, &new_abs)?;
        sync_thread_from_rollout(&codex, &transaction, &source_abs)?;
        carry_thread_title(&transaction, &active_branch.id, &new_id)?;

        family::archive_with_integrity(&mut store, &codex, &family_id, &active_branch.id)?;
        fs::create_dir_all(&archived_dir)?;
        journal.move_file(&source_abs, &archived_dest)?;
        mark_thread_archived(&transaction, &active_branch.id, &archived_dest)?;
        let index_path = paths::session_index_path(&codex);
        journal.mutate_file(&index_path, || remove_index_line(&codex, &active_branch.id))?;

        let new_brief = read_rollout_brief(&codex, &new_abs)?.ok_or_else(|| {
            AppError::Other(format!(
                "新分支 rollout 缺少有效 session_meta.id: {}",
                new_abs.to_string_lossy()
            ))
        })?;
        let thread_name = if new_brief.first_user_message.is_empty() {
            source_brief.first_user_message.clone()
        } else {
            new_brief.first_user_message.clone()
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
            archive_origin: None,
        };
        family::append_branch(&mut store, &family_id, new_branch)?;
        journal.mutate_file(&index_path, || {
            append_index_line(&codex, &new_id, &thread_name, &new_abs)
        })?;
        let family_path = paths::family_store_path(&codex);
        journal.mutate_file(&family_path, || family::save(&codex, &store))?;
        if let Some((thread_id, host_cwd)) = project_assignment_record(&codex, &new_brief) {
            if let Some(receipt) =
                crate::codex_projects::sync_thread_project_assignment_with_receipt(
                    &codex, &thread_id, &host_cwd,
                )?
            {
                journal.register_project_state_receipt(receipt);
            }
        }
        Ok(included_lines)
    })();
    let included_lines = match operation {
        Ok(included_lines) => {
            commit_transaction_with_compensation(transaction, journal)?;
            included_lines
        }
        Err(error) => {
            return Err(rollback_transaction_with_compensation(
                transaction,
                journal,
                error,
            ));
        }
    };

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

fn clone_session_for_provider_locked(
    codex_dir: String,
    session_id: String,
    target_provider: Option<String>,
    strategy: SwitchStrategy,
    dry_run: bool,
) -> AppResult<CloneReport> {
    clone_session_for_provider_locked_with_hint(
        codex_dir,
        session_id,
        target_provider,
        strategy,
        dry_run,
        None,
    )
}

fn clone_session_for_provider_locked_with_hint(
    codex_dir: String,
    session_id: String,
    target_provider: Option<String>,
    strategy: SwitchStrategy,
    dry_run: bool,
    source_rollout_hint: Option<&Path>,
) -> AppResult<CloneReport> {
    let codex = PathBuf::from(&codex_dir);
    if !dry_run {
        crate::codex_projects::ensure_desktop_not_running(&codex)?;
    }
    let configured_provider = effective_current_provider(&codex)?;
    let provider = match target_provider {
        Some(provider) => {
            let provider = provider.trim();
            if provider.is_empty() {
                return Err(AppError::Other("目标 provider 不能为空".into()));
            }
            provider.to_string()
        }
        None => configured_provider,
    };

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
    // 批量同步已在计划阶段完成过全目录扫描，此处直接复用目标路径，避免每条会话
    // 都重新遍历并完整解析此前的所有 rollout，导致会话数增加时接近 O(n²)。
    let mut src_brief: Option<RolloutBrief> = match source_rollout_hint {
        Some(path) => read_rollout_brief(&codex, path)?,
        None => None,
    };
    if src_brief
        .as_ref()
        .is_some_and(|brief| brief.id != session_id)
    {
        return Err(AppError::Other(format!(
            "provider 同步计划路径与会话 ID 不一致: 期望 {session_id}，实际 {}",
            src_brief
                .as_ref()
                .map(|brief| brief.id.as_str())
                .unwrap_or("")
        )));
    }
    if source_rollout_hint.is_none() {
        for p in family::scan_rollouts(&codex)? {
            let Some(b) = read_rollout_brief(&codex, &p)? else {
                continue;
            };
            if b.id == session_id {
                src_brief = Some(b);
                break;
            }
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
    let family_was_registered = store.index.contains_key(&session_id);
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
    if let Some(active) = active_branch.as_ref() {
        if active.id != session_id {
            return Err(AppError::Other(format!(
                "provider 切换只能操作 family 当前 active 分支：当前 active={}，请求会话={}",
                active.id, session_id
            )));
        }
    }
    if let Some(b) = active_branch.as_ref() {
        if b.provider == provider {
            if dry_run {
                report.skipped_reason = Some("dry_run: 将复核并修复本地索引可见性".into());
            } else {
                ensure_state_db_exists(&codex)?;
                let state = state_db::open(&codex)?;
                let project_assignment = project_assignment_record(&codex, &src_brief);
                if let Some(record) = project_assignment.as_ref() {
                    crate::codex_projects::validate_missing_thread_project_assignment_records(
                        &codex,
                        std::slice::from_ref(record),
                    )?;
                }

                let transaction = rusqlite::Transaction::new_unchecked(
                    &state,
                    rusqlite::TransactionBehavior::Immediate,
                )?;
                let mut journal = MutationJournal::default();
                let operation = (|| -> AppResult<()> {
                    sync_thread_from_rollout(&codex, &transaction, &src_brief.path)?;
                    let index_path = paths::session_index_path(&codex);
                    journal.mutate_file(&index_path, || {
                        append_index_line(
                            &codex,
                            &src_brief.id,
                            &src_brief.first_user_message,
                            &src_brief.path,
                        )
                    })?;
                    if let Some(record) = project_assignment.as_ref() {
                        if let Some(receipt) = crate::codex_projects::sync_missing_thread_project_assignment_records_with_receipt(
                            &codex,
                            std::slice::from_ref(record),
                        )? {
                            journal.register_project_state_receipt(receipt);
                        }
                    }
                    if !family_was_registered {
                        let family_path = paths::family_store_path(&codex);
                        journal.mutate_file(&family_path, || family::save(&codex, &store))?;
                    }
                    inject_repair_fault("provider_visibility_after_family_save")?;
                    Ok(())
                })();
                if let Err(error) = operation {
                    return Err(rollback_transaction_with_compensation(
                        transaction,
                        journal,
                        error,
                    ));
                }
                commit_transaction_with_compensation(transaction, journal)?;

                let states = read_thread_state_map(&codex)?;
                let index_ids = read_session_index_ids(&codex)?;
                if !rollout_is_usable_provider_session(
                    &codex,
                    &states,
                    &index_ids,
                    &src_brief.id,
                    &provider,
                    &src_brief.path,
                )? {
                    return Err(AppError::Other(format!(
                        "会话 {} 的可见性修复后复核仍未通过",
                        src_brief.id
                    )));
                }
                report.skipped_reason = Some("已修复并复核本地索引可见性".into());
            }
            report.ok = true;
            return Ok(report);
        }
    }

    let mut existing_usable_target = None;
    if let Some(family) = store.families.get(&family_id) {
        if family
            .chain
            .iter()
            .any(|branch| branch.provider == provider)
        {
            let thread_states = read_thread_state_map(&codex)?;
            let index_ids = read_session_index_ids(&codex)?;
            for branch in &family.chain {
                if branch.provider == provider
                    && family_branch_is_usable_provider(
                        &codex,
                        &thread_states,
                        &index_ids,
                        branch,
                        &provider,
                    )?
                {
                    existing_usable_target = Some(branch);
                    break;
                }
            }
        }
    }
    if let Some(branch) = existing_usable_target {
        report.new_id = Some(branch.id.clone());
        report.skipped_reason = Some("目标 provider 已有可用分支".into());
        report.ok = true;
        if !dry_run {
            let target_rollout = codex.join(paths::checked_relative_path(&branch.rollout_relpath)?);
            let target_cwd =
                crate::codex_rollout_cwd::read_effective_cwd(&target_rollout, &branch.id)?;
            let project_assignment = target_cwd
                .as_deref()
                .map(str::trim)
                .filter(|cwd| !cwd.is_empty())
                .map(|cwd| {
                    (
                        branch.id.clone(),
                        paths::host_path_string_from_codex_record(&codex, cwd),
                    )
                });
            if let Some(record) = project_assignment.as_ref() {
                crate::codex_projects::validate_missing_thread_project_assignment_records(
                    &codex,
                    std::slice::from_ref(record),
                )?;
            }

            let mut journal = MutationJournal::default();
            let operation = (|| -> AppResult<()> {
                if let Some(record) = project_assignment.as_ref() {
                    if let Some(receipt) = crate::codex_projects::sync_missing_thread_project_assignment_records_with_receipt(
                        &codex,
                        std::slice::from_ref(record),
                    )? {
                        journal.register_project_state_receipt(receipt);
                    }
                }
                if !family_was_registered {
                    let family_path = paths::family_store_path(&codex);
                    journal.mutate_file(&family_path, || family::save(&codex, &store))?;
                }
                Ok(())
            })();
            if let Err(error) = operation {
                return Err(journal.compensate_without_transaction(error));
            }
        }
        return Ok(report);
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
            let transaction = rusqlite::Transaction::new_unchecked(
                &state,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let mut journal = MutationJournal::default();
            let operation = (|| -> AppResult<()> {
                if let Some(record) = project_assignment_record(&codex, &src_brief) {
                    if let Some(receipt) = crate::codex_projects::sync_missing_thread_project_assignment_records_with_receipt(
                        &codex,
                        &[record],
                    )? {
                        journal.register_project_state_receipt(receipt);
                    }
                }
                journal.mutate_file(&src_brief.path, || {
                    rewrite_provider_inplace(&src_brief.path, &provider)
                })?;
                inject_repair_fault("follow_after_rollout")?;
                sync_thread_from_rollout(&codex, &transaction, &src_brief.path)?;
                inject_repair_fault("follow_after_thread")?;
                let index_path = paths::session_index_path(&codex);
                journal.mutate_file(&index_path, || {
                    append_index_line(
                        &codex,
                        &src_brief.id,
                        &src_brief.first_user_message,
                        &src_brief.path,
                    )
                })?;
                inject_repair_fault("follow_after_index")?;

                // 家族记录：更新当前 active 分支的 provider
                if let Some(f) = store.families.get_mut(&family_id) {
                    if let Some(b) = f.chain.iter_mut().find(|b| b.id == f.active_id) {
                        b.provider = provider.clone();
                    }
                    f.updated_at = chrono::Utc::now().to_rfc3339();
                }
                let family_path = paths::family_store_path(&codex);
                journal.mutate_file(&family_path, || family::save(&codex, &store))?;
                inject_repair_fault("follow_after_family_save")?;
                Ok(())
            })();
            if let Err(error) = operation {
                return Err(rollback_transaction_with_compensation(
                    transaction,
                    journal,
                    error,
                ));
            }
            commit_transaction_with_compensation(transaction, journal)?;
            report.new_id = Some(src_brief.id.clone());
            report.new_rollout_path = Some(src_brief.path.to_string_lossy().into_owned());
            report.ok = true;
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

            let continuous_archive = if matches!(strategy, SwitchStrategy::Continuous) {
                let branch = active_branch.as_ref().ok_or_else(|| {
                    AppError::Other("continuous 模式缺少 active family 分支".into())
                })?;
                let old_rel = paths::checked_relative_path(&branch.rollout_relpath)?;
                let old_abs = codex.join(old_rel);
                if !old_abs.is_file() {
                    return Err(AppError::NotFound(format!(
                        "旧 active rollout 不存在，不能归档: {}",
                        old_abs.to_string_lossy()
                    )));
                }
                require_thread_row(&state, &branch.id)?;
                family::compute_integrity(&old_abs)?;
                let archived_dir = paths::archived_sessions_dir(&codex);
                let destination = archived_dir.join(old_abs.file_name().ok_or_else(|| {
                    AppError::Path(format!("rollout 缺少文件名: {}", old_abs.to_string_lossy()))
                })?);
                if destination.exists() {
                    return Err(AppError::Other(format!(
                        "归档目标已存在，拒绝覆盖: {}",
                        destination.to_string_lossy()
                    )));
                }
                Some((branch.id.clone(), old_abs, destination))
            } else {
                None
            };
            let source_snapshot = atomic_file::fingerprint(&source_rollout)?;
            let transaction = rusqlite::Transaction::new_unchecked(
                &state,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let mut journal = MutationJournal::default();
            let operation = (|| -> AppResult<()> {
                // 1) 基于固定源快照写新文件并登记新 threads 行。
                journal.mutate_file(&new_abs, || {
                    write_cloned_rollout(
                        &source_rollout,
                        &new_abs,
                        &new_id,
                        &provider,
                        active_branch
                            .as_ref()
                            .map(|b| b.id.as_str())
                            .unwrap_or(&session_id),
                    )
                })?;
                inject_repair_fault("clone_after_new_rollout")?;
                require_unchanged_snapshot(&source_rollout, &source_snapshot, "克隆源 rollout ")?;
                sync_thread_from_rollout(&codex, &transaction, &new_abs)?;
                let new_brief = read_rollout_brief(&codex, &new_abs)?.ok_or_else(|| {
                    AppError::Other(format!(
                        "新分支 rollout 缺少有效 session_meta.id: {}",
                        new_abs.to_string_lossy()
                    ))
                })?;
                if let Some((thread_id, host_cwd)) = project_assignment_record(&codex, &new_brief) {
                    if let Some(receipt) =
                        crate::codex_projects::sync_thread_project_assignment_with_receipt(
                            &codex, &thread_id, &host_cwd,
                        )?
                    {
                        journal.register_project_state_receipt(receipt);
                    }
                }
                carry_thread_title(
                    &transaction,
                    active_branch
                        .as_ref()
                        .map(|b| b.id.as_str())
                        .unwrap_or(&session_id),
                    &new_id,
                )?;
                inject_repair_fault("clone_after_thread")?;

                // 2) Continuous 在移动旧 active 前再次绑定克隆时的同一源快照。
                if let Some((branch_id, old_abs, destination)) = continuous_archive.as_ref() {
                    require_unchanged_snapshot(old_abs, &source_snapshot, "旧 active rollout ")?;
                    family::archive_with_integrity(&mut store, &codex, &family_id, branch_id)?;
                    family::set_archive_origin(
                        &mut store,
                        &family_id,
                        branch_id,
                        ArchiveOrigin::ProviderSync,
                    )?;
                    require_unchanged_snapshot(old_abs, &source_snapshot, "旧 active rollout ")?;
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    journal.move_file(old_abs, destination)?;
                    require_unchanged_snapshot(
                        destination,
                        &source_snapshot,
                        "已归档旧 active rollout ",
                    )?;
                    inject_repair_fault("clone_after_old_archive")?;
                    mark_thread_archived(&transaction, branch_id, destination)?;
                    let index_path = paths::session_index_path(&codex);
                    journal.mutate_file(&index_path, || remove_index_line(&codex, branch_id))?;
                }

                // 3) 追加新分支为 active（Scatter 不移动旧文件，但 family active 同步切换）。
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
                    archive_origin: None,
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

                // 4) 索引和 family 都完成后才提交 SQLite 事务。
                let index_path = paths::session_index_path(&codex);
                journal.mutate_file(&index_path, || {
                    append_index_line(&codex, &new_id, &src_brief.first_user_message, &new_abs)
                })?;
                inject_repair_fault("clone_after_index")?;

                let family_path = paths::family_store_path(&codex);
                journal.mutate_file(&family_path, || family::save(&codex, &store))?;
                inject_repair_fault("clone_after_family_save")?;
                Ok(())
            })();
            if let Err(error) = operation {
                return Err(rollback_transaction_with_compensation(
                    transaction,
                    journal,
                    error,
                ));
            }
            commit_transaction_with_compensation(transaction, journal)?;

            report.new_id = Some(new_id);
            report.new_rollout_path = Some(new_abs.to_string_lossy().into_owned());
            report.ok = true;
            Ok(report)
        }
    }
}

fn rewrite_provider_inplace(path: &Path, new_provider: &str) -> AppResult<()> {
    let expected = atomic_file::fingerprint(path)?;
    let raw = fs::read_to_string(path)?;
    let mut rewritten = false;
    let mut output = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if !rewritten {
            if let Ok(mut v) = serde_json::from_str::<Value>(line) {
                if v.get("type").and_then(|x| x.as_str()) == Some("session_meta") {
                    if let Some(payload) = v.get_mut("payload").and_then(|p| p.as_object_mut()) {
                        payload.insert("model_provider".into(), Value::String(new_provider.into()));
                    }
                    output.push(serde_json::to_string(&v)?);
                    rewritten = true;
                    continue;
                }
            }
        }
        output.push(line.to_string());
    }
    if !rewritten {
        return Err(AppError::InvalidCodexDir(format!(
            "rollout 缺少 session_meta，无法改写 provider: {}",
            path.to_string_lossy()
        )));
    }
    atomic_file::replace_with_writer_if_unchanged(path, &expected, |file| {
        for line in output {
            writeln!(file, "{line}")?;
        }
        Ok(())
    })?;
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

    let expected = if index_path.is_file() {
        Some(atomic_file::fingerprint(&index_path)?)
    } else {
        None
    };
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

    let write_index = |file: &mut fs::File| -> AppResult<()> {
        for line in &lines {
            writeln!(file, "{line}")?;
        }
        Ok(())
    };
    if let Some(expected) = expected.as_ref() {
        atomic_file::replace_with_writer_if_unchanged(&index_path, expected, write_index)?;
    } else {
        atomic_file::create_with_writer_if_absent(&index_path, write_index)?;
    }
    Ok(())
}

/// 列出"active 分支 provider ≠ target_provider"的 session id（去重，稳定顺序）。
///
/// - 优先读 `session_family.json`（单点真相）
/// - 对尚未进入 family store 的历史会话继续扫描 rollout，避免部分迁移状态漏处理
/// - 已在 target_provider 下存在 clone（同家族有匹配 provider 的分支）的不计入
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderSyncTarget {
    session_id: String,
    rollout_path: PathBuf,
}

fn list_mismatched_sessions(
    codex: &Path,
    target_provider: &str,
) -> AppResult<Vec<ProviderSyncTarget>> {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut family_managed_ids: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<ProviderSyncTarget> = Vec::new();
    let thread_states = read_thread_state_map(codex)?;
    let index_ids = read_session_index_ids(codex)?;
    let active_rollouts = scan_active_rollout_identities(codex)?;
    let active_rollout_paths = active_rollouts
        .iter()
        .map(|(path, identity)| (identity.id.as_str(), path))
        .collect::<BTreeMap<_, _>>();

    let store = family::load(codex)?;
    family_managed_ids.extend(store.index.keys().cloned());
    for f in store.families.values() {
        family_managed_ids.extend(f.chain.iter().map(|b| b.id.clone()));
        if let Some(active) = f.chain.iter().find(|b| b.id == f.active_id) {
            if !matches!(active.status, BranchStatus::Active) {
                continue;
            }
            // family/threads 可能遗留已经不存在的 active 分支。没有源 rollout 时，
            // provider 克隆无从执行；这类记录应由 orphan 清理处理，而不是制造必然失败的同步任务。
            let Some(active_rollout_path) = active_rollout_paths.get(active.id.as_str()) else {
                continue;
            };
            // 手工归档的 family head 仍保持逻辑 Active 角色，但不应被 provider
            // 批量同步重新复制成一条可见会话。
            if thread_states
                .get(&active.id)
                .is_some_and(|state| state.archived)
            {
                continue;
            }
            if thread_state_is_subagent(&thread_states, &active.id) {
                continue;
            }
            let mut has_target_branch = false;
            for branch in &f.chain {
                if branch.provider == target_provider
                    && family_branch_is_usable_provider(
                        codex,
                        &thread_states,
                        &index_ids,
                        branch,
                        target_provider,
                    )?
                {
                    has_target_branch = true;
                    break;
                }
            }
            if active.provider != target_provider && has_target_branch {
                continue;
            }
            let state_drift = !family_branch_is_usable_provider(
                codex,
                &thread_states,
                &index_ids,
                active,
                &active.provider,
            )?;
            if (active.provider != target_provider || state_drift) && seen.insert(active.id.clone())
            {
                out.push(ProviderSyncTarget {
                    session_id: active.id.clone(),
                    rollout_path: (*active_rollout_path).clone(),
                });
            }
        }
    }

    for (p, identity) in active_rollouts {
        if family_managed_ids.contains(&identity.id) {
            continue;
        }
        if is_subagent_source(identity.source.as_deref()) {
            continue;
        }
        let provider = identity.model_provider.as_str();
        let state_drift = !rollout_is_usable_provider_session(
            codex,
            &thread_states,
            &index_ids,
            &identity.id,
            provider,
            &p,
        )?;
        if (provider != target_provider || state_drift) && seen.insert(identity.id.clone()) {
            out.push(ProviderSyncTarget {
                session_id: identity.id,
                rollout_path: p,
            });
        }
    }
    Ok(out)
}

fn list_mismatched_session_ids(codex: &Path, target_provider: &str) -> AppResult<Vec<String>> {
    Ok(list_mismatched_sessions(codex, target_provider)?
        .into_iter()
        .map(|target| target.session_id)
        .collect())
}

/// 返回当前 provider 实际会被批量同步处理的会话 ID。
///
/// 该计划与 `batch_clone_for_current_provider_with_lock` 复用同一套扫描逻辑，
/// 因此也包含尚未登记进 family store 的历史 rollout。
pub fn get_provider_sync_plan_with_lock(
    codex_dir: String,
    lock: &family::FamilyLock,
) -> AppResult<Vec<String>> {
    family::with_lock(lock, |_g| {
        let codex = PathBuf::from(codex_dir);
        let current_provider = effective_current_provider(&codex)?;
        list_mismatched_session_ids(&codex, &current_provider)
    })
}

/// 对所有 active 分支 provider ≠ 当前 provider 的家族批量克隆。
pub fn batch_clone_for_current_provider_with_lock(
    codex_dir: String,
    strategy: SwitchStrategy,
    dry_run: bool,
    lock: &family::FamilyLock,
) -> AppResult<Vec<CloneReport>> {
    batch_clone_for_current_provider_with_progress(
        codex_dir,
        strategy,
        dry_run,
        lock,
        |_, _, _, _| {},
    )
}

pub fn batch_clone_for_current_provider_with_progress<F>(
    codex_dir: String,
    strategy: SwitchStrategy,
    dry_run: bool,
    lock: &family::FamilyLock,
    mut on_progress: F,
) -> AppResult<Vec<CloneReport>>
where
    F: FnMut(usize, usize, Option<String>, Option<CloneReport>),
{
    family::with_lock(lock, |_g| {
        let codex = PathBuf::from(&codex_dir);
        let cur = effective_current_provider(&codex)?;

        let targets = list_mismatched_sessions(&codex, &cur)?;
        let total = targets.len();
        on_progress(0, total, None, None);

        let mut out: Vec<CloneReport> = Vec::new();
        for target in targets {
            let id = target.session_id;
            on_progress(out.len(), total, Some(id.clone()), None);
            let report = match clone_session_for_provider_locked_with_hint(
                codex_dir.clone(),
                id.clone(),
                Some(cur.clone()),
                strategy.clone(),
                dry_run,
                Some(&target.rollout_path),
            ) {
                Ok(report) => report,
                Err(e) => CloneReport {
                    source_id: id,
                    new_id: None,
                    new_rollout_path: None,
                    new_provider: cur.clone(),
                    ok: false,
                    skipped_reason: None,
                    error: Some(e.to_string()),
                },
            };
            out.push(report.clone());
            on_progress(out.len(), total, None, Some(report));
        }
        Ok(out)
    })
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

fn rollback_family_active_locked(
    codex_dir: String,
    family_id: String,
    target_branch_id: String,
) -> AppResult<()> {
    let codex = PathBuf::from(&codex_dir);
    crate::codex_projects::ensure_desktop_not_running(&codex)?;
    ensure_state_db_exists(&codex)?;
    let state = state_db::open(&codex)?;
    let mut store = family::load(&codex)?;
    let family = store
        .families
        .get(&family_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("family: {}", family_id)))?;
    if family.active_id == target_branch_id {
        return Err(AppError::Other("目标分支已经是当前 active".into()));
    }

    // 先完成所有可预见条件的预检，再开始移动当前 active，避免目标异常造成半完成状态。
    let cur_active = family
        .chain
        .iter()
        .find(|b| b.id == family.active_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("active branch: {}", family.active_id)))?;
    let target = family
        .chain
        .iter()
        .find(|b| b.id == target_branch_id)
        .cloned()
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "branch not in family {}: {}",
                family_id, target_branch_id
            ))
        })?;

    let cur_rel = paths::checked_relative_path(&cur_active.rollout_relpath)?;
    let cur_abs = codex.join(&cur_rel);
    if !cur_abs.is_file() {
        return Err(AppError::NotFound(format!(
            "当前 active rollout 不存在，不能归档: {}",
            cur_abs.to_string_lossy()
        )));
    }
    require_thread_row(&state, &cur_active.id)?;
    let archived_dir = paths::archived_sessions_dir(&codex);
    let cur_archived_abs = archived_dir.join(cur_abs.file_name().unwrap_or_default());
    if cur_archived_abs.exists() {
        return Err(AppError::Other(format!(
            "当前 active 的归档目标已存在，取消切换: {}",
            cur_archived_abs.to_string_lossy()
        )));
    }

    let target_rel = paths::checked_relative_path(&target.rollout_relpath)?;
    let expected_abs = codex.join(&target_rel);
    let archived_target_abs = archived_dir.join(target_rel.file_name().unwrap_or_default());
    let target_already_active = expected_abs.is_file();
    if target_already_active && archived_target_abs.exists() {
        return Err(AppError::Other(format!(
            "目标分支同时存在 active 与 archived 副本，取消切换: {} / {}",
            expected_abs.to_string_lossy(),
            archived_target_abs.to_string_lossy()
        )));
    }
    if !target_already_active && expected_abs.exists() {
        return Err(AppError::Other(format!(
            "目标分支恢复路径已被非文件条目占用: {}",
            expected_abs.to_string_lossy()
        )));
    }
    let target_source_abs = if target_already_active {
        expected_abs.clone()
    } else if archived_target_abs.is_file() {
        archived_target_abs.clone()
    } else {
        return Err(AppError::NotFound(format!(
            "目标分支 rollout 丢失: {}",
            expected_abs.to_string_lossy()
        )));
    };
    let target_brief = read_rollout_brief(&codex, &target_source_abs)?.ok_or_else(|| {
        AppError::Other(format!(
            "目标分支 rollout 缺少有效 session_meta.id: {}",
            target_source_abs.to_string_lossy()
        ))
    })?;
    if target_brief.id != target_branch_id {
        return Err(AppError::Other(format!(
            "目标分支 rollout id 不匹配：期望 {}，实际 {}",
            target_branch_id, target_brief.id
        )));
    }
    let current_lines = read_rollout_lines(&cur_abs)?;
    let target_lines = read_rollout_lines(&target_source_abs)?;
    let (relation, _, appendable_to_target) = compare_rollout_lines(&current_lines, &target_lines);
    if relation == "active_ahead" {
        return Err(AppError::Other(format!(
            "目标分支落后当前 active {appendable_to_target} 行；请先把当前分支增量同步到目标分支，再设为当前"
        )));
    }
    let current_snapshot = atomic_file::fingerprint(&cur_abs)?;
    let target_snapshot = atomic_file::fingerprint(&target_source_abs)?;
    let transaction =
        rusqlite::Transaction::new_unchecked(&state, rusqlite::TransactionBehavior::Immediate)?;
    let mut journal = MutationJournal::default();
    let operation = (|| -> AppResult<()> {
        if let Some(record) = project_assignment_record(&codex, &target_brief) {
            if let Some(receipt) =
                crate::codex_projects::sync_missing_thread_project_assignment_records_with_receipt(
                    &codex,
                    &[record],
                )?
            {
                journal.register_project_state_receipt(receipt);
            }
        }

        // 当前 active 归档，并在同一 SQLite 事务内更新 threads。
        require_unchanged_snapshot(&cur_abs, &current_snapshot, "当前 active rollout ")?;
        family::archive_with_integrity(&mut store, &codex, &family_id, &cur_active.id)?;
        require_unchanged_snapshot(&cur_abs, &current_snapshot, "当前 active rollout ")?;
        fs::create_dir_all(&archived_dir)?;
        journal.move_file(&cur_abs, &cur_archived_abs)?;
        require_unchanged_snapshot(
            &cur_archived_abs,
            &current_snapshot,
            "已归档当前 active rollout ",
        )?;
        inject_repair_fault("rollback_after_current_archive")?;
        mark_thread_archived(&transaction, &cur_active.id, &cur_archived_abs)?;
        let index_path = paths::session_index_path(&codex);
        journal.mutate_file(&index_path, || remove_index_line(&codex, &cur_active.id))?;

        // 目标分支从归档恢复；Scatter 分支本就在 sessions/ 时无需移动。
        if !target_already_active {
            require_unchanged_snapshot(
                &target_source_abs,
                &target_snapshot,
                "待恢复目标 rollout ",
            )?;
            if let Some(parent) = expected_abs.parent() {
                fs::create_dir_all(parent)?;
            }
            journal.move_file(&target_source_abs, &expected_abs)?;
            require_unchanged_snapshot(&expected_abs, &target_snapshot, "已恢复目标 rollout ")?;
        } else {
            require_unchanged_snapshot(&expected_abs, &target_snapshot, "目标 rollout ")?;
        }
        inject_repair_fault("rollback_after_target_restore")?;
        sync_thread_from_rollout(&codex, &transaction, &expected_abs)?;
        let index_path = paths::session_index_path(&codex);
        journal.mutate_file(&index_path, || {
            append_index_line(
                &codex,
                &target_branch_id,
                &target_brief.first_user_message,
                &expected_abs,
            )
        })?;
        inject_repair_fault("rollback_after_index")?;

        family::set_active(&mut store, &family_id, &target_branch_id)?;
        let family_path = paths::family_store_path(&codex);
        journal.mutate_file(&family_path, || family::save(&codex, &store))?;
        inject_repair_fault("rollback_after_family_save")?;
        Ok(())
    })();
    if let Err(error) = operation {
        return Err(rollback_transaction_with_compensation(
            transaction,
            journal,
            error,
        ));
    }
    commit_transaction_with_compensation(transaction, journal)?;
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

    // sessions 层按数据库、活动/归档 rollout、session_index 三处事实做完整清理与复核。
    let outcome = crate::sessions::delete_codex_artifacts(&codex, &branch_id)?;
    if outcome.structurally_removed {
        family::remove_non_active_branch(&mut store, &family_id, &branch_id)?;
        family::save(&codex, &store)?;
    }
    Ok(outcome.result)
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
                    let error = (relation == "diverged")
                        .then(|| describe_rollout_divergence(&active_lines, &branch_lines));
                    BranchSyncState {
                        branch_id: branch.id.clone(),
                        relation,
                        active_lines: Some(active_lines.len() as u64),
                        branch_lines: Some(branch_lines.len() as u64),
                        appendable_lines_to_active: to_active,
                        appendable_lines_to_branch: to_branch,
                        error,
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
    crate::codex_projects::ensure_desktop_not_running(&codex)?;
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
    let target_archived = target_abs.starts_with(paths::archived_sessions_dir(&codex));
    let source_lines = read_rollout_lines(&source_abs)?;
    let target_fingerprint = atomic_file::fingerprint(&target_abs)?;
    let target_lines = read_rollout_lines(&target_abs)?;

    validate_source_has_target_prefix(&source_lines, &target_lines)?;

    // 取过滤掉克隆痕迹行后的"可比较 body"；写入时也按这个口径来，
    // 避免把 source 里的 trace 又传染给 target。
    let source_body = comparable_body(&source_lines);
    let target_body = comparable_body(&target_lines);
    let extras: Vec<String> = source_body[target_body.len()..]
        .iter()
        .map(|line| {
            rewrite_session_meta_identity(
                line,
                &source_branch.id,
                &target_branch.id,
                &target_branch.provider,
                None,
            )
            .map(|rewritten| rewritten.unwrap_or_else(|| (*line).clone()))
        })
        .collect::<AppResult<_>>()?;
    let appended = u32::try_from(extras.len())
        .map_err(|_| AppError::Other("同步增量行数超过 u32 可表示范围".into()))?;
    let final_line_count = usize::from(!target_lines.is_empty()) + target_body.len() + extras.len();
    let final_line_count = u32::try_from(final_line_count)
        .map_err(|_| AppError::Other("同步后总行数超过 u32 可表示范围".into()))?;

    ensure_state_db_exists(&codex)?;
    let state = state_db::open(&codex)?;
    let transaction =
        rusqlite::Transaction::new_unchecked(&state, rusqlite::TransactionBehavior::Immediate)?;
    let mut journal = MutationJournal::default();
    let operation = (|| -> AppResult<()> {
        journal.mutate_file(&target_abs, || {
            atomic_file::replace_with_writer_if_unchanged(
                &target_abs,
                &target_fingerprint,
                |file| {
                    // 保留目标的 session_meta（首行），后续 body 一律按过滤口径重写
                    if let Some(first) = target_lines.first() {
                        writeln!(file, "{}", first)?;
                    }
                    for line in target_body.iter() {
                        if let Some(rewritten) = rewrite_session_meta_identity(
                            line,
                            &target_branch.id,
                            &target_branch.id,
                            &target_branch.provider,
                            None,
                        )? {
                            writeln!(file, "{rewritten}")?;
                        } else {
                            writeln!(file, "{}", line)?;
                        }
                    }
                    for line in extras.iter() {
                        writeln!(file, "{}", line)?;
                    }
                    Ok(())
                },
            )
        })?;

        if !upsert_thread_from_rollout(&codex, &transaction, &target_abs, target_archived)? {
            return Err(AppError::InvalidCodexDir(format!(
                "同步后的 rollout 缺少有效 session_meta.id: {}",
                target_abs.to_string_lossy()
            )));
        }
        if !target_archived {
            let brief = read_rollout_brief(&codex, &target_abs)?.ok_or_else(|| {
                AppError::InvalidCodexDir(format!(
                    "同步后的 rollout 缺少有效 session_meta.id: {}",
                    target_abs.to_string_lossy()
                ))
            })?;
            let thread_name = brief.first_user_message.clone();
            let index_path = paths::session_index_path(&codex);
            journal.mutate_file(&index_path, || {
                append_index_line(&codex, &target_branch.id, &thread_name, &target_abs)
            })?;
            if let Some(record) = project_assignment_record(&codex, &brief) {
                if let Some(receipt) = crate::codex_projects::sync_missing_thread_project_assignment_records_with_receipt(
                    &codex,
                    &[record],
                )? {
                    journal.register_project_state_receipt(receipt);
                }
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
        let family_path = paths::family_store_path(&codex);
        journal.mutate_file(&family_path, || family::save(&codex, &store))?;
        inject_repair_fault("branch_sync_after_family_save")?;
        Ok(())
    })();
    if let Err(error) = operation {
        return Err(rollback_transaction_with_compensation(
            transaction,
            journal,
            error,
        ));
    }
    commit_transaction_with_compensation(transaction, journal)?;

    Ok(BranchSyncReport {
        source_id: source_branch_id,
        target_id: target_branch_id,
        appended_lines: appended,
        total_lines: final_line_count,
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

fn describe_rollout_divergence(active_lines: &[String], branch_lines: &[String]) -> String {
    let active_body = comparable_body(active_lines);
    let branch_body = comparable_body(branch_lines);
    let common_lines = active_body
        .iter()
        .zip(branch_body.iter())
        .take_while(|(active, branch)| active == branch)
        .count();
    format!(
        "两份 rollout 在共同前缀 {common_lines} 行后均有不同记录，无法安全做前缀同步；模型切换本身不会创建会话分支"
    )
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

    #[test]
    fn opencode_parent_sources_are_subagent_sessions() {
        assert!(is_subagent_source(Some("parent:ses_parent")));
        assert!(is_subagent_source(Some("PARENT: ses_parent")));
        assert!(!is_subagent_source(Some("parent:")));
    }

    fn temp_codex_dir(name: &str) -> PathBuf {
        let unique = format!(
            "{}-{}-{}",
            name,
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        );
        std::env::temp_dir().join(unique)
    }

    fn write_global_state(codex: &Path, state: Value) -> AppResult<()> {
        fs::create_dir_all(codex)?;
        fs::write(
            paths::codex_global_state_json_path(codex),
            serde_json::to_vec(&state)?,
        )?;
        Ok(())
    }

    fn thread_project_assignment(codex: &Path, thread_id: &str) -> AppResult<Option<Value>> {
        let state: Value =
            serde_json::from_slice(&fs::read(paths::codex_global_state_json_path(codex))?)?;
        Ok(state
            .get("thread-project-assignments")
            .and_then(Value::as_object)
            .and_then(|assignments| assignments.get(thread_id))
            .cloned())
    }

    fn assert_thread_project_cwd(codex: &Path, thread_id: &str, cwd: &str) -> AppResult<()> {
        let assignment = thread_project_assignment(codex, thread_id)?
            .unwrap_or_else(|| panic!("missing project assignment for {thread_id}"));
        assert_eq!(assignment["projectKind"], "local");
        assert_eq!(assignment["cwd"], cwd);
        Ok(())
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

    #[test]
    fn rollout_brief_prefers_latest_turn_context_cwd_over_session_meta() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-latest-turn-cwd-test");
        let rollout_dir = codex.join("sessions").join("2026").join("04").join("22");
        fs::create_dir_all(&rollout_dir)?;
        let rollout = rollout_dir.join("rollout-latest-turn-cwd.jsonl");
        let lines = [
            serde_json::json!({
                "timestamp": "2026-04-22T00:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": "latest-turn-cwd",
                    "model_provider": DEFAULT_PROVIDER,
                    "cwd": r"F:\project\old"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-04-22T00:00:01Z",
                "type": "turn_context",
                "payload": {"cwd": r"F:\project\intermediate"}
            }),
            serde_json::json!({
                "timestamp": "2026-04-22T00:00:02Z",
                "type": "turn_context",
                "payload": {"cwd": r"F:\project\current"}
            }),
        ];
        fs::write(
            &rollout,
            format!(
                "{}\n",
                lines
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )?;

        let brief = read_rollout_brief(&codex, &rollout)?.expect("rollout brief");
        assert_eq!(brief.cwd.as_deref(), Some(r"F:\project\current"));

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn rebuild_threads_keeps_cwd_rewritten_by_move_instead_of_restoring_old_turn() -> AppResult<()>
    {
        let codex = temp_codex_dir("cc-session-manager-move-repair-cwd-test");
        let id = "move-then-repair-cwd";
        let rollout_dir = codex.join("sessions").join("2026").join("04").join("22");
        fs::create_dir_all(&rollout_dir)?;
        let rollout = rollout_dir.join(format!("rollout-{id}.jsonl"));
        let lines = [
            serde_json::json!({
                "timestamp": "2026-04-22T00:00:00Z",
                "type": "session_meta",
                "payload": {"id": id, "model_provider": DEFAULT_PROVIDER, "cwd": r"F:\old"}
            }),
            serde_json::json!({
                "timestamp": "2026-04-22T00:00:01Z",
                "type": "turn_context",
                "payload": {"cwd": r"F:\historical"}
            }),
            serde_json::json!({
                "timestamp": "2026-04-22T00:00:02Z",
                "type": "turn_context",
                "payload": {"cwd": r"F:\current-before-move"}
            }),
            serde_json::json!({
                "timestamp": "2026-04-22T00:00:03Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "repair me"}
            }),
        ];
        fs::write(
            &rollout,
            format!(
                "{}\n",
                lines
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )?;
        let state = create_full_state(&codex)?;
        drop(state);

        crate::codex_rollout_cwd::rewrite_effective_cwd(&rollout, id, r"F:\moved")?;
        let report = rebuild_threads_table(codex.to_string_lossy().into_owned(), false)?;
        assert_eq!(report.upserted, 1);

        let state = state_db::open_ro(&codex)?;
        let cwd: String = state.query_row("SELECT cwd FROM threads WHERE id = ?", [id], |row| {
            row.get(0)
        })?;
        assert_eq!(cwd, r"F:\moved");
        let turns = fs::read_to_string(&rollout)?
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value["type"] == "turn_context")
            .collect::<Vec<_>>();
        assert_eq!(turns[0]["payload"]["cwd"], r"F:\historical");
        assert_eq!(turns[1]["payload"]["cwd"], r"F:\moved");

        drop(state);
        fs::remove_dir_all(&codex).ok();
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
                rollout_path TEXT,
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
                } else if *name == "archived" {
                    "archived INTEGER".to_string()
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

    fn prepare_provider_switch_fixture(codex: &Path, id: &str) -> AppResult<PathBuf> {
        let source_rel = format!("sessions/2026/04/24/rollout-{id}.jsonl");
        let source = codex.join(&source_rel);
        write_sync_rollout(&source, id, "custom", &[])?;
        create_full_state(codex)?;
        {
            let state = state_db::open(codex)?;
            sync_thread_from_rollout(codex, &state, &source)?;
        }
        write_index_line(codex, id)?;
        write_global_state(codex, serde_json::json!({"local-projects": {}}))?;
        Ok(source)
    }

    fn write_sync_rollout(path: &Path, id: &str, provider: &str, body: &[String]) -> AppResult<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let meta = serde_json::json!({
            "timestamp": "2026-04-24T00:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "model_provider": provider,
                "cwd": "F:\\project\\example",
                "source": DEFAULT_THREAD_SOURCE
            }
        })
        .to_string();
        let mut lines = vec![meta];
        lines.extend(body.iter().cloned());
        fs::write(path, format!("{}\n", lines.join("\n")))?;
        Ok(())
    }

    fn save_two_branch_family(
        codex: &Path,
        source_id: &str,
        source_provider: &str,
        source_relpath: &str,
        target_id: &str,
        target_provider: &str,
        target_relpath: &str,
    ) -> AppResult<()> {
        let family = Family {
            family_id: source_id.to_string(),
            root_id: source_id.to_string(),
            title: "sync family".to_string(),
            chain: vec![
                FamilyBranch {
                    id: source_id.to_string(),
                    provider: source_provider.to_string(),
                    created_at: "2026-04-24T00:00:00Z".to_string(),
                    status: BranchStatus::Active,
                    rollout_relpath: source_relpath.to_string(),
                    sha256: None,
                    line_count: None,
                    note: None,
                    archive_origin: None,
                },
                FamilyBranch {
                    id: target_id.to_string(),
                    provider: target_provider.to_string(),
                    created_at: "2026-04-24T00:00:00Z".to_string(),
                    status: BranchStatus::Archived,
                    rollout_relpath: target_relpath.to_string(),
                    sha256: None,
                    line_count: None,
                    note: None,
                    archive_origin: None,
                },
            ],
            active_id: source_id.to_string(),
            updated_at: "2026-04-24T00:00:00Z".to_string(),
        };
        let mut families = BTreeMap::new();
        families.insert(source_id.to_string(), family);
        let mut index = BTreeMap::new();
        index.insert(source_id.to_string(), source_id.to_string());
        index.insert(target_id.to_string(), source_id.to_string());
        family::save(
            codex,
            &FamilyStore {
                version: 1,
                families,
                index,
            },
        )
    }

    struct RollbackFixture {
        source_id: String,
        target_id: String,
        source_path: PathBuf,
        source_archived_path: PathBuf,
        target_active_path: PathBuf,
        target_archived_path: PathBuf,
    }

    fn prepare_rollback_fixture(codex: &Path, suffix: &str) -> AppResult<RollbackFixture> {
        let source_id = format!("rollback-source-{suffix}");
        let target_id = format!("rollback-target-{suffix}");
        let source_rel = format!("sessions/2026/04/24/rollout-{source_id}.jsonl");
        let target_rel = format!("sessions/2026/04/24/rollout-{target_id}.jsonl");
        let source_path = codex.join(&source_rel);
        let target_active_path = codex.join(&target_rel);
        let source_archived_path =
            paths::archived_sessions_dir(codex).join(format!("rollout-{source_id}.jsonl"));
        let target_archived_path =
            paths::archived_sessions_dir(codex).join(format!("rollout-{target_id}.jsonl"));

        write_sync_rollout(&source_path, &source_id, "custom", &[])?;
        write_sync_rollout(&target_archived_path, &target_id, DEFAULT_PROVIDER, &[])?;
        create_full_state(codex)?;
        {
            let state = state_db::open(codex)?;
            sync_thread_from_rollout(codex, &state, &source_path)?;
            assert!(upsert_thread_from_rollout(
                codex,
                &state,
                &target_archived_path,
                true,
            )?);
        }
        write_index_line(codex, &source_id)?;
        write_global_state(codex, serde_json::json!({"local-projects": {}}))?;
        save_two_branch_family(
            codex,
            &source_id,
            "custom",
            &source_rel,
            &target_id,
            DEFAULT_PROVIDER,
            &target_rel,
        )?;
        Ok(RollbackFixture {
            source_id,
            target_id,
            source_path,
            source_archived_path,
            target_active_path,
            target_archived_path,
        })
    }

    fn read_thread_location(codex: &Path, id: &str) -> AppResult<(String, i64)> {
        let state = state_db::open_ro(codex)?;
        Ok(state.query_row(
            "SELECT rollout_path, CAST(archived AS INTEGER) FROM threads WHERE id = ?",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    #[test]
    fn duplicate_session_keeps_source_and_registers_independent_copy() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-duplicate-session-test");
        let source_id = "duplicate-session-source";
        let source = write_conversation_rollout(&codex, source_id)?;
        let source_before = fs::read(&source)?;
        let state = create_full_state(&codex)?;
        state.execute("ALTER TABLE threads ADD COLUMN name TEXT", [])?;
        sync_thread_from_rollout(&codex, &state, &source)?;
        state.execute(
            "UPDATE threads SET title = 'Pinned source title', name = 'Pinned source name' WHERE id = ?",
            [source_id],
        )?;
        drop(state);
        write_index_line(&codex, source_id)?;
        write_global_state(&codex, serde_json::json!({"local-projects": {}}))?;
        crate::provenance::record_conversion(
            &codex,
            "codex",
            source_id,
            "claude",
            "claude-source-session",
            Some("native"),
        )?;

        let report = duplicate_session_with_lock(
            codex.to_string_lossy().into_owned(),
            source_id.to_string(),
            source.to_string_lossy().into_owned(),
            &family::FamilyLock::default(),
        )?;

        assert_eq!(report.source_id, source_id);
        assert_ne!(report.new_id, source_id);
        assert!(source.is_file());
        assert_eq!(fs::read(&source)?, source_before);
        let duplicated = PathBuf::from(&report.new_rollout_path);
        assert!(duplicated.is_file());
        let duplicated_brief = read_rollout_brief(&codex, &duplicated)?
            .expect("duplicated rollout must contain session metadata");
        assert_eq!(duplicated_brief.id, report.new_id);
        assert_eq!(
            BufReader::new(fs::File::open(&duplicated)?)
                .lines()
                .collect::<Result<Vec<_>, _>>()?
                .len() as u64,
            report.total_lines
        );

        let state = state_db::open_ro(&codex)?;
        let (rollout_path, title, name, archived): (String, String, String, i64) = state
            .query_row(
            "SELECT rollout_path, title, name, CAST(archived AS INTEGER) FROM threads WHERE id = ?",
            [&report.new_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(PathBuf::from(rollout_path), duplicated);
        assert_eq!(title, "Pinned source title");
        assert_eq!(name, "Pinned source name");
        assert_eq!(archived, 0);
        let thread_count: i64 =
            state.query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))?;
        assert_eq!(thread_count, 2);
        drop(state);

        let index_ids = read_session_index_ids(&codex)?;
        assert!(index_ids.contains(source_id));
        assert!(index_ids.contains(&report.new_id));

        let provenance: Value =
            serde_json::from_slice(&fs::read(paths::session_provenance_path(&codex))?)?;
        let copied_origin = &provenance["sessions"][format!("codex:{}", report.new_id)];
        assert_eq!(copied_origin["source_provider"], "claude");
        assert_eq!(copied_origin["source_id"], "claude-source-session");
        assert_eq!(copied_origin["conversion_mode"], "native");
        assert_thread_project_cwd(&codex, &report.new_id, r"F:\project\example")?;

        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn duplicate_session_rolls_back_file_thread_and_index_after_late_failure() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-duplicate-rollback-test");
        let source_id = "duplicate-rollback-source";
        let source = write_conversation_rollout(&codex, source_id)?;
        let source_before = fs::read(&source)?;
        let state = create_full_state(&codex)?;
        sync_thread_from_rollout(&codex, &state, &source)?;
        drop(state);
        write_index_line(&codex, source_id)?;
        crate::provenance::record_conversion(
            &codex,
            "codex",
            source_id,
            "claude",
            "rollback-source-session",
            Some("native"),
        )?;
        let index_before = fs::read(paths::session_index_path(&codex))?;
        let provenance_before = fs::read(paths::session_provenance_path(&codex))?;
        let global_state = paths::codex_global_state_json_path(&codex);
        fs::write(&global_state, "{broken json")?;
        let global_before = fs::read(&global_state)?;

        let error = duplicate_session_with_lock(
            codex.to_string_lossy().into_owned(),
            source_id.to_string(),
            source.to_string_lossy().into_owned(),
            &family::FamilyLock::default(),
        )
        .expect_err("invalid global state must abort a full duplicate");

        assert!(error.to_string().contains("全局状态 JSON 损坏"), "{error}");
        assert_eq!(fs::read(&source)?, source_before);
        assert_eq!(family::scan_rollouts(&codex)?, vec![source.clone()]);
        assert_eq!(fs::read(paths::session_index_path(&codex))?, index_before);
        assert_eq!(
            fs::read(paths::session_provenance_path(&codex))?,
            provenance_before
        );
        assert_eq!(fs::read(&global_state)?, global_before);
        let state = state_db::open_ro(&codex)?;
        let thread_ids = state
            .prepare("SELECT id FROM threads ORDER BY id")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(thread_ids, vec![source_id.to_string()]);
        drop(state);

        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn duplicate_session_rejects_rollout_outside_codex_roots() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-duplicate-outside-root-test");
        fs::create_dir_all(paths::sessions_dir(&codex))?;
        let source_id = "duplicate-outside-root-source";
        let external_root = temp_codex_dir("cc-session-manager-external-rollout-test");
        let external = external_root.join(format!("rollout-{source_id}.jsonl"));
        write_sync_rollout(&external, source_id, DEFAULT_PROVIDER, &[])?;
        let external_before = fs::read(&external)?;

        let error = duplicate_session_with_lock(
            codex.to_string_lossy().into_owned(),
            source_id.to_string(),
            external.to_string_lossy().into_owned(),
            &family::FamilyLock::default(),
        )
        .expect_err("an external rollout must not be duplicated");

        assert!(error.to_string().contains("不在 sessions"), "{error}");
        assert_eq!(fs::read(&external)?, external_before);
        assert!(family::scan_rollouts(&codex)?.is_empty());
        assert!(!paths::state_db_path(&codex).exists());

        fs::remove_dir_all(codex).ok();
        fs::remove_dir_all(external_root).ok();
        Ok(())
    }

    #[test]
    fn provider_config_defaults_only_for_missing_or_valid_omission() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-provider-config-test");
        fs::create_dir_all(&codex)?;

        assert_eq!(effective_current_provider(&codex)?, DEFAULT_PROVIDER);

        fs::write(paths::config_toml_path(&codex), "")?;
        assert_eq!(effective_current_provider(&codex)?, DEFAULT_PROVIDER);

        fs::write(
            paths::config_toml_path(&codex),
            "[model_providers.custom]\nname = \"Custom\"\n",
        )?;
        assert_eq!(effective_current_provider(&codex)?, DEFAULT_PROVIDER);

        fs::write(
            paths::config_toml_path(&codex),
            "model_provider = \"  custom  \"\n",
        )?;
        assert_eq!(effective_current_provider(&codex)?, "custom");

        for invalid in [
            "model_provider = \"   \"\n",
            "model_provider = 42\n",
            "model_provider = [\n",
        ] {
            fs::write(paths::config_toml_path(&codex), invalid)?;
            assert!(effective_current_provider(&codex).is_err(), "{invalid}");
            assert!(get_provider_info(codex.to_string_lossy().into_owned()).is_err());
            assert!(clone_session_for_provider_locked(
                codex.to_string_lossy().into_owned(),
                "any-session".to_string(),
                Some("custom".to_string()),
                SwitchStrategy::Scatter,
                true,
            )
            .is_err());
        }

        fs::remove_file(paths::config_toml_path(&codex))?;
        fs::create_dir(paths::config_toml_path(&codex))?;
        assert!(effective_current_provider(&codex).is_err());

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn cloned_rollout_never_has_updated_at_before_created_at() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-clone-time-test");
        let source = write_conversation_rollout(&codex, "clone-time-source")?;
        let target = codex
            .join("sessions")
            .join("2026")
            .join("07")
            .join("10")
            .join("rollout-clone-time-target.jsonl");

        write_cloned_rollout(
            &source,
            &target,
            "clone-time-target",
            "custom",
            "clone-time-source",
        )?;
        let brief = read_rollout_brief(&codex, &target)?.expect("cloned rollout brief");

        assert!(brief.created_at_ms > 0);
        assert!(brief.updated_at_ms >= brief.created_at_ms);

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn cloned_rollout_rewrites_only_the_source_session_meta_identity() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-clone-repeated-meta-test");
        let source = codex.join("sessions/source.jsonl");
        let target = codex.join("sessions/target.jsonl");
        let ancestor_meta = serde_json::json!({
            "timestamp": "2026-04-24T00:30:00Z",
            "type": "session_meta",
            "payload": {
                "id": "ancestor",
                "session_id": "ancestor",
                "model_provider": "ancestor-provider",
                "cwd": "F:\\project\\ancestor",
                "source": DEFAULT_THREAD_SOURCE
            }
        })
        .to_string();
        let repeated_meta = serde_json::json!({
            "timestamp": "2026-04-24T01:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": "source",
                "session_id": "source",
                "timestamp": "2026-04-24T01:00:00Z",
                "model_provider": DEFAULT_PROVIDER,
                "cwd": "F:\\project\\example",
                "source": DEFAULT_THREAD_SOURCE
            }
        })
        .to_string();
        write_sync_rollout(
            &source,
            "source",
            DEFAULT_PROVIDER,
            &[ancestor_meta, repeated_meta],
        )?;

        write_cloned_rollout(&source, &target, "target", "custom", "source")?;

        let session_meta = read_rollout_lines(&target)?
            .into_iter()
            .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
            .filter(|value| value.get("type").and_then(Value::as_str) == Some("session_meta"))
            .collect::<Vec<_>>();
        assert_eq!(session_meta.len(), 3);
        assert_eq!(session_meta[0]["payload"]["id"], "target");
        assert_eq!(session_meta[0]["payload"]["session_id"], "target");
        assert_eq!(session_meta[0]["payload"]["model_provider"], "custom");
        assert_eq!(session_meta[1]["payload"]["id"], "ancestor");
        assert_eq!(session_meta[1]["payload"]["session_id"], "ancestor");
        assert_eq!(
            session_meta[1]["payload"]["model_provider"],
            "ancestor-provider"
        );
        assert_eq!(session_meta[2]["payload"]["id"], "target");
        assert_eq!(session_meta[2]["payload"]["session_id"], "target");
        assert_eq!(session_meta[2]["payload"]["model_provider"], "custom");
        assert_eq!(
            session_meta[2]["timestamp"],
            Value::String("2026-04-24T01:00:00Z".into())
        );

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn duplicated_rollout_creates_a_root_meta_and_interrupts_an_unfinished_turn() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-independent-duplicate-test");
        let source = codex.join("sessions/source.jsonl");
        let target = codex.join("sessions/target.jsonl");
        fs::create_dir_all(source.parent().expect("source parent"))?;
        let dynamic_tools = serde_json::json!([{
            "name": "lookup",
            "description": "keep this dynamic tool description",
            "input_schema": {"type": "object"}
        }]);
        let lines = [
            serde_json::json!({
                "timestamp": "2026-04-24T00:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": "source",
                    "session_id": "source",
                    "parent_thread_id": "old-parent",
                    "model_provider": DEFAULT_PROVIDER,
                    "cwd": "F:\\project\\example",
                    "source": {"subagent": {"thread_spawn": {"parent_thread_id": "old-parent"}}},
                    "thread_source": "subagent",
                    "agent_nickname": "Worker",
                    "agent_role": "reviewer",
                    "agent_path": "/root/reviewer",
                    "dynamic_tools": dynamic_tools,
                    "history_mode": "legacy"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-04-24T00:00:01Z",
                "type": "session_meta",
                "payload": {
                    "id": "ancestor",
                    "session_id": "ancestor",
                    "model_provider": "ancestor-provider",
                    "cwd": "F:\\project\\ancestor",
                    "source": "vscode"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-04-24T00:00:02Z",
                "type": "session_meta",
                "payload": {
                    "id": "source",
                    "session_id": "source",
                    "model_provider": DEFAULT_PROVIDER,
                    "cwd": "F:\\project\\example",
                    "source": "vscode"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-04-24T00:00:03Z",
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "active-turn"}
            }),
            serde_json::json!({
                "timestamp": "2026-04-24T00:00:04Z",
                "type": "response_item",
                "payload": {"type": "custom_tool_call", "call_id": "call-1", "name": "lookup", "input": "{}"}
            }),
        ];
        fs::write(
            &source,
            format!(
                "{}\n",
                lines
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )?;

        write_duplicated_rollout(&source, &target, "target", DEFAULT_PROVIDER, "source")?;

        let values = read_rollout_lines(&target)?
            .into_iter()
            .map(|line| serde_json::from_str::<Value>(&line))
            .collect::<Result<Vec<_>, _>>()?;
        let canonical = &values[0];
        assert_eq!(canonical["payload"]["id"], "target");
        assert_eq!(canonical["payload"]["session_id"], "target");
        assert_eq!(canonical["payload"]["forked_from_id"], "source");
        assert_eq!(canonical["payload"]["source"], DEFAULT_THREAD_SOURCE);
        assert_eq!(canonical["payload"]["thread_source"], "user");
        assert_eq!(canonical["payload"]["history_mode"], "legacy");
        assert_eq!(canonical["payload"]["dynamic_tools"], dynamic_tools);
        for removed in [
            "parent_thread_id",
            "agent_nickname",
            "agent_role",
            "agent_path",
        ] {
            assert!(canonical["payload"].get(removed).is_none(), "{removed}");
        }

        let copied_meta = values
            .iter()
            .skip(1)
            .filter(|value| value["type"] == "session_meta")
            .collect::<Vec<_>>();
        assert_eq!(copied_meta.len(), 2);
        assert_eq!(copied_meta[0]["payload"]["id"], "ancestor");
        assert_eq!(
            copied_meta[0]["payload"]["model_provider"],
            "ancestor-provider"
        );
        assert_eq!(copied_meta[1]["payload"]["id"], "source");

        let marker = &values[values.len() - 2];
        assert_eq!(marker["type"], "response_item");
        assert_eq!(marker["payload"]["type"], "message");
        assert_eq!(marker["payload"]["role"], "user");
        assert!(marker["payload"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("<turn_aborted>")));
        let aborted = &values[values.len() - 1];
        assert_eq!(aborted["type"], "event_msg");
        assert_eq!(aborted["payload"]["type"], "turn_aborted");
        assert_eq!(aborted["payload"]["turn_id"], "active-turn");
        assert_eq!(aborted["payload"]["reason"], "interrupted");

        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn duplicated_rollout_interrupts_legacy_history_without_lifecycle_events() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-legacy-duplicate-boundary-test");
        let source = codex.join("sessions/source.jsonl");
        let target = codex.join("sessions/target.jsonl");
        fs::create_dir_all(source.parent().expect("source parent"))?;
        let lines = [
            serde_json::json!({
                "timestamp": "2026-04-24T00:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": "source",
                    "session_id": "source",
                    "model_provider": DEFAULT_PROVIDER,
                    "cwd": "F:\\project\\example",
                    "source": DEFAULT_THREAD_SOURCE,
                    "history_mode": "legacy"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-04-24T00:00:01Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "continue"}]
                }
            }),
            serde_json::json!({
                "timestamp": "2026-04-24T00:00:02Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "partial"}]
                }
            }),
        ];
        fs::write(
            &source,
            format!(
                "{}\n",
                lines
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )?;

        write_duplicated_rollout(&source, &target, "target", DEFAULT_PROVIDER, "source")?;

        let values = read_rollout_lines(&target)?
            .into_iter()
            .map(|line| serde_json::from_str::<Value>(&line))
            .collect::<Result<Vec<_>, _>>()?;
        assert!(values[values.len() - 2]["payload"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("<turn_aborted>")));
        assert_eq!(values[values.len() - 1]["payload"]["type"], "turn_aborted");
        assert!(values[values.len() - 1]["payload"]["turn_id"].is_null());

        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn duplicated_rollout_rejects_paginated_history() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-paginated-duplicate-test");
        let source = codex.join("sessions/source.jsonl");
        let target = codex.join("sessions/target.jsonl");
        fs::create_dir_all(source.parent().expect("source parent"))?;
        fs::write(
            &source,
            format!(
                "{}\n",
                serde_json::json!({
                    "timestamp": "2026-04-24T00:00:00Z",
                    "type": "session_meta",
                    "payload": {
                        "id": "source",
                        "session_id": "source",
                        "model_provider": DEFAULT_PROVIDER,
                        "cwd": "F:\\project\\example",
                        "source": DEFAULT_THREAD_SOURCE,
                        "history_mode": "paginated"
                    }
                })
            ),
        )?;

        let error =
            write_duplicated_rollout(&source, &target, "target", DEFAULT_PROVIDER, "source")
                .expect_err("paginated history must not be copied as a standalone JSONL");

        assert!(error.to_string().contains("paginated"), "{error}");
        assert!(error.to_string().contains("Codex 官方"), "{error}");
        assert!(!target.exists());
        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn provider_clone_rejects_paginated_history() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-paginated-provider-clone-test");
        let source = codex.join("sessions/source.jsonl");
        let target = codex.join("sessions/target.jsonl");
        fs::create_dir_all(source.parent().expect("source parent"))?;
        fs::write(
            &source,
            format!(
                "{}\n",
                serde_json::json!({
                    "timestamp": "2026-04-24T00:00:00Z",
                    "type": "session_meta",
                    "payload": {
                        "id": "source",
                        "session_id": "source",
                        "model_provider": DEFAULT_PROVIDER,
                        "cwd": "F:\\project\\example",
                        "source": DEFAULT_THREAD_SOURCE,
                        "history_mode": "paginated"
                    }
                })
            ),
        )?;

        let error = write_cloned_rollout(&source, &target, "target", "custom", "source")
            .expect_err("paginated provider clone must use the official thread store");

        assert!(error.to_string().contains("paginated"), "{error}");
        assert!(!target.exists());
        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn cloned_rollout_requires_session_meta_and_leaves_no_destination() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-clone-invalid-source-test");
        let source = codex.join("sessions/source.jsonl");
        let target = codex.join("sessions/target.jsonl");
        fs::create_dir_all(source.parent().expect("source parent"))?;
        fs::write(
            &source,
            "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\"}}\n",
        )?;

        let error = write_cloned_rollout(&source, &target, "target", "custom", "source")
            .expect_err("a clone without session_meta must be rejected");

        assert!(error.to_string().contains("session_meta"));
        assert!(!target.exists());
        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn provider_switch_strategies_compensate_after_family_save_failure() -> AppResult<()> {
        let cases = [
            (SwitchStrategy::Follow, "follow_after_family_save", "follow"),
            (
                SwitchStrategy::Scatter,
                "clone_after_family_save",
                "scatter",
            ),
            (
                SwitchStrategy::Continuous,
                "clone_after_family_save",
                "continuous",
            ),
        ];

        for (strategy, fault_stage, label) in cases {
            let codex = temp_codex_dir(&format!(
                "cc-session-manager-provider-compensation-{label}-test"
            ));
            let session_id = format!("provider-compensation-{label}");
            let source = prepare_provider_switch_fixture(&codex, &session_id)?;
            let source_before = fs::read(&source)?;
            let index_before = fs::read(paths::session_index_path(&codex))?;
            let global_before = fs::read(paths::codex_global_state_json_path(&codex))?;
            assert!(!paths::family_store_path(&codex).exists());

            let _fault = RepairTestFaultGuard::error(fault_stage);
            let error = clone_session_for_provider_locked(
                codex.to_string_lossy().into_owned(),
                session_id.clone(),
                Some(DEFAULT_PROVIDER.to_string()),
                strategy,
                false,
            )
            .expect_err("fault after family save must abort provider switch");
            assert!(
                error.to_string().contains("测试故障注入"),
                "{label}: {error}"
            );

            assert_eq!(fs::read(&source)?, source_before, "{label}");
            assert_eq!(
                fs::read(paths::session_index_path(&codex))?,
                index_before,
                "{label}"
            );
            assert!(
                !paths::family_store_path(&codex).exists(),
                "{label}: family store must return to its originally absent state"
            );
            assert_eq!(family::scan_rollouts(&codex)?.len(), 1, "{label}");
            let state = state_db::open_ro(&codex)?;
            let (count, provider, archived): (i64, String, i64) = state.query_row(
                "SELECT COUNT(*), MIN(model_provider), MIN(CAST(archived AS INTEGER)) FROM threads",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            assert_eq!(count, 1, "{label}");
            assert_eq!(provider, "custom", "{label}");
            assert_eq!(archived, 0, "{label}");
            assert_eq!(
                fs::read(paths::codex_global_state_json_path(&codex))?,
                global_before,
                "{label}"
            );

            fs::remove_dir_all(&codex).ok();
        }
        Ok(())
    }

    #[test]
    fn provider_visibility_repair_compensates_all_state_after_project_sync() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-provider-visibility-compensation-test");
        let session_id = "provider-visibility-compensation";
        let source = prepare_provider_switch_fixture(&codex, session_id)?;
        {
            let state = state_db::open(&codex)?;
            state.execute(
                "UPDATE threads SET rollout_path = 'stale', model_provider = 'stale', \
                 source = 'subagent', archived = 1 WHERE id = ?",
                [session_id],
            )?;
        }
        let source_before = fs::read(&source)?;
        let index_before = fs::read(paths::session_index_path(&codex))?;
        let global_before = fs::read(paths::codex_global_state_json_path(&codex))?;
        let thread_before = read_thread_state_map(&codex)?
            .remove(session_id)
            .expect("fixture thread state");
        assert!(!paths::family_store_path(&codex).exists());

        let _fault = RepairTestFaultGuard::error("provider_visibility_after_family_save");
        let error = clone_session_for_provider_locked(
            codex.to_string_lossy().into_owned(),
            session_id.to_string(),
            Some("custom".to_string()),
            SwitchStrategy::Continuous,
            false,
        )
        .expect_err("fault after project and family sync must roll back visibility repair");

        assert!(error.to_string().contains("测试故障注入"), "{error}");
        assert_eq!(fs::read(&source)?, source_before);
        assert_eq!(fs::read(paths::session_index_path(&codex))?, index_before);
        assert_eq!(
            fs::read(paths::codex_global_state_json_path(&codex))?,
            global_before
        );
        assert!(!paths::family_store_path(&codex).exists());
        let thread_after = read_thread_state_map(&codex)?
            .remove(session_id)
            .expect("restored fixture thread state");
        assert_eq!(thread_after.rollout_path, thread_before.rollout_path);
        assert_eq!(thread_after.model_provider, thread_before.model_provider);
        assert_eq!(thread_after.source, thread_before.source);
        assert_eq!(thread_after.archived, thread_before.archived);

        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn provider_switch_strategies_commit_consistent_state_and_rollback() -> AppResult<()> {
        for (strategy, label) in [
            (SwitchStrategy::Follow, "follow"),
            (SwitchStrategy::Scatter, "scatter"),
            (SwitchStrategy::Continuous, "continuous"),
        ] {
            let codex = temp_codex_dir(&format!("cc-session-manager-provider-commit-{label}-test"));
            let source_id = format!("provider-commit-{label}");
            let source = prepare_provider_switch_fixture(&codex, &source_id)?;

            let report = clone_session_for_provider_locked(
                codex.to_string_lossy().into_owned(),
                source_id.clone(),
                Some(DEFAULT_PROVIDER.to_string()),
                strategy,
                false,
            )?;
            assert!(report.ok, "{label}: {:?}", report.error);
            let active_id = report
                .new_id
                .clone()
                .expect("successful provider target id");
            assert_thread_project_cwd(&codex, &active_id, r"F:\project\example")?;
            let store = family::load(&codex)?;
            let family_id = store.index.get(&source_id).expect("source family id");
            let family = store.families.get(family_id).expect("source family");
            assert_eq!(family.active_id, active_id, "{label}");
            let index_ids = read_session_index_ids(&codex)?;

            match label {
                "follow" => {
                    assert_eq!(active_id, source_id);
                    assert_eq!(family.chain.len(), 1);
                    assert_eq!(
                        read_rollout_brief(&codex, &source)?.and_then(|brief| brief.model_provider),
                        Some(DEFAULT_PROVIDER.to_string())
                    );
                    assert_eq!(read_thread_location(&codex, &source_id)?.1, 0);
                    assert!(index_ids.contains(&source_id));
                }
                "scatter" => {
                    assert_ne!(active_id, source_id);
                    assert_eq!(family.chain.len(), 2);
                    assert!(source.is_file());
                    assert!(PathBuf::from(report.new_rollout_path.as_deref().unwrap()).is_file());
                    assert_eq!(read_thread_location(&codex, &source_id)?.1, 0);
                    assert_eq!(read_thread_location(&codex, &active_id)?.1, 0);
                    assert!(index_ids.contains(&source_id));
                    assert!(index_ids.contains(&active_id));
                }
                "continuous" => {
                    assert_ne!(active_id, source_id);
                    assert_eq!(family.chain.len(), 2);
                    let archived_source = paths::archived_sessions_dir(&codex)
                        .join(source.file_name().expect("source filename"));
                    assert!(!source.exists());
                    assert!(archived_source.is_file());
                    assert!(PathBuf::from(report.new_rollout_path.as_deref().unwrap()).is_file());
                    assert_eq!(read_thread_location(&codex, &source_id)?.1, 1);
                    assert_eq!(read_thread_location(&codex, &active_id)?.1, 0);
                    assert!(!index_ids.contains(&source_id));
                    assert!(index_ids.contains(&active_id));
                    let archived_branch = family
                        .chain
                        .iter()
                        .find(|branch| branch.id == source_id)
                        .expect("continuous archived source branch");
                    assert_eq!(
                        archived_branch.archive_origin,
                        Some(ArchiveOrigin::ProviderSync)
                    );

                    rollback_family_active_locked(
                        codex.to_string_lossy().into_owned(),
                        family_id.clone(),
                        source_id.clone(),
                    )?;
                    let restored_store = family::load(&codex)?;
                    let restored_family = restored_store
                        .families
                        .get(family_id)
                        .expect("restored family");
                    assert_eq!(restored_family.active_id, source_id);
                    assert_eq!(
                        restored_family
                            .chain
                            .iter()
                            .find(|branch| branch.id == source_id)
                            .and_then(|branch| branch.archive_origin.as_ref()),
                        None
                    );
                    assert!(source.is_file());
                    let archived_new = paths::archived_sessions_dir(&codex).join(
                        PathBuf::from(report.new_rollout_path.as_deref().unwrap())
                            .file_name()
                            .expect("new rollout filename"),
                    );
                    assert!(archived_new.is_file());
                    assert_eq!(read_thread_location(&codex, &source_id)?.1, 0);
                    assert_eq!(read_thread_location(&codex, &active_id)?.1, 1);
                    let restored_index = read_session_index_ids(&codex)?;
                    assert!(restored_index.contains(&source_id));
                    assert!(!restored_index.contains(&active_id));
                    assert_thread_project_cwd(&codex, &source_id, r"F:\project\example")?;
                }
                _ => unreachable!(),
            }

            fs::remove_dir_all(&codex).ok();
        }
        Ok(())
    }

    #[test]
    fn continuous_switch_rechecks_source_snapshot_before_archive() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-provider-source-race-test");
        let session_id = "provider-source-race";
        let source = prepare_provider_switch_fixture(&codex, session_id)?;
        let index_before = fs::read(paths::session_index_path(&codex))?;

        let _fault = RepairTestFaultGuard::append("clone_after_new_rollout", source.clone());
        let error = clone_session_for_provider_locked(
            codex.to_string_lossy().into_owned(),
            session_id.to_string(),
            Some(DEFAULT_PROVIDER.to_string()),
            SwitchStrategy::Continuous,
            false,
        )
        .expect_err("a changed source snapshot must abort before archive");

        assert!(error.to_string().contains("发生变化"), "{error}");
        assert!(source.is_file());
        assert!(fs::read_to_string(&source)?.contains("test_append"));
        assert_eq!(family::scan_rollouts(&codex)?.len(), 1);
        assert_eq!(fs::read(paths::session_index_path(&codex))?, index_before);
        assert!(!paths::family_store_path(&codex).exists());
        let state = state_db::open_ro(&codex)?;
        let count: i64 = state.query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))?;
        assert_eq!(count, 1);

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn model_switch_on_one_side_is_ahead_not_diverged() {
        let active = vec![
            "active-meta".to_string(),
            "shared-message".to_string(),
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "thread_settings_applied",
                    "thread_settings": {"model": "gpt-next"}
                }
            })
            .to_string(),
            "new-turn".to_string(),
        ];
        let history = vec!["history-meta".to_string(), "shared-message".to_string()];

        assert_eq!(
            compare_rollout_lines(&active, &history),
            ("active_ahead".to_string(), 0, 2)
        );
    }

    #[test]
    fn different_turns_after_model_switch_remain_diverged() {
        let active = vec![
            "active-meta".to_string(),
            "shared-message".to_string(),
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "thread_settings_applied",
                    "thread_settings": {"model": "gpt-next"}
                }
            })
            .to_string(),
            "active-user-turn".to_string(),
        ];
        let history = vec![
            "history-meta".to_string(),
            "shared-message".to_string(),
            "history-user-turn".to_string(),
        ];

        assert_eq!(
            compare_rollout_lines(&active, &history),
            ("diverged".to_string(), 0, 0)
        );
        assert!(describe_rollout_divergence(&active, &history).contains("共同前缀 1 行"));
    }

    #[test]
    fn sync_updates_non_active_main_target_and_reports_written_lines() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-sync-main-target-test");
        let source_id = "sync-main-source";
        let target_id = "sync-main-target";
        let source_rel = "sessions/2026/04/24/rollout-sync-main-source.jsonl";
        let target_rel = "sessions/2026/04/24/rollout-sync-main-target.jsonl";
        let source_path = codex.join(source_rel);
        let target_path = codex.join(target_rel);
        let clone_trace = serde_json::json!({
            "type": "event_msg",
            "payload": {"type": "session_cloned"}
        })
        .to_string();
        let common = serde_json::json!({
            "timestamp": "2026-04-24T00:00:01Z",
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "hello"}
        })
        .to_string();
        let repeated_source_meta = serde_json::json!({
            "timestamp": "2026-04-24T00:00:01.500Z",
            "type": "session_meta",
            "payload": {
                "id": source_id,
                "model_provider": "custom",
                "cwd": "F:\\project\\example",
                "source": DEFAULT_THREAD_SOURCE
            }
        })
        .to_string();
        let extra = serde_json::json!({
            "timestamp": "2026-04-24T00:00:02Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {"total_token_usage": {"total_tokens": 77}}
            }
        })
        .to_string();
        write_sync_rollout(
            &source_path,
            source_id,
            "custom",
            &[clone_trace, common.clone(), repeated_source_meta, extra],
        )?;
        write_sync_rollout(&target_path, target_id, DEFAULT_PROVIDER, &[common])?;
        create_full_state(&codex)?;
        {
            let state = state_db::open(&codex)?;
            sync_thread_from_rollout(&codex, &state, &source_path)?;
            sync_thread_from_rollout(&codex, &state, &target_path)?;
        }
        save_two_branch_family(
            &codex,
            source_id,
            "custom",
            source_rel,
            target_id,
            DEFAULT_PROVIDER,
            target_rel,
        )?;
        write_global_state(&codex, serde_json::json!({"local-projects": {}}))?;

        let report = append_branch_extras_locked(
            codex.to_string_lossy().into_owned(),
            source_id.to_string(),
            source_id.to_string(),
            target_id.to_string(),
        )?;

        assert_eq!(report.appended_lines, 2);
        assert_eq!(report.total_lines, 4);
        let target_lines = read_rollout_lines(&target_path)?;
        assert_eq!(target_lines.len(), 4);
        let appended_meta: Value = serde_json::from_str(&target_lines[2])?;
        assert_eq!(appended_meta["payload"]["id"], target_id);
        assert_eq!(appended_meta["payload"]["model_provider"], DEFAULT_PROVIDER);
        let state = state_db::open_ro(&codex)?;
        let (tokens, archived): (i64, i64) = state.query_row(
            "SELECT CAST(tokens_used AS INTEGER), CAST(archived AS INTEGER) FROM threads WHERE id = ?",
            [target_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(tokens, 77);
        assert_eq!(archived, 0);
        let index = fs::read_to_string(paths::session_index_path(&codex))?;
        assert!(index.contains(target_id));
        assert_thread_project_cwd(&codex, target_id, r"F:\project\example")?;

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn branch_sync_compensates_rollout_sqlite_index_project_and_family() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-branch-sync-compensation-test");
        let source_id = "sync-compensation-source";
        let target_id = "sync-compensation-target";
        let source_rel = "sessions/2026/04/24/rollout-sync-compensation-source.jsonl";
        let target_rel = "sessions/2026/04/24/rollout-sync-compensation-target.jsonl";
        let source_path = codex.join(source_rel);
        let target_path = codex.join(target_rel);
        let common = serde_json::json!({
            "timestamp": "2026-04-24T00:00:01Z",
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "hello"}
        })
        .to_string();
        let extra = serde_json::json!({
            "timestamp": "2026-04-24T00:00:02Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {"total_token_usage": {"total_tokens": 99}}
            }
        })
        .to_string();
        write_sync_rollout(&source_path, source_id, "custom", &[common.clone(), extra])?;
        write_sync_rollout(&target_path, target_id, DEFAULT_PROVIDER, &[common])?;
        create_full_state(&codex)?;
        {
            let state = state_db::open(&codex)?;
            sync_thread_from_rollout(&codex, &state, &source_path)?;
            sync_thread_from_rollout(&codex, &state, &target_path)?;
        }
        write_index_line(&codex, source_id)?;
        save_two_branch_family(
            &codex,
            source_id,
            "custom",
            source_rel,
            target_id,
            DEFAULT_PROVIDER,
            target_rel,
        )?;
        write_global_state(&codex, serde_json::json!({"local-projects": {}}))?;

        let target_before = fs::read(&target_path)?;
        let index_before = fs::read(paths::session_index_path(&codex))?;
        let family_before = fs::read(paths::family_store_path(&codex))?;
        let global_before = fs::read(paths::codex_global_state_json_path(&codex))?;
        let thread_before = read_thread_state_map(&codex)?
            .remove(target_id)
            .expect("fixture target thread state");

        let _fault = RepairTestFaultGuard::error("branch_sync_after_family_save");
        let error = append_branch_extras_locked(
            codex.to_string_lossy().into_owned(),
            source_id.to_string(),
            source_id.to_string(),
            target_id.to_string(),
        )
        .expect_err("fault after every branch-sync mutation must compensate all stores");

        assert!(error.to_string().contains("测试故障注入"), "{error}");
        assert_eq!(fs::read(&target_path)?, target_before);
        assert_eq!(fs::read(paths::session_index_path(&codex))?, index_before);
        assert_eq!(fs::read(paths::family_store_path(&codex))?, family_before);
        assert_eq!(
            fs::read(paths::codex_global_state_json_path(&codex))?,
            global_before
        );
        let thread_after = read_thread_state_map(&codex)?
            .remove(target_id)
            .expect("restored target thread state");
        assert_eq!(thread_after.rollout_path, thread_before.rollout_path);
        assert_eq!(thread_after.model_provider, thread_before.model_provider);
        assert_eq!(thread_after.source, thread_before.source);
        assert_eq!(thread_after.archived, thread_before.archived);

        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn sync_updates_archived_target_without_adding_index_entry() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-sync-archived-target-test");
        let source_id = "sync-archived-source";
        let target_id = "sync-archived-target";
        let source_rel = "sessions/2026/04/24/rollout-sync-archived-source.jsonl";
        let target_rel = "sessions/2026/04/24/rollout-sync-archived-target.jsonl";
        let source_path = codex.join(source_rel);
        let target_path =
            paths::archived_sessions_dir(&codex).join("rollout-sync-archived-target.jsonl");
        let common = serde_json::json!({
            "timestamp": "2026-04-24T00:00:01Z",
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "hello"}
        })
        .to_string();
        let extra = serde_json::json!({
            "timestamp": "2026-04-24T00:00:02Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {"total_token_usage": {"total_tokens": 88}}
            }
        })
        .to_string();
        write_sync_rollout(&source_path, source_id, "custom", &[common.clone(), extra])?;
        write_sync_rollout(&target_path, target_id, DEFAULT_PROVIDER, &[common])?;
        create_full_state(&codex)?;
        {
            let state = state_db::open(&codex)?;
            sync_thread_from_rollout(&codex, &state, &source_path)?;
            assert!(upsert_thread_from_rollout(
                &codex,
                &state,
                &target_path,
                true
            )?);
        }
        save_two_branch_family(
            &codex,
            source_id,
            "custom",
            source_rel,
            target_id,
            DEFAULT_PROVIDER,
            target_rel,
        )?;

        let report = append_branch_extras_locked(
            codex.to_string_lossy().into_owned(),
            source_id.to_string(),
            source_id.to_string(),
            target_id.to_string(),
        )?;

        assert_eq!(report.total_lines, 3);
        let state = state_db::open_ro(&codex)?;
        let (tokens, archived): (i64, i64) = state.query_row(
            "SELECT CAST(tokens_used AS INTEGER), CAST(archived AS INTEGER) FROM threads WHERE id = ?",
            [target_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(tokens, 88);
        assert_eq!(archived, 1);
        assert!(!paths::session_index_path(&codex).exists());

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn rollback_missing_target_has_no_side_effects() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-rollback-preflight-test");
        let source_id = "rollback-source";
        let target_id = "rollback-missing-target";
        let source_rel = "sessions/2026/04/24/rollout-rollback-source.jsonl";
        let target_rel = "sessions/2026/04/24/rollout-rollback-missing-target.jsonl";
        let source_path = codex.join(source_rel);
        write_sync_rollout(&source_path, source_id, "custom", &[])?;
        create_full_state(&codex)?;
        {
            let state = state_db::open(&codex)?;
            sync_thread_from_rollout(&codex, &state, &source_path)?;
        }
        write_index_line(&codex, source_id)?;
        save_two_branch_family(
            &codex,
            source_id,
            "custom",
            source_rel,
            target_id,
            DEFAULT_PROVIDER,
            target_rel,
        )?;
        let store_before = fs::read(paths::family_store_path(&codex))?;
        let index_before = fs::read(paths::session_index_path(&codex))?;

        let error = rollback_family_active_locked(
            codex.to_string_lossy().into_owned(),
            source_id.to_string(),
            target_id.to_string(),
        )
        .expect_err("missing target must fail during preflight");

        assert!(error.to_string().contains("目标分支 rollout 丢失"));
        assert!(source_path.is_file());
        assert!(!paths::archived_sessions_dir(&codex)
            .join(source_path.file_name().unwrap())
            .exists());
        let state = state_db::open_ro(&codex)?;
        let archived: i64 = state.query_row(
            "SELECT CAST(archived AS INTEGER) FROM threads WHERE id = ?",
            [source_id],
            |row| row.get(0),
        )?;
        assert_eq!(archived, 0);
        assert_eq!(fs::read(paths::family_store_path(&codex))?, store_before);
        assert_eq!(fs::read(paths::session_index_path(&codex))?, index_before);

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn rollback_rejects_target_that_is_behind_active_without_side_effects() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-rollback-behind-test");
        let fixture = prepare_rollback_fixture(&codex, "behind")?;
        let extra = serde_json::json!({
            "timestamp": "2026-04-24T00:00:01Z",
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "new active turn"}
        });
        let mut source = fs::OpenOptions::new()
            .append(true)
            .open(&fixture.source_path)?;
        writeln!(source, "{}", serde_json::to_string(&extra)?)?;
        drop(source);
        let family_before = fs::read(paths::family_store_path(&codex))?;
        let index_before = fs::read(paths::session_index_path(&codex))?;
        let target_before = fs::read(&fixture.target_archived_path)?;

        let error = rollback_family_active_locked(
            codex.to_string_lossy().into_owned(),
            fixture.source_id.clone(),
            fixture.target_id.clone(),
        )
        .expect_err("a behind target must be synchronized before switching");

        assert!(error.to_string().contains("目标分支落后当前 active"));
        assert!(fixture.source_path.is_file());
        assert!(!fixture.source_archived_path.exists());
        assert!(!fixture.target_active_path.exists());
        assert_eq!(fs::read(&fixture.target_archived_path)?, target_before);
        assert_eq!(fs::read(paths::family_store_path(&codex))?, family_before);
        assert_eq!(fs::read(paths::session_index_path(&codex))?, index_before);

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn rollback_compensates_files_threads_index_and_family_after_late_failure() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-rollback-compensation-test");
        let fixture = prepare_rollback_fixture(&codex, "compensation")?;
        let family_before = fs::read(paths::family_store_path(&codex))?;
        let index_before = fs::read(paths::session_index_path(&codex))?;
        let source_before = fs::read(&fixture.source_path)?;
        let target_before = fs::read(&fixture.target_archived_path)?;
        let source_thread_before = read_thread_location(&codex, &fixture.source_id)?;
        let target_thread_before = read_thread_location(&codex, &fixture.target_id)?;
        let global_before = fs::read(paths::codex_global_state_json_path(&codex))?;

        let _fault = RepairTestFaultGuard::error("rollback_after_family_save");
        let error = rollback_family_active_locked(
            codex.to_string_lossy().into_owned(),
            fixture.source_id.clone(),
            fixture.target_id.clone(),
        )
        .expect_err("late rollback failure must trigger compensation");
        assert!(error.to_string().contains("测试故障注入"), "{error}");
        assert!(!error.to_string().contains("补偿失败"));

        assert_eq!(fs::read(&fixture.source_path)?, source_before);
        assert_eq!(fs::read(&fixture.target_archived_path)?, target_before);
        assert!(!fixture.source_archived_path.exists());
        assert!(!fixture.target_active_path.exists());
        assert_eq!(fs::read(paths::family_store_path(&codex))?, family_before);
        assert_eq!(fs::read(paths::session_index_path(&codex))?, index_before);
        assert_eq!(
            read_thread_location(&codex, &fixture.source_id)?,
            source_thread_before
        );
        assert_eq!(
            read_thread_location(&codex, &fixture.target_id)?,
            target_thread_before
        );
        assert_eq!(
            fs::read(paths::codex_global_state_json_path(&codex))?,
            global_before
        );

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn rollback_reports_compensation_failures_without_hiding_primary_error() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-rollback-compensation-error-test");
        let fixture = prepare_rollback_fixture(&codex, "compensation-error")?;

        let _fault = RepairTestFaultGuard::create_and_error(
            "rollback_after_family_save",
            fixture.source_path.clone(),
        );
        let error = rollback_family_active_locked(
            codex.to_string_lossy().into_owned(),
            fixture.source_id.clone(),
            fixture.target_id.clone(),
        )
        .expect_err("an occupied original path must be reported as a compensation failure");
        let message = error.to_string();
        assert!(message.contains("测试故障注入"), "{message}");
        assert!(message.contains("补偿失败"), "{message}");
        assert!(message.contains("移动目标已存在"), "{message}");
        assert!(message.contains("拒绝覆盖"), "{message}");

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn thread_rebuild_values_include_rollout_token_count() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-repair-token-test");
        let rollout = write_token_rollout(&codex, "token-session")?;

        let cols: Vec<&str> = THREADS_COLS
            .iter()
            .chain(THREADS_OPTIONAL_COLS.iter())
            .copied()
            .collect();
        let values =
            thread_values_from_rollout(&codex, &rollout, false, &cols)?.expect("thread values");
        fs::remove_dir_all(&codex).ok();

        let token_index = cols
            .iter()
            .position(|name| *name == "tokens_used")
            .expect("tokens_used column");
        assert_eq!(values[token_index], Value::from(2_468_000i64));
        Ok(())
    }

    #[test]
    fn repair_index_and_threads_rebuild_fill_missing_assignments_without_overwriting_pending_moves(
    ) -> AppResult<()> {
        for operation in ["index", "threads"] {
            let codex = temp_codex_dir(&format!(
                "cc-session-manager-{operation}-project-assignment-test"
            ));
            write_rollout(&codex, "repair-missing", DEFAULT_PROVIDER)?;
            write_rollout(&codex, "repair-pending", DEFAULT_PROVIDER)?;
            create_full_state(&codex)?;
            let pending = serde_json::json!({
                "projectKind": "local",
                "projectId": "official-pending-project",
                "cwd": r"F:\official\pending-target",
                "pendingCoreUpdate": true
            });
            write_global_state(
                &codex,
                serde_json::json!({
                    "local-projects": {},
                    "thread-project-assignments": {"repair-pending": pending.clone()}
                }),
            )?;

            if operation == "index" {
                let report = repair_session_index(codex.to_string_lossy().into_owned(), false)?;
                assert_eq!(report.written, 2);
            } else {
                let report = rebuild_threads_table(codex.to_string_lossy().into_owned(), false)?;
                assert_eq!(report.upserted, 2);
            }

            assert_thread_project_cwd(&codex, "repair-missing", r"F:\project\example")?;
            assert_eq!(
                thread_project_assignment(&codex, "repair-pending")?,
                Some(pending),
                "{operation} must preserve an official pending move"
            );
            fs::remove_dir_all(&codex).ok();
        }
        Ok(())
    }

    #[test]
    fn repair_index_rejects_malformed_project_state_before_rewriting_index() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-index-project-preflight-test");
        write_rollout(&codex, "repair-index-preflight", DEFAULT_PROVIDER)?;
        fs::create_dir_all(&codex)?;
        let index_path = paths::session_index_path(&codex);
        fs::write(&index_path, b"sentinel-index-bytes\n")?;
        let before = fs::read(&index_path)?;
        write_global_state(&codex, serde_json::json!({"local-projects": []}))?;

        let error = repair_session_index(codex.to_string_lossy().into_owned(), false)
            .expect_err("malformed project state must fail before the index rewrite");

        assert!(error.to_string().contains("必须是对象"), "{error}");
        assert_eq!(fs::read(&index_path)?, before);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn rebuild_threads_rejects_malformed_project_state_before_sqlite_writes() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-threads-project-preflight-test");
        write_rollout(&codex, "repair-threads-preflight", DEFAULT_PROVIDER)?;
        let state = create_full_state(&codex)?;
        state.execute(
            "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 0)",
            rusqlite::params!["sentinel-thread", "sentinel-rollout"],
        )?;
        drop(state);
        write_global_state(&codex, serde_json::json!({"local-projects": []}))?;

        let error = rebuild_threads_table(codex.to_string_lossy().into_owned(), false)
            .expect_err("malformed project state must fail before SQLite writes");

        assert!(error.to_string().contains("必须是对象"), "{error}");
        let state = state_db::open_ro(&codex)?;
        let rows = state
            .prepare("SELECT id, rollout_path FROM threads ORDER BY id")?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            rows,
            vec![(
                "sentinel-thread".to_string(),
                "sentinel-rollout".to_string()
            )]
        );
        drop(state);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn repair_index_compensates_when_project_state_cas_retries_are_exhausted() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-index-project-cas-test");
        write_rollout(&codex, "repair-index-cas", DEFAULT_PROVIDER)?;
        fs::create_dir_all(&codex)?;
        let index_path = paths::session_index_path(&codex);
        fs::write(&index_path, b"sentinel-index-before-cas\n")?;
        let before = fs::read(&index_path)?;
        write_global_state(&codex, serde_json::json!({"local-projects": {}}))?;
        let _conflict = crate::codex_projects::StateWriteConflictTestGuard::all_attempts();

        let error = repair_session_index(codex.to_string_lossy().into_owned(), false)
            .expect_err("exhausted project-state CAS retries must abort repair");

        assert!(error.to_string().contains("发生变化"), "{error}");
        assert_eq!(fs::read(&index_path)?, before);
        let state: Value =
            serde_json::from_slice(&fs::read(paths::codex_global_state_json_path(&codex))?)?;
        assert_eq!(state["test-concurrent-write"], 3);
        assert!(state.get("thread-project-assignments").is_none());
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn rebuild_threads_rolls_back_when_project_state_cas_retries_are_exhausted() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-threads-project-cas-test");
        write_rollout(&codex, "repair-threads-cas", DEFAULT_PROVIDER)?;
        let state = create_full_state(&codex)?;
        state.execute(
            "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 0)",
            rusqlite::params!["sentinel-thread", "sentinel-rollout"],
        )?;
        drop(state);
        write_global_state(&codex, serde_json::json!({"local-projects": {}}))?;
        let _conflict = crate::codex_projects::StateWriteConflictTestGuard::all_attempts();

        let error = rebuild_threads_table(codex.to_string_lossy().into_owned(), false)
            .expect_err("exhausted project-state CAS retries must roll back SQLite");

        assert!(error.to_string().contains("发生变化"), "{error}");
        let state = state_db::open_ro(&codex)?;
        let rows = state
            .prepare("SELECT id, rollout_path FROM threads ORDER BY id")?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            rows,
            vec![(
                "sentinel-thread".to_string(),
                "sentinel-rollout".to_string()
            )]
        );
        drop(state);
        let global: Value =
            serde_json::from_slice(&fs::read(paths::codex_global_state_json_path(&codex))?)?;
        assert_eq!(global["test-concurrent-write"], 3);
        assert!(global.get("thread-project-assignments").is_none());
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn upsert_thread_fills_current_optional_columns_when_they_exist() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-preview-cols-test");
        let rollout = write_conversation_rollout(&codex, "preview-session")?;
        {
            let conn = create_full_state(&codex)?;
            conn.execute(
                "ALTER TABLE threads ADD COLUMN preview TEXT NOT NULL DEFAULT ''",
                [],
            )?;
            conn.execute("ALTER TABLE threads ADD COLUMN thread_source TEXT", [])?;
            conn.execute(
                "ALTER TABLE threads ADD COLUMN recency_at INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
            conn.execute(
                "ALTER TABLE threads ADD COLUMN recency_at_ms INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
            conn.execute(
                "ALTER TABLE threads ADD COLUMN history_mode TEXT NOT NULL DEFAULT 'legacy'",
                [],
            )?;
            conn.execute("ALTER TABLE threads ADD COLUMN name TEXT", [])?;
        }

        let state = state_db::open(&codex)?;
        assert!(upsert_thread_from_rollout(&codex, &state, &rollout, false)?);
        let (preview, thread_source, updated_at, updated_at_ms, recency_at, recency_at_ms, history_mode, name): (
            String,
            String,
            i64,
            i64,
            i64,
            i64,
            String,
            Option<String>,
        ) = state.query_row(
            "SELECT preview, thread_source, CAST(updated_at AS INTEGER), CAST(updated_at_ms AS INTEGER),
                    recency_at, recency_at_ms, history_mode, name
             FROM threads WHERE id = ?",
            ["preview-session"],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )?;
        assert_eq!(preview, "First request");
        assert_eq!(thread_source, "user");
        assert_eq!(recency_at, updated_at);
        assert_eq!(recency_at_ms, updated_at_ms);
        assert_eq!(history_mode, "legacy");
        assert!(name.is_none());

        state.execute(
            "UPDATE threads
             SET recency_at = 1999999999,
                 recency_at_ms = 1999999999123,
                 preview = 'Pinned preview'
             WHERE id = 'preview-session'",
            [],
        )?;
        let canonical_line = fs::read_to_string(&rollout)?
            .lines()
            .next()
            .expect("canonical session meta")
            .to_string();
        fs::write(&rollout, format!("{canonical_line}\n"))?;
        assert!(upsert_thread_from_rollout(&codex, &state, &rollout, false)?);
        let (preserved_recency, preserved_recency_ms, preserved_preview): (i64, i64, String) =
            state.query_row(
                "SELECT recency_at, recency_at_ms, preview FROM threads WHERE id = ?",
                ["preview-session"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        assert_eq!(preserved_recency, 1_999_999_999);
        assert_eq!(preserved_recency_ms, 1_999_999_999_123);
        assert_eq!(preserved_preview, "Pinned preview");

        drop(state);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn upsert_thread_maps_nested_git_metadata() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-nested-git-test");
        let rollout_dir = codex.join("sessions").join("2026").join("04").join("23");
        fs::create_dir_all(&rollout_dir)?;
        let rollout = rollout_dir.join("rollout-git-session.jsonl");
        let lines = [
            serde_json::json!({
                "timestamp": "2026-04-23T00:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": "git-session",
                    "session_id": "git-session",
                    "model_provider": DEFAULT_PROVIDER,
                    "cwd": "F:\\project\\example",
                    "source": DEFAULT_THREAD_SOURCE,
                    "git": {
                        "commit_hash": "0123456789abcdef",
                        "branch": "feature/nested-git",
                        "repository_url": "https://example.invalid/repo.git"
                    }
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-04-23T00:00:01Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Git request"}
            })
            .to_string(),
        ];
        fs::write(&rollout, format!("{}\n", lines.join("\n")))?;
        create_full_state(&codex)?;

        let state = state_db::open(&codex)?;
        assert!(upsert_thread_from_rollout(&codex, &state, &rollout, false)?);
        let git: (String, String, String) = state.query_row(
            "SELECT git_sha, git_branch, git_origin_url FROM threads WHERE id = ?",
            ["git-session"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(git.0, "0123456789abcdef");
        assert_eq!(git.1, "feature/nested-git");
        assert_eq!(git.2, "https://example.invalid/repo.git");

        state.execute(
            "UPDATE threads
             SET git_sha = 'database-sha',
                 git_branch = 'database-branch',
                 git_origin_url = 'https://example.invalid/database.git'
             WHERE id = 'git-session'",
            [],
        )?;
        assert!(upsert_thread_from_rollout(&codex, &state, &rollout, false)?);
        let preserved_git: (String, String, String) = state.query_row(
            "SELECT git_sha, git_branch, git_origin_url FROM threads WHERE id = ?",
            ["git-session"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(preserved_git.0, "database-sha");
        assert_eq!(preserved_git.1, "database-branch");
        assert_eq!(preserved_git.2, "https://example.invalid/database.git");

        drop(state);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn upsert_thread_marks_subagent_thread_source() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-subagent-source-test");
        let rollout_dir = codex.join("sessions").join("2026").join("04").join("23");
        fs::create_dir_all(&rollout_dir)?;
        let rollout = rollout_dir.join("rollout-subagent-session.jsonl");
        let lines = vec![
            serde_json::json!({
                "timestamp": "2026-04-23T00:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": "subagent-session",
                    "model_provider": DEFAULT_PROVIDER,
                    "cwd": "F:\\project\\example",
                    "source": {"subagent": {"other": "guardian"}}
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-04-23T00:00:01Z",
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": "Subagent request"
                }
            })
            .to_string(),
        ];
        fs::write(&rollout, format!("{}\n", lines.join("\n")))?;
        {
            let conn = create_full_state(&codex)?;
            conn.execute(
                "ALTER TABLE threads ADD COLUMN preview TEXT NOT NULL DEFAULT ''",
                [],
            )?;
            conn.execute("ALTER TABLE threads ADD COLUMN thread_source TEXT", [])?;
        }

        let state = state_db::open(&codex)?;
        assert!(upsert_thread_from_rollout(&codex, &state, &rollout, false)?);
        let thread_source: String = state.query_row(
            "SELECT thread_source FROM threads WHERE id = ?",
            ["subagent-session"],
            |row| row.get(0),
        )?;
        assert_eq!(thread_source, "subagent");

        drop(state);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn upsert_thread_skips_optional_columns_on_legacy_schema() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-legacy-schema-test");
        let rollout = write_conversation_rollout(&codex, "legacy-session")?;
        create_full_state(&codex)?;

        let state = state_db::open(&codex)?;
        assert!(upsert_thread_from_rollout(&codex, &state, &rollout, false)?);
        let title: String = state.query_row(
            "SELECT title FROM threads WHERE id = ?",
            ["legacy-session"],
            |row| row.get(0),
        )?;
        assert_eq!(title, "First request");

        drop(state);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn upsert_thread_preserves_existing_custom_title() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-preserve-title-test");
        let rollout = write_conversation_rollout(&codex, "renamed-session")?;
        create_full_state(&codex)?;

        let state = state_db::open(&codex)?;
        assert!(upsert_thread_from_rollout(&codex, &state, &rollout, false)?);
        state.execute(
            "UPDATE threads SET title = '自定义标题' WHERE id = 'renamed-session'",
            [],
        )?;

        // 再次同步（模拟 provider follow / 分支内容同步 / 重建 threads 表）
        assert!(upsert_thread_from_rollout(&codex, &state, &rollout, false)?);
        let title: String = state.query_row(
            "SELECT title FROM threads WHERE id = ?",
            ["renamed-session"],
            |row| row.get(0),
        )?;
        assert_eq!(
            title, "自定义标题",
            "非空自定义标题不得被 rollout 派生标题覆盖"
        );

        // 空标题仍应被派生标题补齐
        state.execute(
            "UPDATE threads SET title = '   ' WHERE id = 'renamed-session'",
            [],
        )?;
        assert!(upsert_thread_from_rollout(&codex, &state, &rollout, false)?);
        let title: String = state.query_row(
            "SELECT title FROM threads WHERE id = ?",
            ["renamed-session"],
            |row| row.get(0),
        )?;
        assert_eq!(title, "First request");

        drop(state);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn carry_thread_title_copies_only_non_empty_source_title() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-carry-title-test");
        let conn = create_full_state(&codex)?;
        conn.execute(
            "INSERT INTO threads (id, rollout_path, title) VALUES
             ('src-named', 'a.jsonl', '自定义标题'),
             ('src-unnamed', 'b.jsonl', ''),
             ('new-1', 'c.jsonl', 'First request'),
             ('new-2', 'd.jsonl', 'First request')",
            [],
        )?;

        carry_thread_title(&conn, "src-named", "new-1")?;
        carry_thread_title(&conn, "src-unnamed", "new-2")?;

        let title1: String =
            conn.query_row("SELECT title FROM threads WHERE id = 'new-1'", [], |r| {
                r.get(0)
            })?;
        let title2: String =
            conn.query_row("SELECT title FROM threads WHERE id = 'new-2'", [], |r| {
                r.get(0)
            })?;
        assert_eq!(title1, "自定义标题");
        assert_eq!(title2, "First request", "源标题为空时不覆盖新行标题");

        drop(conn);
        fs::remove_dir_all(&codex).ok();
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
        write_global_state(&codex, serde_json::json!({"local-projects": {}}))?;

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
        assert_eq!(first["payload"]["session_id"], report.new_id);
        assert_eq!(first["payload"]["forked_from_id"], source_id);
        assert_eq!(first["payload"]["thread_source"], "user");
        assert_eq!(first["payload"]["history_mode"], "legacy");

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
        let old_archived: i64 = state.query_row(
            "SELECT archived FROM threads WHERE id = ?",
            [source_id],
            |row| row.get(0),
        )?;
        let new_archived: i64 = state.query_row(
            "SELECT archived FROM threads WHERE id = ?",
            [report.new_id.as_str()],
            |row| row.get(0),
        )?;
        assert_eq!(old_archived, 1);
        assert_eq!(new_archived, 0);
        assert_thread_project_cwd(&codex, &report.new_id, r"F:\project\example")?;

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
    fn mismatched_scan_includes_unregistered_rollouts_but_skips_missing_family_heads(
    ) -> AppResult<()> {
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
                        archive_origin: None,
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
                        archive_origin: None,
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
        let plan = get_provider_sync_plan_with_lock(
            codex.to_string_lossy().into_owned(),
            &family::FamilyLock::default(),
        )?;
        fs::remove_dir_all(&codex).ok();

        assert_eq!(targets, plan, "对外同步计划必须与内部可执行目标完全一致");
        assert_eq!(
            plan,
            vec!["legacy-session".to_string()],
            "family 元数据不能把已经没有 active rollout 的孤儿记录变成不可执行的同步任务"
        );
        Ok(())
    }

    #[test]
    fn rollout_identity_stops_before_large_corrupt_tail() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-rollout-identity-tail-test");
        let rollout_dir = codex.join("sessions/2026/04/22");
        fs::create_dir_all(&rollout_dir)?;
        let id = "lightweight-identity-session";
        let provider = "anthropic";
        let rollout = rollout_dir.join(format!("rollout-{id}.jsonl"));
        let meta = serde_json::json!({
            "timestamp": "2026-04-22T00:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "model_provider": provider,
                "source": DEFAULT_THREAD_SOURCE
            }
        });
        let mut bytes = format!("not-json\n{}\n", serde_json::to_string(&meta)?).into_bytes();
        bytes.extend(vec![b'x'; 4 * 1024 * 1024]);
        bytes.push(0xff);
        fs::write(&rollout, bytes)?;

        let identity = read_rollout_identity(&rollout)?.expect("valid session_meta identity");
        assert_eq!(
            identity,
            RolloutIdentity {
                id: id.to_string(),
                model_provider: provider.to_string(),
                source: Some(DEFAULT_THREAD_SOURCE.to_string()),
            }
        );
        assert!(rollout_record_is_usable_provider(
            &codex,
            id,
            provider,
            &rollout,
            Some(rollout.to_string_lossy().as_ref()),
            Some(provider),
            Some(DEFAULT_THREAD_SOURCE),
            false,
            true,
        )?);
        assert_eq!(
            list_mismatched_session_ids(&codex, DEFAULT_PROVIDER)?,
            vec![id.to_string()]
        );

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn provider_sync_plan_includes_unregistered_rollouts() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-provider-sync-plan-test");
        fs::create_dir_all(&codex)?;
        fs::write(
            paths::config_toml_path(&codex),
            "model_provider = \"openai\"\n",
        )?;
        write_rollout(&codex, "unregistered-provider-session", "anthropic")?;

        let lock = family::FamilyLock::default();
        let plan = get_provider_sync_plan_with_lock(codex.to_string_lossy().into_owned(), &lock)?;
        fs::remove_dir_all(&codex).ok();

        assert_eq!(plan, vec!["unregistered-provider-session".to_string()]);
        Ok(())
    }

    #[test]
    fn provider_sync_targets_keep_the_planned_rollout_path() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-provider-sync-target-test");
        fs::create_dir_all(&codex)?;
        let id = "planned-provider-session";
        write_rollout(&codex, id, "anthropic")?;

        let targets = list_mismatched_sessions(&codex, DEFAULT_PROVIDER)?;
        let expected = codex
            .join("sessions")
            .join("2026")
            .join("04")
            .join("22")
            .join(format!("rollout-{id}.jsonl"));
        fs::remove_dir_all(&codex).ok();

        assert_eq!(
            targets,
            vec![ProviderSyncTarget {
                session_id: id.to_string(),
                rollout_path: expected,
            }]
        );
        Ok(())
    }

    #[test]
    fn mismatched_scan_skips_only_usable_target_branch() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-usable-provider-clone-test");
        fs::create_dir_all(&codex)?;
        let source_rollout = codex.join("sessions/2026/04/24/rollout-provider-source.jsonl");
        let target_rollout = codex.join("sessions/2026/04/24/rollout-provider-target.jsonl");
        write_sync_rollout(&source_rollout, "provider-source", "custom", &[])?;
        write_sync_rollout(&target_rollout, "provider-target", DEFAULT_PROVIDER, &[])?;
        fs::write(
            paths::session_index_path(&codex),
            "{\"id\":\"provider-source\"}\n{\"id\":\"provider-target\"}\n",
        )?;
        save_two_branch_family(
            &codex,
            "provider-source",
            "custom",
            "sessions/2026/04/24/rollout-provider-source.jsonl",
            "provider-target",
            DEFAULT_PROVIDER,
            "sessions/2026/04/24/rollout-provider-target.jsonl",
        )?;
        let conn = create_minimal_state(&codex)?;
        conn.execute(
            "INSERT INTO threads (id, rollout_path, model_provider, source, archived)
             VALUES (?1, ?2, ?3, ?4, 0)",
            (
                "provider-source",
                source_rollout.to_string_lossy(),
                "custom",
                DEFAULT_THREAD_SOURCE,
            ),
        )?;
        conn.execute(
            "INSERT INTO threads (id, rollout_path, model_provider, source, archived)
             VALUES (?1, ?2, ?3, ?4, 0)",
            (
                "provider-target",
                target_rollout.to_string_lossy(),
                DEFAULT_PROVIDER,
                DEFAULT_THREAD_SOURCE,
            ),
        )?;

        assert!(list_mismatched_session_ids(&codex, DEFAULT_PROVIDER)?.is_empty());

        conn.execute(
            "UPDATE threads SET archived = 1 WHERE id = ?",
            ["provider-target"],
        )?;
        assert_eq!(
            list_mismatched_session_ids(&codex, DEFAULT_PROVIDER)?,
            vec!["provider-source".to_string()]
        );

        conn.execute(
            "UPDATE threads SET archived = 0, source = 'cc-session-manager' WHERE id = ?",
            ["provider-target"],
        )?;
        assert_eq!(
            list_mismatched_session_ids(&codex, DEFAULT_PROVIDER)?,
            vec!["provider-source".to_string()]
        );

        conn.execute(
            "UPDATE threads SET archived = 1 WHERE id = ?",
            ["provider-source"],
        )?;
        assert!(
            list_mismatched_session_ids(&codex, DEFAULT_PROVIDER)?.is_empty(),
            "手工归档的 active 分支不应被 provider 批量同步重新激活"
        );
        conn.execute(
            "UPDATE threads SET archived = 0 WHERE id = ?",
            ["provider-source"],
        )?;

        let mut store = family::load(&codex)?;
        let managed = store
            .families
            .get_mut("provider-source")
            .expect("provider family");
        let active_id = managed.active_id.clone();
        managed
            .chain
            .iter_mut()
            .find(|branch| branch.id == active_id)
            .expect("active branch")
            .status = BranchStatus::Archived;
        family::save(&codex, &store)?;
        assert!(list_mismatched_session_ids(&codex, DEFAULT_PROVIDER)?.is_empty());

        fs::remove_dir_all(&codex).ok();
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
    fn pruning_orphan_threads_clears_desktop_project_state() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-prune-project-state-test");
        let conn = create_minimal_state(&codex)?;
        conn.execute(
            "INSERT INTO threads (id, model_provider, source, archived) VALUES (?1, ?2, ?3, 0)",
            ("ghost-thread", DEFAULT_PROVIDER, DEFAULT_THREAD_SOURCE),
        )?;
        drop(conn);
        write_global_state(
            &codex,
            serde_json::json!({
                "local-projects": {},
                "thread-project-assignments": {
                    "ghost-thread": {
                        "projectKind": "local",
                        "projectId": "ghost-project",
                        "cwd": r"F:\ghost",
                        "pendingCoreUpdate": false
                    }
                },
                "projectless-thread-ids": ["ghost-thread"],
                "thread-workspace-root-hints": {"ghost-thread": r"F:\ghost"},
                "thread-writable-roots": {"ghost-thread": [r"F:\ghost"]},
                "electron-persisted-atom-state": {
                    "thread-workspace-state-v1:ghost-thread": {"pending": {"cwd": r"F:\ghost"}}
                }
            }),
        )?;

        let report =
            prune_orphan_entries(codex.to_string_lossy().into_owned(), false, true, false)?;

        assert_eq!(report.threads_removed, 1);
        assert!(thread_project_assignment(&codex, "ghost-thread")?.is_none());
        let global: Value =
            serde_json::from_slice(&fs::read(paths::codex_global_state_json_path(&codex))?)?;
        assert!(!global["projectless-thread-ids"]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|id| id == "ghost-thread")));
        assert_eq!(
            global["thread-workspace-root-hints"]["ghost-thread"],
            r"F:\ghost"
        );
        assert_eq!(
            global["thread-writable-roots"]["ghost-thread"],
            serde_json::json!([r"F:\ghost"])
        );
        assert!(
            global["electron-persisted-atom-state"]["thread-workspace-state-v1:ghost-thread"]
                .is_object()
        );

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn prune_rejects_broken_project_state_before_any_core_write() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-prune-broken-project-state-test");
        let live_id = "live-thread";
        let orphan_id = "orphan-thread";
        write_rollout(&codex, live_id, DEFAULT_PROVIDER)?;
        let rollout = family::scan_rollouts(&codex)?
            .into_iter()
            .next()
            .expect("live rollout");
        let conn = create_minimal_state(&codex)?;
        conn.execute(
            "INSERT INTO threads (id, model_provider, source, archived) VALUES (?1, ?2, ?3, 0)",
            (live_id, DEFAULT_PROVIDER, DEFAULT_THREAD_SOURCE),
        )?;
        conn.execute(
            "INSERT INTO threads (id, model_provider, source, archived) VALUES (?1, ?2, ?3, 0)",
            (orphan_id, DEFAULT_PROVIDER, DEFAULT_THREAD_SOURCE),
        )?;
        drop(conn);
        fs::write(
            paths::session_index_path(&codex),
            format!("{{\"id\":\"{live_id}\"}}\n{{\"id\":\"{orphan_id}\"}}\n"),
        )?;
        let global_state_path = paths::codex_global_state_json_path(&codex);
        fs::write(&global_state_path, "{broken global state")?;

        let rollout_before = fs::read(&rollout)?;
        let state_path = paths::state_db_path(&codex);
        let state_before = fs::read(&state_path)?;
        let index_path = paths::session_index_path(&codex);
        let index_before = fs::read(&index_path)?;
        let global_before = fs::read(&global_state_path)?;

        let error = prune_orphan_entries(codex.to_string_lossy().into_owned(), true, true, false)
            .expect_err("broken project state must abort before pruning Core data");

        assert!(error.to_string().contains("全局状态 JSON 损坏"), "{error}");
        assert_eq!(fs::read(&rollout)?, rollout_before);
        assert_eq!(fs::read(&state_path)?, state_before);
        assert_eq!(fs::read(&index_path)?, index_before);
        assert_eq!(fs::read(&global_state_path)?, global_before);
        let state = state_db::open_ro(&codex)?;
        let ids = state
            .prepare("SELECT id FROM threads ORDER BY id")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(ids, vec![live_id.to_string(), orphan_id.to_string()]);
        drop(state);

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn prune_compensates_family_index_threads_and_project_state_after_late_failure() -> AppResult<()>
    {
        let codex = temp_codex_dir("cc-session-manager-prune-compensation-test");
        let live_id = "prune-live";
        let orphan_id = "prune-orphan";
        write_rollout(&codex, live_id, DEFAULT_PROVIDER)?;
        save_two_branch_family(
            &codex,
            live_id,
            DEFAULT_PROVIDER,
            "sessions/2026/04/22/rollout-prune-live.jsonl",
            orphan_id,
            "custom",
            "archived_sessions/rollout-prune-orphan.jsonl",
        )?;
        let conn = create_minimal_state(&codex)?;
        for id in [live_id, orphan_id] {
            conn.execute(
                "INSERT INTO threads (id, model_provider, source, archived) VALUES (?1, ?2, ?3, 0)",
                (id, DEFAULT_PROVIDER, DEFAULT_THREAD_SOURCE),
            )?;
        }
        drop(conn);
        fs::write(
            paths::session_index_path(&codex),
            format!("{{\"id\":\"{live_id}\"}}\n{{\"id\":\"{orphan_id}\"}}\n"),
        )?;
        write_global_state(
            &codex,
            serde_json::json!({
                "thread-project-assignments": {
                    (orphan_id): {
                        "projectKind": "local",
                        "projectId": "prune-project",
                        "cwd": r"F:\prune",
                        "pendingCoreUpdate": false
                    }
                }
            }),
        )?;

        let family_path = paths::family_store_path(&codex);
        let index_path = paths::session_index_path(&codex);
        let global_path = paths::codex_global_state_json_path(&codex);
        let family_before = fs::read(&family_path)?;
        let index_before = fs::read(&index_path)?;
        let global_before = fs::read(&global_path)?;
        let _fault = RepairTestFaultGuard::error("prune_after_project_state");

        let error = prune_orphan_entries_with_lock(
            codex.to_string_lossy().into_owned(),
            true,
            true,
            true,
            false,
            &family::FamilyLock::default(),
        )
        .expect_err("late prune failure must compensate every store");

        assert!(error.to_string().contains("测试故障注入"), "{error}");
        assert_eq!(fs::read(&family_path)?, family_before);
        assert_eq!(fs::read(&index_path)?, index_before);
        assert_eq!(fs::read(&global_path)?, global_before);
        let state = state_db::open_ro(&codex)?;
        let ids = state
            .prepare("SELECT id FROM threads ORDER BY id")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(ids, vec![live_id.to_string(), orphan_id.to_string()]);
        drop(state);

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn prune_compensates_core_when_desktop_starts_after_preflight() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-prune-desktop-race-test");
        let live_id = "prune-race-live";
        let orphan_id = "prune-race-orphan";
        write_rollout(&codex, live_id, DEFAULT_PROVIDER)?;
        let conn = create_minimal_state(&codex)?;
        for id in [live_id, orphan_id] {
            conn.execute(
                "INSERT INTO threads (id, model_provider, source, archived) VALUES (?1, ?2, ?3, 0)",
                (id, DEFAULT_PROVIDER, DEFAULT_THREAD_SOURCE),
            )?;
        }
        drop(conn);
        fs::write(
            paths::session_index_path(&codex),
            format!("{{\"id\":\"{live_id}\"}}\n{{\"id\":\"{orphan_id}\"}}\n"),
        )?;
        write_global_state(
            &codex,
            serde_json::json!({
                "thread-project-assignments": {(orphan_id): {}},
                "projectless-thread-ids": []
            }),
        )?;
        let index_path = paths::session_index_path(&codex);
        let global_path = paths::codex_global_state_json_path(&codex);
        let index_before = fs::read(&index_path)?;
        let global_before = fs::read(&global_path)?;
        // First probe is the business preflight; the second is the guarded state mutation.
        let _desktop = crate::codex_projects::DesktopTestProbeGuard::running_after_not_running(1);

        let error = prune_orphan_entries_with_lock(
            codex.to_string_lossy().into_owned(),
            true,
            true,
            false,
            false,
            &family::FamilyLock::default(),
        )
        .expect_err("Desktop start after preflight must abort and compensate Core changes");

        assert!(error.to_string().contains("完全退出桌面应用"), "{error}");
        assert_eq!(fs::read(&index_path)?, index_before);
        assert_eq!(fs::read(&global_path)?, global_before);
        let state = state_db::open_ro(&codex)?;
        let ids = state
            .prepare("SELECT id FROM threads ORDER BY id")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(ids, vec![live_id.to_string(), orphan_id.to_string()]);
        drop(state);

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn prune_compensates_core_when_project_state_cas_retries_are_exhausted() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-prune-cas-compensation-test");
        let live_id = "prune-cas-live";
        let orphan_id = "prune-cas-orphan";
        write_rollout(&codex, live_id, DEFAULT_PROVIDER)?;
        let conn = create_minimal_state(&codex)?;
        for id in [live_id, orphan_id] {
            conn.execute(
                "INSERT INTO threads (id, model_provider, source, archived) VALUES (?1, ?2, ?3, 0)",
                (id, DEFAULT_PROVIDER, DEFAULT_THREAD_SOURCE),
            )?;
        }
        drop(conn);
        let index_path = paths::session_index_path(&codex);
        fs::write(
            &index_path,
            format!("{{\"id\":\"{live_id}\"}}\n{{\"id\":\"{orphan_id}\"}}\n"),
        )?;
        write_global_state(
            &codex,
            serde_json::json!({"thread-project-assignments": {(orphan_id): {}}}),
        )?;
        let index_before = fs::read(&index_path)?;
        let _conflict = crate::codex_projects::StateWriteConflictTestGuard::all_attempts();

        let error = prune_orphan_entries_with_lock(
            codex.to_string_lossy().into_owned(),
            true,
            true,
            false,
            false,
            &family::FamilyLock::default(),
        )
        .expect_err("exhausted project-state CAS retries must compensate Core changes");

        assert!(error.to_string().contains("发生变化"), "{error}");
        assert_eq!(fs::read(&index_path)?, index_before);
        let state = state_db::open_ro(&codex)?;
        let ids = state
            .prepare("SELECT id FROM threads ORDER BY id")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(ids, vec![live_id.to_string(), orphan_id.to_string()]);
        drop(state);
        let global: Value =
            serde_json::from_slice(&fs::read(paths::codex_global_state_json_path(&codex))?)?;
        assert_eq!(global["test-concurrent-write"], 3);
        assert!(global["thread-project-assignments"][orphan_id].is_object());

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn prune_family_orphans_removes_fully_missing_families() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-prune-missing-family-test");
        fs::create_dir_all(&codex)?;
        save_two_branch_family(
            &codex,
            "missing-active",
            DEFAULT_PROVIDER,
            "sessions/2026/04/24/rollout-missing-active.jsonl",
            "missing-history",
            "custom",
            "archived_sessions/rollout-missing-history.jsonl",
        )?;

        let report = prune_orphan_entries_with_lock(
            codex.to_string_lossy().into_owned(),
            false,
            false,
            true,
            false,
            &family::FamilyLock::default(),
        )?;
        let store = family::load(&codex)?;

        assert_eq!(report.families_removed, 1);
        assert_eq!(report.family_branches_removed, 0);
        assert!(report.families_skipped.is_empty());
        assert!(store.families.is_empty());
        assert!(store.index.is_empty());
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn prune_family_orphans_removes_missing_non_active_branches_and_honors_dry_run() -> AppResult<()>
    {
        let codex = temp_codex_dir("cc-session-manager-prune-missing-branch-test");
        write_rollout(&codex, "active-branch", DEFAULT_PROVIDER)?;
        save_two_branch_family(
            &codex,
            "active-branch",
            DEFAULT_PROVIDER,
            "sessions/2026/04/22/rollout-active-branch.jsonl",
            "missing-history",
            "custom",
            "archived_sessions/rollout-missing-history.jsonl",
        )?;
        let mut store = family::load(&codex)?;
        let family = store
            .families
            .get_mut("active-branch")
            .expect("family fixture");
        family.root_id = "missing-history".to_string();
        family.chain.swap(0, 1);
        family::save(&codex, &store)?;
        let before = serde_json::to_value(&store)?;
        let lock = family::FamilyLock::default();

        let preview = prune_orphan_entries_with_lock(
            codex.to_string_lossy().into_owned(),
            false,
            false,
            true,
            true,
            &lock,
        )?;
        assert_eq!(preview.family_branches_removed, 1);
        assert_eq!(serde_json::to_value(family::load(&codex)?)?, before);

        let report = prune_orphan_entries_with_lock(
            codex.to_string_lossy().into_owned(),
            false,
            false,
            true,
            false,
            &lock,
        )?;
        let store = family::load(&codex)?;
        let family = store
            .families
            .get("active-branch")
            .expect("surviving family");
        assert_eq!(report.family_branches_removed, 1);
        assert_eq!(report.families_removed, 0);
        assert_eq!(family.root_id, "active-branch");
        assert_eq!(family.active_id, "active-branch");
        assert_eq!(family.chain.len(), 1);
        assert!(!store.index.contains_key("missing-history"));
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn prune_family_orphans_skips_family_when_missing_branch_lacks_index_entry() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-prune-branch-index-gap-test");
        write_rollout(&codex, "active-branch", DEFAULT_PROVIDER)?;
        save_two_branch_family(
            &codex,
            "active-branch",
            DEFAULT_PROVIDER,
            "sessions/2026/04/22/rollout-active-branch.jsonl",
            "missing-history",
            "custom",
            "archived_sessions/rollout-missing-history.jsonl",
        )?;
        let mut store = family::load(&codex)?;
        store.index.remove("missing-history");
        family::save(&codex, &store)?;
        let before = serde_json::to_value(&store)?;

        let report = prune_orphan_entries_with_lock(
            codex.to_string_lossy().into_owned(),
            false,
            false,
            true,
            false,
            &family::FamilyLock::default(),
        )?;

        assert_eq!(report.family_branches_removed, 0);
        assert_eq!(report.families_removed, 0);
        assert_eq!(report.families_skipped, vec!["active-branch".to_string()]);
        assert_eq!(serde_json::to_value(family::load(&codex)?)?, before);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn prune_family_orphans_skips_families_sharing_a_duplicated_branch() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-prune-cross-family-dup-test");
        write_rollout(&codex, "active-a", DEFAULT_PROVIDER)?;
        write_rollout(&codex, "active-b", DEFAULT_PROVIDER)?;
        let dup_branch = FamilyBranch {
            id: "dup-branch".to_string(),
            provider: "custom".to_string(),
            created_at: "2026-04-24T00:00:00Z".to_string(),
            status: BranchStatus::Archived,
            rollout_relpath: "archived_sessions/rollout-dup-branch.jsonl".to_string(),
            sha256: None,
            line_count: None,
            note: None,
            archive_origin: None,
        };
        let make_family = |family_id: &str, active_id: &str| Family {
            family_id: family_id.to_string(),
            root_id: active_id.to_string(),
            title: "dup family".to_string(),
            chain: vec![
                FamilyBranch {
                    id: active_id.to_string(),
                    provider: DEFAULT_PROVIDER.to_string(),
                    created_at: "2026-04-24T00:00:00Z".to_string(),
                    status: BranchStatus::Active,
                    rollout_relpath: format!("sessions/2026/04/22/rollout-{active_id}.jsonl"),
                    sha256: None,
                    line_count: None,
                    note: None,
                    archive_origin: None,
                },
                dup_branch.clone(),
            ],
            active_id: active_id.to_string(),
            updated_at: "2026-04-24T00:00:00Z".to_string(),
        };
        let mut families = BTreeMap::new();
        families.insert("family-a".to_string(), make_family("family-a", "active-a"));
        families.insert("family-b".to_string(), make_family("family-b", "active-b"));
        let mut index = BTreeMap::new();
        index.insert("active-a".to_string(), "family-a".to_string());
        index.insert("active-b".to_string(), "family-b".to_string());
        index.insert("dup-branch".to_string(), "family-a".to_string());
        family::save(
            &codex,
            &FamilyStore {
                version: 1,
                families,
                index,
            },
        )?;
        let before = serde_json::to_value(family::load(&codex)?)?;

        let report = prune_orphan_entries_with_lock(
            codex.to_string_lossy().into_owned(),
            false,
            false,
            true,
            false,
            &family::FamilyLock::default(),
        )?;

        assert_eq!(report.family_branches_removed, 0);
        assert_eq!(report.families_removed, 0);
        assert_eq!(
            report.families_skipped,
            vec!["family-a".to_string(), "family-b".to_string()]
        );
        assert_eq!(serde_json::to_value(family::load(&codex)?)?, before);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn prune_family_orphans_preserves_archived_rollouts() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-prune-archived-family-test");
        write_rollout(&codex, "active-branch", DEFAULT_PROVIDER)?;
        write_rollout_in(&codex, "archived_sessions", "archived-branch", "custom")?;
        save_two_branch_family(
            &codex,
            "active-branch",
            DEFAULT_PROVIDER,
            "sessions/2026/04/22/rollout-active-branch.jsonl",
            "archived-branch",
            "custom",
            "archived_sessions/rollout-archived-branch.jsonl",
        )?;

        let report = prune_orphan_entries_with_lock(
            codex.to_string_lossy().into_owned(),
            false,
            false,
            true,
            false,
            &family::FamilyLock::default(),
        )?;
        let store = family::load(&codex)?;

        assert_eq!(report.family_branches_removed, 0);
        assert_eq!(report.families_removed, 0);
        assert!(report.families_skipped.is_empty());
        assert_eq!(store.families["active-branch"].chain.len(), 2);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn prune_family_orphans_normalizes_duplicate_active_markers() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-prune-normalize-active-test");
        write_rollout(&codex, "active-branch", DEFAULT_PROVIDER)?;
        write_rollout_in(&codex, "archived_sessions", "history-branch", "custom")?;
        save_two_branch_family(
            &codex,
            "active-branch",
            DEFAULT_PROVIDER,
            "sessions/2026/04/22/rollout-active-branch.jsonl",
            "history-branch",
            "custom",
            "archived_sessions/rollout-history-branch.jsonl",
        )?;
        let mut store = family::load(&codex)?;
        store
            .families
            .get_mut("active-branch")
            .expect("family fixture")
            .chain[1]
            .status = BranchStatus::Active;
        family::save(&codex, &store)?;
        let before = serde_json::to_value(&store)?;
        let lock = family::FamilyLock::default();

        let preview = prune_orphan_entries_with_lock(
            codex.to_string_lossy().into_owned(),
            false,
            false,
            true,
            true,
            &lock,
        )?;
        assert_eq!(preview.families_normalized, 1);
        assert_eq!(serde_json::to_value(family::load(&codex)?)?, before);

        let report = prune_orphan_entries_with_lock(
            codex.to_string_lossy().into_owned(),
            false,
            false,
            true,
            false,
            &lock,
        )?;
        let restored = family::load(&codex)?;
        let family_record = &restored.families["active-branch"];

        assert_eq!(report.families_normalized, 1);
        assert!(report.families_skipped.is_empty());
        assert!(matches!(
            family_record.chain[0].status,
            BranchStatus::Active
        ));
        assert!(matches!(
            family_record.chain[1].status,
            BranchStatus::Archived
        ));
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn prune_family_orphans_preserves_partial_family_when_active_rollout_is_missing(
    ) -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-prune-missing-active-test");
        write_rollout_in(&codex, "archived_sessions", "surviving-history", "custom")?;
        save_two_branch_family(
            &codex,
            "missing-active",
            DEFAULT_PROVIDER,
            "sessions/2026/04/22/rollout-missing-active.jsonl",
            "surviving-history",
            "custom",
            "archived_sessions/rollout-surviving-history.jsonl",
        )?;
        let before = serde_json::to_value(family::load(&codex)?)?;

        let report = prune_orphan_entries_with_lock(
            codex.to_string_lossy().into_owned(),
            false,
            false,
            true,
            false,
            &family::FamilyLock::default(),
        )?;

        assert_eq!(report.family_branches_removed, 0);
        assert_eq!(report.families_removed, 0);
        assert_eq!(report.families_skipped, vec!["missing-active".to_string()]);
        assert_eq!(serde_json::to_value(family::load(&codex)?)?, before);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn prune_family_orphans_recovers_unique_existing_active_branch() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-prune-recover-stale-active-test");
        write_rollout(&codex, "surviving-active", DEFAULT_PROVIDER)?;
        save_two_branch_family(
            &codex,
            "surviving-active",
            DEFAULT_PROVIDER,
            "sessions/2026/04/22/rollout-surviving-active.jsonl",
            "missing-new-active",
            "custom",
            "sessions/2026/04/24/rollout-missing-new-active.jsonl",
        )?;
        let mut store = family::load(&codex)?;
        let family_record = store
            .families
            .get_mut("surviving-active")
            .expect("family fixture");
        family_record.active_id = "missing-new-active".to_string();
        family_record.chain[0].status = BranchStatus::Active;
        family_record.chain[1].status = BranchStatus::Active;
        family::save(&codex, &store)?;

        let preview = prune_orphan_entries_with_lock(
            codex.to_string_lossy().into_owned(),
            false,
            false,
            true,
            true,
            &family::FamilyLock::default(),
        )?;
        assert_eq!(preview.families_recovered, 1);
        assert_eq!(preview.family_branches_removed, 1);
        assert_eq!(
            family::load(&codex)?.families["surviving-active"]
                .chain
                .len(),
            2
        );

        let report = prune_orphan_entries_with_lock(
            codex.to_string_lossy().into_owned(),
            false,
            false,
            true,
            false,
            &family::FamilyLock::default(),
        )?;
        let restored = family::load(&codex)?;
        let family_record = &restored.families["surviving-active"];
        assert_eq!(report.families_recovered, 1);
        assert_eq!(report.family_branches_removed, 1);
        assert!(report.families_skipped.is_empty());
        assert_eq!(family_record.active_id, "surviving-active");
        assert_eq!(family_record.chain.len(), 1);
        assert!(matches!(
            family_record.chain[0].status,
            BranchStatus::Active
        ));
        assert!(!restored.index.contains_key("missing-new-active"));
        fs::remove_dir_all(&codex).ok();
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
