use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension};

use crate::atomic_file;
use crate::error::{AppError, AppResult};
use crate::family;
use crate::history;
use crate::logs_db;
use crate::models::{
    DeleteResult, DeleteTarget, MoveSessionCwdReport, ProjectGroup, SessionSummary,
};
use crate::paths;
use crate::provenance;
use crate::state_db;

fn provider_or_codex(provider: Option<String>) -> String {
    provider.unwrap_or_else(|| "codex".to_string())
}

/// Codex App 的活跃会话列表标题以 session_index.jsonl 的 thread_name 为准。
///
/// state_5.sqlite 的 threads.title 可能仍停留在首条用户消息，即使 Codex App 已经
/// 为会话生成了简短标题。索引不是核心数据库，单行损坏时跳过该行并回退数据库
/// 标题，避免因为可选缓存损坏导致整个会话列表不可用。
fn read_session_index_titles(codex_dir: &Path) -> AppResult<HashMap<String, String>> {
    let path = paths::session_index_path(codex_dir);
    let mut titles = HashMap::new();
    if !path.is_file() {
        return Ok(titles);
    }

    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(id) = value
            .get("id")
            .or_else(|| value.get("session_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let Some(thread_name) = value
            .get("thread_name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
        else {
            continue;
        };
        // 如果存在重复记录，后出现的记录代表较新的标题。
        titles.insert(id.to_string(), thread_name.to_string());
    }
    Ok(titles)
}

/// 返回 Codex 当前对外展示的会话标题：活跃索引优先，数据库标题兜底。
pub(crate) fn codex_display_title(codex_dir: &Path, id: &str) -> AppResult<Option<String>> {
    let index_title = read_session_index_titles(codex_dir)?.remove(id);
    if !paths::state_db_path(codex_dir).is_file() {
        return Ok(index_title);
    }
    let state = state_db::open_ro(codex_dir)?;
    let row = state
        .query_row(
            "SELECT COALESCE(title,''), COALESCE(first_user_message,''), COALESCE(archived,0)
             FROM threads WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    Ok(match row {
        Some((database_title, first_user_message, archived)) => Some(select_codex_title(
            index_title.as_deref(),
            database_title,
            &first_user_message,
            archived != 0,
        )),
        None => index_title,
    })
}

fn select_codex_title(
    index_title: Option<&str>,
    database_title: String,
    first_user_message: &str,
    archived: bool,
) -> String {
    if archived {
        return database_title;
    }

    let database_trimmed = database_title.trim();
    let first_trimmed = first_user_message.trim();
    let index_is_prompt_only = index_title.is_some_and(|title| title.trim() == first_trimmed);
    if !database_trimmed.is_empty() && database_trimmed != first_trimmed && index_is_prompt_only {
        // 旧版互转会把生成标题写入 threads，却把首条提问写入 session_index。
        // 只在这个明确特征下恢复数据库标题，其他活跃会话仍以官方索引为准。
        return database_title;
    }

    index_title.map(String::from).unwrap_or(database_title)
}

fn query_summaries(
    codex_dir: &Path,
    where_clause: &str,
    params: &[&dyn rusqlite::ToSql],
) -> AppResult<Vec<SessionSummary>> {
    let state = state_db::open_ro(codex_dir)?;
    let logs_conn = logs_db::open_ro(codex_dir).ok();
    let index_titles = read_session_index_titles(codex_dir)?;

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
            let database_title: String = row.get(3)?;
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
            let title = select_codex_title(
                index_titles.get(&id).map(String::as_str),
                database_title,
                &first_user_message,
                archived != 0,
            );
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
                conversion_origin: None,
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

pub fn list_sessions(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
) -> AppResult<Vec<SessionSummary>> {
    let codex = PathBuf::from(&codex_dir);
    match provider_or_codex(provider).as_str() {
        "codex" => {
            let mut list = query_summaries(&codex, "", &[])?;
            // 官方 Codex app 归档会把 rollout 移到 archived_sessions/；
            // threads 记录缺失或漂移时，从归档目录补扫，保证归档会话可见。
            let extra = supplement_archived_summaries(&codex, &list)?;
            if !extra.is_empty() {
                list.extend(extra);
                list.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
            }
            provenance::annotate_sessions(&codex, &mut list);
            Ok(list)
        }
        "claude" => {
            let p = PathBuf::from(
                claude_dir
                    .unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned()),
            );
            let mut list = crate::claude_sessions::scan_sessions(&p)?;
            provenance::annotate_sessions(&codex, &mut list);
            Ok(list)
        }
        other => Err(AppError::Other(format!("不支持的 provider: {other}"))),
    }
}

/// 扫描 archived_sessions/ 下 threads 表没有覆盖的 rollout，合成归档态摘要。
fn supplement_archived_summaries(
    codex_dir: &Path,
    existing: &[SessionSummary],
) -> AppResult<Vec<SessionSummary>> {
    let known_names: std::collections::HashSet<String> = existing
        .iter()
        .filter_map(|s| {
            Path::new(&s.rollout_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .collect();
    let mut out = Vec::new();
    for p in crate::family::scan_archived_rollouts(codex_dir)? {
        let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if known_names.contains(&name) {
            continue;
        }
        let Some(brief) = crate::repair::read_rollout_brief(codex_dir, &p)? else {
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
            conversion_origin: None,
            tokens_used: brief.tokens_used,
            created_at: brief.created_at_ms / 1000,
            updated_at: brief.updated_at_ms / 1000,
            archived: true,
            git_branch: None,
            rollout_bytes: fs::metadata(&p)?.len(),
            logs_count: 0,
            has_backup: false,
        });
    }
    Ok(out)
}

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
                    || s.conversion_origin
                        .as_ref()
                        .map(|origin| origin.source_provider.to_lowercase().contains(&val))
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
pub fn set_archived_with_lock(
    provider: Option<String>,
    codex_dir: String,
    id: String,
    v: bool,
    lock: &family::FamilyLock,
) -> AppResult<()> {
    if provider_or_codex(provider) != "codex" {
        return Err(AppError::Other("Claude 会话不支持归档".into()));
    }
    family::with_lock(lock, |_guard| set_archived_codex_locked(codex_dir, id, v))
}

/// 重命名会话：写 threads.title（官方 App 与本软件共用这一名称来源）。
///
/// 同一家族的全部分支一起改名——provider 切换会产生新 id，只改当前分支的话，
/// 切换后名称又会退回首条消息（用户反馈 #8）。返回实际更新的 threads 行数。
pub fn rename_session_with_lock(
    provider: Option<String>,
    codex_dir: String,
    id: String,
    title: String,
    lock: &family::FamilyLock,
) -> AppResult<u32> {
    if provider_or_codex(provider) != "codex" {
        return Err(AppError::Other("Claude 会话暂不支持重命名".into()));
    }
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err(AppError::Other("会话名称不能为空".into()));
    }
    if title.chars().count() > 120 {
        return Err(AppError::Other("会话名称过长（最多 120 个字符）".into()));
    }
    family::with_lock(lock, |_guard| rename_session_locked(codex_dir, id, title))
}

fn rename_session_locked(codex_dir: String, id: String, title: String) -> AppResult<u32> {
    let codex = PathBuf::from(&codex_dir);
    if !paths::state_db_path(&codex).is_file() {
        return Err(AppError::InvalidCodexDir(format!(
            "state_5.sqlite 不存在，无法重命名会话: {}",
            paths::state_db_path(&codex).to_string_lossy()
        )));
    }

    let mut store = family::load(&codex)?;
    let mut ids: Vec<String> = vec![id.clone()];
    let family_id = store.index.get(&id).cloned();
    if let Some(family_id) = family_id.as_ref() {
        if let Some(family) = store.families.get(family_id) {
            ids = family.chain.iter().map(|b| b.id.clone()).collect();
            if !ids.iter().any(|x| x == &id) {
                ids.push(id.clone());
            }
        }
    }

    let state = state_db::open(&codex)?;
    let now = chrono::Utc::now().timestamp();
    let mut renamed = 0u32;
    for sid in &ids {
        // 只更新 updated_at（秒），新 schema 的触发器会自动同步 updated_at_ms。
        // bump updated_at 是为了让官方 App 的水位线增量同步能拉到这次改名，
        // 否则要重启 App 才能看到新名字。
        renamed += state.execute(
            "UPDATE threads SET title = ?1, updated_at = ?2 WHERE id = ?3",
            params![title, now, sid],
        )? as u32;
    }
    if renamed == 0 {
        return Err(AppError::NotFound(format!("threads 中未找到会话 {id}")));
    }

    // session_index.jsonl：仅刷新已有条目的 thread_name（不给归档会话补条目）。
    let index_ids = crate::repair::read_session_index_ids(&codex)?;
    for sid in &ids {
        if index_ids.contains(sid) {
            crate::repair::append_index_line(&codex, sid, &title, Path::new(""))?;
        }
    }

    if let Some(family_id) = family_id {
        if let Some(family) = store.families.get_mut(&family_id) {
            family.title = title.clone();
            family.updated_at = chrono::Utc::now().to_rfc3339();
            family::save(&codex, &store)?;
        }
    }
    Ok(renamed)
}

// ========================= 移动工作目录 (move session cwd) =========================

/// 流式重写 rollout 首行的 payload.cwd，保留原始换行风格和后续字节。
fn rewrite_rollout_cwd(path: &Path, new_cwd: &str) -> AppResult<bool> {
    let fp = atomic_file::fingerprint(path)?;
    let mut source = BufReader::new(File::open(path)?);
    let mut first_line = Vec::new();
    if source.read_until(b'\n', &mut first_line)? == 0 {
        return Err(AppError::Other(format!(
            "rollout 为空，无法修改工作目录: {}",
            path.to_string_lossy()
        )));
    }
    let (json_end, line_ending): (usize, &[u8]) = if first_line.ends_with(b"\r\n") {
        (first_line.len() - 2, b"\r\n")
    } else if first_line.ends_with(b"\n") {
        (first_line.len() - 1, b"\n")
    } else {
        (first_line.len(), b"")
    };
    let mut meta: serde_json::Value = serde_json::from_slice(&first_line[..json_end])
        .map_err(|error| AppError::Other(format!("无法解析 rollout session_meta: {error}")))?;
    if meta.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
        return Err(AppError::Other(
            "rollout 首行不是 session_meta，拒绝修改工作目录".into(),
        ));
    }
    let payload = meta
        .get_mut("payload")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| AppError::Other("rollout session_meta 缺少 payload 字段".into()))?;
    if payload.get("cwd").and_then(serde_json::Value::as_str) == Some(new_cwd) {
        return Ok(false);
    }
    payload.insert("cwd".into(), serde_json::Value::String(new_cwd.to_owned()));
    let updated_first = serde_json::to_vec(&meta)
        .map_err(|error| AppError::Other(format!("无法序列化 session_meta: {error}")))?;

    atomic_file::replace_with_writer_if_unchanged(path, &fp, |file| {
        let mut source = BufReader::new(File::open(path)?);
        let mut discarded_first_line = Vec::new();
        source.read_until(b'\n', &mut discarded_first_line)?;
        file.write_all(&updated_first)?;
        file.write_all(line_ending)?;
        std::io::copy(&mut source, file)?;
        Ok(())
    })?;
    Ok(true)
}

fn resolve_family_ids_for_move(
    store: &crate::models::FamilyStore,
    id: &str,
) -> AppResult<(Option<String>, Vec<String>)> {
    let family_id = family::resolve_family_id_strict(store, id)?;
    let ids = match family_id.as_ref() {
        Some(family_id) => store
            .families
            .get(family_id)
            .ok_or_else(|| AppError::NotFound(format!("family: {family_id}")))?
            .chain
            .iter()
            .map(|branch| branch.id.clone())
            .collect(),
        None => vec![id.to_string()],
    };
    Ok((family_id, ids))
}

/// 定位会话的 rollout 文件路径。
/// 优先 threads 记录，缺失/漂移时按文件名兜底。
fn locate_session_rollout(codex: &Path, id: &str) -> AppResult<PathBuf> {
    let state = state_db::open(codex)?;
    let db_path: Option<String> = match state.query_row(
        "SELECT rollout_path FROM threads WHERE id = ?",
        [id],
        |row| row.get(0),
    ) {
        Ok(path) => Some(path),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) => return Err(error.into()),
    };
    let mut current: Option<PathBuf> = db_path
        .as_ref()
        .map(|raw| {
            PathBuf::from(paths::strip_verbatim(
                &paths::host_path_string_from_codex_record(codex, raw),
            ))
        })
        .filter(|p| p.is_file());
    let mut discovered = rollout_files_by_id(codex, id)?;
    if discovered.len() > 1 {
        return Err(AppError::Other(format!(
            "发现 {} 个同 ID Codex rollout，无法安全移动 cwd，请先修复重复文件: {id}",
            discovered.len()
        )));
    }
    if current.is_none() {
        current = discovered.pop();
    }
    let Some(current) = current else {
        return Err(AppError::NotFound(format!(
            "找不到会话 {id} 的 rollout 文件"
        )));
    };
    validate_codex_rollout_path(codex, &current, id)?;
    Ok(current)
}

fn normalize_move_target_cwd(codex: &Path, target_cwd: &str) -> AppResult<(PathBuf, String)> {
    if target_cwd.chars().any(char::is_control) {
        return Err(AppError::Path("工作目录路径不能包含控制字符".into()));
    }
    let host_path = paths::host_path_from_codex_record(codex, target_cwd);
    if !host_path.is_absolute() {
        return Err(AppError::Path(format!(
            "工作目录必须是绝对路径: {}",
            host_path.to_string_lossy()
        )));
    }
    let canonical = host_path.canonicalize().map_err(|error| {
        AppError::NotFound(format!(
            "工作目录不存在或无法访问: {} ({error})",
            host_path.to_string_lossy()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(AppError::Path(format!(
            "目标工作目录不是文件夹: {}",
            canonical.to_string_lossy()
        )));
    }
    let record_path = paths::codex_record_path_from_host(codex, &canonical)?;
    Ok((canonical, record_path))
}

fn rollout_is_archived(codex: &Path, rollout: &Path) -> bool {
    let archived_root = PathBuf::from(paths::strip_verbatim(
        &paths::archived_sessions_dir(codex).to_string_lossy(),
    ));
    let rollout = PathBuf::from(paths::strip_verbatim(&rollout.to_string_lossy()));
    rollout.starts_with(archived_root)
}

/// 移动会话工作目录的核心逻辑（已持有锁）。
fn move_session_cwd_locked(
    codex_dir: String,
    id: String,
    target_cwd: String,
) -> AppResult<MoveSessionCwdReport> {
    let codex = PathBuf::from(&codex_dir);
    if !paths::state_db_path(&codex).is_file() {
        return Err(AppError::InvalidCodexDir(format!(
            "state_5.sqlite 不存在，无法移动工作目录: {}",
            paths::state_db_path(&codex).to_string_lossy()
        )));
    }

    let (target_host, target_record) = normalize_move_target_cwd(&codex, &target_cwd)?;
    let new_cwd = paths::strip_verbatim(&target_host.to_string_lossy());
    let mut store = family::load(&codex)?;
    let (family_id, ids) = resolve_family_ids_for_move(&store, &id)?;
    let mut rollouts = HashMap::new();
    let mut old_cwd = String::new();
    for sid in &ids {
        let rollout = locate_session_rollout(&codex, sid)?;
        let meta = family::read_session_meta(&rollout)?;
        let current_cwd = meta
            .get("payload")
            .and_then(|payload| payload.get("cwd"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        if sid == &id {
            old_cwd = paths::host_path_string_from_codex_record(&codex, &current_cwd);
        }
        rollouts.insert(sid.clone(), (rollout, current_cwd));
    }
    let index_ids = crate::repair::read_session_index_ids(&codex)?;
    let state = state_db::open(&codex)?;
    let transaction =
        rusqlite::Transaction::new_unchecked(&state, rusqlite::TransactionBehavior::Immediate)?;
    let mut journal = crate::repair::MutationJournal::default();
    let mut rollout_rewritten = false;
    let operation = (|| -> AppResult<u32> {
        for sid in &ids {
            let (rollout, current_cwd) = rollouts
                .get(sid)
                .ok_or_else(|| AppError::NotFound(format!("rollout: {sid}")))?;
            if current_cwd != &target_record {
                let rewritten = journal
                    .mutate_file(rollout, || rewrite_rollout_cwd(rollout, &target_record))?;
                rollout_rewritten |= rewritten;
            }
            if !crate::repair::upsert_thread_from_rollout(
                &codex,
                &transaction,
                rollout,
                rollout_is_archived(&codex, rollout),
            )? {
                return Err(AppError::InvalidCodexDir(format!(
                    "rollout 缺少有效 session_meta.id，无法同步 threads: {}",
                    rollout.to_string_lossy()
                )));
            }
        }

        let now = chrono::Utc::now().timestamp();
        let mut threads_updated = 0u32;
        for sid in &ids {
            let updated = transaction.execute(
                "UPDATE threads SET cwd = ?1, updated_at = ?2 WHERE id = ?3",
                params![&target_record, now, sid],
            )? as u32;
            if updated == 0 {
                return Err(AppError::NotFound(format!("threads 中未找到会话 {sid}")));
            }
            threads_updated += updated;
        }

        let global_state = paths::codex_global_state_json_path(&codex);
        journal.mutate_file(&global_state, || {
            crate::repair::ensure_workspace_root_registered(&codex, &target_record)
        })?;

        if rollout_rewritten {
            if let Some(family_id) = family_id.as_ref() {
                let family = store
                    .families
                    .get_mut(family_id)
                    .ok_or_else(|| AppError::NotFound(format!("family: {family_id}")))?;
                for branch in &mut family.chain {
                    if matches!(branch.status, crate::models::BranchStatus::Archived) {
                        let (rollout, _) = rollouts
                            .get(&branch.id)
                            .ok_or_else(|| AppError::NotFound(format!("rollout: {}", branch.id)))?;
                        let (sha256, line_count) = family::compute_integrity(rollout)?;
                        branch.sha256 = Some(sha256);
                        branch.line_count = Some(line_count);
                    }
                }
                family.updated_at = chrono::Utc::now().to_rfc3339();
                let family_path = paths::family_store_path(&codex);
                journal.mutate_file(&family_path, || family::save(&codex, &store))?;
            }
        }

        if ids.iter().any(|sid| index_ids.contains(sid)) {
            let index_path = paths::session_index_path(&codex);
            journal.mutate_file(&index_path, || {
                for sid in &ids {
                    if !index_ids.contains(sid) {
                        continue;
                    }
                    let thread_name: String = transaction.query_row(
                        "SELECT COALESCE(NULLIF(title,''), COALESCE(first_user_message,'')) FROM threads WHERE id = ?",
                        [sid],
                        |row| row.get(0),
                    )?;
                    crate::repair::append_index_line(
                        &codex,
                        sid,
                        &thread_name,
                        Path::new(""),
                    )?;
                }
                Ok(())
            })?;
        }
        Ok(threads_updated)
    })();

    let threads_updated = match operation {
        Ok(threads_updated) => {
            crate::repair::commit_transaction_with_compensation(transaction, journal)?;
            threads_updated
        }
        Err(error) => {
            return Err(crate::repair::rollback_transaction_with_compensation(
                transaction,
                journal,
                error,
            ));
        }
    };

    Ok(MoveSessionCwdReport {
        old_cwd,
        new_cwd,
        threads_updated,
        rollout_rewritten,
    })
}

/// 带锁的入口：校验 provider、校验路径、上锁后委托 move_session_cwd_locked。
pub fn move_session_cwd_with_lock(
    provider: Option<String>,
    codex_dir: String,
    id: String,
    target_cwd: String,
    lock: &family::FamilyLock,
) -> AppResult<MoveSessionCwdReport> {
    if provider_or_codex(provider) != "codex" {
        return Err(AppError::Other("Claude 会话暂不支持移动工作目录".into()));
    }
    let target_cwd = target_cwd.trim().to_string();
    if target_cwd.is_empty() {
        return Err(AppError::Other("工作目录路径不能为空".into()));
    }
    if target_cwd.chars().count() > 1024 {
        return Err(AppError::Other(
            "工作目录路径过长（最多 1024 个字符）".into(),
        ));
    }
    family::with_lock(lock, |_guard| {
        move_session_cwd_locked(codex_dir, id, target_cwd)
    })
}

fn set_archived_codex_locked(codex_dir: String, id: String, v: bool) -> AppResult<()> {
    let codex = PathBuf::from(&codex_dir);
    let mut family_store = family::load(&codex)?;
    // Validate the bidirectional mapping before moving any file.
    family::resolve_family_id_strict(&family_store, &id)?;
    let state = state_db::open(&codex)?;

    // 1) 定位当前 rollout 文件：优先 threads 记录，缺失/漂移时按文件名兜底
    let db_path: Option<String> = match state.query_row(
        "SELECT rollout_path FROM threads WHERE id = ?",
        [&id],
        |row| row.get(0),
    ) {
        Ok(path) => Some(path),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) => return Err(error.into()),
    };
    let mut current: Option<PathBuf> = db_path
        .as_ref()
        .map(|raw| {
            PathBuf::from(paths::strip_verbatim(
                &paths::host_path_string_from_codex_record(&codex, raw),
            ))
        })
        .filter(|p| p.is_file());
    let mut discovered = rollout_files_by_id(&codex, &id)?;
    if discovered.len() > 1 {
        return Err(AppError::Other(format!(
            "发现 {} 个同 ID Codex rollout，无法安全归档，请先修复重复文件: {id}",
            discovered.len()
        )));
    }
    if current.is_none() {
        current = discovered.pop();
    }
    let Some(current) = current else {
        return Err(AppError::NotFound(format!(
            "找不到会话 {id} 的 rollout 文件"
        )));
    };
    validate_codex_rollout_path(&codex, &current, &id)?;
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
        if !crate::repair::upsert_thread_from_rollout(&codex, &state, &target, v)? {
            return Err(AppError::Other(format!(
                "无法从 rollout 重建会话 {id} 的 threads 记录"
            )));
        }
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
    if family::update_manual_archive_metadata(&mut family_store, &codex, &id, v, &target)? {
        family::save(&codex, &family_store)?;
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

/// 在 sessions/ 与 archived_sessions/ 中按文件名末尾的会话 uuid 精确查找 rollout。
fn rollout_files_by_id(codex_dir: &Path, id: &str) -> AppResult<Vec<PathBuf>> {
    if id.trim().is_empty() {
        return Ok(Vec::new());
    }
    let expected_suffix = format!("-{id}.jsonl");
    let mut matches = Vec::new();
    for root in [
        paths::archived_sessions_dir(codex_dir),
        paths::sessions_dir(codex_dir),
    ] {
        let root_metadata = match fs::symlink_metadata(&root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !root_metadata.is_dir()
            || crate::path_safety::metadata_is_link_or_reparse(&root_metadata)
        {
            return Err(AppError::Path(format!(
                "Codex rollout 根路径不是普通目录或属于链接/junction: {}",
                root.to_string_lossy()
            )));
        }
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            let entry = entry.map_err(|error| {
                AppError::Other(format!(
                    "扫描 Codex rollout 失败 {}: {error}",
                    root.to_string_lossy()
                ))
            })?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
                return Err(AppError::Path(format!(
                    "Codex rollout 目录包含链接/junction，已拒绝扫描: {}",
                    entry.path().to_string_lossy()
                )));
            }
            if metadata.is_file()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(&expected_suffix)
            {
                matches.push(entry.path().to_path_buf());
            }
        }
    }
    matches.sort();
    matches.dedup();
    Ok(matches)
}

pub fn delete_session_with_lock(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    id: String,
    target: Option<DeleteTarget>,
    lock: &family::FamilyLock,
) -> AppResult<DeleteResult> {
    let target = match target {
        Some(target) if target.id != id => {
            return Err(AppError::Other(format!(
                "删除目标 ID 与请求 ID 不一致: 请求 {id}，目标 {}",
                target.id
            )))
        }
        Some(target) => target,
        None => DeleteTarget {
            id,
            rollout_path: None,
        },
    };
    match provider_or_codex(provider).as_str() {
        "codex" => family::with_lock(lock, |_guard| {
            delete_codex_targets_locked(Path::new(&codex_dir), vec![target])?
                .pop()
                .ok_or_else(|| AppError::Other("Codex 删除未返回结果".to_string()))
        }),
        "claude" => {
            let dir = claude_dir
                .unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned());
            delete_claude_targets(Path::new(&dir), vec![target])?
                .pop()
                .ok_or_else(|| AppError::Other("Claude 删除未返回结果".to_string()))
        }
        other => Err(AppError::Other(format!("不支持的 provider: {other}"))),
    }
}

pub fn delete_sessions_with_lock(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    ids: Vec<String>,
    targets: Option<Vec<DeleteTarget>>,
    lock: &family::FamilyLock,
) -> AppResult<Vec<DeleteResult>> {
    let targets = match targets {
        Some(targets) => {
            if !ids.is_empty()
                && (ids.len() != targets.len()
                    || ids
                        .iter()
                        .zip(&targets)
                        .any(|(id, target)| id != &target.id))
            {
                return Err(AppError::Other(
                    "批量删除的 ids 与精确 targets 不一致，已拒绝执行".to_string(),
                ));
            }
            targets
        }
        None => ids
            .into_iter()
            .map(|id| DeleteTarget {
                id,
                rollout_path: None,
            })
            .collect(),
    };
    match provider_or_codex(provider).as_str() {
        "codex" => family::with_lock(lock, |_guard| {
            delete_codex_targets_locked(Path::new(&codex_dir), targets)
        }),
        "claude" => {
            let dir = PathBuf::from(
                claude_dir
                    .unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned()),
            );
            delete_claude_targets(&dir, targets)
        }
        other => Err(AppError::Other(format!("不支持的 provider: {other}"))),
    }
}

fn empty_delete_result(target: &DeleteTarget) -> DeleteResult {
    DeleteResult {
        id: target.id.clone(),
        rollout_path: target.rollout_path.clone(),
        threads_rows_deleted: 0,
        logs_rows_deleted: 0,
        history_rows_deleted: 0,
        rollout_deleted: false,
        rollout_missing: false,
        sidecar_deleted: false,
        tasks_deleted: false,
        file_history_deleted: false,
        shared_data_preserved: false,
        ok: false,
        error: None,
    }
}

fn failed_delete_result(target: &DeleteTarget, error: String) -> DeleteResult {
    let mut result = empty_delete_result(target);
    result.error = Some(error);
    result
}

fn validate_delete_id(id: &str) -> AppResult<()> {
    let mut components = Path::new(id).components();
    let is_single_normal_component = matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(value)), None) if value == id
    );
    if id.trim().is_empty()
        || !is_single_normal_component
        || id.contains(':')
        || id.chars().any(char::is_control)
    {
        return Err(AppError::Path(format!(
            "会话 ID 不能包含路径或非法字符: {id:?}"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CodexDeletePlanKey {
    Family(String),
    Session(String),
}

fn delete_codex_targets_locked(
    codex_dir: &Path,
    targets: Vec<DeleteTarget>,
) -> AppResult<Vec<DeleteResult>> {
    let mut store = family::load(codex_dir)?;
    // Resolve every target before the first destructive write. A broken family/index mapping
    // must never leave a half-deleted logical conversation.
    let plans = targets
        .iter()
        .map(|target| {
            validate_delete_id(&target.id)
                .and_then(|()| family::resolve_family_id_strict(&store, &target.id))
                .map(|family_id| match family_id {
                    Some(id) => CodexDeletePlanKey::Family(id),
                    None => CodexDeletePlanKey::Session(target.id.clone()),
                })
                .map_err(|error| error.to_string())
        })
        .collect::<Vec<_>>();

    let mut executed = HashMap::<CodexDeletePlanKey, DeleteResult>::new();
    let mut results = Vec::with_capacity(targets.len());
    for (target, plan) in targets.iter().zip(plans) {
        let key = match plan {
            Ok(key) => key,
            Err(error) => {
                results.push(failed_delete_result(target, error));
                continue;
            }
        };
        if !executed.contains_key(&key) {
            let result = match &key {
                CodexDeletePlanKey::Family(family_id) => {
                    delete_codex_family_locked(codex_dir, &mut store, family_id, &target.id)
                        .unwrap_or_else(|error| failed_delete_result(target, error.to_string()))
                }
                CodexDeletePlanKey::Session(id) => delete_codex_artifacts(codex_dir, id)
                    .map(|outcome| outcome.result)
                    .unwrap_or_else(|error| failed_delete_result(target, error.to_string())),
            };
            executed.insert(key.clone(), result);
        }
        let mut result = executed
            .get(&key)
            .cloned()
            .ok_or_else(|| AppError::Other("Codex 删除计划未产生结果".to_string()))?;
        result.id.clone_from(&target.id);
        results.push(result);
    }
    Ok(results)
}

fn delete_codex_family_locked(
    codex_dir: &Path,
    store: &mut crate::models::FamilyStore,
    family_id: &str,
    requested_id: &str,
) -> AppResult<DeleteResult> {
    let snapshot = store
        .families
        .get(family_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("family: {family_id}")))?;
    let target = DeleteTarget {
        id: requested_id.to_string(),
        rollout_path: None,
    };
    let mut aggregate = empty_delete_result(&target);
    // Confirm the family store is writable before deleting the first physical artifact. A later
    // external sharing violation can still occur, but ordinary permission/read-only failures are
    // surfaced before any branch is touched.
    family::save(codex_dir, store)?;

    // Historical branches first. If any core artifact survives, keep the active branch intact.
    for branch in snapshot
        .chain
        .iter()
        .filter(|branch| branch.id != snapshot.active_id)
    {
        let outcome = match delete_codex_artifacts(codex_dir, &branch.id) {
            Ok(outcome) => outcome,
            Err(error) => {
                append_error(
                    &mut aggregate,
                    format!("分支 {} 删除失败: {error}", branch.id),
                );
                return Ok(aggregate);
            }
        };
        merge_codex_delete_result(&mut aggregate, &branch.id, outcome.result);
        if !outcome.structurally_removed {
            if aggregate.error.is_none() {
                append_error(
                    &mut aggregate,
                    format!("分支 {} 的核心记录未删除干净，已保留当前分支", branch.id),
                );
            }
            aggregate.ok = false;
            return Ok(aggregate);
        }
        let before_metadata = store.clone();
        if let Err(error) = family::remove_non_active_branch(store, family_id, &branch.id)
            .and_then(|_| family::save(codex_dir, store))
        {
            *store = before_metadata;
            append_error(
                &mut aggregate,
                format!(
                    "分支 {} 的文件已删除，但 family 元数据保存失败: {error}",
                    branch.id
                ),
            );
            return Ok(aggregate);
        }
    }

    let active_outcome = match delete_codex_artifacts(codex_dir, &snapshot.active_id) {
        Ok(outcome) => outcome,
        Err(error) => {
            append_error(
                &mut aggregate,
                format!("当前分支 {} 删除失败: {error}", snapshot.active_id),
            );
            return Ok(aggregate);
        }
    };
    let active_removed = active_outcome.structurally_removed;
    merge_codex_delete_result(&mut aggregate, &snapshot.active_id, active_outcome.result);
    if active_removed {
        let before_metadata = store.clone();
        if let Err(error) =
            family::remove_family(store, family_id).and_then(|_| family::save(codex_dir, store))
        {
            *store = before_metadata;
            append_error(
                &mut aggregate,
                format!(
                    "当前分支 {} 的文件已删除，但 family 元数据保存失败: {error}",
                    snapshot.active_id
                ),
            );
            return Ok(aggregate);
        }
    } else if aggregate.error.is_none() {
        append_error(
            &mut aggregate,
            format!("当前分支 {} 的核心记录未删除干净", snapshot.active_id),
        );
    }
    aggregate.ok = active_removed && aggregate.error.is_none();
    Ok(aggregate)
}

fn merge_codex_delete_result(target: &mut DeleteResult, branch_id: &str, source: DeleteResult) {
    if target.rollout_path.is_none() {
        target.rollout_path = source.rollout_path.clone();
    }
    target.threads_rows_deleted = target
        .threads_rows_deleted
        .saturating_add(source.threads_rows_deleted);
    target.logs_rows_deleted = target
        .logs_rows_deleted
        .saturating_add(source.logs_rows_deleted);
    target.history_rows_deleted = target
        .history_rows_deleted
        .saturating_add(source.history_rows_deleted);
    if source.rollout_deleted {
        target.rollout_deleted = true;
        target.rollout_missing = false;
    } else if source.rollout_missing && !target.rollout_deleted {
        target.rollout_missing = true;
    }
    if let Some(error) = source.error {
        append_error(target, format!("分支 {branch_id}: {error}"));
    }
}

fn delete_claude_targets(
    claude_dir: &Path,
    targets: Vec<DeleteTarget>,
) -> AppResult<Vec<DeleteResult>> {
    let mut results = Vec::with_capacity(targets.len());
    let projects = paths::claude_projects_dir(claude_dir);
    for target in &targets {
        let mut result = empty_delete_result(target);
        if let Err(error) = validate_delete_id(&target.id) {
            append_error(&mut result, error.to_string());
            results.push(result);
            continue;
        }
        match resolve_claude_delete_target(&projects, target) {
            Ok(Some(jsonl)) => {
                result.rollout_path = Some(jsonl.to_string_lossy().into_owned());
                match fs::remove_file(&jsonl) {
                    Ok(()) => result.rollout_deleted = true,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        result.rollout_missing = true;
                    }
                    Err(error) => {
                        append_error(
                            &mut result,
                            format!(
                                "Claude 会话文件删除失败 {}: {error}",
                                jsonl.to_string_lossy()
                            ),
                        );
                    }
                }

                if result.rollout_deleted || result.rollout_missing {
                    cleanup_claude_sidecar(&projects, &jsonl, &mut result);
                }
            }
            Ok(None) => {
                result.rollout_missing = true;
                if let Some(raw_path) = target.rollout_path.as_deref() {
                    let jsonl = PathBuf::from(paths::strip_verbatim(raw_path));
                    cleanup_claude_sidecar(&projects, &jsonl, &mut result);
                }
            }
            Err(error) => append_error(&mut result, error.to_string()),
        }
        results.push(result);
    }

    // Deleting one imported duplicate must not erase history/tasks shared by another copy.
    let remaining_ids = match scan_claude_session_ids(&projects) {
        Ok(ids) => Some(ids),
        Err(error) => {
            for result in &mut results {
                if result.rollout_deleted || result.rollout_missing {
                    result.shared_data_preserved = true;
                    append_error(
                        result,
                        format!("无法确认是否仍有同 ID 会话副本，已保留共享数据: {error}"),
                    );
                }
            }
            None
        }
    };

    let mut history_ids = HashSet::new();
    if let Some(remaining_ids) = remaining_ids {
        for result in &mut results {
            if !(result.rollout_deleted || result.rollout_missing) {
                continue;
            }
            if remaining_ids.contains(&result.id) {
                result.shared_data_preserved = true;
                if result.rollout_missing && result.rollout_path.is_none() {
                    append_error(
                        result,
                        "未按 ID 定位到待删文件，但扫描后该 ID 会话仍存在，已拒绝报告成功"
                            .to_string(),
                    );
                }
                continue;
            }
            match cleanup_claude_session_dir(
                claude_dir,
                &claude_dir.join("tasks").join(&result.id),
                "tasks",
            ) {
                Ok(deleted) => result.tasks_deleted = deleted,
                Err(error) => append_error(result, error.to_string()),
            }
            match cleanup_claude_session_dir(
                claude_dir,
                &claude_dir.join("file-history").join(&result.id),
                "file-history",
            ) {
                Ok(deleted) => result.file_history_deleted = deleted,
                Err(error) => append_error(result, error.to_string()),
            }
            history_ids.insert(result.id.clone());
        }
    }

    if !history_ids.is_empty() {
        let history_path = paths::history_path(claude_dir);
        match history::filter_file_for_ids(&history_path, &history_ids) {
            Ok(removed) => {
                for result in &mut results {
                    result.history_rows_deleted = removed.get(&result.id).copied().unwrap_or(0);
                }
            }
            Err(error) => {
                for result in &mut results {
                    if history_ids.contains(&result.id) {
                        append_error(result, format!("Claude history.jsonl 清理失败: {error}"));
                    }
                }
            }
        }
    }

    for result in &mut results {
        result.ok = result.error.is_none() && (result.rollout_deleted || result.rollout_missing);
    }
    Ok(results)
}

fn cleanup_claude_sidecar(projects: &Path, jsonl: &Path, result: &mut DeleteResult) {
    let Some(sidecar) = crate::claude_sessions::sidecar_path_for(jsonl) else {
        return;
    };
    if !projects.exists() {
        return;
    }
    match crate::path_safety::remove_path(
        projects,
        &sidecar,
        crate::path_safety::EntryKind::Directory,
        "Claude sidecar",
    ) {
        Ok(deleted) => result.sidecar_deleted = deleted,
        Err(error) => append_error(result, error.to_string()),
    }
}

#[cfg(test)]
fn delete_one_claude(claude_dir: &Path, id: &str) -> AppResult<DeleteResult> {
    delete_claude_targets(
        claude_dir,
        vec![DeleteTarget {
            id: id.to_string(),
            rollout_path: None,
        }],
    )?
    .pop()
    .ok_or_else(|| AppError::Other("Claude 删除未返回结果".to_string()))
}

fn cleanup_claude_session_dir(root: &Path, path: &Path, label: &str) -> AppResult<bool> {
    crate::path_safety::remove_path(
        root,
        path,
        crate::path_safety::EntryKind::Directory,
        &format!("Claude {label}"),
    )
}

fn resolve_claude_delete_target(
    projects: &Path,
    target: &DeleteTarget,
) -> AppResult<Option<PathBuf>> {
    if let Some(raw_path) = target.rollout_path.as_deref() {
        let path = PathBuf::from(paths::strip_verbatim(raw_path));
        let exists = validate_claude_target_path(projects, &path, &target.id)?;
        return Ok(exists.then_some(path));
    }

    if !projects.is_dir() {
        return Ok(None);
    }
    let mut matches = Vec::new();
    for entry in walkdir::WalkDir::new(projects).follow_links(false) {
        let entry = entry.map_err(|error| {
            AppError::Other(format!(
                "扫描 Claude projects 失败 {}: {error}",
                projects.to_string_lossy()
            ))
        })?;
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
            && claude_session_identity(entry.path())?.as_deref() == Some(target.id.as_str())
        {
            validate_claude_target_path(projects, entry.path(), &target.id)?;
            matches.push(entry.path().to_path_buf());
        }
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        count => Err(AppError::Other(format!(
            "发现 {count} 个同 ID Claude 会话，必须提供精确 rollout_path: {}",
            target.id
        ))),
    }
}

fn validate_claude_target_path(projects: &Path, path: &Path, id: &str) -> AppResult<bool> {
    if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
        return Err(AppError::Path(format!(
            "Claude 删除目标不是 jsonl: {}",
            path.to_string_lossy()
        )));
    }

    let exists = if projects.exists() {
        crate::path_safety::validate_descendant(
            projects,
            path,
            crate::path_safety::EntryKind::File,
            true,
            "Claude 删除目标",
        )?
    } else {
        // A file can disappear between list and delete. Validate the absent target lexically so
        // the remaining history/tasks can still be cleaned idempotently without accepting `..`.
        let clean_root = PathBuf::from(paths::strip_verbatim(&projects.to_string_lossy()));
        let clean_path = PathBuf::from(paths::strip_verbatim(&path.to_string_lossy()));
        let relative = clean_path.strip_prefix(&clean_root).map_err(|_| {
            AppError::Path(format!(
                "Claude 删除目标不在 projects 目录内: {}",
                path.to_string_lossy()
            ))
        })?;
        paths::checked_relative_path(&relative.to_string_lossy())?;
        if clean_path.file_stem().and_then(|value| value.to_str()) != Some(id) {
            return Err(AppError::Path(format!(
                "Claude 删除目标文件名与会话 ID 不匹配: {}",
                path.to_string_lossy()
            )));
        }
        false
    };
    if exists {
        let identity = claude_session_identity(path)?;
        if identity.as_deref() != Some(id) {
            return Err(AppError::Other(format!(
                "Claude 删除目标 ID 不匹配: 期望 {id}，文件识别为 {} ({})",
                identity.as_deref().unwrap_or("未知"),
                path.to_string_lossy()
            )));
        }
    } else if path.file_stem().and_then(|value| value.to_str()) != Some(id) {
        return Err(AppError::Path(format!(
            "Claude 删除目标文件名与会话 ID 不匹配: {}",
            path.to_string_lossy()
        )));
    }
    Ok(exists)
}

fn scan_claude_session_ids(projects: &Path) -> AppResult<HashSet<String>> {
    let mut ids = HashSet::new();
    if !projects.exists() {
        return Ok(ids);
    }
    if !projects.is_dir() {
        return Err(AppError::Path(format!(
            "Claude projects 路径不是目录: {}",
            projects.to_string_lossy()
        )));
    }
    for entry in walkdir::WalkDir::new(projects).follow_links(false) {
        let entry = entry.map_err(|error| {
            AppError::Other(format!(
                "扫描 Claude projects 失败 {}: {error}",
                projects.to_string_lossy()
            ))
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "Claude projects 内包含链接或 junction，无法安全确认剩余副本: {}",
                entry.path().to_string_lossy()
            )));
        }
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("jsonl")
        {
            continue;
        }
        if let Some(id) = claude_session_identity(entry.path())? {
            ids.insert(id);
        }
    }
    Ok(ids)
}

fn claude_session_identity(path: &Path) -> AppResult<Option<String>> {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .map(str::to_string);
    if stem
        .as_deref()
        .is_some_and(|value| value.starts_with("agent-"))
    {
        return Ok(stem);
    }
    let file = File::open(path)?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(id) = value
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Ok(Some(id.to_string()));
        }
    }
    Ok(stem)
}

