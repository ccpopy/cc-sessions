use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};
use crate::logs_db;
use crate::models::{
    BackupDetail, BackupSummary, Manifest, ManifestSession, RestoreResult, VerifyItem, VerifyReport,
};
use crate::paths;
use crate::state_db;

const PROVIDER_CODEX: &str = "codex";
const PROVIDER_CLAUDE: &str = "claude";
const PROVIDER_GEMINI: &str = "gemini";

struct BackupThread {
    id: String,
    rollout_path: PathBuf,
    rollout_relpath: PathBuf,
    title: String,
    cwd: String,
    created_at: i64,
    updated_at: i64,
    tokens_used: i64,
    model: Option<String>,
    thread_row: serde_json::Value,
}

fn sha256_file(path: &Path) -> AppResult<String> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn create_backup(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    backup_dir: String,
    ids: Vec<String>,
    name: Option<String>,
    note: Option<String>,
) -> AppResult<BackupSummary> {
    if provider.as_deref().unwrap_or(PROVIDER_CODEX) == PROVIDER_CLAUDE {
        let claude = PathBuf::from(
            claude_dir
                .unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned()),
        );
        return create_claude_backup(claude, PathBuf::from(backup_dir), ids, name, note);
    }
    if provider.as_deref().unwrap_or(PROVIDER_CODEX) == PROVIDER_GEMINI {
        let gemini = PathBuf::from(
            claude_dir
                .unwrap_or_else(|| paths::default_gemini_dir().to_string_lossy().into_owned()),
        );
        return create_gemini_backup(gemini, PathBuf::from(backup_dir), ids, name, note);
    }

    let codex = PathBuf::from(&codex_dir);
    let backup_root = PathBuf::from(&backup_dir);
    fs::create_dir_all(&backup_root)?;

    let final_name = name
        .map(|n| n.trim().to_string())
        .unwrap_or_else(|| format!("backup-{}", chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S")));
    validate_backup_name(&final_name)?;
    let tmp = backup_root.join(format!(".{}.partial", final_name));
    let final_path = backup_root.join(&final_name);
    if final_path.exists() {
        return Err(AppError::Other(format!("备份已存在: {}", final_name)));
    }
    if tmp.exists() {
        return Err(AppError::Other(format!(
            "存在未完成的临时备份目录，请先检查或移除: {}",
            tmp.to_string_lossy()
        )));
    }

    let state = state_db::open_ro(&codex)?;
    let logs = if codex.join("logs_2.sqlite").is_file() {
        Some(logs_db::open_ro(&codex)?)
    } else {
        None
    };
    let mut backup_threads: Vec<BackupThread> = Vec::with_capacity(ids.len());

    for id in &ids {
        let thread = load_backup_thread(&state, &codex, id)?;
        if !thread.rollout_path.is_file() {
            return Err(AppError::NotFound(format!(
                "rollout 文件不存在，备份未开始写入。id={} path={}",
                id,
                thread.rollout_path.to_string_lossy()
            )));
        }
        backup_threads.push(thread);
    }
    let history_ids = backup_threads
        .iter()
        .map(|thread| thread.id.clone())
        .collect::<HashSet<_>>();
    let history_index =
        crate::history::collect_lines_for_ids(&paths::history_path(&codex), &history_ids)?;

    fs::create_dir_all(tmp.join("sessions"))?;

    let mut manifest = Manifest {
        version: 2,
        provider: Some(PROVIDER_CODEX.to_string()),
        created_at: chrono::Utc::now().to_rfc3339(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        codex_dir: codex.to_string_lossy().into_owned(),
        claude_dir: None,
        note,
        sessions: Vec::new(),
    };
    let mut threads_rows: Vec<serde_json::Value> = Vec::new();
    let mut logs_out = File::create(tmp.join("logs.ndjson"))?;

    for thread in &backup_threads {
        threads_rows.push(thread.thread_row.clone());

        let dest = tmp.join(&thread.rollout_relpath);
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p)?;
        }
        fs::copy(&thread.rollout_path, &dest)?;
        let sha = sha256_file(&dest)?;
        let bytes = fs::metadata(&dest)?.len();

        // 导出 logs
        let mut logs_count = 0u32;
        if let Some(conn) = logs.as_ref() {
            let mut stmt = conn.prepare("SELECT * FROM logs WHERE thread_id = ?")?;
            let col_cnt = stmt.column_count();
            let col_names: Vec<String> = (0..col_cnt)
                .map(|i| stmt.column_name(i).unwrap_or("").to_string())
                .collect();
            let rows = stmt.query_map([thread.id.as_str()], |r| {
                let mut obj = serde_json::Map::new();
                for (i, n) in col_names.iter().enumerate() {
                    let v = r.get_ref(i)?;
                    let jv = match v {
                        rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                        rusqlite::types::ValueRef::Integer(x) => serde_json::Value::from(x),
                        rusqlite::types::ValueRef::Real(x) => serde_json::Value::from(x),
                        rusqlite::types::ValueRef::Text(t) => {
                            serde_json::Value::String(String::from_utf8_lossy(t).into_owned())
                        }
                        rusqlite::types::ValueRef::Blob(b) => {
                            serde_json::Value::String(hex::encode(b))
                        }
                    };
                    obj.insert(n.clone(), jv);
                }
                Ok(serde_json::Value::Object(obj))
            })?;
            for row in rows.flatten() {
                writeln!(logs_out, "{}", serde_json::to_string(&row)?)?;
                logs_count += 1;
            }
        }

        let history_rows = history_index
            .get(&thread.id)
            .map(|rows| rows.len() as u32)
            .unwrap_or(0);

        manifest.sessions.push(ManifestSession {
            provider: Some(PROVIDER_CODEX.to_string()),
            id: thread.id.clone(),
            rollout_relpath: thread.rollout_relpath.to_string_lossy().replace('\\', "/"),
            source_relpath: None,
            sidecar_relpath: None,
            title: thread.title.clone(),
            cwd: thread.cwd.clone(),
            created_at: thread.created_at,
            updated_at: thread.updated_at,
            tokens_used: thread.tokens_used,
            model: thread.model.clone(),
            bytes_rollout: bytes,
            logs_count,
            history_rows,
            sha256_rollout: sha,
        });
    }
    write_backup_history(
        &tmp,
        backup_threads.iter().map(|thread| thread.id.as_str()),
        &history_index,
    )?;

    fs::write(
        tmp.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::write(
        tmp.join("threads.json"),
        serde_json::to_vec_pretty(&threads_rows)?,
    )?;
    drop(logs_out);

    fs::rename(&tmp, &final_path)?;

    summarize_backup(&final_path)
}

fn create_claude_backup(
    claude: PathBuf,
    backup_root: PathBuf,
    ids: Vec<String>,
    name: Option<String>,
    note: Option<String>,
) -> AppResult<BackupSummary> {
    fs::create_dir_all(&backup_root)?;

    let final_name = name
        .map(|n| n.trim().to_string())
        .unwrap_or_else(|| format!("backup-{}", chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S")));
    validate_backup_name(&final_name)?;
    let tmp = backup_root.join(format!(".{}.partial", final_name));
    let final_path = backup_root.join(&final_name);
    if final_path.exists() {
        return Err(AppError::Other(format!("备份已存在: {}", final_name)));
    }
    if tmp.exists() {
        return Err(AppError::Other(format!(
            "存在未完成的临时备份目录，请先检查或移除: {}",
            tmp.to_string_lossy()
        )));
    }

    let sessions = crate::claude_sessions::scan_sessions(&claude)?;
    let by_id: HashMap<String, crate::models::SessionSummary> =
        sessions.into_iter().map(|s| (s.id.clone(), s)).collect();
    let history_ids = ids.iter().cloned().collect::<HashSet<_>>();
    let history_index =
        crate::history::collect_lines_for_ids(&paths::history_path(&claude), &history_ids)?;

    let mut manifest = Manifest {
        version: 2,
        provider: Some(PROVIDER_CLAUDE.to_string()),
        created_at: chrono::Utc::now().to_rfc3339(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        codex_dir: String::new(),
        claude_dir: Some(claude.to_string_lossy().into_owned()),
        note,
        sessions: Vec::new(),
    };

    for id in &ids {
        let session = by_id
            .get(id)
            .ok_or_else(|| AppError::NotFound(format!("Claude 会话不存在: {id}")))?;
        let source = PathBuf::from(&session.rollout_path);
        if !source.is_file() {
            return Err(AppError::NotFound(format!(
                "Claude JSONL 文件不存在，备份未开始写入。id={} path={}",
                id,
                source.to_string_lossy()
            )));
        }
        let source_rel = crate::claude_sessions::session_relpath(&claude, &source);
        let source_rel_string = source_rel.to_string_lossy().replace('\\', "/");
        let dest_rel = PathBuf::from(PROVIDER_CLAUDE).join(&source_rel);
        let dest = tmp.join(&dest_rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source, &dest)?;
        let sha = sha256_file(&dest)?;
        let bytes = fs::metadata(&dest)?.len();

        let mut sidecar_rel: Option<String> = None;
        if let Some(sidecar) = crate::claude_sessions::sidecar_path_for(&source) {
            if sidecar.exists() {
                let sidecar_dest_rel = PathBuf::from("sidecars").join(paths::sanitize_slug(id));
                copy_path_recursive(&sidecar, &tmp.join(&sidecar_dest_rel))?;
                sidecar_rel = Some(sidecar_dest_rel.to_string_lossy().replace('\\', "/"));
            }
        }

        let history_rows = history_index
            .get(&session.id)
            .map(|rows| rows.len() as u32)
            .unwrap_or(0);

        manifest.sessions.push(ManifestSession {
            provider: Some(PROVIDER_CLAUDE.to_string()),
            id: session.id.clone(),
            rollout_relpath: dest_rel.to_string_lossy().replace('\\', "/"),
            source_relpath: Some(source_rel_string),
            sidecar_relpath: sidecar_rel,
            title: session.title.clone(),
            cwd: session.cwd.clone(),
            created_at: session.created_at,
            updated_at: session.updated_at,
            tokens_used: session.tokens_used,
            model: session.model.clone(),
            bytes_rollout: bytes,
            logs_count: 0,
            history_rows,
            sha256_rollout: sha,
        });
    }
    write_backup_history(&tmp, ids.iter().map(String::as_str), &history_index)?;

    fs::write(
        tmp.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::rename(&tmp, &final_path)?;
    summarize_backup(&final_path)
}

fn create_gemini_backup(
    gemini: PathBuf,
    backup_root: PathBuf,
    ids: Vec<String>,
    name: Option<String>,
    note: Option<String>,
) -> AppResult<BackupSummary> {
    fs::create_dir_all(&backup_root)?;

    let final_name = name
        .map(|n| n.trim().to_string())
        .unwrap_or_else(|| format!("backup-{}", chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S")));
    validate_backup_name(&final_name)?;
    let tmp = backup_root.join(format!(".{}.partial", final_name));
    let final_path = backup_root.join(&final_name);
    if final_path.exists() {
        return Err(AppError::Other(format!("备份已存在: {}", final_name)));
    }
    if tmp.exists() {
        return Err(AppError::Other(format!(
            "存在未完成的临时备份目录，请先检查或移除: {}",
            tmp.to_string_lossy()
        )));
    }

    let sessions = crate::gemini_sessions::scan_sessions(&gemini)?;
    let by_id: HashMap<String, crate::models::SessionSummary> =
        sessions.into_iter().map(|s| (s.id.clone(), s)).collect();

    let mut manifest = Manifest {
        version: 2,
        provider: Some(PROVIDER_GEMINI.to_string()),
        created_at: chrono::Utc::now().to_rfc3339(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        codex_dir: String::new(),
        claude_dir: Some(gemini.to_string_lossy().into_owned()),
        note,
        sessions: Vec::new(),
    };

    for id in &ids {
        let session = by_id
            .get(id)
            .ok_or_else(|| AppError::NotFound(format!("Gemini 会话不存在: {id}")))?;
        let source = PathBuf::from(&session.rollout_path);
        if !source.is_file() {
            return Err(AppError::NotFound(format!(
                "Gemini 会话文件不存在，备份未开始写入。id={} path={}",
                id,
                source.to_string_lossy()
            )));
        }

        let source_rel = crate::gemini_sessions::session_relpath(&gemini, &source);
        let source_rel_string = source_rel.to_string_lossy().replace('\\', "/");
        let dest_rel = PathBuf::from(PROVIDER_GEMINI).join(&source_rel);
        let dest = tmp.join(&dest_rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source, &dest)?;
        let sha = sha256_file(&dest)?;
        let bytes = fs::metadata(&dest)?.len();

        let associated = crate::gemini_sessions::associated_paths(&gemini, id)?;
        let mut sidecar_rel: Option<String> = None;
        if !associated.is_empty() {
            let rel = PathBuf::from("sidecars").join(paths::sanitize_slug(id));
            let sidecar_root = tmp.join(&rel);
            for item in associated {
                copy_path_recursive(&item.abs, &sidecar_root.join(&item.rel))?;
            }
            sidecar_rel = Some(rel.to_string_lossy().replace('\\', "/"));
        }

        manifest.sessions.push(ManifestSession {
            provider: Some(PROVIDER_GEMINI.to_string()),
            id: session.id.clone(),
            rollout_relpath: dest_rel.to_string_lossy().replace('\\', "/"),
            source_relpath: Some(source_rel_string),
            sidecar_relpath: sidecar_rel,
            title: session.title.clone(),
            cwd: session.cwd.clone(),
            created_at: session.created_at,
            updated_at: session.updated_at,
            tokens_used: session.tokens_used,
            model: session.model.clone(),
            bytes_rollout: bytes,
            logs_count: 0,
            history_rows: 0,
            sha256_rollout: sha,
        });
    }

    fs::write(
        tmp.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::rename(&tmp, &final_path)?;
    summarize_backup(&final_path)
}

fn validate_backup_name(name: &str) -> AppResult<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return Err(AppError::Path("备份名不能为空或路径保留名".into()));
    }
    let invalid = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    if trimmed
        .chars()
        .any(|c| invalid.contains(&c) || c.is_control())
    {
        return Err(AppError::Path(format!(
            "备份名包含 Windows 文件名不允许的字符: {}",
            name
        )));
    }
    Ok(())
}

fn load_backup_thread(
    state: &rusqlite::Connection,
    codex: &Path,
    id: &str,
) -> AppResult<BackupThread> {
    let mut stmt = state.prepare(
        "SELECT id, rollout_path, created_at, updated_at, source, model_provider,
                cwd, title, sandbox_policy, approval_mode, COALESCE(tokens_used,0),
                has_user_event, archived, archived_at, git_sha, git_branch, git_origin_url,
                cli_version, first_user_message, agent_nickname, agent_role, memory_mode,
                model, reasoning_effort, agent_path, created_at_ms, updated_at_ms
         FROM threads WHERE id = ?",
    )?;
    let row_json = match stmt.query_row([id], |row| {
        let mut obj = serde_json::Map::new();
        let cols = [
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
        for (i, name) in cols.iter().enumerate() {
            let v = row.get_ref(i)?;
            let jv = match v {
                rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                rusqlite::types::ValueRef::Integer(i) => serde_json::Value::from(i),
                rusqlite::types::ValueRef::Real(f) => serde_json::Value::from(f),
                rusqlite::types::ValueRef::Text(t) => {
                    serde_json::Value::String(String::from_utf8_lossy(t).into_owned())
                }
                rusqlite::types::ValueRef::Blob(b) => serde_json::Value::String(hex::encode(b)),
            };
            obj.insert((*name).to_string(), jv);
        }
        Ok(serde_json::Value::Object(obj))
    }) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(AppError::NotFound(format!("threads 中未找到 id: {}", id)));
        }
        Err(e) => return Err(e.into()),
    };

    let rollout_path_raw = row_json
        .get("rollout_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let rollout_path_clean = paths::strip_verbatim(&rollout_path_raw);
    let rollout_relpath = rel_path(&rollout_path_clean, codex)?;

    Ok(BackupThread {
        id: id.to_string(),
        rollout_path: PathBuf::from(rollout_path_clean),
        rollout_relpath,
        title: row_json
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        cwd: row_json
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        created_at: row_json
            .get("created_at")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        updated_at: row_json
            .get("updated_at")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        tokens_used: row_json
            .get("tokens_used")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        model: row_json
            .get("model")
            .and_then(|v| v.as_str())
            .map(String::from),
        thread_row: row_json,
    })
}

fn rel_path(abs: &str, codex: &Path) -> AppResult<PathBuf> {
    let abs_clean = paths::strip_verbatim(abs);
    let codex_clean = paths::strip_verbatim(&codex.to_string_lossy());
    let abs_p = PathBuf::from(&abs_clean);
    let cx_p = PathBuf::from(&codex_clean);
    match abs_p.strip_prefix(&cx_p) {
        Ok(rel) => Ok(rel.to_path_buf()),
        Err(_) => Ok(abs_p
            .file_name()
            .map(|n| PathBuf::from("sessions").join(n))
            .unwrap_or_else(|| PathBuf::from("sessions/unknown.jsonl"))),
    }
}

fn summarize_backup(path: &Path) -> AppResult<BackupSummary> {
    let manifest_path = path.join("manifest.json");
    let raw = fs::read_to_string(&manifest_path)?;
    let manifest: Manifest = serde_json::from_str(&raw)?;
    let total_bytes: u64 = manifest.sessions.iter().map(|s| s.bytes_rollout).sum();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(BackupSummary {
        path: path.to_string_lossy().into_owned(),
        name,
        provider: manifest.provider.clone(),
        created_at: manifest.created_at,
        sessions_count: manifest.sessions.len() as u32,
        total_bytes,
        note: manifest.note,
    })
}

fn write_backup_history<'a>(
    backup_dir: &Path,
    ids: impl Iterator<Item = &'a str>,
    history_index: &HashMap<String, Vec<String>>,
) -> AppResult<u32> {
    let mut lines = Vec::new();
    for id in ids {
        if let Some(rows) = history_index.get(id) {
            lines.extend(rows.iter().cloned());
        }
    }
    crate::history::write_lines(&backup_dir.join("history.jsonl"), &lines)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn list_backups(backup_dir: String, provider: Option<String>) -> AppResult<Vec<BackupSummary>> {
    let root = PathBuf::from(&backup_dir);
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&root)? {
        let e = entry?;
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        if p.join("manifest.json").is_file() {
            if let Ok(s) = summarize_backup(&p) {
                if let Some(provider) = provider.as_deref() {
                    let backup_provider = backup_provider(&s.provider);
                    if backup_provider != provider {
                        continue;
                    }
                }
                out.push(s);
            }
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn open_backup(backup_path: String) -> AppResult<BackupDetail> {
    let p = PathBuf::from(&backup_path);
    let summary = summarize_backup(&p)?;
    let raw = fs::read_to_string(p.join("manifest.json"))?;
    let manifest: Manifest = serde_json::from_str(&raw)?;
    Ok(BackupDetail { summary, manifest })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn delete_backup(backup_path: String) -> AppResult<()> {
    let p = PathBuf::from(&backup_path);
    if p.is_dir() {
        fs::remove_dir_all(&p)?;
    }
    Ok(())
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn verify_backup(backup_path: String) -> AppResult<VerifyReport> {
    let p = PathBuf::from(&backup_path);
    let raw = fs::read_to_string(p.join("manifest.json"))?;
    let manifest: Manifest = serde_json::from_str(&raw)?;
    let mut items = Vec::new();
    let mut all_ok = true;
    for s in &manifest.sessions {
        let rel = paths::checked_relative_path(&s.rollout_relpath)?;
        let file = p.join(&rel);
        if !file.exists() {
            all_ok = false;
            items.push(VerifyItem {
                id: s.id.clone(),
                ok: false,
                expected_sha: s.sha256_rollout.clone(),
                actual_sha: None,
                missing: true,
            });
            continue;
        }
        match sha256_file(&file) {
            Ok(sha) => {
                let ok = sha == s.sha256_rollout;
                if !ok {
                    all_ok = false;
                }
                items.push(VerifyItem {
                    id: s.id.clone(),
                    ok,
                    expected_sha: s.sha256_rollout.clone(),
                    actual_sha: Some(sha),
                    missing: false,
                });
            }
            Err(_) => {
                all_ok = false;
                items.push(VerifyItem {
                    id: s.id.clone(),
                    ok: false,
                    expected_sha: s.sha256_rollout.clone(),
                    actual_sha: None,
                    missing: false,
                });
            }
        }
    }
    Ok(VerifyReport { items, all_ok })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn restore_session(
    provider: Option<String>,
    backup_path: String,
    codex_dir: String,
    claude_dir: Option<String>,
    id: String,
    overwrite: bool,
) -> AppResult<RestoreResult> {
    let backup = PathBuf::from(&backup_path);
    let codex = PathBuf::from(&codex_dir);
    let raw = fs::read_to_string(backup.join("manifest.json"))?;
    let manifest: Manifest = serde_json::from_str(&raw)?;
    let target = manifest
        .sessions
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| AppError::NotFound(format!("manifest 中未找到 id: {}", id)))?;
    let provider = provider
        .as_deref()
        .unwrap_or_else(|| manifest_session_provider(&manifest, target));
    if provider == PROVIDER_CLAUDE {
        let claude = PathBuf::from(
            claude_dir
                .unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned()),
        );
        restore_one_claude(&backup, &claude, target, overwrite)
    } else if provider == PROVIDER_GEMINI {
        let gemini = PathBuf::from(
            claude_dir
                .unwrap_or_else(|| paths::default_gemini_dir().to_string_lossy().into_owned()),
        );
        restore_one_gemini(&backup, &gemini, target, overwrite)
    } else {
        restore_one(&backup, &codex, target, overwrite)
    }
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn restore_all(
    provider: Option<String>,
    backup_path: String,
    codex_dir: String,
    claude_dir: Option<String>,
    overwrite: bool,
) -> AppResult<Vec<RestoreResult>> {
    let backup = PathBuf::from(&backup_path);
    let codex = PathBuf::from(&codex_dir);
    let raw = fs::read_to_string(backup.join("manifest.json"))?;
    let manifest: Manifest = serde_json::from_str(&raw)?;
    let mut out = Vec::new();
    for s in &manifest.sessions {
        let session_provider = provider
            .as_deref()
            .unwrap_or_else(|| manifest_session_provider(&manifest, s));
        out.push(
            (if session_provider == PROVIDER_CLAUDE {
                let claude =
                    PathBuf::from(claude_dir.clone().unwrap_or_else(|| {
                        paths::default_claude_dir().to_string_lossy().into_owned()
                    }));
                restore_one_claude(&backup, &claude, s, overwrite)
            } else if session_provider == PROVIDER_GEMINI {
                let gemini =
                    PathBuf::from(claude_dir.clone().unwrap_or_else(|| {
                        paths::default_gemini_dir().to_string_lossy().into_owned()
                    }));
                restore_one_gemini(&backup, &gemini, s, overwrite)
            } else {
                restore_one(&backup, &codex, s, overwrite)
            })
            .unwrap_or_else(|e| RestoreResult {
                id: s.id.clone(),
                ok: false,
                threads_inserted: false,
                logs_inserted: 0,
                history_appended: 0,
                rollout_copied: false,
                conflict: false,
                error: Some(e.to_string()),
            }),
        );
    }
    Ok(out)
}

fn backup_provider(provider: &Option<String>) -> &str {
    provider.as_deref().unwrap_or(PROVIDER_CODEX)
}

fn manifest_session_provider<'a>(manifest: &'a Manifest, session: &'a ManifestSession) -> &'a str {
    session
        .provider
        .as_deref()
        .or(manifest.provider.as_deref())
        .unwrap_or(PROVIDER_CODEX)
}

fn restore_one_claude(
    backup: &Path,
    claude: &Path,
    target: &ManifestSession,
    overwrite: bool,
) -> AppResult<RestoreResult> {
    let mut result = RestoreResult {
        id: target.id.clone(),
        ok: false,
        threads_inserted: false,
        logs_inserted: 0,
        history_appended: 0,
        rollout_copied: false,
        conflict: false,
        error: None,
    };

    let source_rel = target
        .source_relpath
        .as_deref()
        .unwrap_or(&target.rollout_relpath);
    let target_rel = paths::checked_relative_path(source_rel)?;
    let backup_rel = paths::checked_relative_path(&target.rollout_relpath)?;
    let src = backup.join(&backup_rel);
    let dest = paths::claude_projects_dir(claude).join(&target_rel);

    if dest.exists() && !overwrite {
        result.conflict = true;
        return Ok(result);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&src, &dest)?;
    result.rollout_copied = true;

    if let Some(sidecar_rel) = target.sidecar_relpath.as_deref() {
        let sidecar_src = backup.join(paths::checked_relative_path(sidecar_rel)?);
        if sidecar_src.exists() {
            if let Some(sidecar_dest) = crate::claude_sessions::sidecar_path_for(&dest) {
                if sidecar_dest.exists() && overwrite {
                    remove_path_recursive(&sidecar_dest)?;
                }
                if !sidecar_dest.exists() {
                    copy_path_recursive(&sidecar_src, &sidecar_dest)?;
                }
            }
        }
    }
    result.history_appended = crate::history::append_from_file(
        &paths::history_path(claude),
        &backup.join("history.jsonl"),
        &target.id,
    )?;

    result.ok = true;
    Ok(result)
}

fn restore_one_gemini(
    backup: &Path,
    gemini: &Path,
    target: &ManifestSession,
    overwrite: bool,
) -> AppResult<RestoreResult> {
    let mut result = RestoreResult {
        id: target.id.clone(),
        ok: false,
        threads_inserted: false,
        logs_inserted: 0,
        history_appended: 0,
        rollout_copied: false,
        conflict: false,
        error: None,
    };

    let backup_rel = paths::checked_relative_path(&target.rollout_relpath)?;
    let source_rel = target
        .source_relpath
        .as_deref()
        .unwrap_or(&target.rollout_relpath);
    let target_rel = paths::checked_relative_path(source_rel)?;
    let src = backup.join(&backup_rel);
    let dest = gemini.join(&target_rel);

    if dest.exists() && !overwrite {
        result.conflict = true;
        return Ok(result);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&src, &dest)?;
    result.rollout_copied = true;

    if let Some(sidecar_rel) = target.sidecar_relpath.as_deref() {
        let sidecar_src = backup.join(paths::checked_relative_path(sidecar_rel)?);
        if sidecar_src.exists() {
            for entry in walkdir::WalkDir::new(&sidecar_src)
                .into_iter()
                .filter_map(|entry| entry.ok())
            {
                let rel = entry.path().strip_prefix(&sidecar_src).map_err(|e| {
                    AppError::Path(format!(
                        "无法计算 Gemini sidecar 相对路径 {}: {}",
                        entry.path().to_string_lossy(),
                        e
                    ))
                })?;
                if rel.as_os_str().is_empty() {
                    continue;
                }
                let target_path = gemini.join(rel);
                if entry.file_type().is_dir() {
                    fs::create_dir_all(&target_path)?;
                } else if entry.file_type().is_file() {
                    if target_path.exists() && overwrite {
                        remove_path_recursive(&target_path)?;
                    }
                    if !target_path.exists() {
                        if let Some(parent) = target_path.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::copy(entry.path(), &target_path)?;
                    }
                }
            }
        }
    }

    result.ok = true;
    Ok(result)
}

fn restore_one(
    backup: &Path,
    codex: &Path,
    target: &ManifestSession,
    overwrite: bool,
) -> AppResult<RestoreResult> {
    let mut result = RestoreResult {
        id: target.id.clone(),
        ok: false,
        threads_inserted: false,
        logs_inserted: 0,
        history_appended: 0,
        rollout_copied: false,
        conflict: false,
        error: None,
    };
    let target_rel = paths::checked_relative_path(&target.rollout_relpath)?;

    // 1) 冲突检测
    let state = state_db::open(codex)?;
    let exists: bool = state
        .query_row("SELECT 1 FROM threads WHERE id = ?", [&target.id], |_| {
            Ok(true)
        })
        .unwrap_or(false);
    if exists && !overwrite {
        result.conflict = true;
        return Ok(result);
    }

    // 2) 读 threads.json 找对应行
    let threads_raw = fs::read_to_string(backup.join("threads.json"))?;
    let threads: Vec<serde_json::Value> = serde_json::from_str(&threads_raw)?;
    let row = threads
        .iter()
        .find(|v| v.get("id").and_then(|x| x.as_str()) == Some(target.id.as_str()))
        .ok_or_else(|| AppError::NotFound(format!("threads.json 中缺 id: {}", target.id)))?;

    // 3) 拷 rollout 回去
    let src = backup.join(&target_rel);
    let dest = codex.join(&target_rel);
    if let Some(p) = dest.parent() {
        fs::create_dir_all(p)?;
    }
    fs::copy(&src, &dest)?;
    result.rollout_copied = true;

    // 4) INSERT OR REPLACE threads
    let cols_sql = "id, rollout_path, created_at, updated_at, source, model_provider, cwd, title,
                    sandbox_policy, approval_mode, tokens_used, has_user_event, archived, archived_at,
                    git_sha, git_branch, git_origin_url, cli_version, first_user_message,
                    agent_nickname, agent_role, memory_mode, model, reasoning_effort, agent_path,
                    created_at_ms, updated_at_ms";
    let cols: Vec<&str> = cols_sql.split(',').map(|s| s.trim()).collect();
    let placeholders = (0..cols.len()).map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "INSERT OR REPLACE INTO threads ({}) VALUES ({})",
        cols_sql, placeholders
    );
    let mut stmt = state.prepare(&sql)?;

    let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(cols.len());
    for name in &cols {
        let v = row.get(*name).unwrap_or(&serde_json::Value::Null);
        let boxed: Box<dyn rusqlite::ToSql> = match v {
            serde_json::Value::Null => Box::new(Option::<String>::None),
            serde_json::Value::Bool(b) => Box::new(if *b { 1i64 } else { 0i64 }),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Box::new(i)
                } else if let Some(f) = n.as_f64() {
                    Box::new(f)
                } else {
                    Box::new(n.to_string())
                }
            }
            serde_json::Value::String(s) => {
                if *name == "rollout_path" {
                    let resolved = codex.join(&target_rel);
                    Box::new(resolved.to_string_lossy().into_owned())
                } else {
                    Box::new(s.clone())
                }
            }
            other => Box::new(other.to_string()),
        };
        values.push(boxed);
    }
    let params: Vec<&dyn rusqlite::ToSql> = values.iter().map(|b| b.as_ref()).collect();
    stmt.execute(params.as_slice())?;
    result.threads_inserted = true;

    // 5) 还原 logs
    let logs_path = backup.join("logs.ndjson");
    if logs_path.is_file() {
        let logs = logs_db::open(codex)?;
        let file = File::open(&logs_path)?;
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(x) => x,
                Err(_) => continue,
            };
            let tid = v.get("thread_id").and_then(|x| x.as_str()).unwrap_or("");
            if tid != target.id {
                continue;
            }
            // 通用插入：用 obj keys
            if let Some(obj) = v.as_object() {
                let keys: Vec<String> = obj.keys().cloned().collect();
                let placeholders = (0..keys.len()).map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!(
                    "INSERT OR IGNORE INTO logs ({}) VALUES ({})",
                    keys.join(","),
                    placeholders
                );
                let mut stmt = logs.prepare(&sql)?;
                let mut boxed: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
                for k in &keys {
                    let val = &obj[k];
                    let b: Box<dyn rusqlite::ToSql> = match val {
                        serde_json::Value::Null => Box::new(Option::<String>::None),
                        serde_json::Value::Bool(b) => Box::new(if *b { 1i64 } else { 0i64 }),
                        serde_json::Value::Number(n) => {
                            if let Some(i) = n.as_i64() {
                                Box::new(i)
                            } else if let Some(f) = n.as_f64() {
                                Box::new(f)
                            } else {
                                Box::new(n.to_string())
                            }
                        }
                        serde_json::Value::String(s) => Box::new(s.clone()),
                        other => Box::new(other.to_string()),
                    };
                    boxed.push(b);
                }
                let p: Vec<&dyn rusqlite::ToSql> = boxed.iter().map(|b| b.as_ref()).collect();
                if stmt.execute(p.as_slice()).is_ok() {
                    result.logs_inserted += 1;
                }
            }
        }
    }
    result.history_appended = crate::history::append_from_file(
        &paths::history_path(codex),
        &backup.join("history.jsonl"),
        &target.id,
    )?;

    // 6) 更新 session_index.jsonl（append 一行，若已存在则跳过）
    let index_path = codex.join("session_index.jsonl");
    let mut already = false;
    if index_path.exists() {
        let raw = fs::read_to_string(&index_path)?;
        for line in raw.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("id").and_then(|x| x.as_str()) == Some(target.id.as_str()) {
                    already = true;
                    break;
                }
            }
        }
    }
    if !already {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index_path)?;
        let entry = serde_json::json!({
            "id": target.id,
            "rollout_path": codex.join(&target_rel).to_string_lossy(),
            "updated_at": target.updated_at,
        });
        writeln!(f, "{}", entry)?;
    }

    result.ok = true;
    let _ = already;
    Ok(result)
}

fn copy_path_recursive(from: &Path, to: &Path) -> AppResult<()> {
    if from.is_file() {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from, to)?;
        return Ok(());
    }
    if !from.is_dir() {
        return Err(AppError::NotFound(format!(
            "待复制路径不存在: {}",
            from.to_string_lossy()
        )));
    }
    for entry in walkdir::WalkDir::new(from)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let rel = entry.path().strip_prefix(from).map_err(|e| {
            AppError::Path(format!(
                "无法计算相对路径 {}: {}",
                entry.path().to_string_lossy(),
                e
            ))
        })?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dest = to.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dest)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

fn remove_path_recursive(path: &Path) -> AppResult<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)?;
    } else if path.is_file() {
        fs::remove_file(path)?;
    }
    Ok(())
}

