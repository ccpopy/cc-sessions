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

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::atomic_file;
use crate::error::{AppError, AppResult};
use crate::family;
use crate::models::{
    BundleExportTarget, BundleListItem, BundleManifest, ExportReport, ImportMode, ImportReport,
    ManifestArtifact, ProjectPathMapping, SessionSummary, ZipReport,
};
use crate::paths;
use crate::state_db;

const BUNDLE_VERSION: u32 = 2;
const PROVIDER_CODEX: &str = "codex";
const PROVIDER_CLAUDE: &str = "claude";
const DEFAULT_SANDBOX_POLICY: &str = "read-only";
const DEFAULT_APPROVAL_MODE: &str = "on-request";
const DEFAULT_MEMORY_MODE: &str = "enabled";

static CLAUDE_SNAPSHOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static BUNDLE_EXPORT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static ZIP_STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

fn collect_bundle_artifacts(
    bundle_root: &Path,
    has_history: bool,
    sidecar_relpath: Option<&str>,
) -> AppResult<Vec<ManifestArtifact>> {
    let mut files = Vec::new();
    if has_history {
        files.push(bundle_root.join("history.jsonl"));
    }
    if let Some(sidecar_relpath) = sidecar_relpath {
        let relative = paths::checked_relative_path(sidecar_relpath)?;
        let sidecar = bundle_root.join(relative);
        crate::path_safety::validate_tree(bundle_root, &sidecar, "Bundle sidecar")?;
        let metadata = fs::symlink_metadata(&sidecar)?;
        if metadata.is_file() {
            files.push(sidecar);
        } else {
            for entry in walkdir::WalkDir::new(&sidecar).follow_links(false) {
                let entry = entry.map_err(|error| {
                    AppError::Other(format!(
                        "遍历 Bundle sidecar 失败 {}: {error}",
                        sidecar.to_string_lossy()
                    ))
                })?;
                let metadata = fs::symlink_metadata(entry.path())?;
                if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
                    return Err(AppError::Path(format!(
                        "Bundle sidecar 包含链接或 junction: {}",
                        entry.path().to_string_lossy()
                    )));
                }
                if metadata.is_file() {
                    files.push(entry.path().to_path_buf());
                }
            }
        }
    }

    let mut artifacts = Vec::with_capacity(files.len());
    for file in files {
        crate::path_safety::validate_descendant(
            bundle_root,
            &file,
            crate::path_safety::EntryKind::File,
            false,
            "Bundle 辅助文件",
        )?;
        let relative = file.strip_prefix(bundle_root).map_err(|error| {
            AppError::Path(format!(
                "无法计算 Bundle 辅助文件相对路径 {}: {error}",
                file.to_string_lossy()
            ))
        })?;
        let metadata = fs::symlink_metadata(&file)?;
        artifacts.push(ManifestArtifact {
            relpath: relative.to_string_lossy().replace('\\', "/"),
            bytes: metadata.len(),
            sha256: sha256_file(&file)?,
        });
    }
    artifacts.sort_by(|left, right| left.relpath.cmp(&right.relpath));
    Ok(artifacts)
}

/// Read the newest logical event timestamp from a rollout/transcript.
///
/// `KeepLocal` must compare session data timestamps, not filesystem mtimes:
/// extracting a zip necessarily gives an old bundle a brand-new mtime.
fn latest_jsonl_timestamp_seconds(path: &Path) -> AppResult<Option<i64>> {
    let file = File::open(path)?;
    let mut latest: Option<i64> = None;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(raw) = value.get("timestamp").and_then(Value::as_str) else {
            continue;
        };
        let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(raw) else {
            continue;
        };
        let seconds = timestamp.timestamp();
        latest = Some(latest.map_or(seconds, |current| current.max(seconds)));
    }
    Ok(latest)
}

fn keep_local_reason(local_rollout: &Path, bundle_rollout: &Path) -> AppResult<Option<String>> {
    let Some(local_updated_at) = latest_jsonl_timestamp_seconds(local_rollout)? else {
        return Ok(Some(
            "本地会话缺少有效时间戳，无法可靠比较，已保留本地版本".into(),
        ));
    };
    let Some(bundle_updated_at) = latest_jsonl_timestamp_seconds(bundle_rollout)? else {
        return Ok(Some(
            "bundle 会话缺少有效时间戳，无法可靠比较，已保留本地版本".into(),
        ));
    };
    if local_updated_at >= bundle_updated_at {
        return Ok(Some(format!(
            "本地会话较新或相同（本地 {local_updated_at}，bundle {bundle_updated_at}）"
        )));
    }
    Ok(None)
}

fn batch_slug(sequence: u64) -> String {
    format!(
        "batch-{}-{}-{sequence}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%9f"),
        std::process::id()
    )
}

fn create_export_batch(out: &Path, machine: &str, group: &str) -> AppResult<(PathBuf, PathBuf)> {
    let parent = out.join(machine).join(group);
    ensure_plain_directory_path(&parent, "Bundle 导出目录")?;
    loop {
        let sequence = BUNDLE_EXPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = batch_slug(sequence);
        let final_path = parent.join(&name);
        let stage = parent.join(format!(".{name}.partial"));
        if final_path.try_exists()? {
            continue;
        }
        match fs::create_dir(&stage) {
            Ok(()) => return Ok((stage, final_path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn publish_export_batch(
    stage: &Path,
    final_path: &Path,
    reports: &mut [ExportReport],
) -> AppResult<()> {
    for entry in fs::read_dir(stage)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "Bundle 导出暂存批次包含链接/junction: {}",
                path.to_string_lossy()
            )));
        }
        if metadata.is_dir() {
            let manifest = path.join("manifest.json");
            let completed = matches!(
                fs::symlink_metadata(&manifest),
                Ok(manifest_metadata)
                    if manifest_metadata.is_file()
                        && !crate::path_safety::metadata_is_link_or_reparse(&manifest_metadata)
            );
            if !completed {
                fs::remove_dir_all(&path)?;
            }
        } else {
            return Err(AppError::Path(format!(
                "Bundle 导出暂存批次包含意外的顶层文件: {}",
                path.to_string_lossy()
            )));
        }
    }
    crate::path_safety::validate_tree(
        stage.parent().unwrap_or_else(|| Path::new(".")),
        stage,
        "Bundle 导出暂存批次",
    )?;
    if final_path.try_exists()? {
        return Err(AppError::Other(format!(
            "Bundle 导出目标在发布前已存在: {}",
            final_path.to_string_lossy()
        )));
    }
    fs::rename(stage, final_path)?;
    for report in reports.iter_mut().filter(|report| report.ok) {
        let Some(raw_path) = report.bundle_path.as_deref() else {
            continue;
        };
        let staged_bundle = Path::new(raw_path);
        let relative = staged_bundle.strip_prefix(stage).map_err(|error| {
            AppError::Path(format!(
                "Bundle 报告路径不在暂存批次内 {}: {error}",
                staged_bundle.to_string_lossy()
            ))
        })?;
        report.bundle_path = Some(final_path.join(relative).to_string_lossy().into_owned());
    }
    Ok(())
}

fn index_rollouts(codex: &Path) -> AppResult<HashMap<String, RolloutSource>> {
    let mut out = HashMap::new();
    for root in [
        paths::sessions_dir(codex),
        paths::archived_sessions_dir(codex),
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
        let canonical_root = root.canonicalize()?;
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            let entry = entry.map_err(|error| {
                AppError::Other(format!(
                    "遍历 Codex rollout 目录失败 {}: {error}",
                    root.to_string_lossy()
                ))
            })?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
                return Err(AppError::Path(format!(
                    "Codex rollout 目录包含链接/junction，已拒绝导出: {}",
                    entry.path().to_string_lossy()
                )));
            }
            if !entry.path().canonicalize()?.starts_with(&canonical_root) {
                return Err(AppError::Path(format!(
                    "Codex rollout 条目解析后逃出根目录: {}",
                    entry.path().to_string_lossy()
                )));
            }
            if !metadata.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                continue;
            }
            let abs = entry.path().to_path_buf();
            let (id, source) = rollout_source_from_path(codex, abs)?;
            out.entry(id).or_insert(source);
        }
    }
    Ok(out)
}

fn rollout_source_from_path(codex: &Path, abs: PathBuf) -> AppResult<(String, RolloutSource)> {
    validate_codex_rollout_source_path(codex, &abs)?;
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

fn validate_codex_rollout_source_path(codex: &Path, path: &Path) -> AppResult<()> {
    for root in [
        paths::sessions_dir(codex),
        paths::archived_sessions_dir(codex),
    ] {
        let metadata = match fs::symlink_metadata(&root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_dir() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
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
                "Codex Bundle rollout 源",
            )?;
            return Ok(());
        }
    }
    Err(AppError::Path(format!(
        "Codex Bundle rollout 不在 sessions 或 archived_sessions 内: {}",
        path.to_string_lossy()
    )))
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
    let abs = paths::host_path_from_codex_record(codex, &rollout_path);
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

pub fn export_session_bundles(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    out_dir: String,
    ids: Vec<String>,
    targets: Option<Vec<BundleExportTarget>>,
    machine_label: Option<String>,
    export_group: Option<String>,
) -> AppResult<Vec<ExportReport>> {
    let targets = normalize_bundle_export_targets(&ids, targets)?;
    if provider.as_deref().unwrap_or(PROVIDER_CODEX) == PROVIDER_CLAUDE {
        let claude = PathBuf::from(
            claude_dir
                .unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned()),
        );
        return export_claude_session_bundles(
            &claude,
            &PathBuf::from(out_dir),
            &targets,
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

fn normalize_bundle_export_targets(
    ids: &[String],
    targets: Option<Vec<BundleExportTarget>>,
) -> AppResult<Vec<BundleExportTarget>> {
    let targets = match targets {
        Some(targets) => {
            if targets.len() != ids.len() {
                return Err(AppError::Other(format!(
                    "Bundle 导出 ids 与 targets 数量不一致: ids={} targets={}",
                    ids.len(),
                    targets.len()
                )));
            }
            for (index, (id, target)) in ids.iter().zip(&targets).enumerate() {
                if target.id != *id {
                    return Err(AppError::Other(format!(
                        "Bundle 导出 ids 与 targets 第 {} 项不一致: id={} target.id={}",
                        index + 1,
                        id,
                        target.id
                    )));
                }
            }
            targets
        }
        None => ids
            .iter()
            .cloned()
            .map(|id| BundleExportTarget {
                id,
                rollout_path: None,
            })
            .collect(),
    };

    let mut seen = HashSet::new();
    for target in &targets {
        let identity = (
            target.id.clone(),
            target.rollout_path.as_deref().unwrap_or("").to_string(),
        );
        if !seen.insert(identity) {
            return Err(AppError::Other(format!(
                "Bundle 导出目标重复: id={} rollout_path={}",
                target.id,
                target.rollout_path.as_deref().unwrap_or("未提供")
            )));
        }
    }
    Ok(targets)
}

fn export_session_bundles_from_index(
    codex: &Path,
    out: &Path,
    ids: &[String],
    machine_label: Option<&str>,
    export_group: Option<&str>,
    rollout_index: &HashMap<String, RolloutSource>,
) -> AppResult<Vec<ExportReport>> {
    let machine = machine_label
        .map(paths::sanitize_slug)
        .unwrap_or_else(paths::machine_label);
    let group = paths::sanitize_slug(export_group.unwrap_or("default"));

    // 读 state_5.sqlite 以获取 title / cwd / updated_at（没有也能导出）
    let state_conn = state_db::open_ro(codex).ok();
    // 一次扫完 history.jsonl 建索引，避免每条 id 都重扫
    let history_index = build_history_index(codex)?;
    let (batch_root, final_batch_root) = create_export_batch(out, &machine, &group)?;

    let mut reports: Vec<ExportReport> = Vec::with_capacity(ids.len());
    for id in ids {
        let r = export_one(
            codex,
            &batch_root,
            id,
            &machine,
            &group,
            state_conn.as_deref(),
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
    if reports.iter().any(|report| report.ok) {
        if let Err(error) = publish_export_batch(&batch_root, &final_batch_root, &mut reports) {
            return Err(match remove_path_recursive(&batch_root) {
                Ok(()) => error,
                Err(cleanup_error) => AppError::Other(format!(
                    "{error}; 清理 Bundle 导出暂存批次失败: {cleanup_error}"
                )),
            });
        }
    } else {
        remove_path_recursive(&batch_root)?;
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
    crate::path_safety::validate_descendant(
        batch_root,
        &bundle_dir,
        crate::path_safety::EntryKind::Directory,
        true,
        "Codex Bundle 导出目录",
    )?;
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
    let artifacts = collect_bundle_artifacts(&bundle_dir, has_history, None)?;

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
        artifacts,
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
    targets: &[BundleExportTarget],
    machine_label: Option<&str>,
    export_group: Option<&str>,
) -> AppResult<Vec<ExportReport>> {
    let machine = machine_label
        .map(paths::sanitize_slug)
        .unwrap_or_else(paths::machine_label);
    let group = paths::sanitize_slug(export_group.unwrap_or("default"));

    let projects = paths::claude_projects_dir(claude);
    validate_plain_directory_tree(&projects, "Claude projects")?;
    let sessions = crate::claude_sessions::scan_sessions(claude)?;
    let history_ids = targets
        .iter()
        .map(|target| target.id.clone())
        .collect::<std::collections::HashSet<_>>();
    let history_index =
        crate::history::collect_lines_for_ids(&paths::history_path(claude), &history_ids)?;
    let (batch_root, final_batch_root) = create_export_batch(out, &machine, &group)?;
    let mut reports = Vec::with_capacity(targets.len());
    for target in targets {
        reports.push(
            resolve_claude_export_session(&projects, &sessions, target)
                .and_then(|session| {
                    export_one_claude(
                        claude,
                        &projects,
                        &batch_root,
                        session,
                        &machine,
                        &group,
                        &history_index,
                    )
                })
                .unwrap_or_else(|e| ExportReport {
                    session_id: target.id.clone(),
                    ok: false,
                    bundle_path: None,
                    error: Some(e.to_string()),
                    skipped_reason: None,
                }),
        );
    }
    if reports.iter().any(|report| report.ok) {
        if let Err(error) = publish_export_batch(&batch_root, &final_batch_root, &mut reports) {
            return Err(match remove_path_recursive(&batch_root) {
                Ok(()) => error,
                Err(cleanup_error) => AppError::Other(format!(
                    "{error}; 清理 Bundle 导出暂存批次失败: {cleanup_error}"
                )),
            });
        }
    } else {
        remove_path_recursive(&batch_root)?;
    }
    Ok(reports)
}

fn resolve_claude_export_session<'a>(
    projects: &Path,
    sessions: &'a [SessionSummary],
    target: &BundleExportTarget,
) -> AppResult<&'a SessionSummary> {
    let matching_ids = sessions
        .iter()
        .filter(|session| session.id == target.id)
        .collect::<Vec<_>>();
    let session = if let Some(raw_path) = target.rollout_path.as_deref() {
        let requested = paths::strip_verbatim(raw_path);
        matching_ids
            .into_iter()
            .find(|session| paths::strip_verbatim(&session.rollout_path) == requested)
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "Claude 会话精确目标不存在或 ID 不匹配: id={} rollout_path={raw_path}",
                    target.id
                ))
            })?
    } else {
        match matching_ids.as_slice() {
            [session] => *session,
            [] => {
                return Err(AppError::NotFound(format!(
                    "Claude 会话不存在: {}",
                    target.id
                )))
            }
            matches => {
                return Err(AppError::Other(format!(
                    "发现 {} 个同 ID Claude 会话，Bundle 导出必须提供精确 rollout_path: {}",
                    matches.len(),
                    target.id
                )))
            }
        }
    };

    let source = PathBuf::from(&session.rollout_path);
    crate::path_safety::validate_descendant(
        projects,
        &source,
        crate::path_safety::EntryKind::File,
        false,
        "Claude Bundle 导出 transcript",
    )?;
    validate_claude_jsonl_identity(&source, &target.id)?;
    Ok(session)
}