pub(crate) struct CodexDeleteOutcome {
    pub(crate) result: DeleteResult,
    pub(crate) structurally_removed: bool,
}

pub(crate) fn delete_codex_artifacts(codex_dir: &Path, id: &str) -> AppResult<CodexDeleteOutcome> {
    let mut result = DeleteResult {
        id: id.to_string(),
        rollout_path: None,
        threads_rows_deleted: 0,
        logs_rows_deleted: 0,
        history_rows_deleted: 0,
        rollout_deleted: false,
        rollout_missing: false,
        sidecar_deleted: false,
        tasks_deleted: false,
        file_history_deleted: false,
        shared_data_preserved: false,
        ok: false,
        error: None,
    };

    // Preflight every fallible lookup that can be checked before the first write.
    let state = state_db::open(codex_dir)?;
    let rollout_path: Option<String> = match state.query_row(
        "SELECT rollout_path FROM threads WHERE id = ?",
        [id],
        |row| row.get(0),
    ) {
        Ok(path) => Some(path),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) => return Err(error.into()),
    };
    let mut rollout_files = rollout_files_by_id(codex_dir, id)?;
    if let Some(raw_path) = rollout_path.as_deref() {
        let db_path = PathBuf::from(paths::strip_verbatim(
            &paths::host_path_string_from_codex_record(codex_dir, raw_path),
        ));
        if db_path.is_file() {
            validate_codex_rollout_path(codex_dir, &db_path, id)?;
            rollout_files.push(db_path);
        }
    }
    let mut canonical_files = Vec::with_capacity(rollout_files.len());
    for path in rollout_files {
        validate_codex_rollout_path(codex_dir, &path, id)?;
        let canonical = path.canonicalize()?;
        if !canonical_files.contains(&canonical) {
            canonical_files.push(canonical);
        }
    }
    let logs = if codex_dir.join("logs_2.sqlite").is_file() {
        Some(logs_db::open(codex_dir)?)
    } else {
        None
    };

    // 1) threads（外键级联 thread_dynamic_tools / stage1_outputs / thread_spawn_edges）
    let rows = {
        let tx = state.unchecked_transaction()?;
        let n = tx.execute("DELETE FROM threads WHERE id = ?", [id])?;
        tx.commit()?;
        n
    };
    result.threads_rows_deleted = rows as u32;

    // 2) logs_2.sqlite 在部分 Codex 版本中不存在；不存在就没有待清理记录。
    if let Some(logs) = logs {
        let logs_result: AppResult<usize> = (|| {
            let tx = logs.unchecked_transaction()?;
            let n = tx.execute("DELETE FROM logs WHERE thread_id = ?", [id])?;
            tx.commit()?;
            Ok(n)
        })();
        match logs_result {
            Ok(rows_logs) => result.logs_rows_deleted = rows_logs as u32,
            Err(error) => append_error(&mut result, format!("logs delete failed: {error}")),
        }
    }

    // 3) 删除 sessions/ 与 archived_sessions/ 中全部同 ID rollout，避免漂移副本残留。
    if canonical_files.is_empty() {
        result.rollout_missing = true;
    } else {
        result.rollout_path = canonical_files
            .first()
            .map(|path| path.to_string_lossy().into_owned());
        for rollout in canonical_files {
            match fs::remove_file(&rollout) {
                Ok(()) => {
                    result.rollout_deleted = true;
                    cleanup_empty_rollout_ancestors(codex_dir, &rollout, &mut result);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    result.rollout_missing = true;
                }
                Err(error) => append_error(
                    &mut result,
                    format!(
                        "rollout remove failed {}: {error}",
                        rollout.to_string_lossy()
                    ),
                ),
            }
        }
    }

    // 4) session_index.jsonl
    let index_path = paths::session_index_path(codex_dir);
    if index_path.exists() {
        if let Err(error) = filter_index_file(&index_path, id) {
            append_error(&mut result, format!("session_index filter failed: {error}"));
        }
    }

    // Verify the three core locations from disk/database truth before touching family metadata.
    let threads_remaining: i64 =
        state.query_row("SELECT COUNT(*) FROM threads WHERE id = ?", [id], |row| {
            row.get(0)
        })?;
    let rollout_remaining = match rollout_files_by_id(codex_dir, id) {
        Ok(paths) => !paths.is_empty(),
        Err(error) => {
            append_error(
                &mut result,
                format!("rollout deletion verification failed: {error}"),
            );
            true
        }
    };
    let index_remaining = match index_contains_id(&index_path, id) {
        Ok(remaining) => remaining,
        Err(error) => {
            append_error(
                &mut result,
                format!("session_index deletion verification failed: {error}"),
            );
            true
        }
    };
    let structurally_removed = threads_remaining == 0 && !rollout_remaining && !index_remaining;
    if !structurally_removed && result.error.is_none() {
        append_error(
            &mut result,
            "Codex 会话仍有核心记录残留（threads、rollout 或 session_index）".to_string(),
        );
    }
    result.ok = structurally_removed && result.error.is_none();
    Ok(CodexDeleteOutcome {
        result,
        structurally_removed,
    })
}

