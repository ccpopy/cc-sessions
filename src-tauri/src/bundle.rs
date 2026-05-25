//! 会话 bundle 导出/导入（面向跨机器迁移）
//!
//! 与 backup.rs 的区别：
//! - backup.rs 是"整机快照"（threads 行 + logs + rollout 集中在一个备份目录）
//! - bundle.rs 是"单会话包"（每个会话一个子目录 + manifest，便于挑选 / 跨机器）
//!
//! 目录结构：
//! ```text
//! <out_dir>/
//!   <machine>/
//!     <export_group>/
//!       <batch_timestamp>/
//!         <session_id>/
//!           codex/<rollout_relpath>         # 原样复制 rollout
//!           history.jsonl                     # 该会话的 history 行（可空）
//!           manifest.json                     # 元数据 + sha256
//! ```
//!
//! zip 打包：压缩传入的单会话 bundle 目录或批次目录（跨机器：解压后 import_bundles 即可）。

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};
use crate::family;
use crate::models::{
    BundleListItem, BundleManifest, ExportReport, ImportMode, ImportReport, ProjectPathMapping,
    ZipReport,
};
use crate::paths;
use crate::state_db;

const BUNDLE_VERSION: u32 = 1;
const PROVIDER_CODEX: &str = "codex";
const PROVIDER_CLAUDE: &str = "claude";
const PROVIDER_GEMINI: &str = "gemini";
const DEFAULT_SANDBOX_POLICY: &str = "read-only";
const DEFAULT_APPROVAL_MODE: &str = "on-request";
const DEFAULT_MEMORY_MODE: &str = "enabled";

struct RolloutSource {
    abs: PathBuf,
    rel: PathBuf,
    meta: Value,
}

fn sha256_file(path: &Path) -> AppResult<String> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

fn count_jsonl_lines(path: &Path) -> AppResult<u64> {
    if !path.is_file() {
        return Ok(0);
    }
    let f = File::open(path)?;
    let mut n = 0u64;
    for line in BufReader::new(f).lines() {
        if !line?.trim().is_empty() {
            n += 1;
        }
    }
    Ok(n)
}

fn batch_slug() -> String {
    format!("batch-{}", chrono::Utc::now().format("%Y%m%dT%H%M%S"))
}

fn index_rollouts(codex: &Path) -> AppResult<HashMap<String, RolloutSource>> {
    let mut out = HashMap::new();
    for root in [
        paths::sessions_dir(codex),
        paths::archived_sessions_dir(codex),
    ] {
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                continue;
            }
            let abs = entry.path().to_path_buf();
            if let Ok((id, source)) = rollout_source_from_path(codex, abs) {
                out.entry(id).or_insert(source);
            }
        }
    }
    Ok(out)
}

fn rollout_source_from_path(codex: &Path, abs: PathBuf) -> AppResult<(String, RolloutSource)> {
    let meta = family::read_session_meta(&abs).map_err(|e| {
        AppError::Other(format!(
            "rollout 首行不是有效 session_meta: {}: {}",
            abs.to_string_lossy(),
            e
        ))
    })?;
    let id = meta
        .get("payload")
        .and_then(|x| x.get("id"))
        .and_then(|x| x.as_str())
        .ok_or_else(|| {
            AppError::Other(format!(
                "rollout 缺少 session_meta.id: {}",
                abs.to_string_lossy()
            ))
        })?
        .to_string();
    let rel = abs
        .strip_prefix(codex)
        .map(|x| x.to_path_buf())
        .map_err(|_| {
            AppError::Path(format!(
                "rollout 不在 Codex 目录下: {}",
                abs.to_string_lossy()
            ))
        })?;
    Ok((id, RolloutSource { abs, rel, meta }))
}

fn rollout_source_from_state(
    codex: &Path,
    state: &rusqlite::Connection,
    id: &str,
) -> AppResult<Option<RolloutSource>> {
    let rollout_path: Option<String> = state
        .query_row("SELECT rollout_path FROM threads WHERE id = ?", [id], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(rollout_path) = rollout_path else {
        return Ok(None);
    };
    let abs = PathBuf::from(paths::strip_verbatim(&rollout_path));
    if !abs.is_file() {
        return Err(AppError::NotFound(format!(
            "threads.rollout_path 指向的文件不存在: {}",
            abs.to_string_lossy()
        )));
    }
    let (actual_id, source) = rollout_source_from_path(codex, abs)?;
    if actual_id != id {
        return Err(AppError::Other(format!(
            "threads.rollout_path 指向的 rollout id 不匹配: 期望 {}, 实际 {}",
            id, actual_id
        )));
    }
    Ok(Some(source))
}

// ========================= 导出 =========================

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn export_session_bundles(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    out_dir: String,
    ids: Vec<String>,
    machine_label: Option<String>,
    export_group: Option<String>,
) -> AppResult<Vec<ExportReport>> {
    if provider.as_deref().unwrap_or(PROVIDER_CODEX) == PROVIDER_GEMINI {
        return Err(AppError::Other(
            "Gemini / Antigravity 会话包含 protobuf 与跨目录附属状态，当前请使用「备份」功能导出"
                .into(),
        ));
    }
    if provider.as_deref().unwrap_or(PROVIDER_CODEX) == PROVIDER_CLAUDE {
        let claude = PathBuf::from(
            claude_dir
                .unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned()),
        );
        return export_claude_session_bundles(
            &claude,
            &PathBuf::from(out_dir),
            &ids,
            machine_label.as_deref(),
            export_group.as_deref(),
        );
    }

    let codex = PathBuf::from(&codex_dir);
    let out = PathBuf::from(&out_dir);
    let rollout_index = index_rollouts(&codex)?;
    export_session_bundles_from_index(
        &codex,
        &out,
        &ids,
        machine_label.as_deref(),
        export_group.as_deref(),
        &rollout_index,
    )
}

fn export_session_bundles_from_index(
    codex: &Path,
    out: &Path,
    ids: &[String],
    machine_label: Option<&str>,
    export_group: Option<&str>,
    rollout_index: &HashMap<String, RolloutSource>,
) -> AppResult<Vec<ExportReport>> {
    fs::create_dir_all(out)?;

    let machine = machine_label
        .map(paths::sanitize_slug)
        .unwrap_or_else(paths::machine_label);
    let group = paths::sanitize_slug(export_group.unwrap_or("default"));
    let batch = batch_slug();
    let batch_root = out.join(&machine).join(&group).join(&batch);
    fs::create_dir_all(&batch_root)?;

    // 读 state_5.sqlite 以获取 title / cwd / updated_at（没有也能导出）
    let state_conn = state_db::open_ro(codex).ok();
    // 一次扫完 history.jsonl 建索引，避免每条 id 都重扫
    let history_index = build_history_index(codex)?;

    let mut reports: Vec<ExportReport> = Vec::with_capacity(ids.len());
    for id in ids {
        let r = export_one(
            &codex,
            &batch_root,
            id,
            &machine,
            &group,
            state_conn.as_ref(),
            rollout_index,
            &history_index,
        );
        reports.push(r.unwrap_or_else(|e| ExportReport {
            session_id: id.clone(),
            ok: false,
            bundle_path: None,
            error: Some(e.to_string()),
            skipped_reason: None,
        }));
    }
    Ok(reports)
}