fn export_one_claude(
    claude: &Path,
    projects: &Path,
    batch_root: &Path,
    session: &SessionSummary,
    machine: &str,
    group: &str,
    history_index: &HashMap<String, Vec<String>>,
) -> AppResult<ExportReport> {
    let id = &session.id;
    let source = PathBuf::from(&session.rollout_path);
    let source_rel = crate::claude_sessions::session_relpath(claude, &source);
    let source_rel_string = source_rel.to_string_lossy().replace('\\', "/");
    crate::path_safety::validate_descendant(
        projects,
        &source,
        crate::path_safety::EntryKind::File,
        false,
        "Claude Bundle 导出 transcript",
    )?;
    validate_claude_bundle_rollout_relpath(&source_rel_string, id)?;
    validate_claude_jsonl_identity(&source, id)?;
    let bundle_dir = batch_root.join(claude_bundle_dir_name(id, &source_rel_string));
    let claude_sub = bundle_dir.join(PROVIDER_CLAUDE).join(&source_rel);
    if let Some(parent) = claude_sub.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&source, &claude_sub)?;
    let sha = sha256_file(&claude_sub)?;
    let line_count = count_jsonl_lines(&claude_sub)?;

    let mut sidecar_rel: Option<String> = None;
    if let Some(sidecar) = crate::claude_sessions::sidecar_path_for(&source) {
        if path_exists_no_follow(&sidecar)? {
            crate::path_safety::validate_tree(projects, &sidecar, "Claude Bundle 导出 sidecar")?;
            let rel = PathBuf::from("sidecars").join(paths::sanitize_slug(id));
            copy_path_recursive(&sidecar, &bundle_dir.join(&rel))?;
            sidecar_rel = Some(rel.to_string_lossy().replace('\\', "/"));
        }
    }

    let has_history =
        write_history_from_index(history_index, id, &bundle_dir.join("history.jsonl"))?;
    let artifacts = collect_bundle_artifacts(&bundle_dir, has_history, sidecar_rel.as_deref())?;

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
        artifacts,
    };
    fs::write(
        bundle_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    Ok(ExportReport {
        session_id: id.clone(),
        ok: true,
        bundle_path: Some(bundle_dir.to_string_lossy().into_owned()),
        error: None,
        skipped_reason: None,
    })
}

fn claude_bundle_dir_name(id: &str, source_rel: &str) -> String {
    let digest = Sha256::digest(source_rel.as_bytes());
    format!(
        "{}-{}",
        paths::sanitize_slug(id),
        &hex::encode(digest)[..12]
    )
}

pub fn export_all_bundles(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    out_dir: String,
    machine_label: Option<String>,
    export_group: Option<String>,
    active_only: bool,
) -> AppResult<Vec<ExportReport>> {
    if provider.as_deref().unwrap_or(PROVIDER_CODEX) == PROVIDER_CLAUDE {
        let claude = PathBuf::from(
            claude_dir
                .unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned()),
        );
        let projects = paths::claude_projects_dir(&claude);
        validate_plain_directory_tree(&projects, "Claude projects")?;
        let targets = crate::claude_sessions::scan_sessions(&claude)?
            .into_iter()
            .map(|session| BundleExportTarget {
                id: session.id,
                rollout_path: Some(session.rollout_path),
            })
            .collect::<Vec<_>>();
        return export_claude_session_bundles(
            &claude,
            &PathBuf::from(out_dir),
            &targets,
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
        let mut family_managed_ids = store.index.keys().cloned().collect::<HashSet<_>>();
        for f in store.families.values() {
            ids.push(f.active_id.clone());
            family_managed_ids.extend(f.chain.iter().map(|branch| branch.id.clone()));
        }
        for (id, source) in &rollout_index {
            let is_active_rollout = matches!(
                source.rel.components().next(),
                Some(Component::Normal(root)) if root == std::ffi::OsStr::new("sessions")
            );
            if is_active_rollout && !family_managed_ids.contains(id) {
                ids.push(id.clone());
            }
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

pub fn list_bundles(src_dir: String, provider: Option<String>) -> AppResult<Vec<BundleListItem>> {
    let root = PathBuf::from(&src_dir);
    if !plain_directory_root_exists(&root, "Bundle 源目录")? {
        return Ok(Vec::new());
    }
    let canonical_root = root.canonicalize()?;
    let mut out: Vec<BundleListItem> = Vec::new();
    for entry in walkdir::WalkDir::new(&root)
        .max_depth(6)
        .follow_links(false)
    {
        let entry = entry.map_err(|error| {
            AppError::Other(format!(
                "遍历 bundle 目录失败 {}: {error}",
                root.to_string_lossy()
            ))
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "Bundle 源目录包含符号链接或 junction，已拒绝: {}",
                entry.path().to_string_lossy()
            )));
        }
        if !entry.path().canonicalize()?.starts_with(&canonical_root) {
            return Err(AppError::Path(format!(
                "Bundle 源目录条目解析后逃出根目录: {}",
                entry.path().to_string_lossy()
            )));
        }
        if metadata.is_file() && entry.file_name() == std::ffi::OsStr::new("manifest.json") {
            let mp = entry.path();
            crate::path_safety::validate_descendant(
                &root,
                mp,
                crate::path_safety::EntryKind::File,
                false,
                "Bundle manifest",
            )?;
            let raw = fs::read_to_string(mp)?;
            let m = serde_json::from_str::<BundleManifest>(&raw).map_err(|error| {
                AppError::Other(format!(
                    "bundle manifest 不是有效 JSON {}: {error}",
                    mp.to_string_lossy()
                ))
            })?;
            validate_bundle_artifact_manifest(&m)?;
            if let Some(provider) = provider.as_deref() {
                if bundle_provider(&m) != provider {
                    continue;
                }
            }
            let bdir = mp.parent().ok_or_else(|| {
                AppError::Path(format!(
                    "bundle manifest 缺少父目录: {}",
                    mp.to_string_lossy()
                ))
            })?;
            out.push(BundleListItem {
                bundle_dir: bdir.to_string_lossy().into_owned(),
                manifest: m,
                verified: None,
            });
        }
    }
    out.sort_by(|a, b| b.manifest.exported_at.cmp(&a.manifest.exported_at));
    Ok(out)
}

pub fn verify_bundles(src_dir: String, provider: Option<String>) -> AppResult<Vec<BundleListItem>> {
    let mut items = list_bundles(src_dir, provider)?;
    for it in items.iter_mut() {
        let bundle_root = validate_bundle_item_root(it)?;
        let is_claude = bundle_provider(&it.manifest) == PROVIDER_CLAUDE;
        let rel = if is_claude {
            let rel = validate_claude_bundle_rollout_relpath(
                &it.manifest.rollout_relpath,
                &it.manifest.session_id,
            )?;
            let source_rel = it
                .manifest
                .source_relpath
                .as_deref()
                .unwrap_or(&it.manifest.rollout_relpath);
            validate_claude_bundle_rollout_relpath(source_rel, &it.manifest.session_id)?;
            rel
        } else {
            validate_codex_bundle_rollout_relpath(
                &it.manifest.rollout_relpath,
                &it.manifest.session_id,
            )?
        };
        let base = if is_claude {
            PROVIDER_CLAUDE
        } else {
            PROVIDER_CODEX
        };
        let file = bundle_root.join(base).join(&rel);
        if !crate::path_safety::validate_descendant(
            &bundle_root,
            &file,
            crate::path_safety::EntryKind::File,
            true,
            "Bundle rollout",
        )? {
            it.verified = Some(false);
            continue;
        }
        validate_bundle_history_source(&bundle_root)?;
        if is_claude {
            validate_claude_jsonl_identity(&file, &it.manifest.session_id)?;
            claude_bundle_sidecar(&bundle_root, &it.manifest)?;
        }
        let actual = sha256_file(&file)?;
        let artifacts_ok = verify_bundle_artifacts(&bundle_root, &it.manifest)?;
        it.verified =
            Some(it.manifest.version >= 2 && artifacts_ok && actual == it.manifest.sha256_rollout);
    }
    Ok(items)
}

// ========================= 导入 =========================

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

fn validate_bundle_artifact_manifest(manifest: &BundleManifest) -> AppResult<()> {
    let sidecar = manifest
        .sidecar_relpath
        .as_deref()
        .map(paths::checked_relative_path)
        .transpose()?;
    let mut seen = HashSet::new();
    let mut has_history_artifact = false;
    for artifact in &manifest.artifacts {
        let relative = paths::checked_relative_path(&artifact.relpath)?;
        if !seen.insert(relative.clone()) {
            return Err(AppError::Path(format!(
                "Bundle 辅助文件清单包含重复路径: {}",
                artifact.relpath
            )));
        }
        if relative == Path::new("history.jsonl") {
            if !manifest.has_history {
                return Err(AppError::Path(
                    "Bundle 未声明 history 却包含 history 辅助文件".into(),
                ));
            }
            has_history_artifact = true;
            continue;
        }
        if !sidecar
            .as_ref()
            .is_some_and(|sidecar| relative == *sidecar || relative.starts_with(sidecar))
        {
            return Err(AppError::Path(format!(
                "Bundle 辅助文件不在已声明的 history/sidecar 范围内: {}",
                artifact.relpath
            )));
        }
    }
    if manifest.version >= 2 && manifest.has_history != has_history_artifact {
        return Err(AppError::Other(
            "Bundle v2 history 声明与辅助文件清单不一致".into(),
        ));
    }
    Ok(())
}

fn verify_bundle_artifacts(bundle_root: &Path, manifest: &BundleManifest) -> AppResult<bool> {
    validate_bundle_artifact_manifest(manifest)?;
    if manifest.version < 2 {
        return Ok(false);
    }
    let actual = collect_bundle_artifacts(
        bundle_root,
        manifest.has_history,
        manifest.sidecar_relpath.as_deref(),
    )?;
    if actual.len() != manifest.artifacts.len() {
        return Ok(false);
    }
    let expected = manifest
        .artifacts
        .iter()
        .map(|artifact| (artifact.relpath.as_str(), artifact))
        .collect::<HashMap<_, _>>();
    Ok(actual.iter().all(|artifact| {
        expected
            .get(artifact.relpath.as_str())
            .is_some_and(|item| item.bytes == artifact.bytes && item.sha256 == artifact.sha256)
    }))
}

fn plain_directory_root_exists(root: &Path, label: &str) -> AppResult<bool> {
    match fs::symlink_metadata(root) {
        Ok(metadata)
            if metadata.is_dir() && !crate::path_safety::metadata_is_link_or_reparse(&metadata) =>
        {
            Ok(true)
        }
        Ok(_) => Err(AppError::Path(format!(
            "{label} 必须是普通目录且不能是链接或 junction: {}",
            root.to_string_lossy()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn validate_plain_directory_tree(root: &Path, label: &str) -> AppResult<()> {
    if !plain_directory_root_exists(root, label)? {
        return Err(AppError::NotFound(format!(
            "{label} 不存在: {}",
            root.to_string_lossy()
        )));
    }
    let canonical_root = root.canonicalize()?;
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| {
            AppError::Other(format!(
                "遍历 {label} 失败 {}: {error}",
                root.to_string_lossy()
            ))
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "{label} 包含符号链接或 junction，已拒绝: {}",
                entry.path().to_string_lossy()
            )));
        }
        if !entry.path().canonicalize()?.starts_with(&canonical_root) {
            return Err(AppError::Path(format!(
                "{label} 条目解析后逃出根目录: {}",
                entry.path().to_string_lossy()
            )));
        }
    }
    Ok(())
}

fn ensure_plain_directory_path(path: &Path, label: &str) -> AppResult<()> {
    if path.as_os_str().is_empty() {
        return Err(AppError::Path(format!("{label} 不能为空")));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                return Err(AppError::Path(format!(
                    "{label} 不能包含父目录跳转: {}",
                    path.to_string_lossy()
                )))
            }
            std::path::Component::CurDir => continue,
            std::path::Component::Prefix(_) => {
                current.push(component.as_os_str());
                continue;
            }
            std::path::Component::RootDir | std::path::Component::Normal(_) => {
                current.push(component.as_os_str());
            }
        }
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                fs::symlink_metadata(&current)?
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_dir() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "{label} 的父链必须全部是普通目录且不能包含链接或 junction: {}",
                current.to_string_lossy()
            )));
        }
    }
    Ok(())
}

fn validate_bundle_item_root(item: &BundleListItem) -> AppResult<PathBuf> {
    let root = PathBuf::from(&item.bundle_dir);
    if !plain_directory_root_exists(&root, "Bundle 目录")? {
        return Err(AppError::NotFound(format!(
            "Bundle 目录不存在: {}",
            root.to_string_lossy()
        )));
    }
    crate::path_safety::validate_descendant(
        &root,
        &root.join("manifest.json"),
        crate::path_safety::EntryKind::File,
        false,
        "Bundle manifest",
    )?;
    Ok(root)
}

fn validate_claude_bundle_rollout_relpath(raw: &str, session_id: &str) -> AppResult<PathBuf> {
    let relative = paths::checked_relative_path(raw)?;
    let expected_name = format!("{session_id}.jsonl");
    if relative.extension().and_then(|value| value.to_str()) != Some("jsonl")
        || relative.file_name().and_then(|value| value.to_str()) != Some(expected_name.as_str())
    {
        return Err(AppError::Path(format!(
            "Claude bundle JSONL 文件名必须与会话 ID 完全一致: id={session_id} path={raw}"
        )));
    }
    Ok(relative)
}

fn validate_claude_jsonl_identity(path: &Path, expected_id: &str) -> AppResult<()> {
    let mut found = false;
    for (line_no, line) in BufReader::new(File::open(path)?).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line).map_err(|error| {
            AppError::Other(format!(
                "Claude transcript 第 {} 行不是有效 JSON {}: {error}",
                line_no + 1,
                path.to_string_lossy()
            ))
        })?;
        if let Some(actual_id) = value.get("sessionId").and_then(Value::as_str) {
            found = true;
            if actual_id != expected_id {
                return Err(AppError::Other(format!(
                    "Claude transcript 内部 sessionId 不匹配: 期望 {expected_id}，实际 {actual_id}: {}",
                    path.to_string_lossy()
                )));
            }
        }
    }
    if !found {
        return Err(AppError::Other(format!(
            "Claude transcript 缺少 sessionId={expected_id}: {}",
            path.to_string_lossy()
        )));
    }
    Ok(())
}

fn validate_bundle_history_source(bundle_root: &Path) -> AppResult<Option<PathBuf>> {
    let history = bundle_root.join("history.jsonl");
    if crate::path_safety::validate_descendant(
        bundle_root,
        &history,
        crate::path_safety::EntryKind::File,
        true,
        "Bundle history",
    )? {
        Ok(Some(history))
    } else {
        Ok(None)
    }
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

    let bundle_root = validate_bundle_item_root(item)?;
    let artifacts_verified = verify_bundle_artifacts(&bundle_root, &item.manifest)?;
    if item.manifest.version >= 2 && !artifacts_verified {
        return Err(AppError::Other(format!(
            "Claude Bundle 辅助文件大小或 sha256 校验失败: {}",
            item.manifest.session_id
        )));
    }
    if strict
        && item.manifest.version < 2
        && (item.manifest.has_history || item.manifest.sidecar_relpath.is_some())
    {
        report.error = Some("旧版 Bundle 未记录 history/sidecar 哈希，strict 模式拒绝导入".into());
        return Ok(report);
    }
    let rel = validate_claude_bundle_rollout_relpath(
        &item.manifest.rollout_relpath,
        &item.manifest.session_id,
    )?;
    let src_file = bundle_root.join(PROVIDER_CLAUDE).join(&rel);
    if !crate::path_safety::validate_descendant(
        &bundle_root,
        &src_file,
        crate::path_safety::EntryKind::File,
        true,
        "Claude bundle transcript",
    )? {
        report.error = Some(format!(
            "bundle Claude JSONL 缺失: {}",
            src_file.to_string_lossy()
        ));
        return Ok(report);
    }
    validate_claude_jsonl_identity(&src_file, &item.manifest.session_id)?;

    let actual = sha256_file(&src_file)?;
    let source_sha_verified = actual == item.manifest.sha256_rollout;
    if !source_sha_verified {
        report.sha_mismatch = true;
        if strict {
            report.error = Some("sha256 不一致，strict 模式跳过".into());
            return Ok(report);
        }
    } else {
        report.verified = artifacts_verified;
    }

    let source_rel = item
        .manifest
        .source_relpath
        .as_deref()
        .unwrap_or(&item.manifest.rollout_relpath);
    validate_claude_bundle_rollout_relpath(source_rel, &item.manifest.session_id)?;
    ensure_plain_directory_path(claude, "Claude 根目录")?;
    let projects = paths::claude_projects_dir(claude);
    ensure_plain_directory_path(&projects, "Claude projects")?;
    let mapped_cwd = mapped_project_cwd(&item.manifest, project_mappings);
    let dest_abs = claude_import_dest(claude, source_rel, mapped_cwd, &item.manifest.session_id)?;
    let dest_parent = dest_abs.parent().ok_or_else(|| {
        AppError::Path(format!(
            "Claude 导入目标缺少父目录: {}",
            dest_abs.to_string_lossy()
        ))
    })?;
    ensure_plain_directory_path(dest_parent, "Claude 导入目标父目录")?;
    let destination_exists = crate::path_safety::validate_descendant(
        &projects,
        &dest_abs,
        crate::path_safety::EntryKind::File,
        true,
        "Claude bundle 导入目标",
    )?;
    let sidecar_dest = crate::claude_sessions::sidecar_path_for(&dest_abs).ok_or_else(|| {
        AppError::Path(format!(
            "Claude 导入目标缺少 sidecar 路径: {}",
            dest_abs.to_string_lossy()
        ))
    })?;
    if path_exists_no_follow(&sidecar_dest)? {
        crate::path_safety::validate_tree(
            &projects,
            &sidecar_dest,
            "Claude bundle 导入目标 sidecar",
        )?;
    }
    let sidecar_src = claude_bundle_sidecar(&bundle_root, &item.manifest)?;
    let history_src = validate_bundle_history_source(&bundle_root)?;
    crate::path_safety::validate_descendant(
        claude,
        &paths::history_path(claude),
        crate::path_safety::EntryKind::File,
        true,
        "Claude history 导入目标",
    )?;

    if destination_exists {
        match mode {
            ImportMode::Skip => {
                report.skipped_reason = Some("本地已存在，Skip 模式".into());
                report.ok = true;
                report.history_appended = append_optional_history(
                    claude,
                    history_src.as_deref(),
                    &item.manifest.session_id,
                )?;
                return Ok(report);
            }
            ImportMode::KeepLocal => {
                if let Some(reason) = keep_local_reason(&dest_abs, &src_file)? {
                    report.skipped_reason = Some(reason);
                    report.ok = true;
                    report.history_appended = append_optional_history(
                        claude,
                        history_src.as_deref(),
                        &item.manifest.session_id,
                    )?;
                    return Ok(report);
                }
            }
            ImportMode::Overwrite => {}
        }
    }

    replace_claude_snapshot_verified(
        &src_file,
        &dest_abs,
        mapped_cwd,
        sidecar_src.as_deref(),
        source_sha_verified.then_some(item.manifest.sha256_rollout.as_str()),
    )?;
    report.rollout_written = true;

    match append_optional_history(claude, history_src.as_deref(), &item.manifest.session_id) {
        Ok(appended) => report.history_appended = appended,
        Err(error) => {
            report.error = Some(format!("rollout 已写入，但 history 追加失败: {error}"));
            return Ok(report);
        }
    }

    report.ok = true;
    Ok(report)
}