#[cfg(test)]
fn delete_one(codex_dir: &Path, id: &str) -> AppResult<DeleteResult> {
    Ok(delete_codex_artifacts(codex_dir, id)?.result)
}

pub(crate) fn validate_codex_rollout_path(
    codex_dir: &Path,
    path: &Path,
    id: &str,
) -> AppResult<()> {
    let expected_suffix = format!("-{id}.jsonl");
    if !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(&expected_suffix))
    {
        return Err(AppError::Path(format!(
            "Codex rollout 路径与会话 ID 不匹配，拒绝操作: {}",
            path.to_string_lossy()
        )));
    }
    for root in [
        paths::sessions_dir(codex_dir),
        paths::archived_sessions_dir(codex_dir),
    ] {
        let root_metadata = match fs::symlink_metadata(&root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !root_metadata.is_dir()
            || crate::path_safety::metadata_is_link_or_reparse(&root_metadata)
        {
            return Err(AppError::Path(format!(
                "Codex rollout 根路径不是普通目录或属于链接/junction: {}",
                root.to_string_lossy()
            )));
        }

        let clean_root = PathBuf::from(paths::strip_verbatim(&root.to_string_lossy()));
        let clean_path = PathBuf::from(paths::strip_verbatim(&path.to_string_lossy()));
        if clean_path.strip_prefix(&clean_root).is_ok() {
            crate::path_safety::validate_descendant(
                &root,
                path,
                crate::path_safety::EntryKind::File,
                false,
                "Codex rollout 操作目标",
            )?;
            let meta = family::read_session_meta(path)?;
            let actual_id = meta
                .get("payload")
                .and_then(|payload| payload.get("id"))
                .and_then(serde_json::Value::as_str);
            if actual_id == Some(id) {
                return Ok(());
            }
            return Err(AppError::Other(format!(
                "Codex rollout 内容 ID 不匹配，期望 {id}，实际为 {}: {}",
                actual_id.unwrap_or("未知"),
                path.to_string_lossy()
            )));
        }
    }
    Err(AppError::Path(format!(
        "Codex rollout 不在 sessions 或 archived_sessions 内，拒绝操作: {}",
        path.to_string_lossy()
    )))
}