fn export_one(
    codex: &Path,
    batch_root: &Path,
    id: &str,
    machine: &str,
    group: &str,
    state: Option<&rusqlite::Connection>,
    rollout_index: &HashMap<String, RolloutSource>,
    history_index: &HashMap<String, Vec<String>>,
) -> AppResult<ExportReport> {
    let mut report = ExportReport {
        session_id: id.to_string(),
        ok: false,
        bundle_path: None,
        error: None,
        skipped_reason: None,
    };

    let state_source = if rollout_index.contains_key(id) {
        None
    } else if let Some(conn) = state {
        rollout_source_from_state(codex, conn, id)?
    } else {
        None
    };
    let rollout_source = match rollout_index.get(id).or(state_source.as_ref()) {
        Some(source) => source,
        None => {
            report.error = Some(format!(
                "未在 sessions/、archived_sessions/ 或 threads.rollout_path 找到 id={}",
                id
            ));
            return Ok(report);
        }
    };

    // 解析 meta
    let payload = rollout_source
        .meta
        .get("payload")
        .cloned()
        .unwrap_or(Value::Null);
    let cwd = payload
        .get("cwd")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let originator = payload
        .get("originator")
        .and_then(|x| x.as_str())
        .map(String::from);
    let session_source = payload
        .get("source")
        .and_then(|x| x.as_str())
        .map(String::from);
    let provider = payload
        .get("model_provider")
        .and_then(|x| x.as_str())
        .map(String::from);

    // 从 state 读 title / updated_at（可选）
    let (title, updated_at) = if let Some(conn) = state {
        conn.query_row(
            "SELECT COALESCE(title,''), COALESCE(updated_at,0) FROM threads WHERE id = ?",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .unwrap_or((String::new(), 0))
    } else {
        (String::new(), 0)
    };

    // 写 bundle 目录
    let bundle_dir = batch_root.join(id);
    let codex_sub = bundle_dir.join("codex").join(&rollout_source.rel);
    if let Some(p) = codex_sub.parent() {
        fs::create_dir_all(p)?;
    }
    fs::copy(&rollout_source.abs, &codex_sub)?;
    let sha = sha256_file(&codex_sub)?;
    let line_count = count_jsonl_lines(&codex_sub)?;

    // 从索引里查该会话的 history 行（O(1) 查询 + O(k) 写）
    let has_history =
        write_history_from_index(history_index, id, &bundle_dir.join("history.jsonl"))?;

    let manifest = BundleManifest {
        version: BUNDLE_VERSION,
        provider: Some(PROVIDER_CODEX.to_string()),
        session_id: id.to_string(),
        rollout_relpath: rollout_source.rel.to_string_lossy().replace('\\', "/"),
        source_relpath: None,
        sidecar_relpath: None,
        exported_at: chrono::Utc::now().to_rfc3339(),
        updated_at,
        thread_name: title,
        session_cwd: cwd,
        session_source,
        session_originator: originator,
        model_provider: provider,
        export_machine: machine.to_string(),
        export_group: group.to_string(),
        sha256_rollout: sha,
        rollout_line_count: line_count,
        has_history,
    };
    fs::write(
        bundle_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    report.ok = true;
    report.bundle_path = Some(bundle_dir.to_string_lossy().into_owned());
    Ok(report)
}

/// 一次扫完 history.jsonl，按可识别的会话 id 归档，避免批量导出时的 O(N×H) 复扫。
fn build_history_index(codex: &Path) -> AppResult<HashMap<String, Vec<String>>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let hist = paths::history_path(codex);
    if !hist.is_file() {
        return Ok(out);
    }
    let f = File::open(&hist)?;
    for line in BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(id) = crate::history::line_session_id(&line) {
            out.entry(id).or_default().push(line);
        }
    }
    Ok(out)
}

fn write_history_from_index(
    history_index: &HashMap<String, Vec<String>>,
    id: &str,
    out_path: &Path,
) -> AppResult<bool> {
    let rows = match history_index.get(id) {
        Some(r) if !r.is_empty() => r,
        _ => return Ok(false),
    };
    let mut w = BufWriter::new(File::create(out_path)?);
    for line in rows {
        writeln!(w, "{}", line)?;
    }
    w.flush()?;
    Ok(true)
}

fn export_claude_session_bundles(
    claude: &Path,
    out: &Path,
    ids: &[String],
    machine_label: Option<&str>,
    export_group: Option<&str>,
) -> AppResult<Vec<ExportReport>> {
    fs::create_dir_all(out)?;
    let machine = machine_label
        .map(paths::sanitize_slug)
        .unwrap_or_else(paths::machine_label);
    let group = paths::sanitize_slug(export_group.unwrap_or("default"));
    let batch = batch_slug();
    let batch_root = out.join(&machine).join(&group).join(&batch);
    fs::create_dir_all(&batch_root)?;

    let sessions = crate::claude_sessions::scan_sessions(claude)?;
    let by_id: HashMap<String, crate::models::SessionSummary> =
        sessions.into_iter().map(|s| (s.id.clone(), s)).collect();
    let history_ids = ids
        .iter()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let history_index =
        crate::history::collect_lines_for_ids(&paths::history_path(claude), &history_ids)?;
    let mut reports = Vec::with_capacity(ids.len());
    for id in ids {
        reports.push(
            export_one_claude(
                claude,
                &batch_root,
                id,
                &machine,
                &group,
                &by_id,
                &history_index,
            )
            .unwrap_or_else(|e| ExportReport {
                session_id: id.clone(),
                ok: false,
                bundle_path: None,
                error: Some(e.to_string()),
                skipped_reason: None,
            }),
        );
    }
    Ok(reports)
}