fn claude_bundle_sidecar(
    bundle_root: &Path,
    manifest: &BundleManifest,
) -> AppResult<Option<PathBuf>> {
    let Some(sidecar_rel) = manifest.sidecar_relpath.as_deref() else {
        return Ok(None);
    };
    let sidecar = bundle_root.join(paths::checked_relative_path(sidecar_rel)?);
    crate::path_safety::validate_tree(bundle_root, &sidecar, "Claude bundle sidecar")?;
    Ok(Some(sidecar))
}

fn append_optional_history(codex: &Path, source: Option<&Path>, id: &str) -> AppResult<u32> {
    match source {
        Some(source) => append_history(codex, source, id),
        None => Ok(0),
    }
}

pub(crate) fn replace_claude_snapshot_verified(
    src_transcript: &Path,
    dest_transcript: &Path,
    target_cwd: Option<&str>,
    src_sidecar: Option<&Path>,
    expected_source_sha256: Option<&str>,
) -> AppResult<()> {
    let expected_transcript = match fs::symlink_metadata(dest_transcript) {
        Ok(metadata)
            if metadata.is_file()
                && !crate::path_safety::metadata_is_link_or_reparse(&metadata) =>
        {
            Some(atomic_file::fingerprint(dest_transcript)?)
        }
        Ok(_) => {
            return Err(AppError::Path(format!(
                "Claude transcript 导入目标不是普通文件，拒绝覆盖: {}",
                dest_transcript.to_string_lossy()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let dest_sidecar =
        crate::claude_sessions::sidecar_path_for(dest_transcript).ok_or_else(|| {
            AppError::Path(format!(
                "Claude transcript 导入目标缺少有效文件名: {}",
                dest_transcript.to_string_lossy()
            ))
        })?;
    let parent = dest_transcript
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let (staged_transcript, mut staged_file) =
        create_unique_snapshot_file(dest_transcript, "transcript-stage")?;
    let transcript_result = write_claude_jsonl_with_cwd(
        src_transcript,
        &mut staged_file,
        target_cwd,
        expected_source_sha256,
    )
    .and_then(|()| {
        staged_file.flush()?;
        staged_file.sync_all()?;
        Ok(())
    });
    drop(staged_file);
    if let Err(error) = transcript_result {
        return Err(cleanup_snapshot_paths(
            error,
            std::slice::from_ref(&staged_transcript),
        ));
    }

    let staged_sidecar = match src_sidecar {
        Some(source) => {
            let staged = match stage_claude_sidecar(source, &dest_sidecar) {
                Ok(staged) => staged,
                Err(error) => {
                    return Err(cleanup_snapshot_paths(error, &[staged_transcript]));
                }
            };
            Some(staged)
        }
        None => None,
    };

    let result = commit_staged_claude_snapshot(
        dest_transcript,
        &dest_sidecar,
        &staged_transcript,
        staged_sidecar.as_deref(),
        expected_transcript.as_ref(),
    );
    if let Err(error) = result {
        let mut staging = vec![staged_transcript];
        if let Some(staged_sidecar) = staged_sidecar {
            staging.push(staged_sidecar);
        }
        return Err(cleanup_snapshot_paths(error, &staging));
    }
    Ok(())
}

fn commit_staged_claude_snapshot(
    dest_transcript: &Path,
    dest_sidecar: &Path,
    staged_transcript: &Path,
    staged_sidecar: Option<&Path>,
    expected_transcript: Option<&atomic_file::FileFingerprint>,
) -> AppResult<()> {
    commit_staged_claude_snapshot_with_hook(
        dest_transcript,
        dest_sidecar,
        staged_transcript,
        staged_sidecar,
        expected_transcript,
        || Ok(()),
    )
}

fn commit_staged_claude_snapshot_with_hook(
    dest_transcript: &Path,
    dest_sidecar: &Path,
    staged_transcript: &Path,
    staged_sidecar: Option<&Path>,
    expected_transcript: Option<&atomic_file::FileFingerprint>,
    before_transcript_install: impl FnOnce() -> AppResult<()>,
) -> AppResult<()> {
    verify_snapshot_destination(dest_transcript, expected_transcript)?;

    let transcript_backup = unique_snapshot_sibling(dest_transcript, "transcript-backup")?;
    let sidecar_backup = unique_snapshot_sibling(dest_sidecar, "sidecar-backup")?;
    let mut parked_transcript = false;
    let mut parked_sidecar = false;
    let mut installed_sidecar = false;
    let mut installed_transcript = false;

    let commit_result = (|| -> AppResult<()> {
        if expected_transcript.is_some() {
            rename_snapshot_path(dest_transcript, &transcript_backup)?;
            parked_transcript = true;
        }
        if path_exists_no_follow(dest_sidecar)? {
            rename_snapshot_path(dest_sidecar, &sidecar_backup)?;
            parked_sidecar = true;
        }
        if let Some(staged_sidecar) = staged_sidecar {
            rename_snapshot_path(staged_sidecar, dest_sidecar)?;
            installed_sidecar = true;
        }
        before_transcript_install()?;
        rename_snapshot_path(staged_transcript, dest_transcript)?;
        installed_transcript = true;
        Ok(())
    })();

    if let Err(error) = commit_result {
        let rollback_errors = rollback_claude_snapshot(
            dest_transcript,
            dest_sidecar,
            &transcript_backup,
            &sidecar_backup,
            parked_transcript,
            parked_sidecar,
            installed_transcript,
            installed_sidecar,
        );
        return Err(append_operation_errors(
            error,
            "回滚 Claude 快照失败",
            rollback_errors,
        ));
    }

    let mut cleanup_errors = Vec::new();
    if parked_transcript {
        collect_path_removal_error(&transcript_backup, &mut cleanup_errors);
    }
    if parked_sidecar {
        collect_path_removal_error(&sidecar_backup, &mut cleanup_errors);
    }
    if cleanup_errors.is_empty() {
        Ok(())
    } else {
        Err(AppError::Other(format!(
            "Claude 快照已替换，但清理旧快照失败: {}",
            cleanup_errors.join("; ")
        )))
    }
}

fn verify_snapshot_destination(
    dest_transcript: &Path,
    expected: Option<&atomic_file::FileFingerprint>,
) -> AppResult<()> {
    match expected {
        Some(expected) => {
            let current = atomic_file::fingerprint(dest_transcript)?;
            if &current != expected {
                return Err(AppError::Other(format!(
                    "Claude transcript 在导入期间发生变化，已拒绝覆盖: {}",
                    dest_transcript.to_string_lossy()
                )));
            }
        }
        None if path_exists_no_follow(dest_transcript)? => {
            return Err(AppError::Other(format!(
                "Claude transcript 在导入期间已由其他进程创建，已拒绝覆盖: {}",
                dest_transcript.to_string_lossy()
            )))
        }
        None => {}
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn rollback_claude_snapshot(
    dest_transcript: &Path,
    dest_sidecar: &Path,
    transcript_backup: &Path,
    sidecar_backup: &Path,
    parked_transcript: bool,
    parked_sidecar: bool,
    installed_transcript: bool,
    installed_sidecar: bool,
) -> Vec<String> {
    let mut errors = Vec::new();
    if installed_transcript {
        collect_path_removal_error(dest_transcript, &mut errors);
    }
    if installed_sidecar {
        collect_path_removal_error(dest_sidecar, &mut errors);
    }
    if parked_transcript {
        collect_rename_error(transcript_backup, dest_transcript, &mut errors);
    }
    if parked_sidecar {
        collect_rename_error(sidecar_backup, dest_sidecar, &mut errors);
    }
    errors
}

fn create_unique_snapshot_file(dest: &Path, label: &str) -> AppResult<(PathBuf, File)> {
    loop {
        let path = unique_snapshot_sibling(dest, label)?;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn stage_claude_sidecar(source: &Path, dest: &Path) -> AppResult<PathBuf> {
    let metadata = fs::symlink_metadata(source)?;
    if crate::path_safety::metadata_is_link_or_reparse(&metadata)
        || !(metadata.is_file() || metadata.is_dir())
    {
        return Err(AppError::Path(format!(
            "Claude sidecar 必须是普通文件或目录: {}",
            source.to_string_lossy()
        )));
    }
    loop {
        let staged = unique_snapshot_sibling(dest, "sidecar-stage")?;
        if metadata.is_file() {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)
            {
                Ok(mut target) => {
                    let copy_result = (|| -> AppResult<()> {
                        let mut source_file = File::open(source)?;
                        std::io::copy(&mut source_file, &mut target)?;
                        target.flush()?;
                        target.sync_all()?;
                        Ok(())
                    })();
                    drop(target);
                    return copy_result
                        .map(|()| staged.clone())
                        .map_err(|error| cleanup_snapshot_paths(error, &[staged]));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }

        match fs::create_dir(&staged) {
            Ok(()) => {
                return copy_directory_contents(source, &staged)
                    .map(|()| staged.clone())
                    .map_err(|error| cleanup_snapshot_paths(error, &[staged]));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn unique_snapshot_sibling(dest: &Path, label: &str) -> AppResult<PathBuf> {
    let parent = dest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = dest.file_name().ok_or_else(|| {
        AppError::Path(format!(
            "Claude 快照路径缺少文件名: {}",
            dest.to_string_lossy()
        ))
    })?;
    loop {
        let sequence = CLAUDE_SNAPSHOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut candidate = file_name.to_os_string();
        candidate.push(format!(
            ".{}.{}.{}.ccsm-snapshot",
            std::process::id(),
            sequence,
            label
        ));
        let path = parent.join(candidate);
        if !path_exists_no_follow(&path)? {
            return Ok(path);
        }
    }
}

fn path_exists_no_follow(path: &Path) -> AppResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn rename_snapshot_path(from: &Path, to: &Path) -> AppResult<()> {
    if path_exists_no_follow(to)? {
        return Err(AppError::Other(format!(
            "Claude 快照交换目标已存在，拒绝覆盖: {}",
            to.to_string_lossy()
        )));
    }
    fs::rename(from, to)?;
    Ok(())
}

fn collect_path_removal_error(path: &Path, errors: &mut Vec<String>) {
    if let Err(error) = remove_path_recursive(path) {
        errors.push(format!("{}: {error}", path.to_string_lossy()));
    }
}

fn collect_rename_error(from: &Path, to: &Path, errors: &mut Vec<String>) {
    if let Err(error) = rename_snapshot_path(from, to) {
        errors.push(format!(
            "{} -> {}: {error}",
            from.to_string_lossy(),
            to.to_string_lossy()
        ));
    }
}

fn append_operation_errors(original: AppError, context: &str, errors: Vec<String>) -> AppError {
    if errors.is_empty() {
        original
    } else {
        AppError::Other(format!("{original}; {context}: {}", errors.join("; ")))
    }
}

fn cleanup_snapshot_paths(original: AppError, paths: &[PathBuf]) -> AppError {
    let mut errors = Vec::new();
    for path in paths {
        collect_path_removal_error(path, &mut errors);
    }
    append_operation_errors(original, "清理 Claude 快照暂存路径失败", errors)
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
    if !plain_directory_root_exists(&projects, "Claude projects")? {
        return Ok(None);
    }
    let canonical_projects = projects.canonicalize()?;
    for entry in walkdir::WalkDir::new(&projects).follow_links(false) {
        let entry = entry.map_err(|error| {
            AppError::Other(format!(
                "遍历 Claude 项目目录失败 {}: {error}",
                projects.to_string_lossy()
            ))
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "Claude projects 包含符号链接或 junction，已拒绝: {}",
                entry.path().to_string_lossy()
            )));
        }
        if !entry
            .path()
            .canonicalize()?
            .starts_with(&canonical_projects)
        {
            return Err(AppError::Path(format!(
                "Claude projects 条目解析后逃出根目录: {}",
                entry.path().to_string_lossy()
            )));
        }
        if !metadata.is_file() {
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
    let rel = validate_codex_bundle_rollout_relpath(
        &item.manifest.rollout_relpath,
        &item.manifest.session_id,
    )?;
    let bundle_root = validate_bundle_item_root(item)?;
    let artifacts_verified = verify_bundle_artifacts(&bundle_root, &item.manifest)?;
    if item.manifest.version >= 2 && !artifacts_verified {
        return Err(AppError::Other(format!(
            "Codex Bundle 辅助文件大小或 sha256 校验失败: {}",
            item.manifest.session_id
        )));
    }
    if strict && item.manifest.version < 2 && item.manifest.has_history {
        report.error = Some("旧版 Bundle 未记录 history 哈希，strict 模式拒绝导入".into());
        return Ok(report);
    }
    let src_file = bundle_root.join("codex").join(&rel);
    let source_exists = crate::path_safety::validate_descendant(
        &bundle_root,
        &src_file,
        crate::path_safety::EntryKind::File,
        true,
        "Codex bundle rollout",
    )?;
    if !source_exists {
        report.error = Some(format!(
            "bundle rollout 缺失: {}",
            src_file.to_string_lossy()
        ));
        return Ok(report);
    }
    let meta = family::read_session_meta(&src_file).map_err(|error| {
        AppError::Other(format!(
            "Codex bundle rollout 缺少有效 session_meta {}: {error}",
            src_file.to_string_lossy()
        ))
    })?;
    let payload = meta.get("payload").unwrap_or(&Value::Null);
    let actual_id = payload.get("id").and_then(Value::as_str);
    if actual_id != Some(item.manifest.session_id.as_str()) {
        return Err(AppError::Other(format!(
            "Codex bundle rollout 内部 ID 不匹配: 期望 {}，实际 {}",
            item.manifest.session_id,
            actual_id.unwrap_or("未知")
        )));
    }
    if let Some(expected_provider) = item.manifest.model_provider.as_deref() {
        let actual_provider = payload.get("model_provider").and_then(Value::as_str);
        if actual_provider != Some(expected_provider) {
            return Err(AppError::Other(format!(
                "Codex bundle rollout provider 不匹配: 期望 {expected_provider}，实际 {}",
                actual_provider.unwrap_or("未知")
            )));
        }
    }

    // 2) 校验
    let actual = sha256_file(&src_file)?;
    let source_sha_verified = actual == item.manifest.sha256_rollout;
    if !source_sha_verified {
        report.sha_mismatch = true;
        if strict {
            report.error = Some("sha256 不一致，strict 模式跳过".into());
            return Ok(report);
        }
    } else {
        report.verified = artifacts_verified;
    }

    // 3) 目标路径决策
    let dest_abs = codex.join(&rel);
    crate::path_safety::validate_descendant(
        codex,
        &dest_abs,
        crate::path_safety::EntryKind::File,
        true,
        "Codex bundle 导入目标",
    )?;
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
                if let Some(reason) = keep_local_reason(&dest_abs, &src_file)? {
                    report.skipped_reason = Some(reason);
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
            ImportMode::Overwrite => {}
        }
    }

    // 4) 拷 rollout
    if let Some(parent) = dest_abs.parent() {
        fs::create_dir_all(parent)?;
    }
    copy_codex_rollout_with_cwd(
        &src_file,
        &dest_abs,
        mapped_cwd,
        source_sha_verified.then_some(item.manifest.sha256_rollout.as_str()),
    )?;
    report.rollout_written = true;

    // 5) 追加 history
    let hist_src = PathBuf::from(&item.bundle_dir).join("history.jsonl");
    if hist_src.is_file() {
        match append_history(codex, &hist_src, &item.manifest.session_id) {
            Ok(appended) => report.history_appended = appended,
            Err(error) => {
                report.error = Some(format!("rollout 已写入，但 history 追加失败: {error}"));
                return Ok(report);
            }
        }
    }

    // 6) 若需 make_visible，则 upsert threads + 追加 session_index
    if make_visible {
        if paths::state_db_path(codex).is_file() {
            let import_cwd = mapped_cwd.unwrap_or(item.manifest.session_cwd.as_str());
            if let Err(e) = upsert_threads_minimal(codex, &item.manifest, &dest_abs, import_cwd) {
                report.error = Some(format!("threads upsert 失败: {}", e));
                return Ok(report);
            } else {
                report.threads_upserted = true;
            }
        }
        match upsert_bundle_index_line(codex, &item.manifest) {
            Ok(appended) => report.index_appended = appended,
            Err(error) => {
                report.error = Some(format!(
                    "rollout{}已写入，但 session_index 更新失败: {error}",
                    if report.threads_upserted {
                        " 与 threads "
                    } else {
                        " "
                    }
                ));
                return Ok(report);
            }
        }
    }

    report.ok = report.error.is_none();
    Ok(report)
}

fn validate_codex_bundle_rollout_relpath(raw: &str, session_id: &str) -> AppResult<PathBuf> {
    let relative = paths::checked_relative_path(raw)?;
    let root = relative
        .components()
        .next()
        .and_then(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            _ => None,
        });
    if !matches!(root, Some("sessions" | "archived_sessions"))
        || relative.extension().and_then(|value| value.to_str()) != Some("jsonl")
    {
        return Err(AppError::Path(format!(
            "Codex bundle rollout 只能位于 sessions/ 或 archived_sessions/ 下: {raw}"
        )));
    }
    let expected_suffix = format!("-{session_id}.jsonl");
    if !relative
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.ends_with(&expected_suffix))
    {
        return Err(AppError::Path(format!(
            "Codex bundle rollout 文件名与会话 ID 不匹配: id={session_id} path={raw}"
        )));
    }
    Ok(relative)
}

fn append_history(codex: &Path, src: &Path, id: &str) -> AppResult<u32> {
    crate::history::append_from_file(&paths::history_path(codex), src, id)
}

fn upsert_bundle_index_line(codex: &Path, manifest: &BundleManifest) -> AppResult<bool> {
    let path = paths::session_index_path(codex);
    let expected = if path.is_file() {
        Some(atomic_file::fingerprint(&path)?)
    } else {
        None
    };
    let entry = serde_json::to_string(&serde_json::json!({
        "id": manifest.session_id,
        "thread_name": manifest.thread_name,
        "updated_at": unix_seconds_to_rfc3339(manifest.updated_at)?,
    }))?;
    let mut output = Vec::new();
    let mut existed = false;
    if path.is_file() {
        for line in BufReader::new(File::open(&path)?).lines() {
            let line = line?;
            let matches = serde_json::from_str::<Value>(&line)
                .ok()
                .is_some_and(|value| {
                    value.get("id").and_then(Value::as_str) == Some(manifest.session_id.as_str())
                        || value.get("session_id").and_then(Value::as_str)
                            == Some(manifest.session_id.as_str())
                });
            if matches {
                if !existed {
                    output.push(entry.clone());
                    existed = true;
                }
            } else if !line.trim().is_empty() {
                output.push(line);
            }
        }
    }
    if !existed {
        output.push(entry);
    }
    let write = |file: &mut File| -> AppResult<()> {
        for line in &output {
            writeln!(file, "{line}")?;
        }
        Ok(())
    };
    if let Some(expected) = expected.as_ref() {
        atomic_file::replace_with_writer_if_unchanged(&path, expected, write)?;
    } else {
        atomic_file::create_with_writer_if_absent(&path, write)?;
    }
    Ok(!existed)
}

fn replace_import_destination(
    dest: &Path,
    writer: impl FnOnce(&mut File) -> AppResult<()>,
) -> AppResult<()> {
    if dest.exists() && !dest.is_file() {
        return Err(AppError::Path(format!(
            "导入目标不是文件，拒绝覆盖: {}",
            dest.to_string_lossy()
        )));
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if dest.is_file() {
        let expected = atomic_file::fingerprint(dest)?;
        atomic_file::replace_with_writer_if_unchanged(dest, &expected, writer)
    } else {
        atomic_file::create_with_writer_if_absent(dest, writer)
    }
}

fn verify_streamed_sha(
    source: &Path,
    hasher: Sha256,
    expected_sha256: Option<&str>,
) -> AppResult<()> {
    let Some(expected) = expected_sha256 else {
        return Ok(());
    };
    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        return Err(AppError::Other(format!(
            "Bundle 源文件在导入期间发生变化，已拒绝提交: expected={expected} actual={actual} source={}",
            source.to_string_lossy()
        )));
    }
    Ok(())
}

fn copy_codex_rollout_with_cwd(
    src: &Path,
    dest: &Path,
    target_cwd: Option<&str>,
    expected_source_sha256: Option<&str>,
) -> AppResult<()> {
    let Some(target_cwd) = target_cwd else {
        return replace_import_destination(dest, |target| {
            let mut source = File::open(src)?;
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let read = source.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
                target.write_all(&buffer[..read])?;
            }
            verify_streamed_sha(src, hasher, expected_source_sha256)
        });
    };

    replace_import_destination(dest, |target| {
        let mut reader = BufReader::new(File::open(src)?);
        let mut writer = BufWriter::new(target);
        let mut hasher = Sha256::new();
        let mut line = String::new();
        let mut rewrote_meta = false;
        loop {
            line.clear();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                break;
            }
            hasher.update(line.as_bytes());
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
        verify_streamed_sha(src, hasher, expected_source_sha256)?;
        if !rewrote_meta {
            return Err(AppError::Other(format!(
                "无法重写 Codex 项目路径，rollout 没有有效 session_meta: {}",
                src.to_string_lossy()
            )));
        }
        writer.flush()?;
        Ok(())
    })
}

fn write_claude_jsonl_with_cwd(
    src: &Path,
    target: &mut File,
    target_cwd: Option<&str>,
    expected_source_sha256: Option<&str>,
) -> AppResult<()> {
    let Some(target_cwd) = target_cwd else {
        let mut source = File::open(src)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            target.write_all(&buffer[..read])?;
        }
        return verify_streamed_sha(src, hasher, expected_source_sha256);
    };

    let mut reader = BufReader::new(File::open(src)?);
    let mut writer = BufWriter::new(target);
    let mut hasher = Sha256::new();
    let mut raw_line = String::new();
    let mut line_no = 0usize;
    loop {
        raw_line.clear();
        let read = reader.read_line(&mut raw_line)?;
        if read == 0 {
            break;
        }
        line_no += 1;
        hasher.update(raw_line.as_bytes());
        let line = raw_line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            writeln!(writer)?;
            continue;
        }
        let mut value: Value = serde_json::from_str(line).map_err(|e| {
            AppError::Other(format!(
                "无法重写 Claude 项目路径，第 {} 行不是有效 JSON: {}: {}",
                line_no,
                src.to_string_lossy(),
                e
            ))
        })?;
        let obj = value.as_object_mut().ok_or_else(|| {
            AppError::Other(format!(
                "无法重写 Claude 项目路径，第 {} 行不是 JSON 对象: {}",
                line_no,
                src.to_string_lossy()
            ))
        })?;
        obj.insert("cwd".into(), Value::String(target_cwd.to_string()));
        writeln!(writer, "{}", serde_json::to_string(&value)?)?;
    }
    verify_streamed_sha(src, hasher, expected_source_sha256)?;
    writer.flush()?;
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
    // 新版 App 的会话列表要求 preview <> '' 才可见；旧版库没有这些列则跳过。
    let table_cols = crate::repair::threads_table_columns(&conn)?;
    if table_cols.iter().any(|name| name == "preview") {
        conn.execute(
            "UPDATE threads SET preview = ?2 WHERE id = ?1 AND preview = ''",
            params![m.session_id, m.thread_name],
        )?;
    }
    if table_cols.iter().any(|name| name == "thread_source") {
        conn.execute(
            "UPDATE threads SET thread_source = 'user' WHERE id = ?1 AND thread_source IS NULL",
            params![m.session_id],
        )?;
    }
    Ok(())
}

fn unix_seconds_to_rfc3339(ts: i64) -> AppResult<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.to_rfc3339())
        .ok_or_else(|| AppError::Other(format!("manifest updated_at 不是有效 Unix 秒时间戳: {ts}")))
}

fn copy_path_recursive(from: &Path, to: &Path) -> AppResult<()> {
    let metadata = match fs::symlink_metadata(from) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AppError::NotFound(format!(
                "待复制路径不存在: {}",
                from.to_string_lossy()
            )))
        }
        Err(error) => return Err(error.into()),
    };
    if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        return Err(AppError::Path(format!(
            "拒绝复制符号链接: {}",
            from.to_string_lossy()
        )));
    }
    if metadata.is_file() {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from, to)?;
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(AppError::Path(format!(
            "待复制路径不是普通文件或目录: {}",
            from.to_string_lossy()
        )));
    }
    fs::create_dir_all(to)?;
    copy_directory_contents(from, to)
}

fn copy_directory_contents(from: &Path, to: &Path) -> AppResult<()> {
    for entry in walkdir::WalkDir::new(from) {
        let entry = entry.map_err(|error| {
            AppError::Other(format!(
                "遍历待复制目录失败 {}: {error}",
                from.to_string_lossy()
            ))
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "待复制目录包含符号链接或 junction，已拒绝: {}",
                entry.path().to_string_lossy()
            )));
        }
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
        if metadata.is_dir() {
            fs::create_dir_all(&dest)?;
        } else if metadata.is_file() {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &dest)?;
        } else {
            return Err(AppError::Path(format!(
                "待复制目录包含不支持的路径类型: {}",
                entry.path().to_string_lossy()
            )));
        }
    }
    Ok(())
}