fn cleanup_empty_rollout_ancestors(codex_dir: &Path, rollout: &Path, result: &mut DeleteResult) {
    let sessions_root = paths::sessions_dir(codex_dir);
    let mut current = rollout.parent();
    while let Some(dir) = current {
        if dir == sessions_root || !dir.starts_with(&sessions_root) {
            break;
        }
        match fs::remove_dir(dir) {
            Ok(()) => current = dir.parent(),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
                ) =>
            {
                break;
            }
            Err(error) => {
                append_error(
                    result,
                    format!("清理空 rollout 目录失败 {}: {error}", dir.to_string_lossy()),
                );
                break;
            }
        }
    }
}

fn index_contains_id(path: &Path, id: &str) -> AppResult<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(serde_json::Value::as_str) == Some(id)
            || value.get("session_id").and_then(serde_json::Value::as_str) == Some(id)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn append_error(result: &mut DeleteResult, msg: String) {
    result.error = Some(match result.error.take() {
        Some(prev) => format!("{prev}; {msg}"),
        None => msg,
    });
}

fn filter_index_file(path: &Path, id: &str) -> AppResult<()> {
    let expected = atomic_file::fingerprint(path)?;
    let content = fs::read_to_string(path)?;
    let mut kept = Vec::new();
    let mut removed = false;
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
            kept.push(line);
        } else {
            removed = true;
        }
    }
    if !removed {
        return Ok(());
    }
    atomic_file::replace_with_writer_if_unchanged(path, &expected, |file| {
        use std::io::Write;
        for line in kept {
            writeln!(file, "{line}")?;
        }
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::models::{BranchStatus, Family, FamilyBranch, FamilyStore};
    use std::io::Write;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("cc-sessions-{name}-{}-{nanos}", std::process::id()))
    }

    #[cfg(windows)]
    fn create_windows_junction(target: &Path, link: &Path) -> AppResult<()> {
        let output = std::process::Command::new("pwsh")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$ErrorActionPreference = 'Stop'; New-Item -ItemType Junction -Path $env:CC_TEST_LINK -Target $env:CC_TEST_TARGET | Out-Null",
            ])
            .env("CC_TEST_LINK", link)
            .env("CC_TEST_TARGET", target)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(AppError::Other(format!(
                "无法创建 junction 测试夹具: {}",
                String::from_utf8_lossy(&output.stderr)
            )))
        }
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

    fn write_claude_session(path: &Path, id: &str) -> AppResult<()> {
        fs::create_dir_all(path.parent().expect("claude session parent"))?;
        fs::write(
            path,
            format!(
                "{{\"sessionId\":\"{id}\",\"cwd\":\"F:\\\\work\\\\sample\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"hello\"}}}}\n"
            ),
        )?;
        Ok(())
    }

    fn claude_target(id: &str, path: Option<&Path>) -> crate::models::DeleteTarget {
        crate::models::DeleteTarget {
            id: id.to_string(),
            rollout_path: path.map(|path| path.to_string_lossy().into_owned()),
        }
    }

    #[test]
    fn rename_session_updates_title_and_bumps_updated_at() -> AppResult<()> {
        let codex = temp_dir("codex-rename-session");
        let conn = create_codex_threads_table(&codex)?;
        conn.execute(
            "INSERT INTO threads (id, rollout_path, title, updated_at, archived)
             VALUES ('rename-me', 'x.jsonl', '旧标题', 1770000000, 0)",
            [],
        )?;
        drop(conn);

        let lock = family::FamilyLock::default();
        let renamed = rename_session_with_lock(
            Some("codex".into()),
            codex.to_string_lossy().into_owned(),
            "rename-me".into(),
            "  新的会话名  ".into(),
            &lock,
        )?;
        assert_eq!(renamed, 1);

        let conn = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        let (title, updated_at): (String, i64) = conn.query_row(
            "SELECT title, updated_at FROM threads WHERE id = 'rename-me'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(title, "新的会话名");
        assert!(
            updated_at > 1770000000,
            "重命名应 bump updated_at 以便官方 App 增量同步可见"
        );

        // 空名与 Claude 会话必须被拒绝
        assert!(rename_session_with_lock(
            Some("codex".into()),
            codex.to_string_lossy().into_owned(),
            "rename-me".into(),
            "   ".into(),
            &lock,
        )
        .is_err());
        assert!(rename_session_with_lock(
            Some("claude".into()),
            codex.to_string_lossy().into_owned(),
            "rename-me".into(),
            "名字".into(),
            &lock,
        )
        .is_err());
        assert!(rename_session_with_lock(
            Some("codex".into()),
            codex.to_string_lossy().into_owned(),
            "missing-id".into(),
            "名字".into(),
            &lock,
        )
        .is_err());

        drop(conn);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn list_sessions_prefers_active_session_index_title() -> AppResult<()> {
        let codex = temp_dir("codex-session-index-title");
        let conn = create_codex_threads_table(&codex)?;
        conn.execute(
            "INSERT INTO threads (
                id, rollout_path, cwd, title, first_user_message, model, reasoning_effort,
                tokens_used, created_at, updated_at, archived, archived_at, git_branch, source,
                agent_nickname, agent_role
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 0, 1770000000, 1770000300, 0, NULL, NULL, NULL, NULL, NULL)",
            (
                "indexed-title",
                codex.join("sessions/indexed-title.jsonl").to_string_lossy().into_owned(),
                "F:\\work\\indexed-title",
                "数据库中的首条用户消息",
                "数据库中的首条用户消息",
                "gpt-5",
            ),
        )?;
        drop(conn);
        fs::write(
            paths::session_index_path(&codex),
            concat!(
                "{\"id\":\"indexed-title\",\"thread_name\":\"较早的索引标题\"}\n",
                "{broken json\n",
                "{\"id\":\"indexed-title\",\"thread_name\":\"Codex 生成的简短标题\"}\n"
            ),
        )?;

        let sessions = list_sessions(
            Some("codex".to_string()),
            codex.to_string_lossy().into_owned(),
            None,
        )?;
        fs::remove_dir_all(&codex).ok();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "Codex 生成的简短标题");
        Ok(())
    }

    #[test]
    fn list_sessions_recovers_generated_database_title_from_prompt_only_index() -> AppResult<()> {
        let codex = temp_dir("codex-converted-title-mismatch");
        let conn = create_codex_threads_table(&codex)?;
        conn.execute(
            "INSERT INTO threads (
                id, rollout_path, cwd, title, first_user_message, model, reasoning_effort,
                tokens_used, created_at, updated_at, archived, archived_at, git_branch, source,
                agent_nickname, agent_role
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 0, 1770000000, 1770000300, 0, NULL, NULL, NULL, NULL, NULL)",
            (
                "converted-title",
                codex.join("sessions/converted-title.jsonl").to_string_lossy().into_owned(),
                "F:\\work\\converted-title",
                "Claude 自动生成标题",
                "这是首条用户提问",
                "gpt-5",
            ),
        )?;
        drop(conn);
        fs::write(
            paths::session_index_path(&codex),
            "{\"id\":\"converted-title\",\"thread_name\":\"这是首条用户提问\"}\n",
        )?;

        let sessions = list_sessions(
            Some("codex".to_string()),
            codex.to_string_lossy().into_owned(),
            None,
        )?;
        fs::remove_dir_all(&codex).ok();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "Claude 自动生成标题");
        Ok(())
    }

    #[test]
    fn rename_session_updates_family_without_readding_archived_index_entries() -> AppResult<()> {
        let codex = temp_dir("codex-rename-session-family");
        let family_id = "family-rename";
        let archived_id = "019d-family-rename-archived";
        let active_id = "019d-family-rename-active";
        codex_family_fixture(
            &codex,
            family_id,
            active_id,
            &[
                FamilyBranchFixture {
                    id: archived_id,
                    archived: true,
                },
                FamilyBranchFixture {
                    id: active_id,
                    archived: false,
                },
            ],
        )?;

        let lock = family::FamilyLock::default();
        let renamed = rename_session_with_lock(
            Some("codex".into()),
            codex.to_string_lossy().into_owned(),
            active_id.into(),
            "家族新名称".into(),
            &lock,
        )?;
        assert_eq!(renamed, 2);

        let conn = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        let matching: i64 = conn.query_row(
            "SELECT COUNT(*) FROM threads WHERE id IN (?1, ?2) AND title = ?3",
            params![archived_id, active_id, "家族新名称"],
            |row| row.get(0),
        )?;
        assert_eq!(matching, 2, "同一家族的活跃与归档分支都应改名");
        drop(conn);

        let index = fs::read_to_string(paths::session_index_path(&codex))?;
        assert!(index.contains(active_id));
        assert!(index.contains("家族新名称"));
        assert!(
            !index.contains(archived_id),
            "改名不得把归档分支重新写回活跃索引"
        );

        let store = family::load(&codex)?;
        assert_eq!(
            store.families.get(family_id).expect("family").title,
            "家族新名称"
        );

        fs::remove_dir_all(&codex).ok();
        Ok(())
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

    #[test]
    fn delete_claude_session_removes_only_session_scoped_artifacts() -> AppResult<()> {
        let claude = temp_dir("claude-delete-session-artifacts");
        let project = claude.join("projects").join("sample-project");
        let id = "11111111-2222-4333-8444-555555555555";
        let session = project.join(format!("{id}.jsonl"));
        write_claude_session(&session, id)?;

        let sidecar = project.join(id);
        fs::create_dir_all(sidecar.join("subagents"))?;
        fs::write(sidecar.join("subagents").join("agent-one.jsonl"), "{}\n")?;
        fs::create_dir_all(claude.join("tasks").join(id))?;
        fs::write(claude.join("tasks").join(id).join("1.json"), "{}\n")?;
        fs::create_dir_all(claude.join("file-history").join(id))?;
        fs::write(
            claude.join("file-history").join(id).join("snapshot@v1"),
            "original",
        )?;

        let memory = project.join("memory");
        fs::create_dir_all(&memory)?;
        fs::write(memory.join("MEMORY.md"), "keep")?;
        fs::create_dir_all(claude.join("session-env").join(id))?;
        fs::write(claude.join("session-env").join(id).join("env"), "keep")?;
        fs::create_dir_all(claude.join("shell-snapshots"))?;
        fs::write(claude.join("shell-snapshots").join("snapshot.sh"), "keep")?;
        fs::write(
            claude.join("history.jsonl"),
            format!("{{\"sessionId\":\"{id}\",\"display\":\"delete\"}}\n"),
        )?;

        let result = delete_claude_targets(&claude, vec![claude_target(id, Some(&session))])?
            .pop()
            .expect("one delete result");

        assert!(result.ok, "{:?}", result.error);
        assert!(result.rollout_deleted);
        assert!(result.sidecar_deleted);
        assert!(result.tasks_deleted);
        assert!(result.file_history_deleted);
        assert_eq!(result.history_rows_deleted, 1);
        assert!(!session.exists());
        assert!(!sidecar.exists());
        assert!(!claude.join("tasks").join(id).exists());
        assert!(!claude.join("file-history").join(id).exists());
        assert!(memory.is_dir(), "project memory must not be deleted");
        assert!(claude.join("session-env").join(id).is_dir());
        assert!(claude.join("shell-snapshots").join("snapshot.sh").is_file());

        fs::remove_dir_all(claude).ok();
        Ok(())
    }

    #[test]
    fn delete_claude_exact_path_preserves_shared_data_until_last_copy() -> AppResult<()> {
        let claude = temp_dir("claude-delete-duplicate-id");
        let id = "22222222-3333-4444-8555-666666666666";
        let first = claude
            .join("projects")
            .join("first-project")
            .join(format!("{id}.jsonl"));
        let second = claude
            .join("projects")
            .join("second-project")
            .join(format!("{id}.jsonl"));
        write_claude_session(&first, id)?;
        write_claude_session(&second, id)?;
        fs::create_dir_all(claude.join("tasks").join(id))?;
        fs::create_dir_all(claude.join("file-history").join(id))?;
        fs::write(
            claude.join("history.jsonl"),
            format!("{{\"sessionId\":\"{id}\",\"display\":\"keep until last\"}}\n"),
        )?;

        let first_result = delete_session_with_lock(
            Some("claude".to_string()),
            String::new(),
            Some(claude.to_string_lossy().into_owned()),
            id.to_string(),
            Some(claude_target(id, Some(&second))),
            &family::FamilyLock::default(),
        )?;
        assert!(first_result.ok, "{:?}", first_result.error);
        assert!(first.is_file(), "the unselected copy must remain");
        assert!(!second.exists(), "the exact selected copy must be deleted");
        assert!(first_result.shared_data_preserved);
        assert_eq!(first_result.history_rows_deleted, 0);
        assert!(claude.join("tasks").join(id).is_dir());
        assert!(claude.join("file-history").join(id).is_dir());
        assert!(fs::read_to_string(claude.join("history.jsonl"))?.contains(id));

        let last_result = delete_claude_targets(&claude, vec![claude_target(id, Some(&first))])?
            .pop()
            .expect("last delete result");
        assert!(last_result.ok, "{:?}", last_result.error);
        assert!(!last_result.shared_data_preserved);
        assert_eq!(last_result.history_rows_deleted, 1);
        assert!(!claude.join("tasks").join(id).exists());
        assert!(!claude.join("file-history").join(id).exists());

        fs::remove_dir_all(claude).ok();
        Ok(())
    }

    #[test]
    fn delete_session_rejects_mismatched_explicit_target_id() -> AppResult<()> {
        let claude = temp_dir("claude-delete-mismatched-target");
        let requested_id = "23232323-3434-4545-8565-676767676767";
        let target_id = "24242424-3535-4646-8575-686868686868";
        let target = claude
            .join("projects")
            .join("target-project")
            .join(format!("{target_id}.jsonl"));
        write_claude_session(&target, target_id)?;

        let error = delete_session_with_lock(
            Some("claude".to_string()),
            String::new(),
            Some(claude.to_string_lossy().into_owned()),
            requested_id.to_string(),
            Some(claude_target(target_id, Some(&target))),
            &family::FamilyLock::default(),
        )
        .expect_err("mismatched target id must be rejected");

        assert!(error.to_string().contains("不一致"));
        assert!(target.is_file(), "rejected target must remain untouched");
        fs::remove_dir_all(claude).ok();
        Ok(())
    }

    #[test]
    fn delete_claude_batch_cleans_history_without_projects_directory() -> AppResult<()> {
        let claude = temp_dir("claude-delete-no-projects");
        fs::create_dir_all(&claude)?;
        let first = "33333333-4444-4555-8666-777777777777";
        let second = "44444444-5555-4666-8777-888888888888";
        fs::write(
            claude.join("history.jsonl"),
            format!(
                "{{\"sessionId\":\"{first}\",\"display\":\"one\"}}\n\
                 {{\"sessionId\":\"{second}\",\"display\":\"two\"}}\n\
                 {{\"sessionId\":\"{first}\",\"display\":\"three\"}}\n\
                 {{\"sessionId\":\"other\",\"display\":\"keep\"}}\n"
            ),
        )?;
        fs::create_dir_all(claude.join("tasks").join(first))?;
        fs::create_dir_all(claude.join("file-history").join(second))?;
        let missing_first = claude
            .join("projects")
            .join("gone-project")
            .join(format!("{first}.jsonl"));
        let missing_second = claude
            .join("projects")
            .join("gone-project")
            .join(format!("{second}.jsonl"));

        let results = delete_claude_targets(
            &claude,
            vec![
                claude_target(first, Some(&missing_first)),
                claude_target(second, Some(&missing_second)),
            ],
        )?;

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| result.ok));
        assert_eq!(results[0].history_rows_deleted, 2);
        assert_eq!(results[1].history_rows_deleted, 1);
        assert!(results.iter().all(|result| result.rollout_missing));
        let history = fs::read_to_string(claude.join("history.jsonl"))?;
        assert!(!history.contains(first));
        assert!(!history.contains(second));
        assert!(history.contains("other"));
        assert!(!claude.join("tasks").join(first).exists());
        assert!(!claude.join("file-history").join(second).exists());

        fs::remove_dir_all(claude).ok();
        Ok(())
    }

    #[test]
    fn delete_claude_partial_cleanup_is_not_success() -> AppResult<()> {
        let claude = temp_dir("claude-delete-partial");
        let id = "55555555-6666-4777-8888-999999999999";
        let session = claude
            .join("projects")
            .join("sample-project")
            .join(format!("{id}.jsonl"));
        write_claude_session(&session, id)?;
        fs::create_dir_all(claude.join("tasks"))?;
        fs::write(claude.join("tasks").join(id), "not a directory")?;

        let result = delete_claude_targets(&claude, vec![claude_target(id, Some(&session))])?
            .pop()
            .expect("one delete result");

        assert!(result.rollout_deleted);
        assert!(
            !result.ok,
            "partial cleanup must not be reported as success"
        );
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("tasks"));

        fs::remove_dir_all(claude).ok();
        Ok(())
    }

    #[test]
    fn delete_claude_rejects_rollout_outside_projects() -> AppResult<()> {
        let claude = temp_dir("claude-delete-outside-projects");
        let id = "66666666-7777-4888-8999-000000000000";
        let outside = claude.join("outside").join(format!("{id}.jsonl"));
        write_claude_session(&outside, id)?;
        fs::create_dir_all(claude.join("projects"))?;

        let result = delete_claude_targets(&claude, vec![claude_target(id, Some(&outside))])?
            .pop()
            .expect("one delete result");

        assert!(!result.ok);
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("projects"));
        assert!(
            outside.is_file(),
            "an out-of-root target must not be deleted"
        );

        fs::remove_dir_all(claude).ok();
        Ok(())
    }

    #[test]
    fn delete_claude_rejects_jsonl_directory_target() -> AppResult<()> {
        let claude = temp_dir("claude-delete-jsonl-directory");
        let id = "77777777-8888-4999-8000-111111111111";
        let project = claude.join("projects").join("sample-project");
        let invalid_rollout = project.join(format!("{id}.jsonl"));
        let sidecar = project.join(id);
        fs::create_dir_all(&invalid_rollout)?;
        fs::create_dir_all(&sidecar)?;
        fs::write(sidecar.join("sentinel"), "keep")?;

        let result =
            delete_claude_targets(&claude, vec![claude_target(id, Some(&invalid_rollout))])?
                .pop()
                .expect("one delete result");

        assert!(!result.ok);
        assert!(invalid_rollout.is_dir());
        assert_eq!(fs::read_to_string(sidecar.join("sentinel"))?, "keep");
        fs::remove_dir_all(claude).ok();
        Ok(())
    }

    #[test]
    fn delete_claude_rejects_linked_parent_for_missing_rollout() -> AppResult<()> {
        let root = temp_dir("claude-delete-linked-parent");
        let claude = root.join("claude");
        let projects = claude.join("projects");
        let victim = root.join("victim");
        let id = "88888888-9999-4000-8111-222222222222";
        fs::create_dir_all(&projects)?;
        fs::create_dir_all(victim.join(id))?;
        fs::write(victim.join(id).join("sentinel"), "keep")?;
        let link = projects.join("linked-project");

        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_dir(&victim, &link) {
            if error.raw_os_error() == Some(1314) {
                let output = std::process::Command::new("pwsh")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        "$ErrorActionPreference = 'Stop'; New-Item -ItemType Junction -Path $env:CC_TEST_LINK -Target $env:CC_TEST_TARGET | Out-Null",
                    ])
                    .env("CC_TEST_LINK", &link)
                    .env("CC_TEST_TARGET", &victim)
                    .output()?;
                if !output.status.success() {
                    return Err(AppError::Other(format!(
                        "无法创建 junction 测试夹具: {}",
                        String::from_utf8_lossy(&output.stderr)
                    )));
                }
            } else {
                return Err(error.into());
            }
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, &link)?;

        let missing = link.join(format!("{id}.jsonl"));
        let result = delete_claude_targets(&claude, vec![claude_target(id, Some(&missing))])?
            .pop()
            .expect("one delete result");

        assert!(!result.ok);
        assert_eq!(
            fs::read_to_string(victim.join(id).join("sentinel"))?,
            "keep"
        );
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn delete_rejects_session_id_path_traversal() {
        for invalid in ["", ".", "..", "../outside", "..\\outside", "id:stream"] {
            assert!(validate_delete_id(invalid).is_err(), "accepted {invalid:?}");
        }
        assert!(validate_delete_id("agent-019d-safe_session").is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn delete_codex_rejects_rollout_root_junctions_without_touching_external_files() -> AppResult<()>
    {
        for root_name in ["sessions", "archived_sessions"] {
            let root = temp_dir(&format!("codex-delete-{root_name}-junction"));
            let codex = root.join("codex");
            let victim = root.join("external-rollouts");
            let id = format!("019d-{root_name}-junction-7000-8000-000000000001");
            let rollout = victim.join(format!("rollout-2026-07-10T10-00-00-{id}.jsonl"));
            fs::create_dir_all(&victim)?;
            write_test_rollout(&rollout, &id, "external sentinel");
            let expected = fs::read(&rollout)?;

            drop(create_codex_threads_table(&codex)?);
            let junction = codex.join(root_name);
            if root_name == "sessions" {
                fs::remove_dir(&junction)?;
            }
            create_windows_junction(&victim, &junction)?;

            let error = match delete_codex_artifacts(&codex, &id) {
                Ok(_) => panic!("rollout root junction must abort deletion"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains("junction") || error.to_string().contains("链接"),
                "unexpected error: {error}"
            );
            assert_eq!(fs::read(&rollout)?, expected);

            fs::remove_dir(&junction)?;
            fs::remove_dir_all(root).ok();
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn delete_codex_rejects_nested_junction_without_touching_external_files() -> AppResult<()> {
        let root = temp_dir("codex-delete-nested-junction");
        let codex = root.join("codex");
        let victim = root.join("external-rollouts");
        let id = "019d-nested-junction-7000-8000-000000000001";
        let rollout = victim.join(format!("rollout-2026-07-10T10-00-00-{id}.jsonl"));
        fs::create_dir_all(&victim)?;
        write_test_rollout(&rollout, id, "external sentinel");
        let expected = fs::read(&rollout)?;

        drop(create_codex_threads_table(&codex)?);
        let junction = codex.join("sessions").join("linked-day");
        create_windows_junction(&victim, &junction)?;

        let error = match delete_codex_artifacts(&codex, id) {
            Ok(_) => panic!("nested rollout junction must abort deletion"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("junction") || error.to_string().contains("链接"),
            "unexpected error: {error}"
        );
        assert_eq!(fs::read(&rollout)?, expected);

        fs::remove_dir(&junction)?;
        fs::remove_dir_all(root).ok();
        Ok(())
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

    #[derive(Clone, Copy)]
    struct FamilyBranchFixture<'a> {
        id: &'a str,
        archived: bool,
    }

    fn codex_family_fixture(
        codex: &Path,
        family_id: &str,
        active_id: &str,
        branches: &[FamilyBranchFixture<'_>],
    ) -> AppResult<BTreeMap<String, PathBuf>> {
        let conn = create_codex_threads_table(codex)?;
        let mut paths_by_id = BTreeMap::new();
        let mut chain = Vec::with_capacity(branches.len());
        let mut index = BTreeMap::new();
        let mut index_lines = String::new();

        for branch in branches {
            let file_name = format!("rollout-2026-05-10T10-00-00-{}.jsonl", branch.id);
            let rollout = if branch.archived {
                paths::archived_sessions_dir(codex).join(file_name)
            } else {
                paths::sessions_dir(codex)
                    .join("2026")
                    .join("05")
                    .join("10")
                    .join(file_name)
            };
            write_test_rollout(&rollout, branch.id, &format!("message for {}", branch.id));
            conn.execute(
                "INSERT INTO threads (
                    id, rollout_path, cwd, title, first_user_message, model, reasoning_effort,
                    tokens_used, created_at, updated_at, archived, archived_at, git_branch, source,
                    agent_nickname, agent_role
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 0, 1770000000, 1770000300, ?7, ?8, NULL, NULL, NULL, NULL)",
                params![
                    branch.id,
                    rollout.to_string_lossy().into_owned(),
                    "F:\\w",
                    format!("title for {}", branch.id),
                    format!("message for {}", branch.id),
                    "gpt-5",
                    if branch.archived { 1 } else { 0 },
                    if branch.archived { Some(1770000400_i64) } else { None },
                ],
            )?;

            let (sha256, line_count) = if branch.archived {
                let (sha256, line_count) = family::compute_integrity(&rollout)?;
                (Some(sha256), Some(line_count))
            } else {
                (None, None)
            };
            let status = if branch.id == active_id {
                BranchStatus::Active
            } else {
                BranchStatus::Archived
            };
            let relpath = rollout
                .strip_prefix(codex)
                .expect("fixture rollout belongs to codex dir")
                .to_string_lossy()
                .replace('\\', "/");
            chain.push(FamilyBranch {
                id: branch.id.to_string(),
                provider: if branch.id == active_id {
                    "custom".to_string()
                } else {
                    "openai".to_string()
                },
                created_at: "2026-05-10T10:00:00Z".to_string(),
                status,
                rollout_relpath: relpath,
                sha256,
                line_count,
                note: None,
            });
            index.insert(branch.id.to_string(), family_id.to_string());
            if !branch.archived {
                index_lines.push_str(&format!(
                    "{{\"id\":\"{}\",\"thread_name\":\"title for {}\",\"updated_at\":\"2026-05-10T10:00:00Z\"}}\n",
                    branch.id, branch.id
                ));
            }
            paths_by_id.insert(branch.id.to_string(), rollout);
        }
        drop(conn);

        let root_id = branches
            .first()
            .map(|branch| branch.id)
            .ok_or_else(|| AppError::Other("family fixture requires a branch".to_string()))?;
        let mut families = BTreeMap::new();
        families.insert(
            family_id.to_string(),
            Family {
                family_id: family_id.to_string(),
                root_id: root_id.to_string(),
                title: "family fixture".to_string(),
                chain,
                active_id: active_id.to_string(),
                updated_at: "2026-05-10T10:00:00Z".to_string(),
            },
        );
        family::save(
            codex,
            &FamilyStore {
                version: 1,
                families,
                index,
            },
        )?;
        fs::write(paths::session_index_path(codex), index_lines)?;
        Ok(paths_by_id)
    }

    fn rollout_cwd(path: &Path) -> AppResult<String> {
        Ok(family::read_session_meta(path)?
            .get("payload")
            .and_then(|payload| payload.get("cwd"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    #[test]
    fn rewrite_rollout_cwd_preserves_line_endings_and_tail_bytes() -> AppResult<()> {
        let root = temp_dir("rewrite-rollout-cwd-line-endings");
        fs::create_dir_all(&root)?;
        let cases: [(&str, &[u8], &[u8]); 2] = [
            (
                "lf",
                b"\n",
                b"{\"type\":\"event_msg\",\"payload\":{\"message\":\"keep\"}}\nlast-line",
            ),
            (
                "crlf",
                b"\r\n",
                b"{\"type\":\"event_msg\",\"payload\":{\"message\":\"keep\"}}\r\nlast-line",
            ),
        ];

        for (label, line_ending, tail) in cases {
            let path = root.join(format!("{label}.jsonl"));
            let meta = serde_json::json!({
                "timestamp": "2026-05-10T10:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": format!("rewrite-{label}"),
                    "cwd": "F:\\old"
                }
            });
            let mut original = serde_json::to_vec(&meta)?;
            original.extend_from_slice(line_ending);
            original.extend_from_slice(tail);
            fs::write(&path, original)?;

            assert!(rewrite_rollout_cwd(&path, "F:\\new")?);
            let updated = fs::read(&path)?;
            let newline = updated
                .iter()
                .position(|byte| *byte == b'\n')
                .expect("rewritten first line must keep its newline");
            let json_end = if newline > 0 && updated[newline - 1] == b'\r' {
                newline - 1
            } else {
                newline
            };

            assert_eq!(&updated[json_end..=newline], line_ending, "{label}");
            assert_eq!(&updated[newline + 1..], tail, "{label}");
            let rewritten_meta: serde_json::Value = serde_json::from_slice(&updated[..json_end])?;
            assert_eq!(rewritten_meta["payload"]["cwd"], "F:\\new", "{label}");
        }

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn rewrite_rollout_cwd_skips_identical_cwd_without_touching_file() -> AppResult<()> {
        let root = temp_dir("rewrite-rollout-cwd-identical");
        let path = root.join("same.jsonl");
        fs::create_dir_all(&root)?;
        let original = concat!(
            "{\"timestamp\":\"2026-05-10T10:00:00Z\",\"type\":\"session_meta\",",
            "\"payload\":{\"id\":\"same\",\"cwd\":\"F:\\\\same\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"message\":\"keep\"}}\n"
        )
        .as_bytes()
        .to_vec();
        fs::write(&path, &original)?;

        assert!(!rewrite_rollout_cwd(&path, "F:\\same")?);
        assert_eq!(fs::read(&path)?, original);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn move_family_cwd_updates_all_rollouts_threads_workspace_and_integrity() -> AppResult<()> {
        let codex = temp_dir("move-family-cwd");
        let family_id = "family-move-cwd";
        let active_id = "019d-family-move-active";
        let archived_id = "019d-family-move-archived";
        let paths_by_id = codex_family_fixture(
            &codex,
            family_id,
            active_id,
            &[
                FamilyBranchFixture {
                    id: active_id,
                    archived: false,
                },
                FamilyBranchFixture {
                    id: archived_id,
                    archived: true,
                },
            ],
        )?;
        fs::write(paths::codex_global_state_json_path(&codex), "{}")?;
        let target = codex.join("projects").join("new-workspace");
        fs::create_dir_all(&target)?;
        let expected_cwd = paths::strip_verbatim(&target.canonicalize()?.to_string_lossy());
        let archived_before = family::load(&codex)?
            .families
            .get(family_id)
            .and_then(|family| family.chain.iter().find(|branch| branch.id == archived_id))
            .and_then(|branch| branch.sha256.clone())
            .expect("archived branch fixture integrity");

        let report = move_session_cwd_with_lock(
            Some("codex".into()),
            codex.to_string_lossy().into_owned(),
            active_id.into(),
            target.to_string_lossy().into_owned(),
            &family::FamilyLock::default(),
        )?;

        assert!(report.rollout_rewritten);
        assert_eq!(report.threads_updated, 2);
        assert_eq!(report.new_cwd, expected_cwd);
        for id in [active_id, archived_id] {
            assert_eq!(
                rollout_cwd(paths_by_id.get(id).expect("fixture path"))?,
                expected_cwd
            );
        }

        let state = state_db::open_ro(&codex)?;
        for (id, expected_archived) in [(active_id, 0_i64), (archived_id, 1_i64)] {
            let (cwd, archived): (String, i64) = state.query_row(
                "SELECT cwd, CAST(archived AS INTEGER) FROM threads WHERE id = ?",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(cwd, expected_cwd, "{id}");
            assert_eq!(archived, expected_archived, "{id}");
        }
        drop(state);

        let global: serde_json::Value =
            serde_json::from_slice(&fs::read(paths::codex_global_state_json_path(&codex))?)?;
        for key in [
            "electron-saved-workspace-roots",
            "active-workspace-roots",
            "project-order",
        ] {
            assert!(
                global[key].as_array().is_some_and(|values| values
                    .iter()
                    .any(|value| value.as_str() == Some(expected_cwd.as_str()))),
                "workspace key {key} must contain the new cwd"
            );
        }

        let store = family::load(&codex)?;
        let archived_branch = store
            .families
            .get(family_id)
            .and_then(|family| family.chain.iter().find(|branch| branch.id == archived_id))
            .expect("archived family branch");
        let archived_path = paths_by_id.get(archived_id).expect("archived rollout");
        let (expected_sha, expected_lines) = family::compute_integrity(archived_path)?;
        assert_ne!(
            archived_branch.sha256.as_deref(),
            Some(archived_before.as_str())
        );
        assert_eq!(
            archived_branch.sha256.as_deref(),
            Some(expected_sha.as_str())
        );
        assert_eq!(archived_branch.line_count, Some(expected_lines));

        let index_ids = crate::repair::read_session_index_ids(&codex)?;
        assert!(index_ids.contains(active_id));
        assert!(!index_ids.contains(archived_id));

        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn move_family_cwd_rolls_back_rollouts_and_threads_after_late_failure() -> AppResult<()> {
        let codex = temp_dir("move-family-cwd-rollback");
        let active_id = "019d-family-move-rollback-active";
        let archived_id = "019d-family-move-rollback-archived";
        let paths_by_id = codex_family_fixture(
            &codex,
            "family-move-cwd-rollback",
            active_id,
            &[
                FamilyBranchFixture {
                    id: active_id,
                    archived: false,
                },
                FamilyBranchFixture {
                    id: archived_id,
                    archived: true,
                },
            ],
        )?;
        let rollout_before = paths_by_id
            .iter()
            .map(|(id, path)| fs::read(path).map(|bytes| (id.clone(), bytes)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let family_before = fs::read(paths::family_store_path(&codex))?;
        let index_before = fs::read(paths::session_index_path(&codex))?;
        let global_state = paths::codex_global_state_json_path(&codex);
        fs::write(&global_state, "{broken json")?;
        let global_before = fs::read(&global_state)?;
        let target = codex.join("projects").join("rollback-target");
        fs::create_dir_all(&target)?;

        let error = move_session_cwd_with_lock(
            Some("codex".into()),
            codex.to_string_lossy().into_owned(),
            active_id.into(),
            target.to_string_lossy().into_owned(),
            &family::FamilyLock::default(),
        )
        .expect_err("invalid global state must abort the family move");

        assert!(error.to_string().contains("全局状态 JSON 损坏"), "{error}");
        for (id, path) in &paths_by_id {
            assert_eq!(
                fs::read(path)?.as_slice(),
                rollout_before[id].as_slice(),
                "{id}"
            );
            assert_eq!(rollout_cwd(path)?, "F:\\w", "{id}");
        }
        assert_eq!(fs::read(paths::family_store_path(&codex))?, family_before);
        assert_eq!(fs::read(paths::session_index_path(&codex))?, index_before);
        assert_eq!(fs::read(&global_state)?, global_before);
        let state = state_db::open_ro(&codex)?;
        for id in [active_id, archived_id] {
            let cwd: String =
                state.query_row("SELECT cwd FROM threads WHERE id = ?", [id], |row| {
                    row.get(0)
                })?;
            assert_eq!(cwd, "F:\\w", "{id}");
        }
        drop(state);

        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn move_session_cwd_rejects_invalid_targets_before_writing() -> AppResult<()> {
        let codex = temp_dir("move-cwd-invalid-target");
        let session_id = "019d-move-invalid-target";
        let paths_by_id = codex_family_fixture(
            &codex,
            "family-move-invalid-target",
            session_id,
            &[FamilyBranchFixture {
                id: session_id,
                archived: false,
            }],
        )?;
        fs::write(paths::codex_global_state_json_path(&codex), "{}")?;
        let tracked_paths = [
            paths_by_id
                .get(session_id)
                .expect("fixture rollout")
                .clone(),
            paths::state_db_path(&codex),
            paths::session_index_path(&codex),
            paths::family_store_path(&codex),
            paths::codex_global_state_json_path(&codex),
        ];
        let before = tracked_paths
            .iter()
            .map(|path| fs::read(path).map(|bytes| (path.clone(), bytes)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let missing = codex.join("projects").join("missing");
        let lock = family::FamilyLock::default();

        for target in [
            "relative-project".to_string(),
            missing.to_string_lossy().into_owned(),
        ] {
            assert!(move_session_cwd_with_lock(
                Some("codex".into()),
                codex.to_string_lossy().into_owned(),
                session_id.into(),
                target,
                &lock,
            )
            .is_err());
            for (path, expected) in &before {
                let current = fs::read(path)?;
                assert_eq!(
                    current.as_slice(),
                    expected.as_slice(),
                    "must not modify {}",
                    path.display()
                );
            }
        }

        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn move_orphan_rollout_cwd_rebuilds_missing_thread_row() -> AppResult<()> {
        let codex = temp_dir("move-orphan-rollout-cwd");
        let session_id = "019d-move-orphan-rollout";
        drop(create_codex_threads_table(&codex)?);
        let rollout = paths::sessions_dir(&codex)
            .join("2026")
            .join("05")
            .join("10")
            .join(format!("rollout-2026-05-10T10-00-00-{session_id}.jsonl"));
        write_test_rollout(&rollout, session_id, "orphan rollout");
        let target = codex.join("projects").join("orphan-target");
        fs::create_dir_all(&target)?;
        let expected_cwd = paths::strip_verbatim(&target.canonicalize()?.to_string_lossy());

        let report = move_session_cwd_with_lock(
            Some("codex".into()),
            codex.to_string_lossy().into_owned(),
            session_id.into(),
            target.to_string_lossy().into_owned(),
            &family::FamilyLock::default(),
        )?;

        assert!(report.rollout_rewritten);
        assert_eq!(report.threads_updated, 1);
        assert_eq!(rollout_cwd(&rollout)?, expected_cwd);
        let state = state_db::open_ro(&codex)?;
        let (cwd, rollout_path, archived): (String, String, i64) = state.query_row(
            "SELECT cwd, rollout_path, CAST(archived AS INTEGER) FROM threads WHERE id = ?",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(cwd, expected_cwd);
        assert_eq!(PathBuf::from(rollout_path), rollout);
        assert_eq!(archived, 0);
        drop(state);

        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn archive_moves_rollout_updates_threads_and_index() -> AppResult<()> {
        let codex = temp_dir("codex-archive");
        let active = archive_fixture(&codex);
        let codex_str = codex.to_string_lossy().into_owned();
        let family_lock = family::FamilyLock::default();

        set_archived_with_lock(
            Some("codex".into()),
            codex_str.clone(),
            ARCHIVE_TEST_ID.into(),
            true,
            &family_lock,
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
        set_archived_with_lock(
            Some("codex".into()),
            codex_str,
            ARCHIVE_TEST_ID.into(),
            false,
            &family_lock,
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
    fn main_list_delete_removes_single_branch_family_and_all_metadata() -> AppResult<()> {
        let codex = temp_dir("codex-delete-single-family");
        let family_id = "family-single";
        let active_id = "019d-family-single-active";
        let paths_by_id = codex_family_fixture(
            &codex,
            family_id,
            active_id,
            &[FamilyBranchFixture {
                id: active_id,
                archived: false,
            }],
        )?;
        let lock = family::FamilyLock::default();

        let results = delete_sessions_with_lock(
            Some("codex".to_string()),
            codex.to_string_lossy().into_owned(),
            None,
            vec![active_id.to_string()],
            None,
            &lock,
        )?;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, active_id);
        assert!(results[0].ok, "{:?}", results[0].error);
        assert_eq!(results[0].threads_rows_deleted, 1);
        assert!(results[0].rollout_deleted);
        assert!(!paths_by_id[active_id].exists());
        let store = family::load(&codex)?;
        assert!(!store.families.contains_key(family_id));
        assert!(!store.index.contains_key(active_id));
        let conn = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        let remaining: i64 = conn.query_row(
            "SELECT COUNT(*) FROM threads WHERE id = ?",
            [active_id],
            |row| row.get(0),
        )?;
        assert_eq!(remaining, 0);
        assert!(!fs::read_to_string(paths::session_index_path(&codex))?.contains(active_id));

        drop(conn);
        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn main_list_delete_removes_all_continuous_family_branches_without_promotion() -> AppResult<()>
    {
        let codex = temp_dir("codex-delete-continuous-family");
        let family_id = "family-continuous";
        let archived_id = "019d-family-continuous-archived";
        let active_id = "019d-family-continuous-active";
        let paths_by_id = codex_family_fixture(
            &codex,
            family_id,
            active_id,
            &[
                FamilyBranchFixture {
                    id: archived_id,
                    archived: true,
                },
                FamilyBranchFixture {
                    id: active_id,
                    archived: false,
                },
            ],
        )?;
        let lock = family::FamilyLock::default();

        let results = delete_sessions_with_lock(
            Some("codex".to_string()),
            codex.to_string_lossy().into_owned(),
            None,
            vec![active_id.to_string()],
            None,
            &lock,
        )?;

        assert_eq!(results.len(), 1);
        assert!(results[0].ok, "{:?}", results[0].error);
        assert_eq!(results[0].threads_rows_deleted, 2);
        assert!(results[0].rollout_deleted);
        assert!(paths_by_id.values().all(|path| !path.exists()));
        let store = family::load(&codex)?;
        assert!(
            store.families.is_empty(),
            "family must not promote a branch"
        );
        assert!(store.index.is_empty());
        let conn = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        let remaining: i64 = conn.query_row(
            "SELECT COUNT(*) FROM threads WHERE id IN (?1, ?2)",
            params![archived_id, active_id],
            |row| row.get(0),
        )?;
        assert_eq!(remaining, 0);

        drop(conn);
        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn batch_delete_same_family_keeps_input_order_and_executes_physical_delete_once(
    ) -> AppResult<()> {
        let codex = temp_dir("codex-delete-family-batch-dedup");
        let family_id = "family-batch";
        let archived_id = "019d-family-batch-archived";
        let active_id = "019d-family-batch-active";
        let paths_by_id = codex_family_fixture(
            &codex,
            family_id,
            active_id,
            &[
                FamilyBranchFixture {
                    id: archived_id,
                    archived: true,
                },
                FamilyBranchFixture {
                    id: active_id,
                    archived: false,
                },
            ],
        )?;
        let lock = family::FamilyLock::default();

        let results = delete_sessions_with_lock(
            Some("codex".to_string()),
            codex.to_string_lossy().into_owned(),
            None,
            vec![archived_id.to_string(), active_id.to_string()],
            None,
            &lock,
        )?;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, archived_id);
        assert_eq!(results[1].id, active_id);
        assert!(results.iter().all(|result| result.ok));
        assert_eq!(results[0].threads_rows_deleted, 2);
        assert_eq!(
            results[1].threads_rows_deleted, 2,
            "the second result must reuse the one physical family deletion"
        );
        assert!(paths_by_id.values().all(|path| !path.exists()));
        let store = family::load(&codex)?;
        assert!(!store.families.contains_key(family_id));
        assert!(store
            .index
            .values()
            .all(|indexed_family| indexed_family != family_id));

        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn conflicting_family_mapping_causes_zero_destructive_changes() -> AppResult<()> {
        let codex = temp_dir("codex-delete-family-conflict");
        let family_id = "family-conflict";
        let active_id = "019d-family-conflict-active";
        let paths_by_id = codex_family_fixture(
            &codex,
            family_id,
            active_id,
            &[FamilyBranchFixture {
                id: active_id,
                archived: false,
            }],
        )?;
        let mut broken_store = family::load(&codex)?;
        broken_store.index.remove(active_id);
        fs::write(
            paths::family_store_path(&codex),
            serde_json::to_vec_pretty(&broken_store)?,
        )?;
        let family_before = fs::read(paths::family_store_path(&codex))?;
        let index_before = fs::read(paths::session_index_path(&codex))?;
        let rollout_before = fs::read(&paths_by_id[active_id])?;
        let lock = family::FamilyLock::default();

        let results = delete_sessions_with_lock(
            Some("codex".to_string()),
            codex.to_string_lossy().into_owned(),
            None,
            vec![active_id.to_string()],
            None,
            &lock,
        )?;

        assert_eq!(results.len(), 1);
        assert!(!results[0].ok);
        assert!(results[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("index"));
        assert_eq!(fs::read(paths::family_store_path(&codex))?, family_before);
        assert_eq!(fs::read(paths::session_index_path(&codex))?, index_before);
        assert_eq!(fs::read(&paths_by_id[active_id])?, rollout_before);
        let conn = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        let remaining: i64 = conn.query_row(
            "SELECT COUNT(*) FROM threads WHERE id = ?",
            [active_id],
            |row| row.get(0),
        )?;
        assert_eq!(remaining, 1);

        drop(conn);
        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn archive_roundtrip_preserves_family_role_and_refreshes_integrity() -> AppResult<()> {
        let codex = temp_dir("codex-archive-family-roundtrip");
        let family_id = "family-archive-roundtrip";
        let active_id = "019d-family-archive-roundtrip-active";
        let paths_by_id = codex_family_fixture(
            &codex,
            family_id,
            active_id,
            &[FamilyBranchFixture {
                id: active_id,
                archived: false,
            }],
        )?;
        let original_relpath = paths_by_id[active_id]
            .strip_prefix(&codex)
            .expect("fixture rollout belongs to codex dir")
            .to_string_lossy()
            .replace('\\', "/");
        let lock = family::FamilyLock::default();

        set_archived_with_lock(
            Some("codex".to_string()),
            codex.to_string_lossy().into_owned(),
            active_id.to_string(),
            true,
            &lock,
        )?;

        let archived_path = paths::archived_sessions_dir(&codex).join(
            paths_by_id[active_id]
                .file_name()
                .expect("fixture rollout file name"),
        );
        let expected_integrity = family::compute_integrity(&archived_path)?;
        let archived_store = family::load(&codex)?;
        let archived_family = archived_store
            .families
            .get(family_id)
            .expect("family remains after manual archive");
        let archived_branch = archived_family
            .chain
            .iter()
            .find(|branch| branch.id == active_id)
            .expect("active branch remains in family");
        assert_eq!(archived_family.active_id, active_id);
        assert!(matches!(archived_branch.status, BranchStatus::Active));
        assert_eq!(archived_branch.rollout_relpath, original_relpath);
        assert_eq!(
            archived_branch.sha256.as_deref(),
            Some(expected_integrity.0.as_str())
        );
        assert_eq!(archived_branch.line_count, Some(expected_integrity.1));

        set_archived_with_lock(
            Some("codex".to_string()),
            codex.to_string_lossy().into_owned(),
            active_id.to_string(),
            false,
            &lock,
        )?;

        let restored_store = family::load(&codex)?;
        let restored_family = restored_store
            .families
            .get(family_id)
            .expect("family remains after unarchive");
        let restored_branch = restored_family
            .chain
            .iter()
            .find(|branch| branch.id == active_id)
            .expect("active branch remains in family");
        assert_eq!(restored_family.active_id, active_id);
        assert!(matches!(restored_branch.status, BranchStatus::Active));
        assert_eq!(restored_branch.rollout_relpath, original_relpath);
        assert_eq!(restored_branch.sha256, None);
        assert_eq!(restored_branch.line_count, None);

        fs::remove_dir_all(codex).ok();
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