fn export_one_claude(
    claude: &Path,
    batch_root: &Path,
    id: &str,
    machine: &str,
    group: &str,
    sessions: &HashMap<String, crate::models::SessionSummary>,
    history_index: &HashMap<String, Vec<String>>,
) -> AppResult<ExportReport> {
    let session = sessions
        .get(id)
        .ok_or_else(|| AppError::NotFound(format!("Claude 会话不存在: {id}")))?;
    let source = PathBuf::from(&session.rollout_path);
    let source_rel = crate::claude_sessions::session_relpath(claude, &source);
    let source_rel_string = source_rel.to_string_lossy().replace('\\', "/");
    let bundle_dir = batch_root.join(id);
    let claude_sub = bundle_dir.join(PROVIDER_CLAUDE).join(&source_rel);
    if let Some(parent) = claude_sub.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&source, &claude_sub)?;
    let sha = sha256_file(&claude_sub)?;
    let line_count = count_jsonl_lines(&claude_sub)?;

    let mut sidecar_rel: Option<String> = None;
    if let Some(sidecar) = crate::claude_sessions::sidecar_path_for(&source) {
        if sidecar.exists() {
            let rel = PathBuf::from("sidecars").join(paths::sanitize_slug(id));
            copy_path_recursive(&sidecar, &bundle_dir.join(&rel))?;
            sidecar_rel = Some(rel.to_string_lossy().replace('\\', "/"));
        }
    }

    let has_history =
        write_history_from_index(history_index, id, &bundle_dir.join("history.jsonl"))?;

    let manifest = BundleManifest {
        version: BUNDLE_VERSION,
        provider: Some(PROVIDER_CLAUDE.to_string()),
        session_id: id.to_string(),
        rollout_relpath: source_rel_string.clone(),
        source_relpath: Some(source_rel_string),
        sidecar_relpath: sidecar_rel,
        exported_at: chrono::Utc::now().to_rfc3339(),
        updated_at: session.updated_at,
        thread_name: session.title.clone(),
        session_cwd: session.cwd.clone(),
        session_source: Some(PROVIDER_CLAUDE.to_string()),
        session_originator: None,
        model_provider: session.model.clone(),
        export_machine: machine.to_string(),
        export_group: group.to_string(),
        sha256_rollout: sha,
        rollout_line_count: line_count,
        has_history,
    };
    fs::write(
        bundle_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    Ok(ExportReport {
        session_id: id.to_string(),
        ok: true,
        bundle_path: Some(bundle_dir.to_string_lossy().into_owned()),
        error: None,
        skipped_reason: None,
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn export_all_bundles(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    out_dir: String,
    machine_label: Option<String>,
    export_group: Option<String>,
    active_only: bool,
) -> AppResult<Vec<ExportReport>> {
    if provider.as_deref().unwrap_or(PROVIDER_CODEX) == PROVIDER_GEMINI {
        return Err(AppError::Other(
            "Gemini / Antigravity 会话包含 protobuf 与跨目录附属状态，当前请使用「备份」功能导出"
                .into(),
        ));
    }
    if provider.as_deref().unwrap_or(PROVIDER_CODEX) == PROVIDER_CLAUDE {
        let claude = PathBuf::from(
            claude_dir
                .unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned()),
        );
        let ids = crate::claude_sessions::scan_sessions(&claude)?
            .into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>();
        return export_claude_session_bundles(
            &claude,
            &PathBuf::from(out_dir),
            &ids,
            machine_label.as_deref(),
            export_group.as_deref(),
        );
    }

    let codex = PathBuf::from(&codex_dir);
    let out = PathBuf::from(&out_dir);
    let rollout_index = index_rollouts(&codex)?;
    let mut ids: Vec<String> = Vec::new();
    if active_only {
        let store = family::load(&codex)?;
        for f in store.families.values() {
            ids.push(f.active_id.clone());
        }
        if ids.is_empty() {
            ids.extend(rollout_index.keys().cloned());
        }
    } else {
        ids.extend(rollout_index.keys().cloned());
    }
    ids.sort();
    ids.dedup();
    export_session_bundles_from_index(
        &codex,
        &out,
        &ids,
        machine_label.as_deref(),
        export_group.as_deref(),
        &rollout_index,
    )
}

// ========================= 列出 / 校验 =========================

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn list_bundles(src_dir: String, provider: Option<String>) -> AppResult<Vec<BundleListItem>> {
    let root = PathBuf::from(&src_dir);
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out: Vec<BundleListItem> = Vec::new();
    for entry in walkdir::WalkDir::new(&root)
        .max_depth(6)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() && entry.file_name() == std::ffi::OsStr::new("manifest.json")
        {
            let mp = entry.path();
            if let Ok(raw) = fs::read_to_string(mp) {
                if let Ok(m) = serde_json::from_str::<BundleManifest>(&raw) {
                    if let Some(provider) = provider.as_deref() {
                        if bundle_provider(&m) != provider {
                            continue;
                        }
                    }
                    let bdir = mp.parent().unwrap_or(Path::new(""));
                    out.push(BundleListItem {
                        bundle_dir: bdir.to_string_lossy().into_owned(),
                        manifest: m,
                        verified: None,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| b.manifest.exported_at.cmp(&a.manifest.exported_at));
    Ok(out)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn verify_bundles(src_dir: String, provider: Option<String>) -> AppResult<Vec<BundleListItem>> {
    let mut items = list_bundles(src_dir, provider)?;
    for it in items.iter_mut() {
        let rel = paths::checked_relative_path(&it.manifest.rollout_relpath)?;
        let base = if bundle_provider(&it.manifest) == PROVIDER_CLAUDE {
            PROVIDER_CLAUDE
        } else {
            PROVIDER_CODEX
        };
        let file = PathBuf::from(&it.bundle_dir).join(base).join(&rel);
        if !file.is_file() {
            it.verified = Some(false);
            continue;
        }
        let actual = sha256_file(&file).unwrap_or_default();
        it.verified = Some(actual == it.manifest.sha256_rollout);
    }
    Ok(items)
}

// ========================= 导入 =========================

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn import_session_bundles(
    provider: Option<String>,
    src_dir: String,
    codex_dir: String,
    claude_dir: Option<String>,
    mode: ImportMode,
    make_visible: bool,
    strict: bool,
    project_mappings: Vec<ProjectPathMapping>,
) -> AppResult<Vec<ImportReport>> {
    if provider.as_deref() == Some(PROVIDER_GEMINI) {
        return Err(AppError::Other(
            "Gemini / Antigravity bundle 导入暂不支持，请使用「备份」还原".into(),
        ));
    }
    let codex = PathBuf::from(&codex_dir);
    let claude = PathBuf::from(
        claude_dir.unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned()),
    );
    let project_mappings = build_project_mapping(project_mappings)?;
    let items = list_bundles(src_dir, provider.clone())?;
    let mut reports: Vec<ImportReport> = Vec::with_capacity(items.len());
    for it in items {
        let item_provider = provider
            .as_deref()
            .unwrap_or_else(|| bundle_provider(&it.manifest));
        reports.push(
            (if item_provider == PROVIDER_CLAUDE {
                import_one_claude(&claude, &it, &mode, strict, &project_mappings)
            } else {
                import_one(&codex, &it, &mode, make_visible, strict, &project_mappings)
            })
            .unwrap_or_else(|e| ImportReport {
                session_id: it.manifest.session_id.clone(),
                ok: false,
                rollout_written: false,
                history_appended: 0,
                threads_upserted: false,
                index_appended: false,
                skipped_reason: None,
                error: Some(e.to_string()),
                verified: false,
                sha_mismatch: false,
            }),
        );
    }
    Ok(reports)
}

fn build_project_mapping(items: Vec<ProjectPathMapping>) -> AppResult<HashMap<String, String>> {
    let mut out = HashMap::new();
    for item in items {
        let source = item.source_cwd.trim();
        let target = item.target_cwd.trim();
        if source.is_empty() {
            return Err(AppError::Path("项目路径映射的 source_cwd 不能为空".into()));
        }
        if target.is_empty() {
            return Err(AppError::Path(format!(
                "项目路径映射的 target_cwd 不能为空: {source}"
            )));
        }
        if let Some(existing) = out.get(source) {
            if existing != target {
                return Err(AppError::Path(format!(
                    "同一个源项目存在多个目标路径: {source}"
                )));
            }
        }
        out.insert(source.to_string(), target.to_string());
    }
    Ok(out)
}

fn mapped_project_cwd<'a>(
    manifest: &'a BundleManifest,
    mappings: &'a HashMap<String, String>,
) -> Option<&'a str> {
    let source = manifest.session_cwd.trim();
    if source.is_empty() {
        None
    } else {
        mappings.get(source).map(String::as_str)
    }
}

fn bundle_provider(manifest: &BundleManifest) -> &str {
    manifest.provider.as_deref().unwrap_or(PROVIDER_CODEX)
}

fn import_one_claude(
    claude: &Path,
    item: &BundleListItem,
    mode: &ImportMode,
    strict: bool,
    project_mappings: &HashMap<String, String>,
) -> AppResult<ImportReport> {
    let mut report = ImportReport {
        session_id: item.manifest.session_id.clone(),
        ok: false,
        rollout_written: false,
        history_appended: 0,
        threads_upserted: false,
        index_appended: false,
        skipped_reason: None,
        error: None,
        verified: false,
        sha_mismatch: false,
    };

    let rel = paths::checked_relative_path(&item.manifest.rollout_relpath)?;
    let src_file = PathBuf::from(&item.bundle_dir)
        .join(PROVIDER_CLAUDE)
        .join(&rel);
    if !src_file.is_file() {
        report.error = Some(format!(
            "bundle Claude JSONL 缺失: {}",
            src_file.to_string_lossy()
        ));
        return Ok(report);
    }

    let actual = sha256_file(&src_file)?;
    if actual != item.manifest.sha256_rollout {
        report.sha_mismatch = true;
        if strict {
            report.error = Some("sha256 不一致，strict 模式跳过".into());
            return Ok(report);
        }
    } else {
        report.verified = true;
    }

    let source_rel = item
        .manifest
        .source_relpath
        .as_deref()
        .unwrap_or(&item.manifest.rollout_relpath);
    let mapped_cwd = mapped_project_cwd(&item.manifest, project_mappings);
    let dest_abs = claude_import_dest(claude, source_rel, mapped_cwd, &item.manifest.session_id)?;
    if dest_abs.is_file() {
        match mode {
            ImportMode::Skip => {
                report.skipped_reason = Some("本地已存在，Skip 模式".into());
                report.ok = true;
                let hist_src = PathBuf::from(&item.bundle_dir).join("history.jsonl");
                report.history_appended =
                    append_history(claude, &hist_src, &item.manifest.session_id)?;
                return Ok(report);
            }
            ImportMode::KeepLocal => {
                let local_mtime = fs::metadata(&dest_abs).and_then(|m| m.modified()).ok();
                let bundle_mtime = fs::metadata(&src_file).and_then(|m| m.modified()).ok();
                if let (Some(local), Some(bundle)) = (local_mtime, bundle_mtime) {
                    if local >= bundle {
                        report.skipped_reason = Some("本地 mtime 更新，KeepLocal 模式".into());
                        report.ok = true;
                        let hist_src = PathBuf::from(&item.bundle_dir).join("history.jsonl");
                        report.history_appended =
                            append_history(claude, &hist_src, &item.manifest.session_id)?;
                        return Ok(report);
                    }
                }
            }
            ImportMode::Overwrite => {}
        }
    }

    if let Some(parent) = dest_abs.parent() {
        fs::create_dir_all(parent)?;
    }
    copy_claude_jsonl_with_cwd(&src_file, &dest_abs, mapped_cwd)?;
    report.rollout_written = true;

    if let Some(sidecar_rel) = item.manifest.sidecar_relpath.as_deref() {
        let sidecar_src =
            PathBuf::from(&item.bundle_dir).join(paths::checked_relative_path(sidecar_rel)?);
        if sidecar_src.exists() {
            if let Some(sidecar_dest) = crate::claude_sessions::sidecar_path_for(&dest_abs) {
                if sidecar_dest.exists() && matches!(mode, ImportMode::Overwrite) {
                    remove_path_recursive(&sidecar_dest)?;
                }
                if !sidecar_dest.exists() {
                    copy_path_recursive(&sidecar_src, &sidecar_dest)?;
                }
            }
        }
    }
    let hist_src = PathBuf::from(&item.bundle_dir).join("history.jsonl");
    report.history_appended = append_history(claude, &hist_src, &item.manifest.session_id)?;

    report.ok = true;
    Ok(report)
}

fn claude_import_dest(
    claude: &Path,
    source_rel: &str,
    mapped_cwd: Option<&str>,
    session_id: &str,
) -> AppResult<PathBuf> {
    let rel = paths::checked_relative_path(source_rel)?;
    let Some(mapped_cwd) = mapped_cwd else {
        return Ok(paths::claude_projects_dir(claude).join(rel));
    };
    let file_name = rel
        .file_name()
        .map(|name| name.to_owned())
        .unwrap_or_else(|| std::ffi::OsString::from(format!("{session_id}.jsonl")));
    let project_dir = find_claude_project_dir_for_cwd(claude, mapped_cwd)?.unwrap_or_else(|| {
        paths::claude_projects_dir(claude).join(paths::sanitize_slug(mapped_cwd))
    });
    Ok(project_dir.join(file_name))
}

fn find_claude_project_dir_for_cwd(claude: &Path, target_cwd: &str) -> AppResult<Option<PathBuf>> {
    let projects = paths::claude_projects_dir(claude);
    if !projects.is_dir() {
        return Ok(None);
    }
    for entry in walkdir::WalkDir::new(&projects)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let file = File::open(entry.path())?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line)?;
            if value.get("cwd").and_then(Value::as_str) == Some(target_cwd) {
                return Ok(entry.path().parent().map(Path::to_path_buf));
            }
            break;
        }
    }
    Ok(None)
}

fn import_one(
    codex: &Path,
    item: &BundleListItem,
    mode: &ImportMode,
    make_visible: bool,
    strict: bool,
    project_mappings: &HashMap<String, String>,
) -> AppResult<ImportReport> {
    let mut report = ImportReport {
        session_id: item.manifest.session_id.clone(),
        ok: false,
        rollout_written: false,
        history_appended: 0,
        threads_upserted: false,
        index_appended: false,
        skipped_reason: None,
        error: None,
        verified: false,
        sha_mismatch: false,
    };

    // 1) 找源文件
    let rel = paths::checked_relative_path(&item.manifest.rollout_relpath)?;
    let src_file = PathBuf::from(&item.bundle_dir).join("codex").join(&rel);
    if !src_file.is_file() {
        report.error = Some(format!(
            "bundle rollout 缺失: {}",
            src_file.to_string_lossy()
        ));
        return Ok(report);
    }

    // 2) 校验
    let actual = sha256_file(&src_file)?;
    if actual != item.manifest.sha256_rollout {
        report.sha_mismatch = true;
        if strict {
            report.error = Some("sha256 不一致，strict 模式跳过".into());
            return Ok(report);
        }
    } else {
        report.verified = true;
    }

    // 3) 目标路径决策
    let dest_abs = codex.join(&rel);
    let mapped_cwd = mapped_project_cwd(&item.manifest, project_mappings);
    if dest_abs.is_file() {
        match mode {
            ImportMode::Skip => {
                report.skipped_reason = Some("本地已存在，Skip 模式".into());
                report.ok = true;
                let hist_src = PathBuf::from(&item.bundle_dir).join("history.jsonl");
                report.history_appended =
                    append_history(codex, &hist_src, &item.manifest.session_id)?;
                return Ok(report);
            }
            ImportMode::KeepLocal => {
                let local_mtime = fs::metadata(&dest_abs).and_then(|m| m.modified()).ok();
                let bundle_mtime = fs::metadata(&src_file).and_then(|m| m.modified()).ok();
                if let (Some(l), Some(b)) = (local_mtime, bundle_mtime) {
                    if l >= b {
                        report.skipped_reason = Some("本地 mtime 更新，KeepLocal 模式".into());
                        report.ok = true;
                        // 仍然尝试补 history
                        let hist_src = PathBuf::from(&item.bundle_dir).join("history.jsonl");
                        if hist_src.is_file() {
                            report.history_appended =
                                append_history(codex, &hist_src, &item.manifest.session_id)?;
                        }
                        return Ok(report);
                    }
                }
            }
            ImportMode::Overwrite => {}
        }
    }

    // 4) 拷 rollout
    if let Some(parent) = dest_abs.parent() {
        fs::create_dir_all(parent)?;
    }
    copy_codex_rollout_with_cwd(&src_file, &dest_abs, mapped_cwd)?;
    report.rollout_written = true;

    // 5) 追加 history
    let hist_src = PathBuf::from(&item.bundle_dir).join("history.jsonl");
    if hist_src.is_file() {
        report.history_appended = append_history(codex, &hist_src, &item.manifest.session_id)?;
    }

    // 6) 若需 make_visible，则 upsert threads + 追加 session_index
    if make_visible {
        if paths::state_db_path(codex).is_file() {
            let import_cwd = mapped_cwd.unwrap_or(item.manifest.session_cwd.as_str());
            if let Err(e) = upsert_threads_minimal(codex, &item.manifest, &dest_abs, import_cwd) {
                report.error = Some(format!("threads upsert 失败: {}", e));
            } else {
                report.threads_upserted = true;
            }
        }
        let line = serde_json::json!({
            "id": item.manifest.session_id,
            "thread_name": item.manifest.thread_name,
            "updated_at": unix_seconds_to_rfc3339(item.manifest.updated_at)?,
        });
        let idx = paths::session_index_path(codex);
        let mut exist = false;
        if idx.is_file() {
            for l in BufReader::new(File::open(&idx)?).lines() {
                let l = l?;
                if let Ok(v) = serde_json::from_str::<Value>(&l) {
                    if v.get("id").and_then(|x| x.as_str()) == Some(&item.manifest.session_id) {
                        exist = true;
                        break;
                    }
                }
            }
        }
        if !exist {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&idx)?;
            writeln!(f, "{}", line)?;
            report.index_appended = true;
        }
    }

    report.ok = true;
    Ok(report)
}