fn remove_path_recursive(path: &Path) -> AppResult<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() && !crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        fs::remove_dir_all(path)?;
    } else if metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        fs::remove_file(path)?;
    } else {
        return Err(AppError::Path(format!(
            "无法删除不支持的路径类型: {}",
            path.to_string_lossy()
        )));
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

    fn write_test_store_zip(path: &Path, entries: &[(&str, &[u8])]) -> AppResult<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        let mut central = Vec::with_capacity(entries.len());
        let mut offset = 0u32;
        for (name, data) in entries {
            let name_len = u16::try_from(name.len())
                .map_err(|_| AppError::Other(format!("测试 ZIP 文件名过长: {name}")))?;
            let size = u32::try_from(data.len())
                .map_err(|_| AppError::Other(format!("测试 ZIP payload 过大: {name}")))?;
            let crc = crc32(data);
            writer.write_all(&0x04034b50u32.to_le_bytes())?;
            writer.write_all(&20u16.to_le_bytes())?;
            writer.write_all(&0u16.to_le_bytes())?;
            writer.write_all(&0u16.to_le_bytes())?;
            writer.write_all(&0u16.to_le_bytes())?;
            writer.write_all(&0u16.to_le_bytes())?;
            writer.write_all(&crc.to_le_bytes())?;
            writer.write_all(&size.to_le_bytes())?;
            writer.write_all(&size.to_le_bytes())?;
            writer.write_all(&name_len.to_le_bytes())?;
            writer.write_all(&0u16.to_le_bytes())?;
            writer.write_all(name.as_bytes())?;
            writer.write_all(data)?;
            central.push(CentralEntry {
                name: (*name).to_string(),
                crc,
                size,
                offset,
            });
            offset = offset
                .checked_add(30 + name.len() as u32)
                .and_then(|value| value.checked_add(size))
                .ok_or_else(|| AppError::Other("测试 ZIP local offset 溢出".into()))?;
        }

        let central_offset = offset;
        let mut central_size = 0u32;
        for entry in &central {
            let name_len = u16::try_from(entry.name.len())
                .map_err(|_| AppError::Other("测试 ZIP central 文件名过长".into()))?;
            writer.write_all(&0x02014b50u32.to_le_bytes())?;
            writer.write_all(&20u16.to_le_bytes())?;
            writer.write_all(&20u16.to_le_bytes())?;
            writer.write_all(&0u16.to_le_bytes())?;
            writer.write_all(&0u16.to_le_bytes())?;
            writer.write_all(&0u16.to_le_bytes())?;
            writer.write_all(&0u16.to_le_bytes())?;
            writer.write_all(&entry.crc.to_le_bytes())?;
            writer.write_all(&entry.size.to_le_bytes())?;
            writer.write_all(&entry.size.to_le_bytes())?;
            writer.write_all(&name_len.to_le_bytes())?;
            writer.write_all(&0u16.to_le_bytes())?;
            writer.write_all(&0u16.to_le_bytes())?;
            writer.write_all(&0u16.to_le_bytes())?;
            writer.write_all(&0u16.to_le_bytes())?;
            writer.write_all(&0u32.to_le_bytes())?;
            writer.write_all(&entry.offset.to_le_bytes())?;
            writer.write_all(entry.name.as_bytes())?;
            central_size = central_size
                .checked_add(46 + entry.name.len() as u32)
                .ok_or_else(|| AppError::Other("测试 ZIP central size 溢出".into()))?;
        }

        let count = u16::try_from(central.len())
            .map_err(|_| AppError::Other("测试 ZIP 条目过多".into()))?;
        writer.write_all(&0x06054b50u32.to_le_bytes())?;
        writer.write_all(&0u16.to_le_bytes())?;
        writer.write_all(&0u16.to_le_bytes())?;
        writer.write_all(&count.to_le_bytes())?;
        writer.write_all(&count.to_le_bytes())?;
        writer.write_all(&central_size.to_le_bytes())?;
        writer.write_all(&central_offset.to_le_bytes())?;
        writer.write_all(&0u16.to_le_bytes())?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        Ok(())
    }

    fn zip_unpack_artifacts(parent: &Path) -> AppResult<Vec<PathBuf>> {
        if !parent.is_dir() {
            return Ok(Vec::new());
        }
        Ok(fs::read_dir(parent)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".ccsm-unpack-")
            })
            .map(|entry| entry.path())
            .collect())
    }

    fn claude_test_transcript(claude: &Path, id: &str) -> PathBuf {
        claude
            .join("projects")
            .join("sample-project")
            .join(format!("{id}.jsonl"))
    }

    fn write_claude_session(claude: &Path, id: &str) -> AppResult<()> {
        let transcript = claude_test_transcript(claude, id);
        let dir = transcript.parent().ok_or_else(|| {
            AppError::Path(format!(
                "测试 Claude transcript 缺少父目录: {}",
                transcript.to_string_lossy()
            ))
        })?;
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
        fs::write(transcript, format!("{}\n", serde_json::to_string(&line)?))?;
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

    fn write_claude_session_in_project(
        claude: &Path,
        project: &str,
        id: &str,
        content: &str,
    ) -> AppResult<PathBuf> {
        let transcript = claude
            .join("projects")
            .join(project)
            .join(format!("{id}.jsonl"));
        fs::create_dir_all(transcript.parent().unwrap())?;
        let line = serde_json::json!({
            "sessionId": id,
            "cwd": format!("F:\\work\\{project}"),
            "timestamp": "2026-04-20T10:00:00Z",
            "type": "assistant",
            "message": {
                "role": "assistant",
                "model": "claude-3-5-sonnet",
                "content": content
            }
        });
        fs::write(&transcript, format!("{}\n", serde_json::to_string(&line)?))?;
        Ok(transcript)
    }

    #[cfg(windows)]
    fn create_test_directory_link(target: &Path, link: &Path) -> AppResult<()> {
        if let Err(error) = std::os::windows::fs::symlink_dir(target, link) {
            if error.raw_os_error() != Some(1314) {
                return Err(error.into());
            }
            let output = std::process::Command::new("pwsh")
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-Command",
                    "$ErrorActionPreference = 'Stop'; New-Item -ItemType Junction -Path $env:CC_TEST_LINK -Target $env:CC_TEST_TARGET | Out-Null",
                ])
                .env("CC_TEST_LINK", link)
                .env("CC_TEST_TARGET", target)
                .output()?;
            if !output.status.success() {
                return Err(AppError::Other(format!(
                    "创建测试 junction 失败: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn create_test_directory_link(target: &Path, link: &Path) -> AppResult<()> {
        std::os::unix::fs::symlink(target, link)?;
        Ok(())
    }

    #[cfg(windows)]
    fn remove_test_directory_link(link: &Path) {
        fs::remove_dir(link).ok();
    }

    #[cfg(unix)]
    fn remove_test_directory_link(link: &Path) {
        fs::remove_file(link).ok();
    }

    fn write_test_sidecar(
        claude: &Path,
        id: &str,
        name: &str,
        content: &str,
    ) -> AppResult<PathBuf> {
        let transcript = claude_test_transcript(claude, id);
        let sidecar = crate::claude_sessions::sidecar_path_for(&transcript).ok_or_else(|| {
            AppError::Path(format!(
                "测试 Claude transcript 缺少 sidecar 路径: {}",
                transcript.to_string_lossy()
            ))
        })?;
        fs::create_dir_all(&sidecar)?;
        fs::write(sidecar.join(name), content)?;
        Ok(sidecar)
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

    fn write_codex_rollout_only(root: &Path, id: &str, marker: &str) -> AppResult<PathBuf> {
        fs::create_dir_all(root)?;
        let path = root.join(format!("rollout-{id}.jsonl"));
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
            "payload": {"type": "user_message", "message": marker}
        });
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&meta)?,
                serde_json::to_string(&event)?
            ),
        )?;
        Ok(path)
    }

    #[test]
    fn repeated_exports_publish_distinct_complete_batches() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-unique-export-batch-test");
        let codex = root.join("codex");
        let out = root.join("bundles");
        let id = "unique-export-session";
        let rollout = write_codex_session(&codex, id, 1_747_050_000)?;

        let first = export_session_bundles(
            Some(PROVIDER_CODEX.to_string()),
            codex.to_string_lossy().into_owned(),
            None,
            out.to_string_lossy().into_owned(),
            vec![id.to_string()],
            None,
            Some("test-machine".to_string()),
            Some("default".to_string()),
        )?;
        let first_bundle = PathBuf::from(first[0].bundle_path.as_deref().unwrap());
        let first_manifest: BundleManifest =
            serde_json::from_str(&fs::read_to_string(first_bundle.join("manifest.json"))?)?;
        let first_payload = first_bundle
            .join(PROVIDER_CODEX)
            .join(paths::checked_relative_path(
                &first_manifest.rollout_relpath,
            )?);
        let first_bytes = fs::read(&first_payload)?;

        let mut updated = fs::read_to_string(&rollout)?;
        updated.push_str(
            "{\"timestamp\":\"2026-05-12T10:01:00Z\",\"type\":\"event_msg\",\"payload\":{\"message\":\"second-export\"}}\n",
        );
        fs::write(&rollout, updated)?;
        let second = export_session_bundles(
            Some(PROVIDER_CODEX.to_string()),
            codex.to_string_lossy().into_owned(),
            None,
            out.to_string_lossy().into_owned(),
            vec![id.to_string()],
            None,
            Some("test-machine".to_string()),
            Some("default".to_string()),
        )?;
        let second_bundle = PathBuf::from(second[0].bundle_path.as_deref().unwrap());
        assert_ne!(first_bundle.parent(), second_bundle.parent());
        assert_eq!(fs::read(&first_payload)?, first_bytes);
        let second_manifest: BundleManifest =
            serde_json::from_str(&fs::read_to_string(second_bundle.join("manifest.json"))?)?;
        let second_payload = second_bundle
            .join(PROVIDER_CODEX)
            .join(paths::checked_relative_path(
                &second_manifest.rollout_relpath,
            )?);
        assert!(fs::read_to_string(second_payload)?.contains("second-export"));
        let mut partials = 0usize;
        for entry in walkdir::WalkDir::new(&out) {
            let entry = entry.map_err(|error| {
                AppError::Other(format!("遍历测试导出目录失败 {}: {error}", out.display()))
            })?;
            if entry.file_name().to_string_lossy().ends_with(".partial") {
                partials += 1;
            }
        }
        assert_eq!(partials, 0);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn active_only_exports_family_heads_and_unregistered_active_rollouts() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-active-only-union-test");
        let codex = root.join("codex");
        let out = root.join("bundles");
        let active_id = "family-active";
        let unregistered_id = "unregistered-active";
        let archived_id = "unregistered-archived";
        let active_path = write_codex_session(&codex, active_id, 1_747_050_000)?;
        write_codex_rollout_only(
            &codex.join("sessions").join("2026").join("05").join("13"),
            unregistered_id,
            "unregistered",
        )?;
        write_codex_rollout_only(&codex.join("archived_sessions"), archived_id, "archived")?;

        let mut store = family::load(&codex)?;
        let active_rel = active_path.strip_prefix(&codex).map_err(|error| {
            AppError::Path(format!("测试 active rollout 相对路径失败: {error}"))
        })?;
        family::ensure_family_for(
            &mut store,
            active_id,
            "openai",
            &active_rel.to_string_lossy(),
            "family active",
        );
        family::save(&codex, &store)?;

        let reports = export_all_bundles(
            Some(PROVIDER_CODEX.to_string()),
            codex.to_string_lossy().into_owned(),
            None,
            out.to_string_lossy().into_owned(),
            Some("test-machine".to_string()),
            Some("default".to_string()),
            true,
        )?;
        assert!(reports.iter().all(|report| report.ok));
        let ids = reports
            .iter()
            .map(|report| report.session_id.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(ids, HashSet::from([active_id, unregistered_id]));
        assert!(!ids.contains(archived_id));

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn rollout_index_rejects_linked_roots_without_reading_external_sessions() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-bundle-linked-rollout-root-test");
        let codex = root.join("codex");
        let external = root.join("external-sessions");
        fs::create_dir_all(&codex)?;
        let external_rollout = write_codex_rollout_only(&external, "external-id", "sentinel")?;
        create_test_directory_link(&external, &codex.join("sessions"))?;

        let error = match index_rollouts(&codex) {
            Err(error) => error,
            Ok(_) => panic!("linked rollout root must be rejected"),
        };
        assert!(error.to_string().contains("junction") || error.to_string().contains("链接"));
        assert!(external_rollout.is_file());

        remove_test_directory_link(&codex.join("sessions"));
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn keep_local_compares_logical_timestamps_instead_of_file_mtime() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-keep-local-time-test");
        fs::create_dir_all(&root)?;
        let local = root.join("local.jsonl");
        fs::write(
            &local,
            concat!(
                "{\"timestamp\":\"2026-05-12T10:00:00Z\",\"type\":\"session_meta\"}\n",
                "{\"timestamp\":\"2026-05-12T11:00:00Z\",\"type\":\"event_msg\"}\n"
            ),
        )?;
        let older_bundle = root.join("older-bundle.jsonl");
        let newer_bundle = root.join("newer-bundle.jsonl");
        fs::write(
            &older_bundle,
            "{\"timestamp\":\"2026-05-12T10:30:00Z\",\"type\":\"event_msg\"}\n",
        )?;
        fs::write(
            &newer_bundle,
            "{\"timestamp\":\"2026-05-12T11:30:00Z\",\"type\":\"event_msg\"}\n",
        )?;

        assert!(keep_local_reason(&local, &older_bundle)?.is_some());
        assert!(keep_local_reason(&local, &newer_bundle)?.is_none());

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn keep_local_preserves_existing_rollout_when_timestamps_are_unknown() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-keep-local-unknown-time-test");
        fs::create_dir_all(&root)?;
        let local = root.join("local.jsonl");
        let bundle = root.join("bundle.jsonl");
        fs::write(&local, "{\"type\":\"session_meta\"}\n")?;
        fs::write(
            &bundle,
            "{\"timestamp\":\"2026-05-12T11:30:00Z\",\"type\":\"event_msg\"}\n",
        )?;

        assert!(keep_local_reason(&local, &bundle)?.is_some());
        fs::write(
            &local,
            "{\"timestamp\":\"2026-05-12T11:00:00Z\",\"type\":\"event_msg\"}\n",
        )?;
        fs::write(&bundle, "{\"type\":\"session_meta\"}\n")?;
        assert!(keep_local_reason(&local, &bundle)?.is_some());

        fs::remove_dir_all(root).ok();
        Ok(())
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
    fn keep_local_remote_winner_and_overwrite_replace_claude_sidecar() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-claude-sidecar-replace-test");
        let source_claude = root.join("source-claude");
        let import_claude = root.join("import-claude");
        let bundle_dir = root.join("bundles");
        let id = "claude-sidecar-replace";
        write_claude_session(&source_claude, id)?;
        write_test_sidecar(&source_claude, id, "remote.txt", "remote snapshot")?;

        let exported = export_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            String::new(),
            Some(source_claude.to_string_lossy().into_owned()),
            bundle_dir.to_string_lossy().into_owned(),
            vec![id.to_string()],
            None,
            Some("test-machine".to_string()),
            Some("default".to_string()),
        )?;
        assert_eq!(exported.len(), 1);
        assert!(exported[0].ok);

        write_claude_session(&import_claude, id)?;
        let local_transcript = claude_test_transcript(&import_claude, id);
        let local_raw = fs::read_to_string(&local_transcript)?
            .replace("2026-04-20T10:00:00Z", "2026-04-20T09:00:00Z")
            .replace("hello", "local old transcript");
        fs::write(&local_transcript, local_raw)?;
        let local_sidecar = write_test_sidecar(&import_claude, id, "old.txt", "old snapshot")?;

        let keep_local = import_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            bundle_dir.to_string_lossy().into_owned(),
            String::new(),
            Some(import_claude.to_string_lossy().into_owned()),
            ImportMode::KeepLocal,
            false,
            true,
            vec![],
        )?;
        assert_eq!(keep_local.len(), 1);
        assert!(keep_local[0].ok);
        assert!(keep_local[0].rollout_written);
        assert_eq!(
            fs::read_to_string(local_sidecar.join("remote.txt"))?,
            "remote snapshot"
        );
        assert!(!local_sidecar.join("old.txt").exists());
        assert!(fs::read_to_string(&local_transcript)?.contains("hello"));

        fs::write(
            &local_transcript,
            fs::read_to_string(&local_transcript)?.replace("hello", "overwrite old transcript"),
        )?;
        remove_path_recursive(&local_sidecar)?;
        write_test_sidecar(&import_claude, id, "old.txt", "overwrite old snapshot")?;
        let overwrite = import_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            bundle_dir.to_string_lossy().into_owned(),
            String::new(),
            Some(import_claude.to_string_lossy().into_owned()),
            ImportMode::Overwrite,
            false,
            true,
            vec![],
        )?;
        assert_eq!(overwrite.len(), 1);
        assert!(overwrite[0].ok);
        assert!(overwrite[0].rollout_written);
        assert_eq!(
            fs::read_to_string(local_sidecar.join("remote.txt"))?,
            "remote snapshot"
        );
        assert!(!local_sidecar.join("old.txt").exists());
        assert!(fs::read_to_string(&local_transcript)?.contains("hello"));

        let snapshot_artifacts = fs::read_dir(local_transcript.parent().unwrap())?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("ccsm-snapshot")
            })
            .count();
        assert_eq!(snapshot_artifacts, 0);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn overwrite_removes_local_claude_sidecar_when_bundle_has_none() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-claude-sidecar-remove-test");
        let source_claude = root.join("source-claude");
        let import_claude = root.join("import-claude");
        let bundle_dir = root.join("bundles");
        let id = "claude-sidecar-remove";
        write_claude_session(&source_claude, id)?;
        let exported = export_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            String::new(),
            Some(source_claude.to_string_lossy().into_owned()),
            bundle_dir.to_string_lossy().into_owned(),
            vec![id.to_string()],
            None,
            Some("test-machine".to_string()),
            Some("default".to_string()),
        )?;
        assert!(exported[0].ok);

        write_claude_session(&import_claude, id)?;
        let local_sidecar = write_test_sidecar(&import_claude, id, "old.txt", "old snapshot")?;
        let imported = import_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            bundle_dir.to_string_lossy().into_owned(),
            String::new(),
            Some(import_claude.to_string_lossy().into_owned()),
            ImportMode::Overwrite,
            false,
            true,
            vec![],
        )?;
        assert_eq!(imported.len(), 1);
        assert!(imported[0].ok);
        assert!(imported[0].rollout_written);
        assert!(!local_sidecar.exists());

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn missing_declared_sidecar_keeps_existing_claude_snapshot_unchanged() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-claude-sidecar-missing-test");
        let source_claude = root.join("source-claude");
        let import_claude = root.join("import-claude");
        let bundle_dir = root.join("bundles");
        let id = "claude-sidecar-missing";
        write_claude_session(&source_claude, id)?;
        write_test_sidecar(&source_claude, id, "remote.txt", "remote snapshot")?;
        let exported = export_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            String::new(),
            Some(source_claude.to_string_lossy().into_owned()),
            bundle_dir.to_string_lossy().into_owned(),
            vec![id.to_string()],
            None,
            Some("test-machine".to_string()),
            Some("default".to_string()),
        )?;
        assert!(exported[0].ok);
        let bundle_path = PathBuf::from(exported[0].bundle_path.as_ref().unwrap());
        remove_path_recursive(&bundle_path.join("sidecars").join(paths::sanitize_slug(id)))?;

        write_claude_session(&import_claude, id)?;
        let local_transcript = claude_test_transcript(&import_claude, id);
        let local_before = fs::read(&local_transcript)?;
        let local_sidecar = write_test_sidecar(&import_claude, id, "old.txt", "old snapshot")?;
        let imported = import_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            bundle_dir.to_string_lossy().into_owned(),
            String::new(),
            Some(import_claude.to_string_lossy().into_owned()),
            ImportMode::Overwrite,
            false,
            true,
            vec![],
        )?;
        assert_eq!(imported.len(), 1);
        assert!(!imported[0].ok);
        assert!(imported[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("sidecar"));
        assert_eq!(fs::read(&local_transcript)?, local_before);
        assert_eq!(
            fs::read_to_string(local_sidecar.join("old.txt"))?,
            "old snapshot"
        );

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn claude_snapshot_commit_rolls_back_sidecar_when_transcript_install_fails() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-claude-snapshot-rollback-test");
        fs::create_dir_all(&root)?;
        let transcript = root.join("session.jsonl");
        let sidecar = root.join("session");
        let staged_transcript = root.join("session.transcript.stage");
        let staged_sidecar = root.join("session.sidecar.stage");
        fs::write(&transcript, "old transcript\n")?;
        fs::create_dir_all(&sidecar)?;
        fs::write(sidecar.join("old.txt"), "old sidecar")?;
        fs::write(&staged_transcript, "new transcript\n")?;
        fs::create_dir_all(&staged_sidecar)?;
        fs::write(staged_sidecar.join("new.txt"), "new sidecar")?;
        let expected = atomic_file::fingerprint(&transcript)?;

        let error = commit_staged_claude_snapshot_with_hook(
            &transcript,
            &sidecar,
            &staged_transcript,
            Some(&staged_sidecar),
            Some(&expected),
            || Err(AppError::Other("模拟 transcript 提交失败".into())),
        )
        .expect_err("transcript commit failure must abort the snapshot replacement");
        assert!(error.to_string().contains("模拟 transcript 提交失败"));
        assert_eq!(fs::read_to_string(&transcript)?, "old transcript\n");
        assert_eq!(fs::read_to_string(sidecar.join("old.txt"))?, "old sidecar");
        assert!(!sidecar.join("new.txt").exists());
        assert!(staged_transcript.is_file());
        assert!(!staged_sidecar.exists());
        let backup_artifacts = fs::read_dir(&root)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("ccsm-snapshot")
            })
            .count();
        assert_eq!(backup_artifacts, 0);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn claude_snapshot_does_not_park_a_concurrently_created_transcript() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-claude-snapshot-create-race-test");
        fs::create_dir_all(&root)?;
        let transcript = root.join("session.jsonl");
        let sidecar = root.join("session");
        let staged_transcript = root.join("session.transcript.stage");
        let staged_sidecar = root.join("session.sidecar.stage");
        fs::create_dir_all(&sidecar)?;
        fs::write(sidecar.join("old.txt"), "old sidecar")?;
        fs::write(&staged_transcript, "bundle transcript\n")?;
        fs::create_dir_all(&staged_sidecar)?;
        fs::write(staged_sidecar.join("new.txt"), "bundle sidecar")?;

        let error = commit_staged_claude_snapshot_with_hook(
            &transcript,
            &sidecar,
            &staged_transcript,
            Some(&staged_sidecar),
            None,
            || {
                fs::write(&transcript, "concurrent transcript\n")?;
                Ok(())
            },
        )
        .expect_err("a concurrently created transcript must abort snapshot replacement");
        assert!(error.to_string().contains("交换目标已存在"));
        assert_eq!(fs::read_to_string(&transcript)?, "concurrent transcript\n");
        assert_eq!(fs::read_to_string(sidecar.join("old.txt"))?, "old sidecar");
        assert!(!sidecar.join("new.txt").exists());

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn exports_duplicate_claude_ids_by_exact_rollout_without_collision() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-claude-duplicate-export-test");
        let source_claude = root.join("source-claude");
        let bundle_dir = root.join("bundles");
        let id = "duplicate-claude-id";
        let first = write_claude_session_in_project(&source_claude, "project-one", id, "first")?;
        let second = write_claude_session_in_project(&source_claude, "project-two", id, "second")?;

        let ambiguous = export_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            String::new(),
            Some(source_claude.to_string_lossy().into_owned()),
            bundle_dir.to_string_lossy().into_owned(),
            vec![id.to_string()],
            None,
            Some("test-machine".to_string()),
            Some("default".to_string()),
        )?;
        assert_eq!(ambiguous.len(), 1);
        assert!(!ambiguous[0].ok);
        assert!(ambiguous[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("必须提供精确 rollout_path"));

        let reports = export_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            String::new(),
            Some(source_claude.to_string_lossy().into_owned()),
            bundle_dir.to_string_lossy().into_owned(),
            vec![id.to_string(), id.to_string()],
            Some(vec![
                BundleExportTarget {
                    id: id.to_string(),
                    rollout_path: Some(first.to_string_lossy().into_owned()),
                },
                BundleExportTarget {
                    id: id.to_string(),
                    rollout_path: Some(second.to_string_lossy().into_owned()),
                },
            ]),
            Some("test-machine".to_string()),
            Some("default".to_string()),
        )?;
        assert_eq!(reports.len(), 2);
        assert!(reports.iter().all(|report| report.ok));
        assert_ne!(reports[0].bundle_path, reports[1].bundle_path);

        for (report, expected_content) in reports.iter().zip(["first", "second"]) {
            let bundle = PathBuf::from(report.bundle_path.as_deref().unwrap());
            let manifest: BundleManifest =
                serde_json::from_str(&fs::read_to_string(bundle.join("manifest.json"))?)?;
            let transcript = bundle
                .join(PROVIDER_CLAUDE)
                .join(paths::checked_relative_path(&manifest.rollout_relpath)?);
            assert!(fs::read_to_string(transcript)?.contains(expected_content));
        }

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn claude_bundle_import_rejects_escape_and_mismatched_session_identity() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-claude-malicious-import-test");
        let source_claude = root.join("source-claude");
        let import_claude = root.join("import-claude");
        let bundle_dir = root.join("bundles");
        let id = "safe-claude-id";
        write_claude_session(&source_claude, id)?;
        let reports = export_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            String::new(),
            Some(source_claude.to_string_lossy().into_owned()),
            bundle_dir.to_string_lossy().into_owned(),
            vec![id.to_string()],
            None,
            Some("test-machine".to_string()),
            Some("default".to_string()),
        )?;
        let exported = PathBuf::from(reports[0].bundle_path.as_deref().unwrap());
        let manifest_path = exported.join("manifest.json");
        let original_manifest: BundleManifest =
            serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;

        fs::create_dir_all(&import_claude)?;
        let config = import_claude.join("config.json");
        fs::write(&config, "do not overwrite")?;
        let mut escaping_manifest = original_manifest.clone();
        escaping_manifest.source_relpath = Some(format!("../{id}.jsonl"));
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&escaping_manifest)?,
        )?;
        let escaped = import_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            bundle_dir.to_string_lossy().into_owned(),
            String::new(),
            Some(import_claude.to_string_lossy().into_owned()),
            ImportMode::Overwrite,
            false,
            true,
            vec![],
        )?;
        assert_eq!(escaped.len(), 1);
        assert!(!escaped[0].ok);
        assert_eq!(fs::read_to_string(&config)?, "do not overwrite");

        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&original_manifest)?,
        )?;
        let transcript = exported
            .join(PROVIDER_CLAUDE)
            .join(paths::checked_relative_path(
                &original_manifest.rollout_relpath,
            )?);
        let mismatched = fs::read_to_string(&transcript)?.replace(id, "attacker-session");
        fs::write(&transcript, mismatched)?;
        let mut mismatched_manifest = original_manifest;
        mismatched_manifest.sha256_rollout = sha256_file(&transcript)?;
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&mismatched_manifest)?,
        )?;
        let identity = import_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            bundle_dir.to_string_lossy().into_owned(),
            String::new(),
            Some(import_claude.to_string_lossy().into_owned()),
            ImportMode::Overwrite,
            false,
            true,
            vec![],
        )?;
        assert_eq!(identity.len(), 1);
        assert!(!identity[0].ok);
        assert!(identity[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("sessionId 不匹配"));
        assert!(!paths::claude_projects_dir(&import_claude)
            .join("sample-project")
            .join(format!("{id}.jsonl"))
            .exists());
        assert_eq!(fs::read_to_string(&config)?, "do not overwrite");

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn claude_bundle_import_rejects_linked_projects_root() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-claude-linked-projects-test");
        let source_claude = root.join("source-claude");
        let import_claude = root.join("import-claude");
        let external = root.join("external-projects");
        let bundle_dir = root.join("bundles");
        let id = "linked-projects-session";
        write_claude_session(&source_claude, id)?;
        let reports = export_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            String::new(),
            Some(source_claude.to_string_lossy().into_owned()),
            bundle_dir.to_string_lossy().into_owned(),
            vec![id.to_string()],
            None,
            Some("test-machine".to_string()),
            Some("default".to_string()),
        )?;
        assert_eq!(reports.len(), 1);
        assert!(reports[0].ok);
        fs::create_dir_all(&import_claude)?;
        fs::create_dir_all(&external)?;
        let sentinel = external.join("sentinel.txt");
        fs::write(&sentinel, "external must stay untouched")?;
        let projects_link = paths::claude_projects_dir(&import_claude);
        create_test_directory_link(&external, &projects_link)?;

        let imported = import_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            bundle_dir.to_string_lossy().into_owned(),
            String::new(),
            Some(import_claude.to_string_lossy().into_owned()),
            ImportMode::Overwrite,
            false,
            true,
            vec![],
        )?;
        assert_eq!(imported.len(), 1);
        assert!(!imported[0].ok);
        assert_eq!(
            fs::read_to_string(&sentinel)?,
            "external must stay untouched"
        );
        assert_eq!(fs::read_dir(&external)?.count(), 1);

        remove_test_directory_link(&projects_link);
        fs::remove_dir_all(root).ok();
        Ok(())
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
            None,
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

        fs::write(bundle_path.join("history.jsonl"), "tampered history\n")?;
        let corrupted = verify_bundles(
            bundle_dir.to_string_lossy().into_owned(),
            Some(PROVIDER_CLAUDE.to_string()),
        )?;
        assert_eq!(corrupted[0].verified, Some(false));
        let rejected = import_session_bundles(
            Some(PROVIDER_CLAUDE.to_string()),
            bundle_dir.to_string_lossy().into_owned(),
            String::new(),
            Some(import_claude.to_string_lossy().into_owned()),
            ImportMode::Overwrite,
            false,
            true,
            vec![],
        )?;
        assert!(!rejected[0].ok);
        assert!(!rejected[0].rollout_written);
        assert!(rejected[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("辅助文件"));

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
            None,
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
        fs::create_dir(&unpacked)?;
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

    #[test]
    fn pack_rejects_linked_source_and_preserves_existing_zip() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-pack-linked-source-test");
        let external = root.join("external-bundle");
        let linked_source = root.join("linked-bundle");
        let zip_path = root.join("existing.zip");
        fs::create_dir_all(&external)?;
        fs::write(external.join("manifest.json"), "external payload")?;
        create_test_directory_link(&external, &linked_source)?;
        fs::write(&zip_path, "existing zip must remain unchanged")?;

        let error = pack_bundles_zip(
            linked_source.to_string_lossy().into_owned(),
            zip_path.to_string_lossy().into_owned(),
        )
        .expect_err("linked ZIP source root must be rejected");
        assert!(error.to_string().contains("junction") || error.to_string().contains("链接"));
        assert_eq!(
            fs::read_to_string(&zip_path)?,
            "existing zip must remain unchanged"
        );
        let leftovers = fs::read_dir(&root)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("ccsm-pack.tmp")
            })
            .count();
        assert_eq!(leftovers, 0);

        remove_test_directory_link(&linked_source);
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn pack_atomically_replaces_an_existing_regular_zip() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-pack-atomic-replace-test");
        let source = root.join("bundle");
        let zip_path = root.join("bundle.zip");
        fs::create_dir_all(&source)?;
        fs::write(source.join("manifest.json"), "new payload")?;
        fs::write(&zip_path, "old archive")?;

        let report = pack_bundles_zip(
            source.to_string_lossy().into_owned(),
            zip_path.to_string_lossy().into_owned(),
        )?;
        assert_eq!(report.files, 1);
        let unpacked = root.join("unpacked-replacement");
        unpack_zip(
            zip_path.to_string_lossy().into_owned(),
            unpacked.to_string_lossy().into_owned(),
        )?;
        assert_eq!(
            fs::read_to_string(unpacked.join("manifest.json"))?,
            "new payload"
        );
        let leftovers = fs::read_dir(&root)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("ccsm-pack.tmp")
            })
            .count();
        assert_eq!(leftovers, 0);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn unpack_zip_rejects_corrupt_crc_and_preserves_existing_empty_target() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-unpack-crc-test");
        fs::create_dir_all(&root)?;
        let zip = root.join("corrupt.zip");
        let destination = root.join("destination");
        fs::create_dir(&destination)?;
        write_test_store_zip(&zip, &[("payload.txt", b"valid payload")])?;

        let mut bytes = fs::read(&zip)?;
        let payload_offset = 30 + "payload.txt".len();
        bytes[payload_offset] ^= 0x5a;
        fs::write(&zip, bytes)?;

        let error = unpack_zip(
            zip.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
        )
        .expect_err("损坏 CRC 的 ZIP 必须被拒绝");
        assert!(error.to_string().contains("CRC 校验失败"));
        assert!(destination.is_dir());
        assert_eq!(fs::read_dir(&destination)?.count(), 0);
        assert!(zip_unpack_artifacts(&root)?.is_empty());

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn unpack_zip_rejects_duplicate_and_escape_paths_before_creating_target() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-unpack-path-test");
        fs::create_dir_all(&root)?;
        let duplicate_zip = root.join("duplicate.zip");
        let duplicate_destination = root.join("duplicate-destination");
        write_test_store_zip(
            &duplicate_zip,
            &[("same.txt", b"first"), ("same.txt", b"second")],
        )?;
        let duplicate_error = unpack_zip(
            duplicate_zip.to_string_lossy().into_owned(),
            duplicate_destination.to_string_lossy().into_owned(),
        )
        .expect_err("重复 ZIP 路径必须被拒绝");
        assert!(duplicate_error.to_string().contains("重复或等价路径"));
        assert!(!duplicate_destination.exists());

        let escape_zip = root.join("escape.zip");
        let escape_destination = root.join("escape-destination");
        let escaped_output = root.join("escaped.txt");
        write_test_store_zip(&escape_zip, &[("../escaped.txt", b"escape")])?;
        let escape_error = unpack_zip(
            escape_zip.to_string_lossy().into_owned(),
            escape_destination.to_string_lossy().into_owned(),
        )
        .expect_err("目录穿越 ZIP 路径必须被拒绝");
        assert!(escape_error.to_string().contains("目录穿越"));
        assert!(!escape_destination.exists());
        assert!(!escaped_output.exists());
        assert!(zip_unpack_artifacts(&root)?.is_empty());

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn unpack_zip_rejects_nonempty_target_without_changing_it() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-unpack-existing-target-test");
        fs::create_dir_all(&root)?;
        let zip = root.join("valid.zip");
        let destination = root.join("destination");
        fs::create_dir(&destination)?;
        let sentinel = destination.join("sentinel.txt");
        fs::write(&sentinel, "must remain unchanged")?;
        write_test_store_zip(&zip, &[("payload.txt", b"new payload")])?;

        let error = unpack_zip(
            zip.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
        )
        .expect_err("非空目标必须被拒绝");
        assert!(error.to_string().contains("已存在且非空"));
        assert_eq!(fs::read_to_string(&sentinel)?, "must remain unchanged");
        assert!(!destination.join("payload.txt").exists());
        assert!(zip_unpack_artifacts(&root)?.is_empty());

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn unpack_zip_rejects_linked_destination_parent() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-unpack-linked-parent-test");
        let external = root.join("external");
        let linked_parent = root.join("linked-parent");
        fs::create_dir_all(&external)?;
        let sentinel = external.join("sentinel.txt");
        fs::write(&sentinel, "external must remain unchanged")?;
        create_test_directory_link(&external, &linked_parent)?;
        let zip = root.join("valid.zip");
        write_test_store_zip(&zip, &[("payload.txt", b"payload")])?;

        let destination = linked_parent.join("destination");
        let error = unpack_zip(
            zip.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
        )
        .expect_err("链接父目录必须被拒绝");
        assert!(error.to_string().contains("junction") || error.to_string().contains("符号链接"));
        assert_eq!(
            fs::read_to_string(&sentinel)?,
            "external must remain unchanged"
        );
        assert!(!external.join("destination").exists());

        remove_test_directory_link(&linked_parent);
        fs::remove_dir_all(root).ok();
        Ok(())
    }
}