// 让 compiler 不要抱怨未使用的 HashMap（保留作未来扩展）
#[allow(dead_code)]
fn _unused() {
    let _: HashMap<String, u32> = HashMap::new();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ))
    }

    fn write_claude_session(claude: &Path, id: &str) -> AppResult<()> {
        let dir = claude.join("projects").join("sample-project");
        fs::create_dir_all(&dir)?;
        let line = serde_json::json!({
            "sessionId": id,
            "cwd": "F:\\work\\sample-project",
            "timestamp": "2026-04-20T10:00:00Z",
            "type": "user",
            "message": {"role": "user", "content": "hello claude"}
        });
        fs::write(
            dir.join(format!("{id}.jsonl")),
            format!("{}\n", serde_json::to_string(&line)?),
        )?;
        fs::write(
            claude.join("history.jsonl"),
            format!(
                "{{\"sessionId\":\"{id}\",\"display\":\"keep one\"}}\n\
                 {{\"session_id\":\"other-session\",\"display\":\"ignore\"}}\n\
                 {{\"id\":\"{id}\",\"display\":\"keep two\"}}\n"
            ),
        )?;
        Ok(())
    }

    #[test]
    fn backs_up_and_restores_claude_session() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-claude-backup-test");
        let source_claude = root.join("source-claude");
        let restore_claude = root.join("restore-claude");
        let backup_dir = root.join("backups");
        write_claude_session(&source_claude, "claude-backup-1")?;

        let summary = create_backup(
            Some(PROVIDER_CLAUDE.to_string()),
            String::new(),
            Some(source_claude.to_string_lossy().into_owned()),
            backup_dir.to_string_lossy().into_owned(),
            vec!["claude-backup-1".to_string()],
            Some("claude-backup".to_string()),
            Some("test".to_string()),
        )?;
        assert_eq!(summary.provider.as_deref(), Some(PROVIDER_CLAUDE));
        let backup_path = summary.path.clone();
        let detail = open_backup(backup_path.clone())?;
        assert_eq!(detail.manifest.sessions[0].history_rows, 2);
        let backup_history = fs::read_to_string(PathBuf::from(&backup_path).join("history.jsonl"))?;
        assert!(backup_history.contains("keep one"));
        assert!(backup_history.contains("keep two"));
        assert!(!backup_history.contains("ignore"));

        let restored = restore_session(
            Some(PROVIDER_CLAUDE.to_string()),
            backup_path,
            String::new(),
            Some(restore_claude.to_string_lossy().into_owned()),
            "claude-backup-1".to_string(),
            false,
        )?;

        assert!(restored.ok);
        assert_eq!(restored.history_appended, 2);
        assert!(paths::claude_projects_dir(&restore_claude)
            .join("sample-project")
            .join("claude-backup-1.jsonl")
            .is_file());
        let restored_history = fs::read_to_string(restore_claude.join("history.jsonl"))?;
        assert!(restored_history.contains("keep one"));
        assert!(restored_history.contains("keep two"));
        assert!(!restored_history.contains("ignore"));
        fs::remove_dir_all(root).ok();
        Ok(())
    }
}
