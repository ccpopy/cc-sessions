use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::params;

use crate::error::{AppError, AppResult};
use crate::history;
use crate::logs_db;
use crate::models::{DeleteResult, ProjectGroup, SessionSummary};
use crate::paths;
use crate::state_db;

fn provider_or_codex(provider: Option<String>) -> String {
    provider.unwrap_or_else(|| "codex".to_string())
}

fn query_summaries(
    codex_dir: &Path,
    where_clause: &str,
    params: &[&dyn rusqlite::ToSql],
) -> AppResult<Vec<SessionSummary>> {
    let state = state_db::open_ro(codex_dir)?;
    let logs_conn = logs_db::open_ro(codex_dir).ok();

    let sql = format!(
        "SELECT id, rollout_path, cwd, title, COALESCE(first_user_message,''), model, reasoning_effort,
                COALESCE(tokens_used,0), created_at, updated_at, COALESCE(archived,0),
                git_branch, source, agent_nickname, agent_role
         FROM threads
         {where_clause}
         ORDER BY updated_at DESC"
    );
    let mut stmt = state.prepare(&sql)?;

    let rows: Vec<SessionSummary> = stmt
        .query_map(params, |row| {
            let id: String = row.get(0)?;
            let rollout_path_raw: String = row.get(1)?;
            let cwd_raw: String = row.get(2)?;
            let title: String = row.get(3)?;
            let first_user_message: String = row.get(4)?;
            let model: Option<String> = row.get(5)?;
            let reasoning_effort: Option<String> = row.get(6)?;
            let tokens_used: i64 = row.get(7)?;
            let created_at: i64 = row.get(8)?;
            let updated_at: i64 = row.get(9)?;
            let archived: i64 = row.get(10)?;
            let git_branch: Option<String> = row.get(11)?;
            let source: Option<String> = row.get(12)?;
            let agent_nickname: Option<String> = row.get(13)?;
            let agent_role: Option<String> = row.get(14)?;

            let rollout_path =
                paths::host_path_string_from_codex_record(codex_dir, &rollout_path_raw);
            let cwd = paths::host_path_string_from_codex_record(codex_dir, &cwd_raw);
            let cwd_display = paths::basename_display(&cwd);

            let rollout_bytes = fs::metadata(&rollout_path).map(|m| m.len()).unwrap_or(0);

            let resume_command = format!("codex resume {}", id);
            Ok(SessionSummary {
                provider: "codex".into(),
                id,
                resume_command,
                rollout_path,
                cwd,
                cwd_display,
                title,
                first_user_message,
                model,
                reasoning_effort,
                source,
                agent_nickname,
                agent_role,
                tokens_used,
                created_at,
                updated_at,
                archived: archived != 0,
                git_branch,
                rollout_bytes,
                logs_count: 0,
                has_backup: false,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // 补充 logs_count（批量预查，避免 N+1）
    // NOTE: 在 SQL 层过滤 NULL / 空 thread_id，避免 `r.get::<_, String>(0)` 在 NULL 上报
    // "Invalid column type Null"。某些历史数据里 logs.thread_id 存在 NULL 值。
    let mut out = rows;
    for s in out.iter_mut() {
        if s.tokens_used <= 0 {
            s.tokens_used = rollout_token_total(&s.rollout_path);
        }
    }
    if let Some(conn) = logs_conn {
        let mut counts: HashMap<String, i64> = HashMap::new();
        let mut stmt = conn.prepare(
            "SELECT thread_id, COUNT(*) FROM logs \
             WHERE thread_id IS NOT NULL AND thread_id != '' \
             GROUP BY thread_id",
        )?;
        let iter = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for it in iter {
            let (id, count) = it?;
            counts.insert(id, count);
        }
        for s in out.iter_mut() {
            if let Some(c) = counts.get(&s.id) {
                s.logs_count = *c;
            }
        }
    }
    Ok(out)
}

fn rollout_token_total(rollout_path: &str) -> i64 {
    let cleaned = paths::strip_verbatim(rollout_path);
    crate::rollout::read_rollout_token_total(Path::new(&cleaned)).unwrap_or(0)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn list_sessions(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
) -> AppResult<Vec<SessionSummary>> {
    match provider_or_codex(provider).as_str() {
        "codex" => {
            let p = PathBuf::from(&codex_dir);
            let mut list = query_summaries(&p, "", &[])?;
            // 官方 Codex app 归档会把 rollout 移到 archived_sessions/；
            // threads 记录缺失或漂移时，从归档目录补扫，保证归档会话可见。
            let extra = supplement_archived_summaries(&p, &list);
            if !extra.is_empty() {
                list.extend(extra);
                list.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
            }
            Ok(list)
        }
        "claude" => {
            let p = PathBuf::from(
                claude_dir
                    .unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned()),
            );
            crate::claude_sessions::scan_sessions(&p)
        }
        other => Err(AppError::Other(format!("不支持的 provider: {other}"))),
    }
}

/// 扫描 archived_sessions/ 下 threads 表没有覆盖的 rollout，合成归档态摘要。
fn supplement_archived_summaries(
    codex_dir: &Path,
    existing: &[SessionSummary],
) -> Vec<SessionSummary> {
    let known_names: std::collections::HashSet<String> = existing
        .iter()
        .filter_map(|s| {
            Path::new(&s.rollout_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .collect();
    let mut out = Vec::new();
    for p in crate::family::scan_archived_rollouts(codex_dir) {
        let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if known_names.contains(&name) {
            continue;
        }
        let Ok(Some(brief)) = crate::repair::read_rollout_brief(codex_dir, &p) else {
            continue;
        };
        if existing.iter().any(|s| s.id == brief.id) {
            continue;
        }
        let cwd = brief.cwd.clone().unwrap_or_default();
        let cwd_display = paths::basename_display(&cwd);
        let title: String = brief.first_user_message.chars().take(80).collect();
        out.push(SessionSummary {
            provider: "codex".into(),
            resume_command: format!("codex resume {}", brief.id),
            id: brief.id.clone(),
            rollout_path: p.to_string_lossy().into_owned(),
            cwd,
            cwd_display,
            title,
            first_user_message: brief.first_user_message.clone(),
            model: brief.model.clone(),
            reasoning_effort: brief.reasoning_effort.clone(),
            source: brief.source.clone(),
            agent_nickname: None,
            agent_role: None,
            tokens_used: brief.tokens_used,
            created_at: brief.created_at_ms / 1000,
            updated_at: brief.updated_at_ms / 1000,
            archived: true,
            git_branch: None,
            rollout_bytes: fs::metadata(&p).map(|m| m.len()).unwrap_or(0),
            logs_count: 0,
            has_backup: false,
        });
    }
    out
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn group_sessions_by_project(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
) -> AppResult<Vec<ProjectGroup>> {
    let list = list_sessions(provider, codex_dir, claude_dir)?;
    let mut groups: HashMap<String, ProjectGroup> = HashMap::new();
    for s in list {
        let key = s.cwd.clone();
        let disp = s.cwd_display.clone();
        let tokens = s.tokens_used;
        let updated = s.updated_at;
        let g = groups.entry(key.clone()).or_insert(ProjectGroup {
            cwd: key,
            cwd_display: disp,
            sessions: Vec::new(),
            latest_updated_at: 0,
            total_tokens: 0,
        });
        g.latest_updated_at = g.latest_updated_at.max(updated);
        g.total_tokens += tokens;
        g.sessions.push(s);
    }
    let mut out: Vec<ProjectGroup> = groups.into_values().collect();
    out.sort_by_key(|g| std::cmp::Reverse(g.latest_updated_at));
    Ok(out)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn search_sessions(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    query: String,
) -> AppResult<Vec<SessionSummary>> {
    let q = query.trim();
    if q.is_empty() {
        return list_sessions(provider, codex_dir, claude_dir);
    }
    let all = list_sessions(provider, codex_dir, claude_dir)?;
    let low = q.to_lowercase();

    // 前缀/过滤：id: cwd: model: archived:
    let (key, val) = if let Some((k, v)) = q.split_once(':') {
        let key = k.trim().to_lowercase();
        if matches!(key.as_str(), "id" | "cwd" | "model" | "archived") {
            (Some(key), v.trim().to_lowercase())
        } else {
            (None, low.clone())
        }
    } else {
        (None, low.clone())
    };

    let hits: Vec<SessionSummary> = all
        .into_iter()
        .filter(|s| match key.as_deref() {
            Some("id") => s.id.to_lowercase().starts_with(&val),
            Some("cwd") => s.cwd.to_lowercase().contains(&val),
            Some("model") => s
                .model
                .as_deref()
                .map(|m| m.to_lowercase().contains(&val))
                .unwrap_or(false),
            Some("archived") => {
                let truthy = matches!(val.as_str(), "true" | "1" | "yes" | "on");
                s.archived == truthy
            }
            _ => {
                let id_hit = {
                    let hex_like = val.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
                    hex_like && val.len() >= 4 && s.id.to_lowercase().starts_with(&val)
                };
                id_hit
                    || s.title.to_lowercase().contains(&val)
                    || s.first_user_message.to_lowercase().contains(&val)
                    || s.source
                        .as_deref()
                        .map(|x| x.to_lowercase().contains(&val))
                        .unwrap_or(false)
                    || s.agent_nickname
                        .as_deref()
                        .map(|x| x.to_lowercase().contains(&val))
                        .unwrap_or(false)
                    || s.agent_role
                        .as_deref()
                        .map(|x| x.to_lowercase().contains(&val))
                        .unwrap_or(false)
                    || s.cwd.to_lowercase().contains(&val)
            }
        })
        .collect();
    Ok(hits)
}

pub fn session_is_subagent(session: &SessionSummary) -> bool {
    crate::repair::is_subagent_source(session.source.as_deref())
        || session
            .agent_nickname
            .as_deref()
            .is_some_and(|v| !v.trim().is_empty())
        || session
            .agent_role
            .as_deref()
            .is_some_and(|v| !v.trim().is_empty())
}

/// 按官方 Codex app 的归档语义执行：
/// - 归档：rollout 移入 `archived_sessions/`，threads 行 `archived=1`、
///   `archived_at` 置时间、`rollout_path` 指向新位置，并从 session_index 移除；
/// - 取消归档：rollout 按文件名日期移回 `sessions/YYYY/MM/DD/`，threads 行
///   复位，并补回 session_index 行。
#[cfg_attr(feature = "desktop", tauri::command)]
pub fn set_archived(
    provider: Option<String>,
    codex_dir: String,
    id: String,
    v: bool,
) -> AppResult<()> {
    if provider_or_codex(provider) != "codex" {
        return Err(AppError::Other("Claude 会话不支持归档".into()));
    }
    let codex = PathBuf::from(&codex_dir);
    let state = state_db::open(&codex)?;

    // 1) 定位当前 rollout 文件：优先 threads 记录，缺失/漂移时按文件名兜底
    let db_path: Option<String> = state
        .query_row(
            "SELECT rollout_path FROM threads WHERE id = ?",
            [&id],
            |r| r.get(0),
        )
        .ok();
    let mut current: Option<PathBuf> = db_path
        .as_ref()
        .map(|raw| {
            PathBuf::from(paths::strip_verbatim(
                &paths::host_path_string_from_codex_record(&codex, raw),
            ))
        })
        .filter(|p| p.is_file());
    if current.is_none() {
        current = find_rollout_file_by_id(&codex, &id);
    }
    let Some(current) = current else {
        return Err(AppError::NotFound(format!(
            "找不到会话 {id} 的 rollout 文件"
        )));
    };
    let Some(file_name) = current.file_name().map(|n| n.to_os_string()) else {
        return Err(AppError::Other("rollout 路径缺少文件名".into()));
    };

    // 2) 移动文件到目标位置
    let target = if v {
        paths::archived_sessions_dir(&codex).join(&file_name)
    } else {
        let (y, m, d) = active_rollout_date(&current, &file_name.to_string_lossy());
        paths::sessions_dir(&codex)
            .join(y)
            .join(m)
            .join(d)
            .join(&file_name)
    };
    if current != target {
        if target.exists() {
            return Err(AppError::Other(format!(
                "目标位置已存在同名文件，取消操作: {}",
                target.to_string_lossy()
            )));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&current, &target)?;
        // 归档后尽量清理空的 YYYY/MM/DD 目录
        if v {
            if let Some(day) = current.parent() {
                let _ = fs::remove_dir(day);
                if let Some(month) = day.parent() {
                    let _ = fs::remove_dir(month);
                    if let Some(year) = month.parent() {
                        let _ = fs::remove_dir(year);
                    }
                }
            }
        }
    }

    // 3) threads 行更新；记录缺失时尝试从 rollout 重建
    let now = chrono::Utc::now().timestamp();
    let target_str = target.to_string_lossy().into_owned();
    let updated = state.execute(
        "UPDATE threads SET archived = ?1, archived_at = CASE WHEN ?1 = 1 THEN ?2 ELSE NULL END, rollout_path = ?3 WHERE id = ?4",
        params![if v { 1 } else { 0 }, now, target_str, id],
    )?;
    if updated == 0 {
        let _ = crate::repair::upsert_thread_from_rollout(&codex, &state, &target, v);
    }

    // 4) session_index 维护：归档移除、取消归档补回（官方索引只含活跃会话）
    let index_path = paths::session_index_path(&codex);
    if v {
        if index_path.exists() {
            filter_index_file(&index_path, &id)?;
        }
    } else {
        let thread_name: String = state
            .query_row(
                "SELECT COALESCE(NULLIF(title,''), COALESCE(first_user_message,'')) FROM threads WHERE id = ?",
                [&id],
                |r| r.get(0),
            )
            .unwrap_or_default();
        crate::repair::append_index_line(&codex, &id, &thread_name, &target)?;
    }
    Ok(())
}

/// 从 rollout 文件名（rollout-YYYY-MM-DDTHH-MM-SS-<uuid>.jsonl）推导归属日期；
/// 文件名不规范时回退到 session_meta 时间戳，再回退到文件修改时间。
fn active_rollout_date(current: &Path, file_name: &str) -> (String, String, String) {
    if let Some(rest) = file_name.strip_prefix("rollout-") {
        let b = rest.as_bytes();
        let digits = |range: std::ops::Range<usize>| b[range].iter().all(|c| c.is_ascii_digit());
        if b.len() >= 10
            && digits(0..4)
            && b[4] == b'-'
            && digits(5..7)
            && b[7] == b'-'
            && digits(8..10)
        {
            return (
                rest[0..4].to_string(),
                rest[5..7].to_string(),
                rest[8..10].to_string(),
            );
        }
    }
    if let Ok(meta) = crate::family::read_session_meta(current) {
        let ts = meta
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                meta.get("payload")
                    .and_then(|p| p.get("timestamp"))
                    .and_then(serde_json::Value::as_str)
            })
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
        if let Some(dt) = ts {
            return (
                dt.format("%Y").to_string(),
                dt.format("%m").to_string(),
                dt.format("%d").to_string(),
            );
        }
    }
    let dt: chrono::DateTime<chrono::Utc> = fs::metadata(current)
        .and_then(|m| m.modified())
        .map(chrono::DateTime::<chrono::Utc>::from)
        .unwrap_or_else(|_| chrono::Utc::now());
    (
        dt.format("%Y").to_string(),
        dt.format("%m").to_string(),
        dt.format("%d").to_string(),
    )
}

/// 在 sessions/ 与 archived_sessions/ 中按文件名（内嵌会话 uuid）查找 rollout。
fn find_rollout_file_by_id(codex_dir: &Path, id: &str) -> Option<PathBuf> {
    if id.trim().is_empty() {
        return None;
    }
    crate::family::scan_archived_rollouts(codex_dir)
        .into_iter()
        .chain(crate::family::scan_rollouts(codex_dir))
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(id))
        })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn delete_session(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    id: String,
) -> AppResult<DeleteResult> {
    match provider_or_codex(provider).as_str() {
        "codex" => {
            let p = PathBuf::from(&codex_dir);
            delete_one(&p, &id)
        }
        "claude" => {
            let dir = claude_dir
                .unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned());
            delete_one_claude(Path::new(&dir), &id)
        }
        other => Err(AppError::Other(format!("不支持的 provider: {other}"))),
    }
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn delete_sessions(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    ids: Vec<String>,
) -> AppResult<Vec<DeleteResult>> {
    match provider_or_codex(provider).as_str() {
        "codex" => {
            let p = PathBuf::from(&codex_dir);
            Ok(ids
                .iter()
                .map(|id| {
                    delete_one(&p, id).unwrap_or_else(|e| DeleteResult {
                        id: id.clone(),
                        threads_rows_deleted: 0,
                        logs_rows_deleted: 0,
                        history_rows_deleted: 0,
                        rollout_deleted: false,
                        rollout_missing: false,
                        ok: false,
                        error: Some(e.to_string()),
                    })
                })
                .collect())
        }
        "claude" => {
            let dir = PathBuf::from(
                claude_dir
                    .unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned()),
            );
            Ok(ids
                .iter()
                .map(|id| {
                    delete_one_claude(&dir, id).unwrap_or_else(|e| DeleteResult {
                        id: id.clone(),
                        threads_rows_deleted: 0,
                        logs_rows_deleted: 0,
                        history_rows_deleted: 0,
                        rollout_deleted: false,
                        rollout_missing: false,
                        ok: false,
                        error: Some(e.to_string()),
                    })
                })
                .collect())
        }
        other => Err(AppError::Other(format!("不支持的 provider: {other}"))),
    }
}

fn delete_one_claude(claude_dir: &Path, id: &str) -> AppResult<DeleteResult> {
    let mut result = DeleteResult {
        id: id.to_string(),
        threads_rows_deleted: 0,
        logs_rows_deleted: 0,
        history_rows_deleted: 0,
        rollout_deleted: false,
        rollout_missing: false,
        ok: false,
        error: None,
    };

    let projects = paths::claude_projects_dir(claude_dir);
    if !projects.is_dir() {
        result.rollout_missing = true;
        append_error(&mut result, "claude projects 目录不存在".into());
        return Ok(result);
    }

    let target_filename = format!("{id}.jsonl");
    let mut jsonl_path: Option<PathBuf> = None;
    find_jsonl_by_name(&projects, &target_filename, &mut jsonl_path);

    if let Some(jsonl) = jsonl_path {
        match fs::remove_file(&jsonl) {
            Ok(_) => result.rollout_deleted = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                result.rollout_missing = true;
            }
            Err(e) => {
                append_error(&mut result, format!("rollout remove failed: {}", e));
                return Ok(result);
            }
        }

        if let Some(stem) = jsonl.file_stem() {
            let sidecar = jsonl.with_file_name(stem);
            if sidecar.is_dir() {
                if let Err(e) = fs::remove_dir_all(&sidecar) {
                    append_error(&mut result, format!("sidecar remove failed: {}", e));
                }
            }
        }

        if let Some(parent) = jsonl.parent() {
            let _ = fs::remove_dir(parent);
        }
    } else {
        result.rollout_missing = true;
    }

    let history_path = paths::history_path(claude_dir);
    if history_path.exists() {
        match history::filter_file(&history_path, id) {
            Ok(rows) => result.history_rows_deleted = rows,
            Err(e) => append_error(&mut result, format!("history filter failed: {}", e)),
        }
    }

    result.ok = result.rollout_deleted || result.history_rows_deleted > 0;
    Ok(result)
}

fn find_jsonl_by_name(dir: &Path, target: &str, found: &mut Option<PathBuf>) {
    if found.is_some() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_jsonl_by_name(&path, target, found);
            if found.is_some() {
                return;
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(target) {
            *found = Some(path);
            return;
        }
    }
}

pub(crate) fn delete_one_for_family(codex_dir: &Path, id: &str) -> AppResult<DeleteResult> {
    delete_one(codex_dir, id)
}

fn delete_one(codex_dir: &Path, id: &str) -> AppResult<DeleteResult> {
    let mut result = DeleteResult {
        id: id.to_string(),
        threads_rows_deleted: 0,
        logs_rows_deleted: 0,
        history_rows_deleted: 0,
        rollout_deleted: false,
        rollout_missing: false,
        ok: false,
        error: None,
    };

    // 1) 查出 rollout_path
    let state = state_db::open(codex_dir)?;
    let rollout_path: Option<String> = state
        .query_row("SELECT rollout_path FROM threads WHERE id = ?", [id], |r| {
            r.get(0)
        })
        .ok();

    // 2) 事务删 threads（外键级联 thread_dynamic_tools / stage1_outputs / thread_spawn_edges）
    let rows = {
        let tx = state.unchecked_transaction()?;
        let n = tx.execute("DELETE FROM threads WHERE id = ?", [id])?;
        tx.commit()?;
        n
    };
    result.threads_rows_deleted = rows as u32;

    // 3) 事务删 logs。logs_2.sqlite 可能在新版本或精简环境中不存在；
    // 这不应掩盖 threads 已删除的事实，但需要显式反馈给前端。
    match logs_db::open(codex_dir) {
        Ok(logs) => {
            let logs_result: AppResult<usize> = (|| {
                let tx = logs.unchecked_transaction()?;
                let n = tx.execute("DELETE FROM logs WHERE thread_id = ?", [id])?;
                tx.commit()?;
                Ok(n)
            })();
            match logs_result {
                Ok(rows_logs) => {
                    result.logs_rows_deleted = rows_logs as u32;
                }
                Err(e) => append_error(&mut result, format!("logs delete failed: {}", e)),
            }
        }
        Err(e) => append_error(&mut result, format!("logs database unavailable: {}", e)),
    }

    // 4) 直接删除 rollout 文件（失败不回滚）。threads 记录缺失或路径漂移时
    //    按文件名兜底搜索（覆盖官方归档在 archived_sessions/ 的文件）。
    let mut file_to_remove: Option<PathBuf> = rollout_path
        .as_ref()
        .map(|path| PathBuf::from(paths::strip_verbatim(path)));
    if file_to_remove
        .as_ref()
        .map(|p| !p.is_file())
        .unwrap_or(true)
    {
        if let Some(found) = find_rollout_file_by_id(codex_dir, id) {
            file_to_remove = Some(found);
        }
    }
    if let Some(cleaned) = file_to_remove {
        match fs::remove_file(&cleaned) {
            Ok(_) => {
                result.rollout_deleted = true;
                // 尝试清理空的 YYYY/MM/DD 目录
                if let Some(parent) = cleaned.parent() {
                    let _ = fs::remove_dir(parent);
                    if let Some(month) = parent.parent() {
                        let _ = fs::remove_dir(month);
                        if let Some(year) = month.parent() {
                            let _ = fs::remove_dir(year);
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                result.rollout_missing = true;
            }
            Err(e) => {
                result.rollout_deleted = false;
                append_error(&mut result, format!("rollout remove failed: {}", e));
            }
        }
    } else {
        result.rollout_missing = true;
    }

    // 5) 过滤 session_index.jsonl
    let index_path = codex_dir.join("session_index.jsonl");
    if index_path.exists() {
        if let Err(e) = filter_index_file(&index_path, id) {
            append_error(&mut result, format!("session_index filter failed: {}", e));
        }
    }

    result.ok = result.threads_rows_deleted > 0 || result.rollout_deleted;
    Ok(result)
}

fn append_error(result: &mut DeleteResult, msg: String) {
    result.error = Some(match result.error.take() {
        Some(prev) => format!("{prev}; {msg}"),
        None => msg,
    });
}

fn filter_index_file(path: &Path, id: &str) -> AppResult<()> {
    let content = fs::read_to_string(path)?;
    let tmp = path.with_extension("jsonl.tmp");
    {
        use std::io::Write;
        let mut f = fs::File::create(&tmp)?;
        for line in content.lines() {
            if line.is_empty() {
                continue;
            }
            let keep = match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) => {
                    v.get("id").and_then(|x| x.as_str()) != Some(id)
                        && v.get("session_id").and_then(|x| x.as_str()) != Some(id)
                }
                Err(_) => true,
            };
            if keep {
                writeln!(f, "{}", line)?;
            }
        }
    }
    fs::rename(&tmp, path).map_err(AppError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use std::io::Write;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("cc-sessions-{name}-{}-{nanos}", std::process::id()))
    }

    fn create_codex_threads_table(codex: &Path) -> AppResult<rusqlite::Connection> {
        fs::create_dir_all(codex.join("sessions"))?;
        let conn = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        conn.execute(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT,
                cwd TEXT,
                title TEXT,
                first_user_message TEXT,
                model TEXT,
                reasoning_effort TEXT,
                tokens_used INTEGER,
                created_at INTEGER,
                updated_at INTEGER,
                archived INTEGER,
                archived_at INTEGER,
                git_branch TEXT,
                source TEXT,
                agent_nickname TEXT,
                agent_role TEXT
            )",
            [],
        )?;
        Ok(conn)
    }

    #[test]
    fn list_sessions_reads_rollout_tokens_when_thread_cache_is_zero() -> AppResult<()> {
        let codex = temp_dir("codex-token-fallback");
        let rollout = codex.join("sessions").join("rollout-codex-token.jsonl");
        fs::create_dir_all(rollout.parent().expect("rollout parent"))?;
        {
            let mut out = fs::File::create(&rollout)?;
            for value in [
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "total_tokens": 1234
                            }
                        }
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "total_tokens": 2_468_000
                            }
                        }
                    }
                }),
            ] {
                writeln!(out, "{}", serde_json::to_string(&value)?)?;
            }
        }
        let conn = create_codex_threads_table(&codex)?;
        conn.execute(
            "INSERT INTO threads (
                id, rollout_path, cwd, title, first_user_message, model, reasoning_effort,
                tokens_used, created_at, updated_at, archived, git_branch, source,
                agent_nickname, agent_role
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 0, 1770000000, 1770000300, 0, NULL, NULL, NULL, NULL)",
            (
                "codex-token",
                rollout.to_string_lossy().into_owned(),
                "F:\\work\\codex-project",
                "Codex title",
                "hello codex",
                "gpt-5",
            ),
        )?;
        drop(conn);

        let sessions = list_sessions(
            Some("codex".to_string()),
            codex.to_string_lossy().into_owned(),
            None,
        )?;
        fs::remove_dir_all(&codex).ok();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].tokens_used, 2_468_000);
        Ok(())
    }

    #[test]
    fn delete_claude_session_prunes_matching_history_rows_only() {
        let claude = temp_dir("claude-delete-history");
        let project = claude.join("projects").join("-tmp-project");
        fs::create_dir_all(&project).expect("create project dir");

        let target_id = "claude-target-session";
        let other_id = "claude-other-session";
        fs::write(project.join(format!("{target_id}.jsonl")), "{}\n").expect("write session");
        fs::write(
            claude.join("history.jsonl"),
            format!(
                "{{\"session_id\":\"{target_id}\",\"message\":\"first\"}}\n\
                 {{\"id\":\"{target_id}\",\"message\":\"second\"}}\n\
                 {{\"sessionId\":\"{target_id}\",\"message\":\"third\"}}\n\
                 not-json\n\
                 {{\"session_id\":\"{other_id}\",\"message\":\"keep\"}}\n"
            ),
        )
        .expect("write history");

        let result = delete_one_claude(&claude, target_id).expect("delete claude session");

        assert!(result.ok);
        assert!(result.rollout_deleted);
        assert_eq!(result.history_rows_deleted, 3);
        assert!(!project.join(format!("{target_id}.jsonl")).exists());

        let history = fs::read_to_string(claude.join("history.jsonl")).expect("read history");
        assert!(!history.contains(target_id));
        assert!(history.contains(other_id));
        assert!(history.contains("not-json"));

        fs::remove_dir_all(claude).expect("cleanup temp dir");
    }

    #[test]
    fn delete_claude_session_prunes_history_even_when_jsonl_is_missing() {
        let claude = temp_dir("claude-delete-missing-jsonl-history");
        let project = claude.join("projects").join("-tmp-project");
        fs::create_dir_all(&project).expect("create project dir");

        let target_id = "claude-target-session";
        let other_id = "claude-other-session";
        fs::write(
            claude.join("history.jsonl"),
            format!(
                "{{\"sessionId\":\"{target_id}\",\"message\":\"delete\"}}\n\
                 {{\"sessionId\":\"{other_id}\",\"message\":\"keep\"}}\n"
            ),
        )
        .expect("write history");

        let result = delete_one_claude(&claude, target_id).expect("delete claude session");

        assert!(result.ok);
        assert!(result.rollout_missing);
        assert!(!result.rollout_deleted);
        assert_eq!(result.history_rows_deleted, 1);

        let history = fs::read_to_string(claude.join("history.jsonl")).expect("read history");
        assert!(!history.contains(target_id));
        assert!(history.contains(other_id));

        fs::remove_dir_all(claude).expect("cleanup temp dir");
    }

    const ARCHIVE_TEST_ID: &str = "019dtest-1111-7000-8000-000000000001";

    fn write_test_rollout(path: &Path, id: &str, msg: &str) {
        fs::create_dir_all(path.parent().expect("rollout parent")).expect("mkdir");
        let meta = serde_json::json!({
            "timestamp": "2026-05-10T10:00:00Z",
            "type": "session_meta",
            "payload": {"id": id, "timestamp": "2026-05-10T10:00:00Z", "cwd": "F:\\w"}
        });
        let user = serde_json::json!({
            "timestamp": "2026-05-10T10:00:01Z",
            "type": "event_msg",
            "payload": {"type": "user_message", "message": msg}
        });
        fs::write(
            path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&meta).unwrap(),
                serde_json::to_string(&user).unwrap()
            ),
        )
        .expect("write rollout");
    }

    fn archive_fixture(codex: &Path) -> PathBuf {
        let active = codex
            .join("sessions")
            .join("2026")
            .join("05")
            .join("10")
            .join(format!(
                "rollout-2026-05-10T10-00-00-{ARCHIVE_TEST_ID}.jsonl"
            ));
        write_test_rollout(&active, ARCHIVE_TEST_ID, "hello archive");
        let conn = create_codex_threads_table(codex).expect("create table");
        conn.execute(
            "INSERT INTO threads (
                id, rollout_path, cwd, title, first_user_message, model, reasoning_effort,
                tokens_used, created_at, updated_at, archived, archived_at, git_branch, source,
                agent_nickname, agent_role
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 0, 1770000000, 1770000300, 0, NULL, NULL, NULL, NULL, NULL)",
            (
                ARCHIVE_TEST_ID,
                active.to_string_lossy().into_owned(),
                "F:\\w",
                "archive test",
                "hello archive",
                "gpt-5",
            ),
        )
        .expect("insert thread");
        fs::write(
            codex.join("session_index.jsonl"),
            format!(
                "{{\"id\":\"{ARCHIVE_TEST_ID}\",\"thread_name\":\"archive test\",\"updated_at\":\"2026-05-10T10:00:00Z\"}}\n{{\"id\":\"other-id\",\"thread_name\":\"keep\",\"updated_at\":\"2026-05-10T10:00:00Z\"}}\n"
            ),
        )
        .expect("write index");
        active
    }

    #[test]
    fn archive_moves_rollout_updates_threads_and_index() -> AppResult<()> {
        let codex = temp_dir("codex-archive");
        let active = archive_fixture(&codex);
        let codex_str = codex.to_string_lossy().into_owned();

        set_archived(
            Some("codex".into()),
            codex_str.clone(),
            ARCHIVE_TEST_ID.into(),
            true,
        )?;

        let archived_path = codex
            .join("archived_sessions")
            .join(active.file_name().unwrap());
        assert!(!active.exists(), "归档后活跃位置不应再有文件");
        assert!(archived_path.is_file(), "文件应移动到 archived_sessions/");

        let conn = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        let (archived, archived_at, rollout_path): (i64, Option<i64>, String) = conn.query_row(
            "SELECT archived, archived_at, rollout_path FROM threads WHERE id = ?",
            [ARCHIVE_TEST_ID],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        assert_eq!(archived, 1);
        assert!(archived_at.is_some());
        assert!(rollout_path.contains("archived_sessions"));

        let index = fs::read_to_string(codex.join("session_index.jsonl"))?;
        assert!(!index.contains(ARCHIVE_TEST_ID), "归档应移除索引行");
        assert!(index.contains("other-id"), "其他索引行应保留");
        drop(conn);

        // 取消归档：搬回原日期目录、复位 threads、补回索引
        set_archived(
            Some("codex".into()),
            codex_str,
            ARCHIVE_TEST_ID.into(),
            false,
        )?;
        assert!(
            active.is_file(),
            "取消归档应按文件名日期移回 sessions/YYYY/MM/DD/"
        );
        assert!(!archived_path.exists());

        let conn = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        let (archived, archived_at): (i64, Option<i64>) = conn.query_row(
            "SELECT archived, archived_at FROM threads WHERE id = ?",
            [ARCHIVE_TEST_ID],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert_eq!(archived, 0);
        assert!(archived_at.is_none());
        let index = fs::read_to_string(codex.join("session_index.jsonl"))?;
        assert!(index.contains(ARCHIVE_TEST_ID), "取消归档应补回索引行");

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn list_sessions_includes_orphan_archived_rollout() -> AppResult<()> {
        let codex = temp_dir("codex-orphan-archived");
        // threads 表为空，但 archived_sessions/ 有官方归档留下的 rollout
        let _conn = create_codex_threads_table(&codex)?;
        let orphan = codex.join("archived_sessions").join(format!(
            "rollout-2026-05-10T10-00-00-{ARCHIVE_TEST_ID}.jsonl"
        ));
        write_test_rollout(&orphan, ARCHIVE_TEST_ID, "orphan archived hello");

        let sessions = list_sessions(
            Some("codex".into()),
            codex.to_string_lossy().into_owned(),
            None,
        )?;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, ARCHIVE_TEST_ID);
        assert!(sessions[0].archived, "补扫出的归档会话应标记 archived");
        assert_eq!(sessions[0].first_user_message, "orphan archived hello");

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn delete_orphan_archived_rollout_succeeds() -> AppResult<()> {
        let codex = temp_dir("codex-del-orphan-archived");
        let _conn = create_codex_threads_table(&codex)?;
        let orphan = codex.join("archived_sessions").join(format!(
            "rollout-2026-05-10T10-00-00-{ARCHIVE_TEST_ID}.jsonl"
        ));
        write_test_rollout(&orphan, ARCHIVE_TEST_ID, "to delete");

        let result = delete_one(&codex, ARCHIVE_TEST_ID)?;
        assert!(result.ok, "threads 无记录但删掉了文件也应算成功");
        assert!(result.rollout_deleted);
        assert_eq!(result.threads_rows_deleted, 0);
        assert!(!orphan.exists());

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }
}