fn append_history(codex: &Path, src: &Path, id: &str) -> AppResult<u32> {
    crate::history::append_from_file(&paths::history_path(codex), src, id)
}

fn copy_codex_rollout_with_cwd(src: &Path, dest: &Path, target_cwd: Option<&str>) -> AppResult<()> {
    let Some(target_cwd) = target_cwd else {
        fs::copy(src, dest)?;
        return Ok(());
    };

    let tmp = dest.with_extension("jsonl.tmp");
    let mut reader = BufReader::new(File::open(src)?);
    let mut writer = BufWriter::new(File::create(&tmp)?);
    let mut line = String::new();
    let mut rewrote_meta = false;
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if !rewrote_meta && !trimmed.trim().is_empty() {
            let mut value: Value = serde_json::from_str(trimmed).map_err(|e| {
                AppError::Other(format!(
                    "无法重写 Codex 项目路径，rollout 首个事件不是有效 JSON: {}: {}",
                    src.to_string_lossy(),
                    e
                ))
            })?;
            if value.get("type").and_then(Value::as_str) != Some("session_meta") {
                return Err(AppError::Other(format!(
                    "无法重写 Codex 项目路径，rollout 首个事件不是 session_meta: {}",
                    src.to_string_lossy()
                )));
            }
            let payload = value
                .get_mut("payload")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| {
                    AppError::Other(format!(
                        "无法重写 Codex 项目路径，session_meta.payload 不是对象: {}",
                        src.to_string_lossy()
                    ))
                })?;
            payload.insert("cwd".into(), Value::String(target_cwd.to_string()));
            writeln!(writer, "{}", serde_json::to_string(&value)?)?;
            rewrote_meta = true;
        } else {
            writer.write_all(line.as_bytes())?;
        }
    }
    if !rewrote_meta {
        return Err(AppError::Other(format!(
            "无法重写 Codex 项目路径，rollout 没有有效 session_meta: {}",
            src.to_string_lossy()
        )));
    }
    writer.flush()?;
    if dest.exists() {
        fs::remove_file(dest)?;
    }
    fs::rename(tmp, dest)?;
    Ok(())
}

