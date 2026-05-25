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
            let rollout_path: String = row.get(1)?;
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

            let cwd = paths::strip_verbatim(&cwd_raw);
            let cwd_display = paths::basename_display(&cwd_raw);

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

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn list_sessions(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
) -> AppResult<Vec<SessionSummary>> {
    match provider_or_codex(provider).as_str() {
        "codex" => {
            let p = PathBuf::from(&codex_dir);
            query_summaries(&p, "", &[])
        }
        "claude" => {
            let p = PathBuf::from(
                claude_dir
                    .unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned()),
            );
            crate::claude_sessions::scan_sessions(&p)
        }
        "gemini" => {
            let p = PathBuf::from(
                claude_dir
                    .unwrap_or_else(|| paths::default_gemini_dir().to_string_lossy().into_owned()),
            );
            crate::gemini_sessions::scan_sessions(&p)
        }
        other => Err(AppError::Other(format!("不支持的 provider: {other}"))),
    }
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
        if matches!(k, "id" | "cwd" | "model" | "archived") {
            (Some(k.to_string()), v.trim().to_lowercase())
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

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn set_archived(
    provider: Option<String>,
    codex_dir: String,
    id: String,
    v: bool,
) -> AppResult<()> {
    if provider_or_codex(provider) != "codex" {
        return Err(AppError::Other("该 provider 不支持归档".into()));
    }
    let p = PathBuf::from(&codex_dir);
    let conn = state_db::open(&p)?;
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "UPDATE threads SET archived = ?, archived_at = CASE WHEN ?=1 THEN ? ELSE NULL END WHERE id = ?",
        params![if v { 1 } else { 0 }, if v { 1 } else { 0 }, now, id],
    )?;
    Ok(())
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
        "gemini" => {
            let dir = claude_dir
                .unwrap_or_else(|| paths::default_gemini_dir().to_string_lossy().into_owned());
            crate::gemini_sessions::delete_session(Path::new(&dir), &id)
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
        "gemini" => {
            let dir = PathBuf::from(
                claude_dir
                    .unwrap_or_else(|| paths::default_gemini_dir().to_string_lossy().into_owned()),
            );
            Ok(ids
                .iter()
                .map(|id| {
                    crate::gemini_sessions::delete_session(&dir, id).unwrap_or_else(|e| {
                        DeleteResult {
                            id: id.clone(),
                            threads_rows_deleted: 0,
                            logs_rows_deleted: 0,
                            history_rows_deleted: 0,
                            rollout_deleted: false,
                            rollout_missing: false,
                            ok: false,
                            error: Some(e.to_string()),
                        }
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

    // 4) 直接删除 rollout 文件（失败不回滚）
    if let Some(path) = rollout_path.as_ref() {
        let cleaned = paths::strip_verbatim(path);
        match fs::remove_file(&cleaned) {
            Ok(_) => {
                result.rollout_deleted = true;
                // 尝试清理空的 YYYY/MM/DD 目录
                if let Some(parent) = Path::new(&cleaned).parent() {
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

    result.ok = result.threads_rows_deleted > 0;
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

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("cc-sessions-{name}-{}-{nanos}", std::process::id()))
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
}