// ========================= zip 打包 =========================

pub fn pack_bundles_zip(src_dir: String, zip_path: String) -> AppResult<ZipReport> {
    let src = absolute_lexical_path(Path::new(&src_dir), "ZIP 打包源")?;
    validate_existing_directory_chain(&src, "ZIP 打包源父链")?;
    validate_plain_directory_tree(&src, "ZIP 打包源")?;

    let out = absolute_lexical_path(Path::new(&zip_path), "ZIP 输出")?;
    let parent = out
        .parent()
        .ok_or_else(|| AppError::Path(format!("ZIP 输出缺少父目录: {}", out.to_string_lossy())))?;
    ensure_plain_directory_path(parent, "ZIP 输出父目录")?;
    validate_existing_directory_chain(parent, "ZIP 输出父目录")?;
    match fs::symlink_metadata(&out) {
        Ok(metadata)
            if metadata.is_file()
                && !crate::path_safety::metadata_is_link_or_reparse(&metadata) => {}
        Ok(_) => {
            return Err(AppError::Path(format!(
                "ZIP 输出已存在但不是普通文件，或属于链接/reparse point: {}",
                out.to_string_lossy()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let src_canon = fs::canonicalize(&src)?;
    let out_parent_canon = fs::canonicalize(parent)?;
    if out_parent_canon == src_canon || out_parent_canon.starts_with(&src_canon) {
        return Err(AppError::Path(format!(
            "zip 输出路径不能位于被打包目录内部: 输出 {}, 源目录 {}",
            out.to_string_lossy(),
            src.to_string_lossy()
        )));
    }

    let (stage_path, stage_file) = create_unique_zip_pack_stage(parent, &out)?;
    let write_result = write_store_zip(&src, stage_file);
    let (file_count, total_bytes) = match write_result {
        Ok(report) => report,
        Err(error) => {
            return Err(match fs::remove_file(&stage_path) {
                Ok(()) => error,
                Err(cleanup_error) => AppError::Other(format!(
                    "{error}; 清理 ZIP 打包暂存文件失败 {}: {cleanup_error}",
                    stage_path.to_string_lossy()
                )),
            })
        }
    };
    if let Err(error) = publish_packed_zip(&stage_path, &out) {
        return Err(match fs::remove_file(&stage_path) {
            Ok(()) => error,
            Err(cleanup_error) => AppError::Other(format!(
                "{error}; 清理 ZIP 打包暂存文件失败 {}: {cleanup_error}",
                stage_path.to_string_lossy()
            )),
        });
    }
    fs::remove_file(&stage_path).map_err(|error| {
        AppError::Other(format!(
            "ZIP 已原子发布，但清理暂存文件失败 {}: {error}",
            stage_path.to_string_lossy()
        ))
    })?;

    Ok(ZipReport {
        path: out.to_string_lossy().into_owned(),
        files: file_count,
        bytes: total_bytes,
    })
}

fn create_unique_zip_pack_stage(parent: &Path, output: &Path) -> AppResult<(PathBuf, File)> {
    let file_name = output.file_name().ok_or_else(|| {
        AppError::Path(format!("ZIP 输出缺少文件名: {}", output.to_string_lossy()))
    })?;
    loop {
        let sequence = ZIP_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut stage_name = file_name.to_os_string();
        stage_name.push(format!(
            ".{}.{}.ccsm-pack.tmp",
            std::process::id(),
            sequence
        ));
        let stage = parent.join(stage_name);
        match OpenOptions::new().write(true).create_new(true).open(&stage) {
            Ok(file) => return Ok((stage, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn write_store_zip(src: &Path, file: File) -> AppResult<(u32, u64)> {
    let canonical_src = src.canonicalize()?;
    let mut writer = BufWriter::new(file);
    let mut central: Vec<CentralEntry> = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut file_count: u32 = 0;
    let mut offset: u32 = 0;

    for entry in walkdir::WalkDir::new(src).follow_links(false) {
        let entry = entry.map_err(|error| {
            AppError::Other(format!(
                "遍历待打包 bundle 目录失败 {}: {error}",
                src.to_string_lossy()
            ))
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "ZIP 打包源包含链接/junction/reparse point: {}",
                entry.path().to_string_lossy()
            )));
        }
        if !entry.path().canonicalize()?.starts_with(&canonical_src) {
            return Err(AppError::Path(format!(
                "ZIP 打包源条目解析后逃出根目录: {}",
                entry.path().to_string_lossy()
            )));
        }
        if metadata.is_dir() {
            continue;
        }
        if !metadata.is_file() {
            return Err(AppError::Path(format!(
                "ZIP 打包源包含不支持的文件类型: {}",
                entry.path().to_string_lossy()
            )));
        }
        let rel = entry
            .path()
            .strip_prefix(src)
            .map(|p| p.to_path_buf())
            .map_err(|error| {
                AppError::Path(format!(
                    "无法计算 zip 内相对路径 {}: {error}",
                    entry.path().to_string_lossy()
                ))
            })?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        checked_zip_entry_path(&rel_str, false)?;
        let name_len = u16::try_from(rel_str.len())
            .map_err(|_| AppError::Other(format!("ZIP 文件名过长: {rel_str}")))?;
        let size = u32::try_from(metadata.len()).map_err(|_| {
            AppError::Other(format!(
                "ZIP STORE 不支持超过 4 GiB 的文件: {}",
                entry.path().to_string_lossy()
            ))
        })?;
        let mut data: Vec<u8> = Vec::new();
        File::open(entry.path())?.read_to_end(&mut data)?;
        if data.len() as u64 != metadata.len() {
            return Err(AppError::Other(format!(
                "ZIP 打包源在读取期间发生变化: {}",
                entry.path().to_string_lossy()
            )));
        }
        let crc = crc32(&data);
        total_bytes = total_bytes
            .checked_add(size as u64)
            .ok_or_else(|| AppError::Other("ZIP 总字节数溢出".into()))?;
        file_count = file_count
            .checked_add(1)
            .ok_or_else(|| AppError::Other("ZIP 文件数溢出".into()))?;
        if file_count > u16::MAX as u32 {
            return Err(AppError::Other("ZIP32 最多支持 65535 个条目".into()));
        }
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
        writer.write_all(&name_len.to_le_bytes())?;
        writer.write_all(&0u16.to_le_bytes())?; // extra len
        writer.write_all(rel_str.as_bytes())?;
        writer.write_all(&data)?;

        let local_header_size = 30u32
            .checked_add(rel_str.len() as u32)
            .ok_or_else(|| AppError::Other("ZIP local header 大小溢出".into()))?;
        central.push(CentralEntry {
            name: rel_str,
            crc,
            size,
            offset,
        });
        offset = offset
            .checked_add(local_header_size)
            .and_then(|value| value.checked_add(size))
            .ok_or_else(|| AppError::Other("ZIP32 local offset 溢出".into()))?;
    }

    // Central directory
    let cd_offset = offset;
    let mut cd_size: u32 = 0;
    for e in &central {
        let name_len = u16::try_from(e.name.len())
            .map_err(|_| AppError::Other(format!("ZIP central 文件名过长: {}", e.name)))?;
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
        writer.write_all(&name_len.to_le_bytes())?;
        writer.write_all(&0u16.to_le_bytes())?; // extra len
        writer.write_all(&0u16.to_le_bytes())?; // comment len
        writer.write_all(&0u16.to_le_bytes())?; // disk start
        writer.write_all(&0u16.to_le_bytes())?; // int attrs
        writer.write_all(&0u32.to_le_bytes())?; // ext attrs
        writer.write_all(&e.offset.to_le_bytes())?;
        writer.write_all(e.name.as_bytes())?;
        cd_size = cd_size
            .checked_add(46)
            .and_then(|value| value.checked_add(e.name.len() as u32))
            .ok_or_else(|| AppError::Other("ZIP32 central directory 大小溢出".into()))?;
    }

    // EOCD
    writer.write_all(&0x06054b50u32.to_le_bytes())?;
    writer.write_all(&0u16.to_le_bytes())?; // disk
    writer.write_all(&0u16.to_le_bytes())?; // cd start disk
    let entry_count = u16::try_from(central.len())
        .map_err(|_| AppError::Other("ZIP32 最多支持 65535 个条目".into()))?;
    writer.write_all(&entry_count.to_le_bytes())?;
    writer.write_all(&entry_count.to_le_bytes())?;
    writer.write_all(&cd_size.to_le_bytes())?;
    writer.write_all(&cd_offset.to_le_bytes())?;
    writer.write_all(&0u16.to_le_bytes())?; // comment len
    writer.flush()?;
    writer.get_ref().sync_all()?;

    Ok((file_count, total_bytes))
}

fn publish_packed_zip(stage: &Path, output: &Path) -> AppResult<()> {
    let stage_fingerprint = atomic_file::fingerprint(stage)?;
    let copy_stage = |file: &mut File| -> AppResult<()> {
        let mut source = File::open(stage)?;
        std::io::copy(&mut source, file)?;
        if atomic_file::fingerprint(stage)? != stage_fingerprint {
            return Err(AppError::Other(format!(
                "ZIP 暂存文件在发布期间发生变化: {}",
                stage.to_string_lossy()
            )));
        }
        Ok(())
    };
    match fs::symlink_metadata(output) {
        Ok(metadata) => {
            if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
                return Err(AppError::Path(format!(
                    "ZIP 输出在发布前不是普通文件或属于链接/reparse point: {}",
                    output.to_string_lossy()
                )));
            }
            let expected = atomic_file::fingerprint(output)?;
            atomic_file::replace_with_writer_if_unchanged(output, &expected, copy_stage)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            atomic_file::create_with_writer_if_absent(output, copy_stage)
        }
        Err(error) => Err(error.into()),
    }
}

struct CentralEntry {
    name: String,
    crc: u32,
    size: u32,
    offset: u32,
}

fn crc32_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 == 1 {
                    0xEDB88320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            table[i as usize] = c;
        }
        table
    })
}

struct Crc32Hasher {
    value: u32,
}

impl Crc32Hasher {
    fn new() -> Self {
        Self { value: 0xFFFFFFFF }
    }

    fn update(&mut self, data: &[u8]) {
        let table = crc32_table();
        for &byte in data {
            let index = ((self.value ^ byte as u32) & 0xFF) as usize;
            self.value = table[index] ^ (self.value >> 8);
        }
    }

    fn finish(self) -> u32 {
        self.value ^ 0xFFFFFFFF
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut hasher = Crc32Hasher::new();
    hasher.update(data);
    hasher.finish()
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

#[derive(Debug)]
struct ParsedZipEntry {
    name: String,
    relative_path: PathBuf,
    is_directory: bool,
    crc32: u32,
    size: u64,
    payload_start: u64,
    local_range_end: u64,
    local_offset: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZipDestinationState {
    Missing,
    EmptyDirectory,
}

fn validate_plain_directory_metadata(path: &Path, label: &str) -> AppResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        return Err(AppError::Path(format!(
            "{label} 包含符号链接、junction 或 reparse point，已拒绝: {}",
            path.to_string_lossy()
        )));
    }
    if !metadata.is_dir() {
        return Err(AppError::Path(format!(
            "{label} 必须是普通目录: {}",
            path.to_string_lossy()
        )));
    }
    Ok(())
}

fn absolute_lexical_path(path: &Path, label: &str) -> AppResult<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(AppError::Path(format!(
                        "{label} 包含无法解析的父目录跳转: {}",
                        path.to_string_lossy()
                    )));
                }
            }
        }
    }
    if normalized.file_name().is_none() {
        return Err(AppError::Path(format!(
            "{label} 不能指向文件系统根目录: {}",
            path.to_string_lossy()
        )));
    }
    Ok(normalized)
}