fn copy_claude_jsonl_with_cwd(src: &Path, dest: &Path, target_cwd: Option<&str>) -> AppResult<()> {
    let Some(target_cwd) = target_cwd else {
        fs::copy(src, dest)?;
        return Ok(());
    };

    let tmp = dest.with_extension("jsonl.tmp");
    let reader = BufReader::new(File::open(src)?);
    let mut writer = BufWriter::new(File::create(&tmp)?);
    for (line_no, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            writeln!(writer)?;
            continue;
        }
        let mut value: Value = serde_json::from_str(&line).map_err(|e| {
            AppError::Other(format!(
                "无法重写 Claude 项目路径，第 {} 行不是有效 JSON: {}: {}",
                line_no + 1,
                src.to_string_lossy(),
                e
            ))
        })?;
        let obj = value.as_object_mut().ok_or_else(|| {
            AppError::Other(format!(
                "无法重写 Claude 项目路径，第 {} 行不是 JSON 对象: {}",
                line_no + 1,
                src.to_string_lossy()
            ))
        })?;
        obj.insert("cwd".into(), Value::String(target_cwd.to_string()));
        writeln!(writer, "{}", serde_json::to_string(&value)?)?;
    }
    writer.flush()?;
    if dest.exists() {
        fs::remove_file(dest)?;
    }
    fs::rename(tmp, dest)?;
    Ok(())
}

fn upsert_threads_minimal(
    codex: &Path,
    m: &BundleManifest,
    dest_abs: &Path,
    import_cwd: &str,
) -> AppResult<()> {
    let conn = state_db::open(codex)?;
    let updated_at = m.updated_at;
    let source = m
        .session_source
        .as_deref()
        .filter(|source| crate::repair::is_desktop_visible_source(Some(source)))
        .unwrap_or("cli")
        .to_string();
    let sql = "INSERT INTO threads (
            id, rollout_path, created_at, updated_at, source, model_provider, cwd, title,
            sandbox_policy, approval_mode, memory_mode, archived, tokens_used, has_user_event,
            first_user_message, cli_version
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0, 1, ?, '')
        ON CONFLICT(id) DO UPDATE SET
            rollout_path=excluded.rollout_path,
            updated_at=excluded.updated_at,
            model_provider=excluded.model_provider,
            title=excluded.title,
            first_user_message=excluded.first_user_message";
    conn.execute(
        sql,
        params![
            m.session_id,
            dest_abs.to_string_lossy(),
            updated_at,
            updated_at,
            source,
            m.model_provider.clone().unwrap_or_else(|| "openai".into()),
            import_cwd,
            m.thread_name,
            DEFAULT_SANDBOX_POLICY,
            DEFAULT_APPROVAL_MODE,
            DEFAULT_MEMORY_MODE,
            m.thread_name,
        ],
    )?;
    Ok(())
}