fn validate_existing_directory_chain(path: &Path, label: &str) -> AppResult<()> {
    let mut ancestors = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    let mut missing_parent_seen = false;
    for ancestor in ancestors {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => {
                if missing_parent_seen {
                    return Err(AppError::Path(format!(
                        "{label} 在缺失父目录之后出现已有条目: {}",
                        ancestor.to_string_lossy()
                    )));
                }
                validate_plain_directory_metadata(ancestor, label)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing_parent_seen = true;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn remove_created_directories(created: &[PathBuf]) -> Vec<String> {
    let mut errors = Vec::new();
    for directory in created.iter().rev() {
        match fs::remove_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => errors.push(format!(
                "清理新建父目录失败 {}: {error}",
                directory.to_string_lossy()
            )),
        }
    }
    errors
}

fn ensure_plain_directory_chain(path: &Path, label: &str) -> AppResult<Vec<PathBuf>> {
    let mut ancestors = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    ancestors.reverse();
    let mut created = Vec::new();
    for ancestor in ancestors {
        let result = match fs::symlink_metadata(&ancestor) {
            Ok(_) => validate_plain_directory_metadata(&ancestor, label),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&ancestor) {
                    Ok(()) => created.push(ancestor.clone()),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        let cleanup = remove_created_directories(&created);
                        let suffix = if cleanup.is_empty() {
                            String::new()
                        } else {
                            format!("；{}", cleanup.join("；"))
                        };
                        return Err(AppError::Other(format!(
                            "创建 {label} 失败 {}: {error}{suffix}",
                            ancestor.to_string_lossy()
                        )));
                    }
                }
                validate_plain_directory_metadata(&ancestor, label)
            }
            Err(error) => Err(error.into()),
        };
        if let Err(error) = result {
            let cleanup = remove_created_directories(&created);
            if cleanup.is_empty() {
                return Err(error);
            }
            return Err(AppError::Other(format!("{error}；{}", cleanup.join("；"))));
        }
    }
    if let Err(error) = validate_existing_directory_chain(path, label) {
        let cleanup = remove_created_directories(&created);
        if cleanup.is_empty() {
            return Err(error);
        }
        return Err(AppError::Other(format!("{error}；{}", cleanup.join("；"))));
    }
    Ok(created)
}