fn unix_seconds_to_rfc3339(ts: i64) -> AppResult<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.to_rfc3339())
        .ok_or_else(|| AppError::Other(format!("manifest updated_at 不是有效 Unix 秒时间戳: {ts}")))
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
            "type": "assistant",
            "message": {
                "role": "assistant",
                "model": "claude-3-5-sonnet",
                "usage": {"input_tokens": 2, "output_tokens": 3},
                "content": "hello"
            }
        });
        fs::write(
            dir.join(format!("{id}.jsonl")),
            format!("{}\n", serde_json::to_string(&line)?),
        )?;
        fs::write(
            claude.join("history.jsonl"),
            format!(
                "{{\"sessionId\":\"{id}\",\"display\":\"bundle one\"}}\n\
                 {{\"session_id\":\"other-session\",\"display\":\"ignore\"}}\n\
                 {{\"id\":\"{id}\",\"display\":\"bundle two\"}}\n"
            ),
        )?;
        Ok(())
    }

    fn write_codex_session(codex: &Path, id: &str, updated_at: i64) -> AppResult<PathBuf> {
        let rollout_dir = codex.join("sessions").join("2026").join("05").join("12");
        fs::create_dir_all(&rollout_dir)?;
        let path = rollout_dir.join(format!("rollout-{id}.jsonl"));
        let meta = serde_json::json!({
            "timestamp": "2026-05-12T10:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "cwd": "F:\\project\\portable-context",
                "source": "cli",
                "model_provider": "openai"
            }
        });
        let event = serde_json::json!({
            "timestamp": "2026-05-12T10:00:30Z",
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "portable context"}
        });
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&meta)?,
                serde_json::to_string(&event)?
            ),
        )?;

        let conn = create_bundle_state(codex)?;
        conn.execute(
            "INSERT INTO threads (
                id, rollout_path, created_at, updated_at, source, model_provider, cwd, title,
                sandbox_policy, approval_mode, memory_mode, archived, tokens_used, has_user_event,
                first_user_message, cli_version
            ) VALUES (?1, ?2, ?3, ?4, 'cli', 'openai', 'F:\\project\\portable-context',
                'Portable context', 'read-only', 'on-request', 'enabled', 0, 0, 1,
                'Portable context', '')",
            params![id, path.to_string_lossy(), updated_at, updated_at],
        )?;
        Ok(path)
    }

    fn create_bundle_state(codex: &Path) -> AppResult<rusqlite::Connection> {
        fs::create_dir_all(codex)?;
        let conn = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        conn.execute(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT,
                created_at INTEGER,
                updated_at INTEGER,
                source TEXT,
                model_provider TEXT,
                cwd TEXT,
                title TEXT,
                sandbox_policy TEXT,
                approval_mode TEXT,
                memory_mode TEXT,
                archived INTEGER,
                tokens_used INTEGER,
                has_user_event INTEGER,
                first_user_message TEXT,
                cli_version TEXT
            )",
            [],
        )?;
        Ok(conn)
    }

    #[test]
    fn exports_verifies_and_imports_claude_bundle() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-claude-bundle-test");
        let source_claude = root.join("source-claude");
        let import_claude = root.join("import-claude");
        let bundle_dir = root.join("bundles");
        write_claude_session(&source_claude, "claude-bundle-1")?;

        let reports = export_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            String::new(),
            Some(source_claude.to_string_lossy().into_owned()),
            bundle_dir.to_string_lossy().into_owned(),
            vec!["claude-bundle-1".to_string()],
            Some("test-machine".to_string()),
            Some("default".to_string()),
        )?;
        assert_eq!(reports.len(), 1);
        assert!(reports[0].ok);
        let bundle_path = PathBuf::from(reports[0].bundle_path.as_deref().unwrap());
        let exported_history = fs::read_to_string(bundle_path.join("history.jsonl"))?;
        assert!(exported_history.contains("bundle one"));
        assert!(exported_history.contains("bundle two"));
        assert!(!exported_history.contains("ignore"));

        let verified = verify_bundles(
            bundle_dir.to_string_lossy().into_owned(),
            Some(PROVIDER_CLAUDE.to_string()),
        )?;
        assert_eq!(verified.len(), 1);
        assert_eq!(verified[0].verified, Some(true));
        assert!(verified[0].manifest.has_history);

        let imported = import_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            bundle_dir.to_string_lossy().into_owned(),
            String::new(),
            Some(import_claude.to_string_lossy().into_owned()),
            ImportMode::Skip,
            false,
            true,
            vec![ProjectPathMapping {
                source_cwd: r"F:\work\sample-project".to_string(),
                target_cwd: r"D:\work\sample-project".to_string(),
            }],
        )?;
        assert_eq!(imported.len(), 1);
        assert!(imported[0].ok);
        assert_eq!(imported[0].history_appended, 2);
        let imported_claude_path = paths::claude_projects_dir(&import_claude)
            .join(paths::sanitize_slug(r"D:\work\sample-project"))
            .join("claude-bundle-1.jsonl");
        assert!(imported_claude_path.is_file());
        let imported_jsonl = fs::read_to_string(&imported_claude_path)?;
        let imported_event: Value = serde_json::from_str(imported_jsonl.lines().next().unwrap())?;
        assert_eq!(
            imported_event.get("cwd").and_then(Value::as_str),
            Some(r"D:\work\sample-project")
        );
        let imported_history = fs::read_to_string(import_claude.join("history.jsonl"))?;
        assert!(imported_history.contains("bundle one"));
        assert!(imported_history.contains("bundle two"));
        assert!(!imported_history.contains("ignore"));

        fs::remove_file(import_claude.join("history.jsonl"))?;
        let skipped = import_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            bundle_dir.to_string_lossy().into_owned(),
            String::new(),
            Some(import_claude.to_string_lossy().into_owned()),
            ImportMode::Skip,
            false,
            true,
            vec![ProjectPathMapping {
                source_cwd: r"F:\work\sample-project".to_string(),
                target_cwd: r"D:\work\sample-project".to_string(),
            }],
        )?;
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].ok);
        assert!(!skipped[0].rollout_written);
        assert_eq!(skipped[0].history_appended, 2);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn imports_codex_bundle_with_seconds_timestamp() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-codex-bundle-time-test");
        let source_codex = root.join("source-codex");
        let import_codex = root.join("import-codex");
        let bundle_dir = root.join("bundles");
        let id = "codex-bundle-time";
        let updated_at = 1_777_777_777;
        write_codex_session(&source_codex, id, updated_at)?;
        create_bundle_state(&import_codex)?;

        let reports = export_session_bundles(
            Some(PROVIDER_CODEX.to_string()),
            source_codex.to_string_lossy().into_owned(),
            None,
            bundle_dir.to_string_lossy().into_owned(),
            vec![id.to_string()],
            Some("test-machine".to_string()),
            Some("default".to_string()),
        )?;
        assert_eq!(reports.len(), 1);
        assert!(reports[0].ok);

        let imported = import_session_bundles(
            Some(PROVIDER_CODEX.to_string()),
            bundle_dir.to_string_lossy().into_owned(),
            import_codex.to_string_lossy().into_owned(),
            None,
            ImportMode::Overwrite,
            true,
            true,
            vec![ProjectPathMapping {
                source_cwd: r"F:\project\portable-context".to_string(),
                target_cwd: r"D:\work\portable-context".to_string(),
            }],
        )?;
        assert_eq!(imported.len(), 1);
        assert!(imported[0].ok);
        assert!(imported[0].threads_upserted);

        let conn = rusqlite::Connection::open(import_codex.join("state_5.sqlite"))?;
        let actual_updated_at: i64 =
            conn.query_row("SELECT updated_at FROM threads WHERE id = ?1", [id], |r| {
                r.get(0)
            })?;
        assert_eq!(actual_updated_at, updated_at);
        let actual_cwd: String =
            conn.query_row("SELECT cwd FROM threads WHERE id = ?1", [id], |r| r.get(0))?;
        assert_eq!(actual_cwd, r"D:\work\portable-context");

        let imported_rollout = import_codex
            .join("sessions")
            .join("2026")
            .join("05")
            .join("12")
            .join(format!("rollout-{id}.jsonl"));
        let first_line = fs::read_to_string(imported_rollout)?
            .lines()
            .next()
            .unwrap()
            .to_string();
        let meta: Value = serde_json::from_str(&first_line)?;
        assert_eq!(
            meta.get("payload")
                .and_then(|payload| payload.get("cwd"))
                .and_then(Value::as_str),
            Some(r"D:\work\portable-context")
        );

        let index_raw = fs::read_to_string(paths::session_index_path(&import_codex))?;
        let index_line: Value = serde_json::from_str(index_raw.lines().next().unwrap())?;
        assert_eq!(index_line.get("id").and_then(|v| v.as_str()), Some(id));
        assert_eq!(
            index_line.get("updated_at").and_then(|v| v.as_str()),
            Some(unix_seconds_to_rfc3339(updated_at)?.as_str())
        );
        assert!(index_line.get("rollout_path").is_none());

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn packs_only_the_requested_bundle_source() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-bundle-zip-source-test");
        let export_root = root.join("export");
        let bundle = export_root
            .join("test-machine")
            .join("default")
            .join("batch-20260512T100000")
            .join("session-1");
        fs::create_dir_all(&bundle)?;
        fs::write(bundle.join("manifest.json"), "{}")?;
        fs::write(bundle.join("history.jsonl"), "history\n")?;
        fs::write(export_root.join("unrelated.txt"), "must not be zipped")?;

        let zip_path = export_root.join("session-1.zip");
        let report = pack_bundles_zip(
            bundle.to_string_lossy().into_owned(),
            zip_path.to_string_lossy().into_owned(),
        )?;
        assert_eq!(report.files, 2);

        let unpacked = root.join("unpacked");
        unpack_zip(
            zip_path.to_string_lossy().into_owned(),
            unpacked.to_string_lossy().into_owned(),
        )?;
        assert!(unpacked.join("manifest.json").is_file());
        assert!(unpacked.join("history.jsonl").is_file());
        assert!(!unpacked.join("unrelated.txt").exists());

        let temp_report = unpack_zip_to_temp(zip_path.to_string_lossy().into_owned())?;
        let temp_unpacked = PathBuf::from(&temp_report.path);
        assert!(temp_unpacked.join("manifest.json").is_file());
        assert!(temp_unpacked.join("history.jsonl").is_file());
        assert!(!temp_unpacked.join("unrelated.txt").exists());
        fs::remove_dir_all(temp_unpacked).ok();

        fs::remove_dir_all(root).ok();
        Ok(())
    }
}