fn inspect_zip_destination(path: &Path) -> AppResult<ZipDestinationState> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
                return Err(AppError::Path(format!(
                    "ZIP 解包目标不能是符号链接、junction 或 reparse point: {}",
                    path.to_string_lossy()
                )));
            }
            if !metadata.is_dir() {
                return Err(AppError::Path(format!(
                    "ZIP 解包目标已存在且不是目录: {}",
                    path.to_string_lossy()
                )));
            }
            if fs::read_dir(path)?.next().transpose()?.is_some() {
                return Err(AppError::Path(format!(
                    "ZIP 解包目标已存在且非空，拒绝覆盖: {}",
                    path.to_string_lossy()
                )));
            }
            Ok(ZipDestinationState::EmptyDirectory)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ZipDestinationState::Missing)
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_zip_flags(flags: u16, name: &str, header: &str) -> AppResult<()> {
    const UTF8_FLAG: u16 = 1 << 11;
    if flags & !UTF8_FLAG != 0 {
        return Err(AppError::Other(format!(
            "{header} 含不支持的 ZIP flags=0x{flags:04x}: {name}"
        )));
    }
    Ok(())
}

fn validate_zip_entry_attributes(
    name: &str,
    version_made_by: u16,
    external_attributes: u32,
    is_directory: bool,
) -> AppResult<()> {
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    if external_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(AppError::Path(format!(
            "ZIP 条目标记为 reparse point，已拒绝: {name}"
        )));
    }
    if external_attributes & FILE_ATTRIBUTE_DIRECTORY != 0 && !is_directory {
        return Err(AppError::Other(format!(
            "ZIP 条目的目录属性与文件名不一致: {name}"
        )));
    }

    let creator_system = (version_made_by >> 8) as u8;
    if creator_system == 3 {
        let unix_mode = (external_attributes >> 16) as u16;
        match unix_mode & 0o170000 {
            0 => {}
            0o040000 if is_directory => {}
            0o100000 if !is_directory => {}
            0o120000 => {
                return Err(AppError::Path(format!(
                    "ZIP 条目是符号链接，已拒绝: {name}"
                )))
            }
            _ => {
                return Err(AppError::Path(format!(
                    "ZIP 条目不是普通文件或目录，已拒绝: {name}"
                )))
            }
        }
    }
    Ok(())
}

fn checked_zip_entry_path(name: &str, is_directory: bool) -> AppResult<(PathBuf, String)> {
    if name != name.trim() || name.contains('\\') {
        return Err(AppError::Path(format!(
            "ZIP 条目路径包含歧义空白或反斜杠，已拒绝: {name}"
        )));
    }
    let path_text = if is_directory {
        name.strip_suffix('/').unwrap_or(name)
    } else {
        name
    };
    if path_text.is_empty()
        || path_text
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(AppError::Path(format!(
            "ZIP 条目路径无效或包含目录穿越: {name}"
        )));
    }
    #[cfg(windows)]
    for segment in path_text.split('/') {
        if segment.ends_with(' ') || segment.ends_with('.') {
            return Err(AppError::Path(format!(
                "ZIP 条目在 Windows 上具有歧义的尾随空格或点: {name}"
            )));
        }
    }
    let relative = paths::checked_relative_path(path_text)?;
    let mut key = relative.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        key = key.to_lowercase();
    }
    Ok((relative, key))
}

fn parse_zip_entries(file: &mut File, file_len: u64) -> AppResult<Vec<ParsedZipEntry>> {
    if file_len < 22 {
        return Err(AppError::Other("不是合法的 zip 文件（过短）".into()));
    }

    let tail_window = file_len.min(65_557);
    let tail_base = file_len - tail_window;
    file.seek(SeekFrom::Start(tail_base))?;
    let tail_len = usize::try_from(tail_window)
        .map_err(|_| AppError::Other("ZIP 尾部窗口长度超出平台限制".into()))?;
    let mut tail = vec![0u8; tail_len];
    file.read_exact(&mut tail)?;
    let eocd_signature = [0x50u8, 0x4b, 0x05, 0x06];
    let eocd_in_tail = (0..=tail.len() - 22)
        .rev()
        .find(|&index| {
            if tail[index..index + 4] != eocd_signature {
                return false;
            }
            let comment_len = u16::from_le_bytes([tail[index + 20], tail[index + 21]]) as usize;
            index
                .checked_add(22)
                .and_then(|value| value.checked_add(comment_len))
                == Some(tail.len())
        })
        .ok_or_else(|| AppError::Other("不是合法的 zip 文件（未找到有效 EOCD）".into()))?;
    let eocd_offset = tail_base
        .checked_add(eocd_in_tail as u64)
        .ok_or_else(|| AppError::Other("EOCD 偏移溢出".into()))?;
    let disk_number = read_zip_u16(&tail, eocd_in_tail + 4, "EOCD disk number")?;
    let central_disk = read_zip_u16(&tail, eocd_in_tail + 6, "EOCD central disk")?;
    let disk_entry_count = read_zip_u16(&tail, eocd_in_tail + 8, "EOCD 当前磁盘条目数")?;
    let entry_count = read_zip_u16(&tail, eocd_in_tail + 10, "EOCD 总条目数")?;
    let central_size = read_zip_u32(&tail, eocd_in_tail + 12, "central directory 总大小")?;
    let central_offset = read_zip_u32(&tail, eocd_in_tail + 16, "central directory 偏移")?;
    if disk_number != 0 || central_disk != 0 || disk_entry_count != entry_count {
        return Err(AppError::Other("不支持分卷 ZIP".into()));
    }
    if entry_count == u16::MAX || central_size == u32::MAX || central_offset == u32::MAX {
        return Err(AppError::Other("不支持 ZIP64".into()));
    }
    let central_offset = central_offset as u64;
    let central_size = central_size as u64;
    if central_offset.checked_add(central_size) != Some(eocd_offset) {
        return Err(AppError::Other(format!(
            "central directory 范围与 EOCD 不一致: offset={central_offset} size={central_size} eocd={eocd_offset}"
        )));
    }
    let central_len = usize::try_from(central_size)
        .map_err(|_| AppError::Other("central directory 大小超出平台限制".into()))?;
    file.seek(SeekFrom::Start(central_offset))?;
    let mut central = vec![0u8; central_len];
    file.read_exact(&mut central)?;

    let mut position = 0usize;
    let mut entries = Vec::with_capacity(entry_count as usize);
    let mut path_kinds = HashMap::<String, bool>::new();
    for _ in 0..entry_count {
        if checked_zip_slice(&central, position, 4, "central directory 签名")?
            != [0x50, 0x4b, 0x01, 0x02]
        {
            return Err(AppError::Other("central directory 损坏".into()));
        }
        checked_zip_slice(&central, position, 46, "central directory header")?;
        let version_made_by =
            read_zip_u16(&central, position + 4, "central directory version made by")?;
        let flags = read_zip_u16(&central, position + 8, "central directory flags")?;
        let method = read_zip_u16(&central, position + 10, "central directory 压缩方式")?;
        let crc = read_zip_u32(&central, position + 16, "central directory CRC")?;
        let compressed_size =
            read_zip_u32(&central, position + 20, "central directory 压缩后大小")? as u64;
        let uncompressed_size =
            read_zip_u32(&central, position + 24, "central directory 原始大小")? as u64;
        let name_len =
            read_zip_u16(&central, position + 28, "central directory 文件名长度")? as usize;
        let extra_len =
            read_zip_u16(&central, position + 30, "central directory extra 长度")? as usize;
        let comment_len =
            read_zip_u16(&central, position + 32, "central directory 注释长度")? as usize;
        let disk_start = read_zip_u16(&central, position + 34, "central directory disk start")?;
        let external_attributes =
            read_zip_u32(&central, position + 38, "central directory 外部属性")?;
        let local_offset = read_zip_u32(&central, position + 42, "local header 偏移")? as u64;
        let name_bytes = checked_zip_slice(
            &central,
            position + 46,
            name_len,
            "central directory 文件名",
        )?;
        let name = String::from_utf8(name_bytes.to_vec())
            .map_err(|error| AppError::Other(format!("zip 文件名不是 UTF-8: {error}")))?;
        let advance = 46usize
            .checked_add(name_len)
            .and_then(|value| value.checked_add(extra_len))
            .and_then(|value| value.checked_add(comment_len))
            .ok_or_else(|| AppError::Other("central directory entry 长度溢出".into()))?;
        checked_zip_slice(&central, position, advance, "central directory entry")?;
        position = position
            .checked_add(advance)
            .ok_or_else(|| AppError::Other("central directory 游标溢出".into()))?;

        if disk_start != 0 {
            return Err(AppError::Other(format!("ZIP 条目位于其他分卷: {name}")));
        }
        validate_zip_flags(flags, &name, "central directory")?;
        if method != 0 {
            return Err(AppError::Other(format!(
                "不支持的压缩方式 method={method}（仅支持 STORE）: {name}"
            )));
        }
        if compressed_size != uncompressed_size {
            return Err(AppError::Other(format!(
                "STORE 条目的压缩大小与原始大小不一致: {name}"
            )));
        }
        let is_directory = name.ends_with('/');
        if is_directory && (compressed_size != 0 || crc != 0) {
            return Err(AppError::Other(format!(
                "ZIP 目录条目声明了 payload 或非零 CRC: {name}"
            )));
        }
        validate_zip_entry_attributes(&name, version_made_by, external_attributes, is_directory)?;
        let (relative_path, path_key) = checked_zip_entry_path(&name, is_directory)?;
        if path_kinds.insert(path_key.clone(), is_directory).is_some() {
            return Err(AppError::Path(format!(
                "ZIP 包含重复或等价路径，已拒绝: {name}"
            )));
        }

        let local_header_end = local_offset
            .checked_add(30)
            .ok_or_else(|| AppError::Other(format!("local header 偏移溢出: {name}")))?;
        if local_header_end > central_offset {
            return Err(AppError::Other(format!("local header 越界: {name}")));
        }
        file.seek(SeekFrom::Start(local_offset))?;
        let mut local_header = [0u8; 30];
        file.read_exact(&mut local_header)?;
        if local_header[..4] != [0x50, 0x4b, 0x03, 0x04] {
            return Err(AppError::Other(format!("local header 损坏: {name}")));
        }
        let local_flags = read_zip_u16(&local_header, 6, "local header flags")?;
        let local_method = read_zip_u16(&local_header, 8, "local header 压缩方式")?;
        let local_crc = read_zip_u32(&local_header, 14, "local header CRC")?;
        let local_compressed_size =
            read_zip_u32(&local_header, 18, "local header 压缩后大小")? as u64;
        let local_uncompressed_size =
            read_zip_u32(&local_header, 22, "local header 原始大小")? as u64;
        let local_name_len = read_zip_u16(&local_header, 26, "local header 文件名长度")? as u64;
        let local_extra_len = read_zip_u16(&local_header, 28, "local header extra 长度")? as u64;
        validate_zip_flags(local_flags, &name, "local header")?;
        if local_flags != flags
            || local_method != method
            || local_crc != crc
            || local_compressed_size != compressed_size
            || local_uncompressed_size != uncompressed_size
        {
            return Err(AppError::Other(format!(
                "central directory 与 local header 的 flags/method/CRC/size 不一致: {name}"
            )));
        }
        let local_name_start = local_offset + 30;
        let payload_start = local_name_start
            .checked_add(local_name_len)
            .and_then(|value| value.checked_add(local_extra_len))
            .ok_or_else(|| AppError::Other(format!("payload 偏移溢出: {name}")))?;
        let payload_end = payload_start
            .checked_add(compressed_size)
            .ok_or_else(|| AppError::Other(format!("payload 范围溢出: {name}")))?;
        if payload_end > central_offset {
            return Err(AppError::Other(format!("payload 范围越界: {name}")));
        }
        let local_name_len_usize = usize::try_from(local_name_len)
            .map_err(|_| AppError::Other(format!("local header 文件名过长: {name}")))?;
        file.seek(SeekFrom::Start(local_name_start))?;
        let mut local_name = vec![0u8; local_name_len_usize];
        file.read_exact(&mut local_name)?;
        if local_name != name_bytes {
            return Err(AppError::Other(format!(
                "central directory 与 local header 的文件名不一致: {name}"
            )));
        }

        entries.push(ParsedZipEntry {
            name,
            relative_path,
            is_directory,
            crc32: crc,
            size: compressed_size,
            payload_start,
            local_range_end: payload_end,
            local_offset,
        });
    }
    if position != central.len() {
        return Err(AppError::Other(format!(
            "central directory 条目数量或总大小不一致: 已解析 {position} 字节，声明 {} 字节",
            central.len()
        )));
    }

    for path in path_kinds.keys() {
        let mut parent = path.as_str();
        while let Some(separator) = parent.rfind('/') {
            parent = &parent[..separator];
            if path_kinds.get(parent) == Some(&false) {
                return Err(AppError::Path(format!(
                    "ZIP 文件条目同时被用作父目录，已拒绝: {parent} -> {path}"
                )));
            }
        }
    }

    let mut ranges = entries
        .iter()
        .map(|entry| {
            (
                entry.local_offset,
                entry.local_range_end,
                entry.name.as_str(),
            )
        })
        .collect::<Vec<_>>();
    ranges.sort_unstable_by_key(|range| range.0);
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(AppError::Other(format!(
                "ZIP local header/payload 范围重叠: {} 与 {}",
                pair[0].2, pair[1].2
            )));
        }
    }
    Ok(entries)
}

fn create_unique_stage_directory(parent: &Path) -> AppResult<PathBuf> {
    loop {
        let sequence = ZIP_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".ccsm-unpack-{}-{sequence}.stage",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                if let Err(error) = validate_plain_directory_metadata(&path, "ZIP 解包暂存目录")
                {
                    return match fs::remove_dir(&path) {
                        Ok(()) => Err(error),
                        Err(cleanup_error) => Err(AppError::Other(format!(
                            "{error}；清理新建 ZIP 暂存目录失败 {}: {cleanup_error}",
                            path.to_string_lossy()
                        ))),
                    };
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn ensure_plain_stage_subdirectory(stage: &Path, relative: &Path) -> AppResult<PathBuf> {
    let mut current = stage.to_path_buf();
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            return Err(AppError::Path(format!(
                "ZIP 暂存相对目录包含非法组件: {}",
                relative.to_string_lossy()
            )));
        };
        current.push(segment);
        match fs::symlink_metadata(&current) {
            Ok(_) => validate_plain_directory_metadata(&current, "ZIP 暂存子目录")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                validate_plain_directory_metadata(&current, "ZIP 暂存子目录")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

fn extract_zip_entries_to_stage(
    file: &mut File,
    entries: &[ParsedZipEntry],
    stage: &Path,
) -> AppResult<(u32, u64)> {
    let mut file_count = 0u32;
    let mut total_bytes = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    for entry in entries {
        if entry.is_directory {
            ensure_plain_stage_subdirectory(stage, &entry.relative_path)?;
            continue;
        }
        let parent_relative = entry
            .relative_path
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let parent = ensure_plain_stage_subdirectory(stage, parent_relative)?;
        let file_name = entry
            .relative_path
            .file_name()
            .ok_or_else(|| AppError::Path(format!("ZIP 文件条目缺少文件名: {}", entry.name)))?;
        let output_path = parent.join(file_name);
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output_path)?;
        let output_metadata = output.metadata()?;
        if !output_metadata.is_file()
            || crate::path_safety::metadata_is_link_or_reparse(&output_metadata)
        {
            return Err(AppError::Path(format!(
                "ZIP 暂存输出不是普通文件: {}",
                output_path.to_string_lossy()
            )));
        }

        file.seek(SeekFrom::Start(entry.payload_start))?;
        let mut remaining = entry.size;
        let mut hasher = Crc32Hasher::new();
        while remaining > 0 {
            let requested = usize::try_from(remaining.min(buffer.len() as u64))
                .map_err(|_| AppError::Other("ZIP payload 读取长度超出平台限制".into()))?;
            let read = file.read(&mut buffer[..requested])?;
            if read == 0 {
                return Err(AppError::Other(format!(
                    "ZIP payload 提前结束，仍缺少 {remaining} 字节: {}",
                    entry.name
                )));
            }
            output.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
            remaining -= read as u64;
        }
        output.flush()?;
        output.sync_all()?;
        let actual_crc = hasher.finish();
        if actual_crc != entry.crc32 {
            return Err(AppError::Other(format!(
                "ZIP 条目 CRC 校验失败: {}，声明 {:08x}，实际 {:08x}",
                entry.name, entry.crc32, actual_crc
            )));
        }
        let written_metadata = fs::symlink_metadata(&output_path)?;
        if !written_metadata.is_file()
            || crate::path_safety::metadata_is_link_or_reparse(&written_metadata)
        {
            return Err(AppError::Path(format!(
                "ZIP 暂存文件在写入期间被替换: {}",
                output_path.to_string_lossy()
            )));
        }
        if written_metadata.len() != entry.size {
            return Err(AppError::Other(format!(
                "ZIP 暂存文件大小不一致: {}，声明 {}，实际 {}",
                entry.name,
                entry.size,
                written_metadata.len()
            )));
        }
        total_bytes = total_bytes
            .checked_add(entry.size)
            .ok_or_else(|| AppError::Other("ZIP 解包总字节数溢出".into()))?;
        file_count = file_count
            .checked_add(1)
            .ok_or_else(|| AppError::Other("ZIP 解包文件数溢出".into()))?;
    }
    Ok((file_count, total_bytes))
}

fn unique_missing_sibling(parent: &Path, suffix: &str) -> AppResult<PathBuf> {
    loop {
        let sequence = ZIP_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".ccsm-unpack-{}-{sequence}.{suffix}",
            std::process::id()
        ));
        match fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn publish_zip_stage(
    stage: &Path,
    destination: &Path,
    parent: &Path,
    initial_state: ZipDestinationState,
) -> AppResult<()> {
    validate_existing_directory_chain(parent, "ZIP 解包目标父目录")?;
    match initial_state {
        ZipDestinationState::Missing => {
            if inspect_zip_destination(destination)? != ZipDestinationState::Missing {
                return Err(AppError::Other(format!(
                    "ZIP 解包目标在发布前被其他进程创建: {}",
                    destination.to_string_lossy()
                )));
            }
            fs::rename(stage, destination)?;
        }
        ZipDestinationState::EmptyDirectory => {
            if inspect_zip_destination(destination)? != ZipDestinationState::EmptyDirectory {
                return Err(AppError::Other(format!(
                    "ZIP 解包目标在发布前发生变化: {}",
                    destination.to_string_lossy()
                )));
            }
            let backup = unique_missing_sibling(parent, "empty-destination")?;
            fs::rename(destination, &backup)?;
            if let Err(publish_error) = fs::rename(stage, destination) {
                let restore_error = fs::rename(&backup, destination).err();
                return match restore_error {
                    None => Err(publish_error.into()),
                    Some(restore_error) => Err(AppError::Other(format!(
                        "发布 ZIP 解包结果失败: {publish_error}；恢复原空目标也失败: {restore_error}"
                    ))),
                };
            }
            if let Err(cleanup_error) = fs::remove_dir(&backup) {
                let park_result = fs::rename(destination, stage);
                let restore_result = fs::rename(&backup, destination);
                return match (park_result, restore_result) {
                    (Ok(()), Ok(())) => Err(AppError::Other(format!(
                        "清理原空目标暂存目录失败，已回滚发布: {cleanup_error}"
                    ))),
                    (park, restore) => Err(AppError::Other(format!(
                        "清理原空目标暂存目录失败: {cleanup_error}；回滚新目标结果={park:?}；恢复原目标结果={restore:?}"
                    ))),
                };
            }
        }
    }
    Ok(())
}

fn cleanup_failed_zip_unpack(
    stage: &Path,
    created_parents: &[PathBuf],
    primary_error: AppError,
) -> AppError {
    let mut cleanup_errors = Vec::new();
    match fs::symlink_metadata(stage) {
        Ok(metadata) => {
            if crate::path_safety::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                cleanup_errors.push(format!(
                    "暂存路径不再是普通目录，拒绝递归清理: {}",
                    stage.to_string_lossy()
                ));
            } else if let Err(error) = fs::remove_dir_all(stage) {
                cleanup_errors.push(format!(
                    "清理 ZIP 暂存目录失败 {}: {error}",
                    stage.to_string_lossy()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => cleanup_errors.push(format!(
            "检查 ZIP 暂存目录失败 {}: {error}",
            stage.to_string_lossy()
        )),
    }
    cleanup_errors.extend(remove_created_directories(created_parents));
    if cleanup_errors.is_empty() {
        primary_error
    } else {
        AppError::Other(format!(
            "{primary_error}；清理失败：{}",
            cleanup_errors.join("；")
        ))
    }
}

pub fn unpack_zip(zip_path: String, dst_dir: String) -> AppResult<ZipReport> {
    let destination = absolute_lexical_path(Path::new(&dst_dir), "ZIP 解包目标")?;
    let parent = destination.parent().ok_or_else(|| {
        AppError::Path(format!(
            "ZIP 解包目标缺少父目录: {}",
            destination.to_string_lossy()
        ))
    })?;
    validate_existing_directory_chain(parent, "ZIP 解包目标父目录")?;
    let initial_state = inspect_zip_destination(&destination)?;

    let zip_source = PathBuf::from(&zip_path);
    let source_metadata = fs::symlink_metadata(&zip_source)?;
    if crate::path_safety::metadata_is_link_or_reparse(&source_metadata)
        || !source_metadata.is_file()
    {
        return Err(AppError::Path(format!(
            "ZIP 源必须是普通文件且不能是链接或 reparse point: {}",
            zip_source.to_string_lossy()
        )));
    }
    let mut file = File::open(&zip_source)?;
    let file_len = file.metadata()?.len();
    let entries = parse_zip_entries(&mut file, file_len)?;

    let created_parents = ensure_plain_directory_chain(parent, "ZIP 解包目标父目录")?;
    if let Err(error) = match (initial_state, inspect_zip_destination(&destination)) {
        (ZipDestinationState::Missing, Ok(ZipDestinationState::Missing))
        | (ZipDestinationState::EmptyDirectory, Ok(ZipDestinationState::EmptyDirectory)) => Ok(()),
        (_, Ok(_)) => Err(AppError::Other(format!(
            "ZIP 解包目标在验证期间发生变化: {}",
            destination.to_string_lossy()
        ))),
        (_, Err(error)) => Err(error),
    } {
        let cleanup = remove_created_directories(&created_parents);
        if cleanup.is_empty() {
            return Err(error);
        }
        return Err(AppError::Other(format!("{error}；{}", cleanup.join("；"))));
    }

    let stage = match create_unique_stage_directory(parent) {
        Ok(stage) => stage,
        Err(error) => {
            let cleanup = remove_created_directories(&created_parents);
            if cleanup.is_empty() {
                return Err(error);
            }
            return Err(AppError::Other(format!("{error}；{}", cleanup.join("；"))));
        }
    };
    let (file_count, total_bytes) = match extract_zip_entries_to_stage(&mut file, &entries, &stage)
    {
        Ok(report) => report,
        Err(error) => return Err(cleanup_failed_zip_unpack(&stage, &created_parents, error)),
    };
    if let Err(error) = crate::path_safety::validate_tree(parent, &stage, "ZIP 解包暂存树") {
        return Err(cleanup_failed_zip_unpack(&stage, &created_parents, error));
    }
    if let Err(error) = publish_zip_stage(&stage, &destination, parent, initial_state) {
        return Err(cleanup_failed_zip_unpack(&stage, &created_parents, error));
    }

    Ok(ZipReport {
        path: destination.to_string_lossy().into_owned(),
        files: file_count,
        bytes: total_bytes,
    })
}

pub fn unpack_zip_to_temp(zip_path: String) -> AppResult<ZipReport> {
    let dir = std::env::temp_dir().join(format!(
        "cc-session-manager-import-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    unpack_zip(zip_path, dir.to_string_lossy().into_owned())
}