// ========================= zip 打包 =========================

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn pack_bundles_zip(src_dir: String, zip_path: String) -> AppResult<ZipReport> {
    let src = PathBuf::from(&src_dir);
    let out = PathBuf::from(&zip_path);
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    let src_canon = fs::canonicalize(&src)?;
    if let Some(parent) = out.parent() {
        let out_parent_canon = fs::canonicalize(parent)?;
        if out_parent_canon == src_canon || out_parent_canon.starts_with(&src_canon) {
            return Err(AppError::Path(format!(
                "zip 输出路径不能位于被打包目录内部: 输出 {}, 源目录 {}",
                out.to_string_lossy(),
                src.to_string_lossy()
            )));
        }
    }
    let file = File::create(&out)?;
    let mut writer = BufWriter::new(file);

    // 手工实现 STORE 模式的 zip：跨机器只要能解包即可。CRC-32 + 无压缩。
    let mut central: Vec<CentralEntry> = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut file_count: u32 = 0;
    let mut offset: u32 = 0;

    for entry in walkdir::WalkDir::new(&src)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(&src)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| entry.path().to_path_buf());
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let mut data: Vec<u8> = Vec::new();
        File::open(entry.path())?.read_to_end(&mut data)?;
        let crc = crc32(&data);
        let size = data.len() as u32;
        total_bytes += size as u64;
        file_count += 1;
        // Local file header
        writer.write_all(&0x04034b50u32.to_le_bytes())?; // signature
        writer.write_all(&20u16.to_le_bytes())?; // version needed
        writer.write_all(&0u16.to_le_bytes())?; // flags
        writer.write_all(&0u16.to_le_bytes())?; // method STORE
        writer.write_all(&0u16.to_le_bytes())?; // mod time
        writer.write_all(&0u16.to_le_bytes())?; // mod date
        writer.write_all(&crc.to_le_bytes())?;
        writer.write_all(&size.to_le_bytes())?; // compressed size
        writer.write_all(&size.to_le_bytes())?; // uncompressed size
        writer.write_all(&(rel_str.len() as u16).to_le_bytes())?;
        writer.write_all(&0u16.to_le_bytes())?; // extra len
        writer.write_all(rel_str.as_bytes())?;
        writer.write_all(&data)?;

        let local_header_size = 30 + rel_str.len() as u32;
        central.push(CentralEntry {
            name: rel_str,
            crc,
            size,
            offset,
        });
        offset += local_header_size + size;
    }
    writer.flush()?;

    // Central directory
    let cd_offset = offset;
    let mut cd_size: u32 = 0;
    for e in &central {
        writer.write_all(&0x02014b50u32.to_le_bytes())?; // signature
        writer.write_all(&20u16.to_le_bytes())?; // version made by
        writer.write_all(&20u16.to_le_bytes())?; // version needed
        writer.write_all(&0u16.to_le_bytes())?; // flags
        writer.write_all(&0u16.to_le_bytes())?; // method
        writer.write_all(&0u16.to_le_bytes())?; // mod time
        writer.write_all(&0u16.to_le_bytes())?; // mod date
        writer.write_all(&e.crc.to_le_bytes())?;
        writer.write_all(&e.size.to_le_bytes())?;
        writer.write_all(&e.size.to_le_bytes())?;
        writer.write_all(&(e.name.len() as u16).to_le_bytes())?;
        writer.write_all(&0u16.to_le_bytes())?; // extra len
        writer.write_all(&0u16.to_le_bytes())?; // comment len
        writer.write_all(&0u16.to_le_bytes())?; // disk start
        writer.write_all(&0u16.to_le_bytes())?; // int attrs
        writer.write_all(&0u32.to_le_bytes())?; // ext attrs
        writer.write_all(&e.offset.to_le_bytes())?;
        writer.write_all(e.name.as_bytes())?;
        cd_size += 46 + e.name.len() as u32;
    }

    // EOCD
    writer.write_all(&0x06054b50u32.to_le_bytes())?;
    writer.write_all(&0u16.to_le_bytes())?; // disk
    writer.write_all(&0u16.to_le_bytes())?; // cd start disk
    writer.write_all(&(central.len() as u16).to_le_bytes())?;
    writer.write_all(&(central.len() as u16).to_le_bytes())?;
    writer.write_all(&cd_size.to_le_bytes())?;
    writer.write_all(&cd_offset.to_le_bytes())?;
    writer.write_all(&0u16.to_le_bytes())?; // comment len
    writer.flush()?;

    Ok(ZipReport {
        path: out.to_string_lossy().into_owned(),
        files: file_count,
        bytes: total_bytes,
    })
}

struct CentralEntry {
    name: String,
    crc: u32,
    size: u32,
    offset: u32,
}

fn crc32(data: &[u8]) -> u32 {
    // 标准 CRC-32（IEEE 802.3），查表法
    static mut TABLE: [u32; 256] = [0; 256];
    static INIT: std::sync::Once = std::sync::Once::new();
    unsafe {
        INIT.call_once(|| {
            for i in 0..256u32 {
                let mut c = i;
                for _ in 0..8 {
                    c = if c & 1 == 1 {
                        0xEDB88320 ^ (c >> 1)
                    } else {
                        c >> 1
                    };
                }
                TABLE[i as usize] = c;
            }
        });
        let mut crc: u32 = 0xFFFFFFFF;
        for &b in data {
            let idx = ((crc ^ b as u32) & 0xFF) as usize;
            crc = TABLE[idx] ^ (crc >> 8);
        }
        crc ^ 0xFFFFFFFF
    }
}

fn checked_zip_slice<'a>(
    data: &'a [u8],
    start: usize,
    len: usize,
    label: &str,
) -> AppResult<&'a [u8]> {
    let end = start
        .checked_add(len)
        .ok_or_else(|| AppError::Other(format!("{label} 偏移溢出")))?;
    data.get(start..end)
        .ok_or_else(|| AppError::Other(format!("{label} 超出 zip 文件边界")))
}

fn read_zip_u16(data: &[u8], start: usize, label: &str) -> AppResult<u16> {
    let bytes = checked_zip_slice(data, start, 2, label)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_zip_u32(data: &[u8], start: usize, label: &str) -> AppResult<u32> {
    let bytes = checked_zip_slice(data, start, 4, label)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn unpack_zip(zip_path: String, dst_dir: String) -> AppResult<ZipReport> {
    // 流式解压：只支持 STORE 模式。只把尾部 + central directory 读进内存，
    // 每个文件的 payload 用 seek + Read::take + io::copy，不再一次性装全文件。
    let dst = PathBuf::from(&dst_dir);
    fs::create_dir_all(&dst)?;

    let mut f = File::open(&zip_path)?;
    let file_len = f.metadata()?.len();
    if file_len < 22 {
        return Err(AppError::Other("不是合法的 zip 文件（过短）".into()));
    }

    // EOCD 最多携带 65535 字节注释，尾部窗口够 65557 字节即可覆盖。
    let tail_window = file_len.min(65557);
    let tail_base = file_len - tail_window;
    f.seek(SeekFrom::Start(tail_base))?;
    let mut tail = vec![0u8; tail_window as usize];
    f.read_exact(&mut tail)?;

    // 在尾部 buffer 中倒序找 EOCD 签名。
    let sig = [0x50u8, 0x4b, 0x05, 0x06];
    let eocd_in_tail = (0..=tail.len().saturating_sub(22))
        .rev()
        .find(|&i| tail[i..i + 4] == sig)
        .ok_or_else(|| AppError::Other("不是合法的 zip 文件（未找到 EOCD）".into()))?;

    let cd_count = read_zip_u16(&tail, eocd_in_tail + 10, "central directory 数量")? as usize;
    let cd_size = read_zip_u32(&tail, eocd_in_tail + 12, "central directory 总大小")? as u64;
    let cd_offset = read_zip_u32(&tail, eocd_in_tail + 16, "central directory 偏移")? as u64;
    if cd_offset
        .checked_add(cd_size)
        .map(|v| v > file_len)
        .unwrap_or(true)
    {
        return Err(AppError::Other("central directory 范围越界".into()));
    }

    // 一次性读 central directory（通常远小于 payload 总和）。
    f.seek(SeekFrom::Start(cd_offset))?;
    let mut cd = vec![0u8; cd_size as usize];
    f.read_exact(&mut cd)?;

    let mut pos: usize = 0;
    let mut total_bytes: u64 = 0;
    let mut file_count: u32 = 0;
    for _ in 0..cd_count {
        if checked_zip_slice(&cd, pos, 4, "central directory 签名")? != [0x50, 0x4b, 0x01, 0x02] {
            return Err(AppError::Other("central directory 损坏".into()));
        }
        checked_zip_slice(&cd, pos, 46, "central directory header")?;
        let method = read_zip_u16(&cd, pos + 10, "central directory 压缩方式")?;
        if method != 0 {
            return Err(AppError::Other(format!(
                "不支持的压缩方式 method={} (仅支持 STORE)",
                method
            )));
        }
        let csize = read_zip_u32(&cd, pos + 20, "central directory 压缩后大小")? as u64;
        let name_len = read_zip_u16(&cd, pos + 28, "central directory 文件名长度")? as usize;
        let extra_len = read_zip_u16(&cd, pos + 30, "central directory extra 长度")? as usize;
        let cmt_len = read_zip_u16(&cd, pos + 32, "central directory 注释长度")? as usize;
        let local_off = read_zip_u32(&cd, pos + 42, "local header 偏移")? as u64;
        let name_bytes = checked_zip_slice(&cd, pos + 46, name_len, "central directory 文件名")?;
        let name = String::from_utf8(name_bytes.to_vec())
            .map_err(|e| AppError::Other(format!("zip 文件名不是 UTF-8: {}", e)))?;
        let advance = 46usize
            .checked_add(name_len)
            .and_then(|v| v.checked_add(extra_len))
            .and_then(|v| v.checked_add(cmt_len))
            .ok_or_else(|| AppError::Other("central directory entry 长度溢出".into()))?;
        pos = pos
            .checked_add(advance)
            .ok_or_else(|| AppError::Other("central directory 游标溢出".into()))?;

        // 读 local header（30 字节固定区）
        if local_off
            .checked_add(30)
            .map(|v| v > file_len)
            .unwrap_or(true)
        {
            return Err(AppError::Other(format!("local header 越界: {}", name)));
        }
        f.seek(SeekFrom::Start(local_off))?;
        let mut lh = [0u8; 30];
        f.read_exact(&mut lh)?;
        if lh[..4] != [0x50, 0x4b, 0x03, 0x04] {
            return Err(AppError::Other(format!("local header 损坏: {}", name)));
        }
        let local_method = read_zip_u16(&lh, 8, "local header 压缩方式")?;
        if local_method != 0 {
            return Err(AppError::Other(format!(
                "local header 压缩方式不支持 method={} (仅支持 STORE): {}",
                local_method, name
            )));
        }
        let l_name_len = read_zip_u16(&lh, 26, "local header 文件名长度")? as u64;
        let l_extra_len = read_zip_u16(&lh, 28, "local header extra 长度")? as u64;
        let payload_start = local_off
            .checked_add(30)
            .and_then(|v| v.checked_add(l_name_len))
            .and_then(|v| v.checked_add(l_extra_len))
            .ok_or_else(|| AppError::Other(format!("payload 偏移溢出: {}", name)))?;
        if payload_start
            .checked_add(csize)
            .map(|v| v > file_len)
            .unwrap_or(true)
        {
            return Err(AppError::Other(format!("payload 范围越界: {}", name)));
        }

        let rel = paths::checked_relative_path(&name)?;
        let out_path = dst.join(&rel);
        if name.ends_with('/') {
            fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // 流式 copy payload：不再把文件内容一次性读进内存
        f.seek(SeekFrom::Start(payload_start))?;
        let mut out_file = File::create(&out_path)?;
        let copied = std::io::copy(&mut (&mut f).take(csize), &mut out_file)?;
        if copied != csize {
            return Err(AppError::Other(format!(
                "payload 实际读取字节 {} ≠ 声明 {}: {}",
                copied, csize, name
            )));
        }
        out_file.flush().ok();

        total_bytes += csize;
        file_count += 1;
    }

    Ok(ZipReport {
        path: dst.to_string_lossy().into_owned(),
        files: file_count,
        bytes: total_bytes,
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn unpack_zip_to_temp(zip_path: String) -> AppResult<ZipReport> {
    let dir = std::env::temp_dir().join(format!(
        "cc-session-manager-import-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    unpack_zip(zip_path, dir.to_string_lossy().into_owned())
}
