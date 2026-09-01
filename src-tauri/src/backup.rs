use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::atomic_file;
use crate::error::{AppError, AppResult};
use crate::logs_db;
use crate::models::{
    ArchiveOrigin, BackupDetail, BackupRestoreTarget, BackupSummary, BundleExportTarget, Manifest,
    ManifestArtifact, ManifestSession, ProviderDirs, RestoreResult, VerifyItem, VerifyReport,
};
use crate::path_safety::{self, EntryKind};
use crate::paths;
use crate::state_db;

mod restore_snapshot;
use restore_snapshot::{inject_restore_file_fault, restore_failure_message, RestoreFileSnapshots};

const PROVIDER_CODEX: &str = "codex";
const PROVIDER_CLAUDE: &str = "claude";
const PROVIDER_OPENCODE: &str = "opencode";
const PROVIDER_CURSOR: &str = "cursor";
const CODEX_THREAD_HISTORY_FILE: &str = "thread_history.ndjson";
const CODEX_THREAD_HISTORY_TABLES: [&str; 4] = [
    "thread_turns",
    "thread_items",
    "thread_history_projection_state",
    "thread_realtime_items",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RolloutPresence {
    Required,
    AllowMissing,
}

struct BackupThread {
    id: String,
    rollout_id: String,
    rollout_path: PathBuf,
    rollout_relpath: PathBuf,
    title: String,
    cwd: String,
    created_at: i64,
    updated_at: i64,
    tokens_used: i64,
    model: Option<String>,
    thread_row: serde_json::Value,
    rollout_cwd_override: Option<String>,
}

#[derive(Clone, Debug)]
struct CodexHistoryBaseRollout {
    thread_id: String,
    source_path: PathBuf,
    relpath: PathBuf,
}

struct CodexHistoryBaseRestoreFile {
    source_path: PathBuf,
    destination_path: PathBuf,
    sha256: String,
    label: String,
    copy_required: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum PortableSqliteValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CodexThreadHistoryBackupRow {
    table: String,
    values: BTreeMap<String, PortableSqliteValue>,
}

fn sha256_file(path: &Path) -> AppResult<String> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

fn same_existing_path(left: &Path, right: &Path) -> AppResult<bool> {
    let left = left.canonicalize()?;
    let right = right.canonicalize()?;
    #[cfg(windows)]
    {
        Ok(paths::strip_verbatim(&left.to_string_lossy())
            .eq_ignore_ascii_case(&paths::strip_verbatim(&right.to_string_lossy())))
    }
    #[cfg(not(windows))]
    {
        Ok(left == right)
    }
}

fn codex_rollout_filename_id(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let rest = stem.strip_prefix("rollout-")?;
    if rest.len() < 36 {
        return None;
    }
    let candidate = &rest[rest.len() - 36..];
    is_codex_thread_uuid(candidate).then(|| candidate.to_string())
}

fn is_codex_thread_uuid(candidate: &str) -> bool {
    candidate.len() == 36
        && candidate
            .as_bytes()
            .iter()
            .enumerate()
            .all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    *byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            })
}

fn codex_history_base_thread_id(path: &Path) -> AppResult<Option<String>> {
    let meta = crate::family::read_session_meta(path).map_err(|error| {
        AppError::Other(format!(
            "读取 Codex rollout session_meta 失败 {}: {error}",
            path.to_string_lossy()
        ))
    })?;
    if meta.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
        return Err(AppError::Other(format!(
            "Codex rollout 首行不是 session_meta: {}",
            path.to_string_lossy()
        )));
    }
    let payload = meta
        .get("payload")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            AppError::Other(format!(
                "Codex rollout session_meta.payload 不是对象: {}",
                path.to_string_lossy()
            ))
        })?;
    let Some(history_base) = payload.get("history_base") else {
        return Ok(None);
    };
    if history_base.is_null() {
        return Ok(None);
    }
    let thread_id = history_base
        .as_object()
        .and_then(|base| base.get("thread_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|thread_id| !thread_id.trim().is_empty())
        .ok_or_else(|| {
            AppError::Other(format!(
                "Codex rollout history_base.thread_id 缺失或无效: {}",
                path.to_string_lossy()
            ))
        })?;
    if !is_codex_thread_uuid(thread_id) {
        return Err(AppError::Other(format!(
            "Codex rollout history_base.thread_id 不是有效 UUID: {thread_id}"
        )));
    }
    Ok(Some(thread_id.to_string()))
}

/// Copy a Codex rollout into the backup, optionally materializing Desktop's pending cwd into the
/// backup copy. The source rollout is never modified.
fn copy_codex_rollout_for_backup(
    source: &Path,
    destination: &Path,
    thread_id: &str,
    target_cwd: Option<&str>,
) -> AppResult<()> {
    let Some(target_cwd) = target_cwd else {
        fs::copy(source, destination)?;
        return Ok(());
    };

    let copy_result = (|| -> AppResult<()> {
        let mut output = File::create(destination)?;
        crate::codex_rollout_cwd::copy_with_effective_cwd(
            source,
            &mut output,
            thread_id,
            target_cwd,
            None,
        )?;
        output.sync_all()?;
        Ok(())
    })();
    if let Err(error) = copy_result {
        return match fs::remove_file(destination) {
            Ok(()) => Err(error),
            Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => Err(error),
            Err(remove_error) => Err(AppError::Other(format!(
                "{error}; 清理未完成的备份 rollout 失败 {}: {remove_error}",
                destination.to_string_lossy()
            ))),
        };
    }
    Ok(())
}

fn collect_manifest_artifacts(root: &Path) -> AppResult<Vec<ManifestArtifact>> {
    let metadata = fs::symlink_metadata(root)?;
    if path_safety::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(AppError::Path(format!(
            "sidecar 必须是普通目录且不能是链接或 junction: {}",
            root.to_string_lossy()
        )));
    }
    let mut artifacts = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| {
            AppError::Other(format!(
                "遍历 sidecar 失败 {}: {error}",
                root.to_string_lossy()
            ))
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "sidecar 包含链接或 junction: {}",
                entry.path().to_string_lossy()
            )));
        }
        if metadata.is_dir() {
            continue;
        }
        if !metadata.is_file() {
            return Err(AppError::Path(format!(
                "sidecar 包含不支持的文件类型: {}",
                entry.path().to_string_lossy()
            )));
        }
        let relative = entry.path().strip_prefix(root).map_err(|error| {
            AppError::Path(format!(
                "无法计算 sidecar 相对路径 {}: {error}",
                entry.path().to_string_lossy()
            ))
        })?;
        let relative = paths::checked_relative_path(&relative.to_string_lossy())?;
        artifacts.push(ManifestArtifact {
            relpath: relative.to_string_lossy().replace('\\', "/"),
            bytes: metadata.len(),
            sha256: sha256_file(entry.path())?,
        });
    }
    artifacts.sort_by(|left, right| left.relpath.cmp(&right.relpath));
    Ok(artifacts)
}

fn collect_backup_artifacts(root: &Path, names: &[&str]) -> AppResult<Vec<ManifestArtifact>> {
    let mut artifacts = Vec::with_capacity(names.len());
    for name in names {
        let path = root.join(name);
        path_safety::validate_descendant(
            root,
            &path,
            EntryKind::File,
            false,
            &format!("备份辅助文件 {name}"),
        )?;
        let metadata = fs::symlink_metadata(&path)?;
        artifacts.push(ManifestArtifact {
            relpath: (*name).to_string(),
            bytes: metadata.len(),
            sha256: sha256_file(&path)?,
        });
    }
    Ok(artifacts)
}

fn verify_restore_source(
    backup: &Path,
    source: &Path,
    target: &ManifestSession,
    provider: &str,
) -> AppResult<()> {
    path_safety::validate_descendant(backup, source, EntryKind::File, false, "备份会话文件")?;
    verify_rollout_identity(source, &target.id, provider)?;
    let actual = sha256_file(source)?;
    if actual != target.sha256_rollout {
        return Err(AppError::Other(format!(
            "备份校验失败，拒绝还原 id={}: expected={} actual={}",
            target.id, target.sha256_rollout, actual
        )));
    }
    Ok(())
}

fn prepare_codex_history_base_restore_files(
    backup: &Path,
    codex: &Path,
    target: &ManifestSession,
) -> AppResult<Vec<CodexHistoryBaseRestoreFile>> {
    let validated = validate_codex_history_base_payload(backup, target)?;
    validated
        .into_iter()
        .zip(&target.history_base_rollouts)
        .enumerate()
        .map(|(index, (dependency, artifact))| {
            let destination_path = codex.join(&dependency.relpath);
            path_safety::validate_descendant(
                codex,
                &destination_path,
                EntryKind::File,
                true,
                "Codex history_base 还原目标",
            )?;
            let copy_required = match fs::symlink_metadata(&destination_path) {
                Ok(metadata) => {
                    if path_safety::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
                        return Err(AppError::Path(format!(
                            "Codex history_base 还原目标不是普通文件: {}",
                            destination_path.to_string_lossy()
                        )));
                    }
                    let actual_sha256 = sha256_file(&destination_path)?;
                    if metadata.len() != artifact.bytes || actual_sha256 != artifact.sha256 {
                        return Err(AppError::Other(format!(
                            "目标 Codex home 已有同 UUID 但内容不同的 history_base rollout，拒绝覆盖: id={} path={}",
                            dependency.thread_id,
                            destination_path.to_string_lossy()
                        )));
                    }
                    false
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(error) => return Err(error.into()),
            };
            Ok(CodexHistoryBaseRestoreFile {
                source_path: dependency.source_path,
                destination_path,
                sha256: artifact.sha256.clone(),
                label: format!("history base rollout {}", index + 1),
                copy_required,
            })
        })
        .collect()
}

fn copy_restore_file_atomically(
    destination_root: &Path,
    source: &Path,
    destination: &Path,
    expected_sha256: &str,
    label: &str,
) -> AppResult<()> {
    path_safety::validate_descendant(destination_root, destination, EntryKind::File, true, label)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let copy = |target: &mut File| -> AppResult<()> {
        let mut source_file = File::open(source)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = source_file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            target.write_all(&buffer[..read])?;
        }
        let actual = hex::encode(hasher.finalize());
        if actual != expected_sha256 {
            return Err(AppError::Other(format!(
                "备份源在还原期间发生变化，已拒绝写入: expected={expected_sha256} actual={actual} source={}",
                source.to_string_lossy()
            )));
        }
        Ok(())
    };
    if destination.is_file() {
        let expected = atomic_file::fingerprint(destination)?;
        atomic_file::replace_with_writer_if_unchanged(destination, &expected, copy)
    } else {
        atomic_file::create_with_writer_if_absent(destination, copy)
    }
}

pub fn create_backup(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    backup_dir: String,
    ids: Vec<String>,
    targets: Option<Vec<BundleExportTarget>>,
    name: Option<String>,
    note: Option<String>,
) -> AppResult<BackupSummary> {
    create_backup_with_opencode(
        provider, codex_dir, claude_dir, None, backup_dir, ids, targets, name, note,
    )
}

pub fn create_backup_with_opencode(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    backup_dir: String,
    ids: Vec<String>,
    targets: Option<Vec<BundleExportTarget>>,
    name: Option<String>,
    note: Option<String>,
) -> AppResult<BackupSummary> {
    create_backup_with_dirs(
        provider,
        ProviderDirs {
            codex_dir,
            claude_dir,
            opencode_dir,
            ..ProviderDirs::default()
        },
        backup_dir,
        ids,
        targets,
        name,
        note,
    )
}

pub fn create_backup_with_dirs(
    provider: Option<String>,
    dirs: ProviderDirs,
    backup_dir: String,
    ids: Vec<String>,
    targets: Option<Vec<BundleExportTarget>>,
    name: Option<String>,
    note: Option<String>,
) -> AppResult<BackupSummary> {
    let codex_dir = dirs.codex_dir.clone();
    let targets = normalize_backup_targets(&ids, targets)?;
    let provider_name = provider.as_deref().unwrap_or(PROVIDER_CODEX);
    if provider_name == PROVIDER_CLAUDE {
        return create_claude_backup(
            dirs.claude_path(),
            PathBuf::from(backup_dir),
            targets,
            name,
            note,
        );
    }
    if provider_name == PROVIDER_OPENCODE {
        return create_opencode_backup(
            dirs.opencode_path(),
            PathBuf::from(backup_dir),
            targets,
            name,
            note,
        );
    }
    if provider_name == PROVIDER_CURSOR {
        return create_cursor_backup(
            dirs.cursor_path(),
            PathBuf::from(backup_dir),
            targets,
            name,
            note,
        );
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
    path_safety::validate_descendant(
        &backup_root,
        &tmp,
        EntryKind::Directory,
        true,
        "备份临时目录",
    )?;
    path_safety::validate_descendant(
        &backup_root,
        &final_path,
        EntryKind::Directory,
        true,
        "备份目标目录",
    )?;
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

    for target in &targets {
        let id = &target.id;
        let thread = load_backup_thread(&state, &codex, id)?;
        if let Some(requested) = target.rollout_path.as_deref() {
            let requested = PathBuf::from(paths::strip_verbatim(requested));
            if !same_existing_path(&requested, &thread.rollout_path)? {
                return Err(AppError::Other(format!(
                    "Codex 备份精确目标与 threads.rollout_path 不一致: id={id} requested={} actual={}",
                    requested.to_string_lossy(),
                    thread.rollout_path.to_string_lossy()
                )));
            }
        }
        validate_codex_rollout_relpath(&thread.rollout_relpath.to_string_lossy(), id)?;
        path_safety::validate_descendant(
            &codex,
            &thread.rollout_path,
            EntryKind::File,
            false,
            "Codex rollout 备份源",
        )?;
        verify_rollout_identity(&thread.rollout_path, id, PROVIDER_CODEX)?;
        backup_threads.push(thread);
    }
    let history_base_chains = collect_codex_history_base_chains(&codex, &backup_threads)?;
    let mut rollout_source_fingerprints = HashMap::new();
    for path in backup_threads
        .iter()
        .map(|thread| &thread.rollout_path)
        .chain(
            history_base_chains
                .iter()
                .flatten()
                .map(|dependency| &dependency.source_path),
        )
    {
        rollout_source_fingerprints
            .entry(path.clone())
            .or_insert(atomic_file::fingerprint(path)?);
    }
    let history_ids = backup_threads
        .iter()
        .map(|thread| thread.id.clone())
        .collect::<HashSet<_>>();
    let history_index =
        crate::history::collect_lines_for_ids(&paths::history_path(&codex), &history_ids)?;

    fs::create_dir_all(tmp.join("sessions"))?;
    let mut copied_history_base_paths = HashSet::new();
    for dependency in history_base_chains.iter().flatten() {
        if !copied_history_base_paths.insert(dependency.relpath.clone()) {
            continue;
        }
        let destination = tmp.join(&dependency.relpath);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        copy_codex_rollout_for_backup(
            &dependency.source_path,
            &destination,
            &dependency.thread_id,
            None,
        )?;
    }
    // Primary copies run after dependencies so a selected base session has one deterministic
    // backup representation even when another selected session references it.
    for thread in &backup_threads {
        let destination = tmp.join(&thread.rollout_relpath);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        copy_codex_rollout_for_backup(
            &thread.rollout_path,
            &destination,
            &thread.rollout_id,
            thread.rollout_cwd_override.as_deref(),
        )?;
    }
    let projection_ids = codex_projection_ids_for_backup(&backup_threads, &history_base_chains);
    let mut modified_rollout_ids = HashSet::new();
    for thread in &backup_threads {
        if sha256_file(&thread.rollout_path)? != sha256_file(&tmp.join(&thread.rollout_relpath))? {
            modified_rollout_ids.insert(thread.id.clone());
            if let Some(rollout_id) = codex_rollout_filename_id(&thread.rollout_path) {
                modified_rollout_ids.insert(rollout_id);
            }
        }
    }
    export_codex_thread_history(
        &codex,
        &tmp.join(CODEX_THREAD_HISTORY_FILE),
        &projection_ids,
        &modified_rollout_ids,
    )?;
    for (path, expected) in &rollout_source_fingerprints {
        if atomic_file::fingerprint(path)? != *expected {
            return Err(AppError::Other(format!(
                "Codex rollout 在备份期间发生变化，请重试: {}",
                path.to_string_lossy()
            )));
        }
    }

    let mut manifest = Manifest {
        version: 6,
        provider: Some(PROVIDER_CODEX.to_string()),
        created_at: chrono::Utc::now().to_rfc3339(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        codex_dir: codex.to_string_lossy().into_owned(),
        claude_dir: None,
        opencode_dir: None,
        note,
        artifacts: Vec::new(),
        sessions: Vec::new(),
    };
    let mut threads_rows: Vec<serde_json::Value> = Vec::new();
    let mut logs_out = File::create(tmp.join("logs.ndjson"))?;

    for (thread, history_base_chain) in backup_threads.iter().zip(&history_base_chains) {
        threads_rows.push(thread.thread_row.clone());

        let dest = tmp.join(&thread.rollout_relpath);
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
        let history_base_rollouts = history_base_chain
            .iter()
            .map(|dependency| {
                let path = tmp.join(&dependency.relpath);
                validate_codex_history_base_relpath(
                    &dependency.relpath.to_string_lossy(),
                    &dependency.thread_id,
                )?;
                Ok(ManifestArtifact {
                    relpath: dependency.relpath.to_string_lossy().replace('\\', "/"),
                    bytes: fs::metadata(&path)?.len(),
                    sha256: sha256_file(&path)?,
                })
            })
            .collect::<AppResult<Vec<_>>>()?;

        manifest.sessions.push(ManifestSession {
            provider: Some(PROVIDER_CODEX.to_string()),
            id: thread.id.clone(),
            rollout_relpath: thread.rollout_relpath.to_string_lossy().replace('\\', "/"),
            history_base_rollouts,
            source_relpath: None,
            sidecar_relpath: None,
            sidecar_files: Vec::new(),
            companions_relpath: None,
            companion_files: Vec::new(),
            tasks_relpath: None,
            task_files: Vec::new(),
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
        tmp.join("threads.json"),
        serde_json::to_vec_pretty(&threads_rows)?,
    )?;
    logs_out.flush()?;
    logs_out.sync_all()?;
    drop(logs_out);
    manifest.artifacts = collect_backup_artifacts(
        &tmp,
        &[
            "threads.json",
            "logs.ndjson",
            "history.jsonl",
            CODEX_THREAD_HISTORY_FILE,
        ],
    )?;
    fs::write(
        tmp.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    fs::rename(&tmp, &final_path)?;

    summarize_backup(&final_path)
}

fn create_claude_backup(
    claude: PathBuf,
    backup_root: PathBuf,
    targets: Vec<BundleExportTarget>,
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
    path_safety::validate_descendant(
        &backup_root,
        &tmp,
        EntryKind::Directory,
        true,
        "备份临时目录",
    )?;
    path_safety::validate_descendant(
        &backup_root,
        &final_path,
        EntryKind::Directory,
        true,
        "备份目标目录",
    )?;
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
    let projects = paths::claude_projects_dir(&claude);
    let history_ids = targets
        .iter()
        .map(|target| target.id.clone())
        .collect::<HashSet<_>>();
    let history_index =
        crate::history::collect_lines_for_ids(&paths::history_path(&claude), &history_ids)?;

    let mut manifest = Manifest {
        version: 5,
        provider: Some(PROVIDER_CLAUDE.to_string()),
        created_at: chrono::Utc::now().to_rfc3339(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        codex_dir: String::new(),
        claude_dir: Some(claude.to_string_lossy().into_owned()),
        opencode_dir: None,
        note,
        artifacts: Vec::new(),
        sessions: Vec::new(),
    };

    let id_counts = targets.iter().fold(HashMap::new(), |mut counts, target| {
        *counts.entry(target.id.as_str()).or_insert(0usize) += 1;
        counts
    });
    for target in &targets {
        let id = &target.id;
        let session = resolve_claude_backup_session(&projects, &sessions, target)?;
        let source = PathBuf::from(&session.rollout_path);
        path_safety::validate_descendant(
            &projects,
            &source,
            EntryKind::File,
            false,
            "Claude JSONL 备份源",
        )?;
        verify_rollout_identity(&source, id, PROVIDER_CLAUDE)?;
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

        let artifact_name = if id_counts.get(id.as_str()).copied().unwrap_or(0) > 1 {
            exact_artifact_name(id, &source_rel_string)
        } else {
            paths::sanitize_slug(id)
        };

        let mut sidecar_rel: Option<String> = None;
        let mut sidecar_files = Vec::new();
        if let Some(sidecar) = crate::claude_sessions::sidecar_path_for(&source) {
            match fs::symlink_metadata(&sidecar) {
                Ok(_) => {
                    path_safety::validate_tree(&projects, &sidecar, "Claude sidecar 备份源")?;
                    let sidecar_dest_rel = PathBuf::from("sidecars").join(&artifact_name);
                    let sidecar_dest = tmp.join(&sidecar_dest_rel);
                    copy_path_recursive(&sidecar, &sidecar_dest)?;
                    sidecar_files = collect_manifest_artifacts(&sidecar_dest)?;
                    sidecar_rel = Some(sidecar_dest_rel.to_string_lossy().replace('\\', "/"));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }

        let companions = crate::claude_sessions::companion_files_for(&source)?;
        let mut companions_rel = None;
        let mut companion_files = Vec::new();
        if !companions.is_empty() {
            let relative = PathBuf::from("companions").join(&artifact_name);
            let root = tmp.join(&relative);
            fs::create_dir_all(&root)?;
            for companion in companions {
                path_safety::validate_descendant(
                    &projects,
                    &companion,
                    EntryKind::File,
                    false,
                    "Claude companion 备份源",
                )?;
                let name = companion.file_name().ok_or_else(|| {
                    AppError::Path(format!(
                        "Claude companion 文件名无效: {}",
                        companion.to_string_lossy()
                    ))
                })?;
                fs::copy(&companion, root.join(name))?;
            }
            companion_files = collect_manifest_artifacts(&root)?;
            companions_rel = Some(relative.to_string_lossy().replace('\\', "/"));
        }

        let task_source = crate::claude_sessions::task_path_for(&claude, id);
        let mut tasks_rel = None;
        let mut task_files = Vec::new();
        if task_source.exists() {
            path_safety::validate_tree(&claude, &task_source, "Claude tasks 备份源")?;
            let relative = PathBuf::from("tasks").join(&artifact_name);
            let root = tmp.join(&relative);
            copy_path_recursive(&task_source, &root)?;
            task_files = collect_manifest_artifacts(&root)?;
            tasks_rel = Some(relative.to_string_lossy().replace('\\', "/"));
        }

        let history_rows = history_index
            .get(&session.id)
            .map(|rows| rows.len() as u32)
            .unwrap_or(0);

        manifest.sessions.push(ManifestSession {
            provider: Some(PROVIDER_CLAUDE.to_string()),
            id: session.id.clone(),
            rollout_relpath: dest_rel.to_string_lossy().replace('\\', "/"),
            history_base_rollouts: Vec::new(),
            source_relpath: Some(source_rel_string),
            sidecar_relpath: sidecar_rel,
            sidecar_files,
            companions_relpath: companions_rel,
            companion_files,
            tasks_relpath: tasks_rel,
            task_files,
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
    write_backup_history(
        &tmp,
        targets.iter().map(|target| target.id.as_str()),
        &history_index,
    )?;
    manifest.artifacts = collect_backup_artifacts(&tmp, &["history.jsonl"])?;

    fs::write(
        tmp.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::rename(&tmp, &final_path)?;
    summarize_backup(&final_path)
}

fn create_opencode_backup(
    data_dir: PathBuf,
    backup_root: PathBuf,
    targets: Vec<BundleExportTarget>,
    name: Option<String>,
    note: Option<String>,
) -> AppResult<BackupSummary> {
    fs::create_dir_all(&backup_root)?;
    let final_name = name
        .map(|name| name.trim().to_string())
        .unwrap_or_else(|| format!("backup-{}", chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S")));
    validate_backup_name(&final_name)?;
    let tmp = backup_root.join(format!(".{final_name}.partial"));
    let final_path = backup_root.join(&final_name);
    path_safety::validate_descendant(
        &backup_root,
        &tmp,
        EntryKind::Directory,
        true,
        "备份临时目录",
    )?;
    path_safety::validate_descendant(
        &backup_root,
        &final_path,
        EntryKind::Directory,
        true,
        "备份目标目录",
    )?;
    if final_path.exists() {
        return Err(AppError::Other(format!("备份已存在: {final_name}")));
    }
    if tmp.exists() {
        return Err(AppError::Other(format!(
            "存在未完成的临时备份目录，请先检查或移除: {}",
            tmp.to_string_lossy()
        )));
    }

    let sessions = crate::opencode_sessions::list_sessions(&data_dir)?
        .into_iter()
        .map(|session| (session.id.clone(), session))
        .collect::<HashMap<_, _>>();
    let mut manifest = Manifest {
        version: 5,
        provider: Some(PROVIDER_OPENCODE.to_string()),
        created_at: chrono::Utc::now().to_rfc3339(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        codex_dir: String::new(),
        claude_dir: None,
        opencode_dir: Some(data_dir.to_string_lossy().into_owned()),
        note,
        artifacts: Vec::new(),
        sessions: Vec::new(),
    };
    for target in targets {
        if let Some(locator) = target.rollout_path.as_deref() {
            let (db, located_id) = crate::opencode_sessions::resolve_locator(locator)?;
            if located_id != target.id || db != crate::opencode_sessions::database_path(&data_dir) {
                return Err(AppError::Other(format!(
                    "OpenCode 备份精确目标与当前数据库不一致: {}",
                    target.id
                )));
            }
        }
        let summary = sessions
            .get(&target.id)
            .ok_or_else(|| AppError::NotFound(format!("OpenCode 会话不存在: {}", target.id)))?;
        let snapshot = crate::opencode_transfer::export_snapshot(&data_dir, &target.id)?;
        let relative = PathBuf::from(PROVIDER_OPENCODE)
            .join("sessions")
            .join(format!("{}.json", paths::sanitize_slug(&target.id)));
        let destination = tmp.join(&relative);
        crate::opencode_transfer::write_snapshot(&destination, &snapshot)?;
        let sha = sha256_file(&destination)?;
        let bytes = fs::metadata(&destination)?.len();
        manifest.sessions.push(ManifestSession {
            provider: Some(PROVIDER_OPENCODE.to_string()),
            id: target.id,
            rollout_relpath: relative.to_string_lossy().replace('\\', "/"),
            history_base_rollouts: Vec::new(),
            source_relpath: None,
            sidecar_relpath: None,
            sidecar_files: Vec::new(),
            companions_relpath: None,
            companion_files: Vec::new(),
            tasks_relpath: None,
            task_files: Vec::new(),
            title: summary.title.clone(),
            cwd: snapshot.source_cwd.clone(),
            created_at: summary.created_at,
            updated_at: snapshot.source_updated_at / 1000,
            tokens_used: summary.tokens_used,
            model: summary.model.clone(),
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

/// Cursor 备份：每个会话一份自包含的 JSON 快照。
///
/// 不能拷 `state.vscdb`——那个文件里还有登录态和全部工作区状态，而且实测有 8 GB。
fn create_cursor_backup(
    cursor_dir: PathBuf,
    backup_root: PathBuf,
    targets: Vec<BundleExportTarget>,
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
    path_safety::validate_descendant(
        &backup_root,
        &tmp,
        EntryKind::Directory,
        true,
        "备份临时目录",
    )?;
    path_safety::validate_descendant(
        &backup_root,
        &final_path,
        EntryKind::Directory,
        true,
        "备份目标目录",
    )?;
    if final_path.exists() {
        return Err(AppError::Other(format!("备份目录已存在: {final_name}")));
    }
    if tmp.exists() {
        return Err(AppError::Other(format!(
            "存在未完成的临时备份目录，请先检查或移除: {}",
            tmp.to_string_lossy()
        )));
    }

    let sessions = crate::cursor_sessions::list_sessions(
        &cursor_dir,
        &crate::paths::default_cursor_agent_dir(),
    )?
    .into_iter()
    .map(|session| (session.id.clone(), session))
    .collect::<HashMap<_, _>>();
    let mut manifest = Manifest {
        version: 5,
        provider: Some(PROVIDER_CURSOR.to_string()),
        created_at: chrono::Utc::now().to_rfc3339(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        codex_dir: String::new(),
        claude_dir: None,
        opencode_dir: None,
        note,
        artifacts: Vec::new(),
        sessions: Vec::new(),
    };
    for target in targets {
        let summary = sessions
            .get(&target.id)
            .ok_or_else(|| AppError::NotFound(format!("Cursor 会话不存在: {}", target.id)))?;
        if !summary.resume_command.is_empty() {
            return Err(AppError::Other(format!(
                "cursor-agent 会话暂不支持备份: {}",
                target.id
            )));
        }
        let snapshot = crate::cursor_transfer::export_snapshot(&cursor_dir, &target.id)?;
        let relative = snapshot_relpath(PROVIDER_CURSOR, &target.id);
        let destination = tmp.join(&relative);
        crate::cursor_transfer::write_snapshot(&destination, &snapshot)?;
        let sha = sha256_file(&destination)?;
        let bytes = fs::metadata(&destination)?.len();
        manifest.sessions.push(ManifestSession {
            provider: Some(PROVIDER_CURSOR.to_string()),
            id: target.id,
            rollout_relpath: relative.to_string_lossy().replace('\\', "/"),
            history_base_rollouts: Vec::new(),
            source_relpath: None,
            sidecar_relpath: None,
            sidecar_files: Vec::new(),
            companions_relpath: None,
            companion_files: Vec::new(),
            tasks_relpath: None,
            task_files: Vec::new(),
            title: summary.title.clone(),
            cwd: summary.cwd.clone(),
            created_at: summary.created_at,
            updated_at: summary.updated_at,
            tokens_used: summary.tokens_used,
            model: summary.model.clone(),
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

fn normalize_backup_targets(
    ids: &[String],
    targets: Option<Vec<BundleExportTarget>>,
) -> AppResult<Vec<BundleExportTarget>> {
    let targets = match targets {
        Some(targets) => {
            if targets.len() != ids.len() {
                return Err(AppError::Other(format!(
                    "备份 ids 与 targets 数量不一致: ids={} targets={}",
                    ids.len(),
                    targets.len()
                )));
            }
            for (index, (id, target)) in ids.iter().zip(&targets).enumerate() {
                if target.id != *id {
                    return Err(AppError::Other(format!(
                        "备份 ids 与 targets 第 {} 项不一致: id={} target.id={}",
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
                "备份目标重复: id={} rollout_path={}",
                target.id,
                target.rollout_path.as_deref().unwrap_or("未提供")
            )));
        }
    }
    Ok(targets)
}

fn resolve_claude_backup_session<'a>(
    projects: &Path,
    sessions: &'a [crate::models::SessionSummary],
    target: &BundleExportTarget,
) -> AppResult<&'a crate::models::SessionSummary> {
    let matches = sessions
        .iter()
        .filter(|session| session.id == target.id)
        .collect::<Vec<_>>();
    let session = if let Some(requested) = target.rollout_path.as_deref() {
        let requested = paths::strip_verbatim(requested);
        matches
            .into_iter()
            .find(|session| paths::strip_verbatim(&session.rollout_path) == requested)
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "Claude 备份精确目标不存在或 ID 不匹配: id={} rollout_path={}",
                    target.id, requested
                ))
            })?
    } else {
        match matches.as_slice() {
            [session] => *session,
            [] => {
                return Err(AppError::NotFound(format!(
                    "Claude 会话不存在: {}",
                    target.id
                )))
            }
            duplicates => {
                return Err(AppError::Other(format!(
                    "发现 {} 个同 ID Claude 会话，备份必须提供精确 rollout_path: {}",
                    duplicates.len(),
                    target.id
                )))
            }
        }
    };
    let source = PathBuf::from(&session.rollout_path);
    path_safety::validate_descendant(
        projects,
        &source,
        EntryKind::File,
        false,
        "Claude JSONL 备份源",
    )?;
    verify_rollout_identity(&source, &target.id, PROVIDER_CLAUDE)?;
    Ok(session)
}

fn exact_artifact_name(id: &str, source_relpath: &str) -> String {
    let digest = Sha256::digest(source_relpath.as_bytes());
    format!(
        "{}-{}",
        paths::sanitize_slug(id),
        &hex::encode(digest)[..12]
    )
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
    // SELECT * 以捕获全部列（含新版 App 增加的 preview/thread_source 等），
    // 还原时按目标表实际结构取交集写回。
    let mut stmt = state.prepare("SELECT * FROM threads WHERE id = ?")?;
    let cols: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut row_json = match stmt.query_row([id], |row| {
        let mut obj = serde_json::Map::new();
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
            obj.insert(name.clone(), jv);
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
    let rollout_path = paths::host_path_from_codex_record(codex, &rollout_path_raw);
    let rollout_relpath = rel_path(&rollout_path.to_string_lossy(), codex)?;
    let rollout_brief =
        crate::repair::read_rollout_brief(codex, &rollout_path)?.ok_or_else(|| {
            AppError::Other(format!(
                "Codex rollout 缺少 session_meta: {}",
                rollout_path.to_string_lossy()
            ))
        })?;
    let database_cwd = row_json
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let rollout_cwd = rollout_brief.cwd.clone();
    // Official Desktop moves update the explicit assignment immediately and may defer the Core
    // cwd until the next turn. A backup must preserve that newer intent without carrying the
    // source machine's projectId. Convert the Desktop-visible host path back to this Codex home's
    // Core record format and make only the backup artifacts self-consistent.
    let assignment_core_cwd =
        crate::codex_projects::pending_thread_project_assignment_cwd(codex, id)?
            .map(|host_cwd| paths::codex_record_path_from_host(codex, Path::new(&host_cwd)))
            .transpose()?;
    let effective_cwd = assignment_core_cwd
        .or(rollout_cwd)
        .filter(|cwd| !cwd.trim().is_empty())
        .unwrap_or(database_cwd);
    row_json
        .as_object_mut()
        .ok_or_else(|| AppError::Other("threads 查询结果必须是 JSON 对象".to_string()))?
        .insert(
            "cwd".to_string(),
            serde_json::Value::String(effective_cwd.clone()),
        );
    // Always normalize the backup copy from the same effective cwd used by its manifest and
    // threads row. This also heals older rollouts whose session_meta lags the latest turn_context.
    let rollout_cwd_override = (!effective_cwd.trim().is_empty()).then(|| effective_cwd.clone());

    Ok(BackupThread {
        id: id.to_string(),
        rollout_id: rollout_brief.id,
        rollout_path,
        rollout_relpath,
        title: row_json
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        cwd: effective_cwd,
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
        rollout_cwd_override,
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

fn validate_codex_rollout_relpath(raw: &str, id: &str) -> AppResult<PathBuf> {
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
            "Codex 备份目标只能是 sessions/ 或 archived_sessions/ 下的 jsonl: {raw}"
        )));
    }
    let stem = relative
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if stem != id && !stem.ends_with(id) && codex_rollout_filename_id(&relative).is_none() {
        return Err(AppError::Path(format!(
            "Codex rollout 文件名与会话 ID 不匹配: id={id} path={raw}"
        )));
    }
    Ok(relative)
}

fn validate_codex_history_base_relpath(raw: &str, thread_id: &str) -> AppResult<PathBuf> {
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
        || codex_rollout_filename_id(&relative).as_deref() != Some(thread_id)
    {
        return Err(AppError::Path(format!(
            "Codex history_base rollout 路径与物理线程 UUID 不匹配: id={thread_id} path={raw}"
        )));
    }
    Ok(relative)
}

fn codex_history_base_rollout_index(codex: &Path) -> AppResult<HashMap<String, Vec<PathBuf>>> {
    let mut rollouts = crate::family::scan_rollouts(codex)?;
    rollouts.extend(crate::family::scan_archived_rollouts(codex)?);
    let mut index = HashMap::<String, Vec<PathBuf>>::new();
    for rollout in rollouts {
        if let Some(thread_id) = codex_rollout_filename_id(&rollout) {
            index.entry(thread_id).or_default().push(rollout);
        }
    }
    Ok(index)
}

fn collect_codex_history_base_chain(
    codex: &Path,
    primary: &Path,
    index: &HashMap<String, Vec<PathBuf>>,
) -> AppResult<Vec<CodexHistoryBaseRollout>> {
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    if let Some(primary_id) = codex_rollout_filename_id(primary) {
        visited.insert(primary_id);
    }
    let mut current = primary.to_path_buf();
    while let Some(thread_id) = codex_history_base_thread_id(&current)? {
        if !visited.insert(thread_id.clone()) {
            return Err(AppError::Other(format!(
                "Codex history_base 依赖链存在循环引用: {thread_id}"
            )));
        }
        let candidates = index.get(&thread_id).map(Vec::as_slice).unwrap_or(&[]);
        let source_path = match candidates {
            [path] => path.clone(),
            [] => {
                return Err(AppError::NotFound(format!(
                    "Codex history_base 依赖 rollout 不存在: {thread_id}"
                )))
            }
            duplicates => {
                return Err(AppError::Other(format!(
                    "Codex history_base 依赖 rollout 不唯一: id={thread_id} count={}",
                    duplicates.len()
                )))
            }
        };
        path_safety::validate_descendant(
            codex,
            &source_path,
            EntryKind::File,
            false,
            "Codex history_base 备份源",
        )?;
        let relpath = rel_path(&source_path.to_string_lossy(), codex)?;
        validate_codex_history_base_relpath(&relpath.to_string_lossy(), &thread_id)?;
        // Besides checking the physical UUID above, require a readable session_meta before a
        // dependency can enter a self-contained backup.
        codex_history_base_thread_id(&source_path)?;
        chain.push(CodexHistoryBaseRollout {
            thread_id,
            source_path: source_path.clone(),
            relpath,
        });
        current = source_path;
    }
    Ok(chain)
}

fn collect_codex_history_base_chains(
    codex: &Path,
    threads: &[BackupThread],
) -> AppResult<Vec<Vec<CodexHistoryBaseRollout>>> {
    let index = codex_history_base_rollout_index(codex)?;
    threads
        .iter()
        .map(|thread| collect_codex_history_base_chain(codex, &thread.rollout_path, &index))
        .collect()
}

fn codex_projection_ids_for_backup(
    threads: &[BackupThread],
    chains: &[Vec<CodexHistoryBaseRollout>],
) -> HashSet<String> {
    let mut ids = HashSet::new();
    for (thread, chain) in threads.iter().zip(chains) {
        ids.insert(thread.id.clone());
        if let Some(rollout_id) = codex_rollout_filename_id(&thread.rollout_path) {
            ids.insert(rollout_id);
        }
        ids.extend(chain.iter().map(|dependency| dependency.thread_id.clone()));
    }
    ids
}

fn codex_projection_ids_for_manifest_session(
    session: &ManifestSession,
) -> AppResult<HashSet<String>> {
    let mut ids = HashSet::from([session.id.clone()]);
    let primary = paths::checked_relative_path(&session.rollout_relpath)?;
    if let Some(rollout_id) = codex_rollout_filename_id(&primary) {
        ids.insert(rollout_id);
    }
    for artifact in &session.history_base_rollouts {
        let relative = paths::checked_relative_path(&artifact.relpath)?;
        let thread_id = codex_rollout_filename_id(&relative).ok_or_else(|| {
            AppError::Path(format!(
                "Codex history_base rollout 文件名缺少物理线程 UUID: {}",
                artifact.relpath
            ))
        })?;
        ids.insert(thread_id);
    }
    Ok(ids)
}

fn portable_sqlite_value(value: rusqlite::types::ValueRef<'_>) -> PortableSqliteValue {
    match value {
        rusqlite::types::ValueRef::Null => PortableSqliteValue::Null,
        rusqlite::types::ValueRef::Integer(value) => PortableSqliteValue::Integer(value),
        rusqlite::types::ValueRef::Real(value) => PortableSqliteValue::Real(value),
        rusqlite::types::ValueRef::Text(value) => {
            PortableSqliteValue::Text(String::from_utf8_lossy(value).into_owned())
        }
        rusqlite::types::ValueRef::Blob(value) => PortableSqliteValue::Blob(hex::encode(value)),
    }
}

fn validate_codex_thread_history_schema(
    connection: &rusqlite::Connection,
    database: &str,
) -> AppResult<HashMap<String, HashSet<String>>> {
    let mut columns = HashMap::new();
    for table in CODEX_THREAD_HISTORY_TABLES {
        let exists: bool = connection.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {database}.sqlite_schema WHERE type='table' AND name=?1)"
            ),
            [table],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(AppError::Other(format!(
                "Codex thread history 数据库缺少 {table} 表"
            )));
        }
        let mut statement =
            connection.prepare(&format!("PRAGMA {database}.table_info({table})"))?;
        let names = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<HashSet<_>, _>>()?;
        if !names.contains("thread_id") {
            return Err(AppError::Other(format!(
                "Codex thread history 的 {table} 表缺少 thread_id 列"
            )));
        }
        columns.insert(table.to_string(), names);
    }
    Ok(columns)
}

fn export_codex_thread_history(
    codex: &Path,
    destination: &Path,
    projection_ids: &HashSet<String>,
    modified_rollout_ids: &HashSet<String>,
) -> AppResult<u32> {
    let mut output = File::create(destination)?;
    let database_path = codex.join("thread_history_1.sqlite");
    let metadata = match fs::symlink_metadata(&database_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            output.sync_all()?;
            return Ok(0);
        }
        Err(error) => return Err(error.into()),
    };
    if path_safety::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(AppError::Path(format!(
            "Codex thread history 数据库不是普通文件: {}",
            database_path.to_string_lossy()
        )));
    }
    let connection = rusqlite::Connection::open_with_flags(
        &database_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    validate_codex_thread_history_schema(&connection, "main")?;
    connection.execute_batch("BEGIN")?;
    let mut ids = projection_ids.iter().cloned().collect::<Vec<_>>();
    ids.sort();
    let mut written = 0u32;
    for table in CODEX_THREAD_HISTORY_TABLES {
        let mut statement = connection.prepare(&format!(
            "SELECT * FROM {table} WHERE thread_id = ?1 ORDER BY rowid"
        ))?;
        let column_names = statement
            .column_names()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        for thread_id in &ids {
            // Rewriting even only cwd changes rollout byte offsets. Keeping the old projection
            // rows with cleared offsets is not enough: Codex deliberately preserves the first
            // persisted turn boundary on conflict, so those offsets would never be rebuilt and
            // paginated fork-at-turn would remain broken. Omit this rollout's projection
            // completely and let the official projector rebuild it from the canonical JSONL.
            if modified_rollout_ids.contains(thread_id) {
                continue;
            }
            let rows = statement.query_map([thread_id], |row| {
                let mut values = BTreeMap::new();
                for (index, name) in column_names.iter().enumerate() {
                    values.insert(name.clone(), portable_sqlite_value(row.get_ref(index)?));
                }
                Ok(values)
            })?;
            for values in rows {
                writeln!(
                    output,
                    "{}",
                    serde_json::to_string(&CodexThreadHistoryBackupRow {
                        table: table.to_string(),
                        values: values?,
                    })?
                )?;
                written = written
                    .checked_add(1)
                    .ok_or_else(|| AppError::Other("Codex thread history 备份行数溢出".into()))?;
            }
        }
    }
    connection.execute_batch("COMMIT")?;
    output.flush()?;
    output.sync_all()?;
    Ok(written)
}

fn codex_thread_history_primary_key(table: &str) -> AppResult<&'static [&'static str]> {
    match table {
        "thread_turns" => Ok(&["thread_id", "turn_id"]),
        "thread_items" => Ok(&["thread_id", "turn_id", "item_id"]),
        "thread_history_projection_state" => Ok(&["thread_id"]),
        "thread_realtime_items" => Ok(&["thread_id", "item_id"]),
        other => Err(AppError::Other(format!(
            "thread_history.ndjson 包含未知表: {other}"
        ))),
    }
}

fn codex_thread_history_row_thread_id(row: &CodexThreadHistoryBackupRow) -> AppResult<&str> {
    match row.values.get("thread_id") {
        Some(PortableSqliteValue::Text(thread_id)) if !thread_id.is_empty() => Ok(thread_id),
        _ => Err(AppError::Other(format!(
            "thread_history.ndjson 的 {} 行缺少文本 thread_id",
            row.table
        ))),
    }
}

fn read_codex_thread_history_backup_rows(
    backup: &Path,
) -> AppResult<Vec<CodexThreadHistoryBackupRow>> {
    let path = backup.join(CODEX_THREAD_HISTORY_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if path_safety::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(AppError::Path(format!(
            "Codex thread history 备份不是普通文件: {}",
            path.to_string_lossy()
        )));
    }
    let mut rows = Vec::new();
    let mut keys = HashSet::new();
    for (line_index, line) in BufReader::new(File::open(&path)?).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let row: CodexThreadHistoryBackupRow = serde_json::from_str(&line).map_err(|error| {
            AppError::Other(format!(
                "thread_history.ndjson 第 {} 行损坏: {error}",
                line_index + 1
            ))
        })?;
        let primary_key = codex_thread_history_primary_key(&row.table)?;
        let mut key = vec![row.table.clone()];
        for column in primary_key {
            let value = row.values.get(*column).ok_or_else(|| {
                AppError::Other(format!(
                    "thread_history.ndjson 的 {} 行缺少主键列 {column}",
                    row.table
                ))
            })?;
            key.push(serde_json::to_string(value)?);
        }
        if !keys.insert(key) {
            return Err(AppError::Other(format!(
                "thread_history.ndjson 包含重复的 {} 主键",
                row.table
            )));
        }
        codex_thread_history_row_thread_id(&row)?;
        rows.push(row);
    }
    Ok(rows)
}

fn validate_codex_thread_history_backup(backup: &Path, manifest: &Manifest) -> AppResult<()> {
    let mut allowed_ids = HashSet::new();
    for session in &manifest.sessions {
        if manifest_session_provider(manifest, session) == PROVIDER_CODEX {
            allowed_ids.extend(codex_projection_ids_for_manifest_session(session)?);
        }
    }
    for row in read_codex_thread_history_backup_rows(backup)? {
        let thread_id = codex_thread_history_row_thread_id(&row)?;
        if !allowed_ids.contains(thread_id) {
            return Err(AppError::Other(format!(
                "thread_history.ndjson 包含未声明会话的投影行: {thread_id}"
            )));
        }
    }
    Ok(())
}

fn codex_thread_history_rows_for_session(
    backup: &Path,
    session: &ManifestSession,
) -> AppResult<Vec<CodexThreadHistoryBackupRow>> {
    let ids = codex_projection_ids_for_manifest_session(session)?;
    let mut selected = Vec::new();
    for row in read_codex_thread_history_backup_rows(backup)? {
        if ids.contains(codex_thread_history_row_thread_id(&row)?) {
            selected.push(row);
        }
    }
    Ok(selected)
}

fn validate_claude_source_relpath(raw: &str, id: &str) -> AppResult<PathBuf> {
    let relative = paths::checked_relative_path(raw)?;
    if relative.extension().and_then(|value| value.to_str()) != Some("jsonl")
        || relative.file_stem().and_then(|value| value.to_str()) != Some(id)
    {
        return Err(AppError::Path(format!(
            "Claude 备份目标必须是以会话 ID 命名的 jsonl: id={id} path={raw}"
        )));
    }
    Ok(relative)
}

fn validate_opencode_snapshot_relpath(raw: &str, id: &str) -> AppResult<PathBuf> {
    validate_snapshot_relpath(PROVIDER_OPENCODE, raw, id)
}

fn validate_cursor_snapshot_relpath(raw: &str, id: &str) -> AppResult<PathBuf> {
    validate_snapshot_relpath(PROVIDER_CURSOR, raw, id)
}

/// 以会话快照 JSON 形式落盘的 provider 共用同一套路径约束。
fn validate_snapshot_relpath(provider: &str, raw: &str, id: &str) -> AppResult<PathBuf> {
    let relative = paths::checked_relative_path(raw)?;
    let expected = snapshot_relpath(provider, id);
    if relative != expected {
        return Err(AppError::Path(format!(
            "{provider} 备份快照路径与会话 ID 不匹配: id={id} path={raw}"
        )));
    }
    Ok(relative)
}

fn snapshot_relpath(provider: &str, id: &str) -> PathBuf {
    PathBuf::from(provider)
        .join("sessions")
        .join(format!("{}.json", paths::sanitize_slug(id)))
}

fn validate_manifest_paths(manifest: &Manifest) -> AppResult<()> {
    let allowed_artifacts = [
        "history.jsonl",
        "logs.ndjson",
        "threads.json",
        CODEX_THREAD_HISTORY_FILE,
    ]
    .into_iter()
    .collect::<HashSet<_>>();
    let mut artifact_paths = HashSet::new();
    for artifact in &manifest.artifacts {
        let relative = paths::checked_relative_path(&artifact.relpath)?;
        let normalized = relative.to_string_lossy().replace('\\', "/");
        if !allowed_artifacts.contains(normalized.as_str()) {
            return Err(AppError::Path(format!(
                "备份辅助文件清单包含不允许的路径: {}",
                artifact.relpath
            )));
        }
        if !artifact_paths.insert(normalized) {
            return Err(AppError::Path(format!(
                "备份辅助文件清单包含重复路径: {}",
                artifact.relpath
            )));
        }
    }
    if manifest.version >= 4 {
        let provider = manifest.provider.as_deref().unwrap_or_else(|| {
            manifest
                .sessions
                .first()
                .map(|session| manifest_session_provider(manifest, session))
                .unwrap_or(PROVIDER_CODEX)
        });
        let expected = match provider {
            PROVIDER_CLAUDE => ["history.jsonl"].into_iter().collect::<HashSet<_>>(),
            PROVIDER_OPENCODE | PROVIDER_CURSOR => HashSet::new(),
            _ if manifest.version >= 6 => [
                "history.jsonl",
                "logs.ndjson",
                "threads.json",
                CODEX_THREAD_HISTORY_FILE,
            ]
            .into_iter()
            .collect::<HashSet<_>>(),
            _ => ["history.jsonl", "logs.ndjson", "threads.json"]
                .into_iter()
                .collect::<HashSet<_>>(),
        };
        let actual = artifact_paths
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        if actual != expected {
            return Err(AppError::Other(format!(
                "备份 v{} 的辅助文件清单不完整: provider={provider}",
                manifest.version
            )));
        }
    }

    for session in &manifest.sessions {
        let mut history_base_paths = HashSet::new();
        for artifact in &session.history_base_rollouts {
            let relative = paths::checked_relative_path(&artifact.relpath)?;
            if !history_base_paths.insert(relative) {
                return Err(AppError::Path(format!(
                    "Codex history_base manifest 包含重复路径: {}",
                    artifact.relpath
                )));
            }
        }
        for (label, root, artifacts) in [
            (
                "sidecar",
                session.sidecar_relpath.as_deref(),
                session.sidecar_files.as_slice(),
            ),
            (
                "companions",
                session.companions_relpath.as_deref(),
                session.companion_files.as_slice(),
            ),
            (
                "tasks",
                session.tasks_relpath.as_deref(),
                session.task_files.as_slice(),
            ),
        ] {
            let mut artifact_paths = HashSet::new();
            for artifact in artifacts {
                let relative = paths::checked_relative_path(&artifact.relpath)?;
                if !artifact_paths.insert(relative) {
                    return Err(AppError::Path(format!(
                        "{label} manifest 包含重复路径: {}",
                        artifact.relpath
                    )));
                }
            }
            if root.is_none() && !artifacts.is_empty() {
                return Err(AppError::Path(format!(
                    "会话未声明 {label} 目录却包含文件清单: {}",
                    session.id
                )));
            }
        }
        let provider = manifest_session_provider(manifest, session);
        match provider {
            PROVIDER_CODEX => {
                validate_codex_rollout_relpath(&session.rollout_relpath, &session.id)?;
                if manifest.version < 5 && !session.history_base_rollouts.is_empty() {
                    return Err(AppError::Other(format!(
                        "Codex history_base 文件清单要求备份 manifest v5: {}",
                        session.id
                    )));
                }
                for artifact in &session.history_base_rollouts {
                    let relative = paths::checked_relative_path(&artifact.relpath)?;
                    let thread_id = codex_rollout_filename_id(&relative).ok_or_else(|| {
                        AppError::Path(format!(
                            "Codex history_base rollout 文件名缺少物理线程 UUID: {}",
                            artifact.relpath
                        ))
                    })?;
                    validate_codex_history_base_relpath(&artifact.relpath, &thread_id)?;
                }
                if session.sidecar_relpath.is_some()
                    || session.companions_relpath.is_some()
                    || session.tasks_relpath.is_some()
                {
                    return Err(AppError::Path(format!(
                        "Codex 备份不应声明 Claude sidecar: {}",
                        session.id
                    )));
                }
                if let Some(path) = session.source_relpath.as_deref() {
                    paths::checked_relative_path(path)?;
                }
            }
            PROVIDER_CLAUDE => {
                if !session.history_base_rollouts.is_empty() {
                    return Err(AppError::Path(format!(
                        "Claude 备份不应声明 Codex history_base rollout: {}",
                        session.id
                    )));
                }
                let source = session.source_relpath.as_deref().ok_or_else(|| {
                    AppError::Path(format!("Claude 备份缺少 source_relpath: {}", session.id))
                })?;
                let source = validate_claude_source_relpath(source, &session.id)?;
                let rollout = paths::checked_relative_path(&session.rollout_relpath)?;
                if rollout != PathBuf::from(PROVIDER_CLAUDE).join(&source) {
                    return Err(AppError::Path(format!(
                        "Claude 备份文件路径与目标路径不对应: {}",
                        session.id
                    )));
                }
                if let Some(sidecar) = session.sidecar_relpath.as_deref() {
                    let sidecar = paths::checked_relative_path(sidecar)?;
                    let legacy = PathBuf::from("sidecars").join(paths::sanitize_slug(&session.id));
                    let normalized_source = source.to_string_lossy().replace('\\', "/");
                    let exact = PathBuf::from("sidecars")
                        .join(exact_artifact_name(&session.id, &normalized_source));
                    if sidecar != legacy && sidecar != exact {
                        return Err(AppError::Path(format!(
                            "Claude sidecar 路径与会话 ID 不对应: {}",
                            session.id
                        )));
                    }
                }
                let normalized_source = source.to_string_lossy().replace('\\', "/");
                let names = [
                    paths::sanitize_slug(&session.id),
                    exact_artifact_name(&session.id, &normalized_source),
                ];
                for (label, declared, root) in [
                    (
                        "companions",
                        session.companions_relpath.as_deref(),
                        "companions",
                    ),
                    ("tasks", session.tasks_relpath.as_deref(), "tasks"),
                ] {
                    if let Some(declared) = declared {
                        let declared = paths::checked_relative_path(declared)?;
                        if !names
                            .iter()
                            .any(|name| declared == PathBuf::from(root).join(name))
                        {
                            return Err(AppError::Path(format!(
                                "Claude {label} 路径与会话 ID 不对应: {}",
                                session.id
                            )));
                        }
                    }
                }
            }
            // 这两个 provider 都以单份 JSON 快照落盘，不该带任何文件型附属资产。
            PROVIDER_OPENCODE | PROVIDER_CURSOR => {
                validate_snapshot_relpath(provider, &session.rollout_relpath, &session.id)?;
                if !session.history_base_rollouts.is_empty()
                    || session.source_relpath.is_some()
                    || session.sidecar_relpath.is_some()
                    || session.companions_relpath.is_some()
                    || session.tasks_relpath.is_some()
                {
                    return Err(AppError::Path(format!(
                        "{provider} 备份不应声明文件型附属资产: {}",
                        session.id
                    )));
                }
            }
            other => {
                return Err(AppError::Other(format!("备份包含未知 provider: {other}")));
            }
        }
    }
    Ok(())
}

fn verify_rollout_identity(source: &Path, expected_id: &str, provider: &str) -> AppResult<()> {
    if provider == PROVIDER_OPENCODE {
        return crate::opencode_transfer::verify_snapshot_file(source, expected_id);
    }
    if provider == PROVIDER_CURSOR {
        return crate::cursor_transfer::verify_snapshot_file(source, expected_id);
    }
    if provider == PROVIDER_CODEX {
        let brief = crate::repair::read_rollout_brief(source.parent().unwrap_or(source), source)?
            .ok_or_else(|| {
            AppError::Other(format!(
                "备份 Codex rollout 缺少 session_meta: {}",
                source.to_string_lossy()
            ))
        })?;
        let filename_id = codex_rollout_filename_id(source);
        if brief.id != expected_id && filename_id.as_deref() != Some(brief.id.as_str()) {
            return Err(AppError::Other(format!(
                "备份 Codex rollout 内部 ID 不匹配: 逻辑会话 {}，文件 UUID {:?}，实际 {}",
                expected_id, filename_id, brief.id
            )));
        }
        return Ok(());
    }

    let stem = source
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if stem.starts_with("agent-") {
        if stem == expected_id {
            return Ok(());
        }
        return Err(AppError::Other(format!(
            "备份 Claude 子代理文件名与 ID 不匹配: {}",
            expected_id
        )));
    }
    let file = File::open(source)?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line).map_err(|error| {
            AppError::Other(format!(
                "备份 Claude JSONL 损坏 {}: {error}",
                source.to_string_lossy()
            ))
        })?;
        if let Some(id) = value.get("sessionId").and_then(serde_json::Value::as_str) {
            if id == expected_id {
                return Ok(());
            }
            return Err(AppError::Other(format!(
                "备份 Claude rollout 内部 ID 不匹配: 期望 {}，实际 {}",
                expected_id, id
            )));
        }
    }
    Err(AppError::Other(format!(
        "备份 Claude rollout 缺少 sessionId: {}",
        source.to_string_lossy()
    )))
}

fn validate_codex_history_base_payload(
    backup: &Path,
    session: &ManifestSession,
) -> AppResult<Vec<CodexHistoryBaseRollout>> {
    let primary_rel = validate_codex_rollout_relpath(&session.rollout_relpath, &session.id)?;
    let mut current = backup.join(primary_rel);
    let mut visited = HashSet::new();
    if let Some(primary_id) = codex_rollout_filename_id(&current) {
        visited.insert(primary_id);
    }
    let mut validated = Vec::with_capacity(session.history_base_rollouts.len());
    for artifact in &session.history_base_rollouts {
        let thread_id = codex_history_base_thread_id(&current)?.ok_or_else(|| {
            AppError::Other(format!(
                "Codex history_base manifest 声明了多余 rollout: session={} path={}",
                session.id, artifact.relpath
            ))
        })?;
        if !visited.insert(thread_id.clone()) {
            return Err(AppError::Other(format!(
                "Codex history_base 备份依赖链存在循环引用: {thread_id}"
            )));
        }
        let relpath = validate_codex_history_base_relpath(&artifact.relpath, &thread_id)?;
        let source_path = backup.join(&relpath);
        path_safety::validate_descendant(
            backup,
            &source_path,
            EntryKind::File,
            false,
            "Codex history_base 备份文件",
        )?;
        let metadata = fs::symlink_metadata(&source_path)?;
        let actual_sha256 = sha256_file(&source_path)?;
        if metadata.len() != artifact.bytes || actual_sha256 != artifact.sha256 {
            return Err(AppError::Other(format!(
                "Codex history_base rollout 大小或 sha256 校验失败: {}",
                artifact.relpath
            )));
        }
        // Parse the dependency now as well as on the next loop iteration so a terminal base with
        // malformed session_meta cannot pass validation.
        codex_history_base_thread_id(&source_path)?;
        validated.push(CodexHistoryBaseRollout {
            thread_id,
            source_path: source_path.clone(),
            relpath,
        });
        current = source_path;
    }
    if let Some(missing_id) = codex_history_base_thread_id(&current)? {
        return Err(AppError::Other(format!(
            "Codex 备份缺少 history_base 依赖 rollout: session={} id={missing_id}",
            session.id
        )));
    }
    Ok(validated)
}

fn validate_optional_backup_file(backup: &Path, name: &str, required: bool) -> AppResult<()> {
    let path = backup.join(name);
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            path_safety::validate_descendant(
                backup,
                &path,
                EntryKind::File,
                false,
                &format!("备份 {name}"),
            )?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(AppError::NotFound(format!("备份缺少必需文件: {name}")))
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_backup_payload(
    backup: &Path,
    manifest: &Manifest,
    rollout_presence: RolloutPresence,
) -> AppResult<()> {
    let sessions = manifest.sessions.iter().collect::<Vec<_>>();
    validate_backup_payload_sessions(backup, manifest, &sessions, rollout_presence)
}

fn validate_backup_payload_sessions(
    backup: &Path,
    manifest: &Manifest,
    sessions: &[&ManifestSession],
    rollout_presence: RolloutPresence,
) -> AppResult<()> {
    if manifest.version >= 4 {
        for artifact in &manifest.artifacts {
            let relative = paths::checked_relative_path(&artifact.relpath)?;
            let path = backup.join(relative);
            path_safety::validate_descendant(
                backup,
                &path,
                EntryKind::File,
                false,
                "备份辅助文件",
            )?;
            let metadata = fs::symlink_metadata(&path)?;
            let actual_sha = sha256_file(&path)?;
            if metadata.len() != artifact.bytes || actual_sha != artifact.sha256 {
                return Err(AppError::Other(format!(
                    "备份辅助文件大小或 sha256 校验失败: {}",
                    artifact.relpath
                )));
            }
        }
    }
    let has_codex = sessions
        .iter()
        .any(|session| manifest_session_provider(manifest, session) == PROVIDER_CODEX);
    validate_optional_backup_file(backup, "threads.json", has_codex)?;
    validate_optional_backup_file(
        backup,
        "logs.ndjson",
        sessions.iter().any(|session| session.logs_count > 0),
    )?;
    validate_optional_backup_file(
        backup,
        "history.jsonl",
        sessions.iter().any(|session| session.history_rows > 0),
    )?;
    validate_optional_backup_file(
        backup,
        CODEX_THREAD_HISTORY_FILE,
        has_codex && manifest.version >= 6,
    )?;
    if has_codex && backup.join(CODEX_THREAD_HISTORY_FILE).is_file() {
        validate_codex_thread_history_backup(backup, manifest)?;
    }

    for session in sessions {
        let provider = manifest_session_provider(manifest, session);
        let source = backup.join(paths::checked_relative_path(&session.rollout_relpath)?);
        let source_exists = path_safety::validate_descendant(
            backup,
            &source,
            EntryKind::File,
            rollout_presence == RolloutPresence::AllowMissing,
            "备份会话文件",
        )?;
        if source_exists {
            verify_rollout_identity(&source, &session.id, provider)?;
            if provider == PROVIDER_CODEX {
                validate_codex_history_base_payload(backup, session)?;
            }
        }
        if let Some(sidecar) = session.sidecar_relpath.as_deref() {
            let sidecar = backup.join(paths::checked_relative_path(sidecar)?);
            path_safety::validate_tree(backup, &sidecar, "Claude 备份 sidecar")?;
            if manifest.version >= 3 {
                let actual = collect_manifest_artifacts(&sidecar)?;
                if actual.len() != session.sidecar_files.len()
                    || actual
                        .iter()
                        .zip(&session.sidecar_files)
                        .any(|(left, right)| {
                            left.relpath != right.relpath
                                || left.bytes != right.bytes
                                || left.sha256 != right.sha256
                        })
                {
                    return Err(AppError::Other(format!(
                        "Claude sidecar 文件清单或 sha256 校验失败: {}",
                        session.id
                    )));
                }
            }
        }
        for (label, root, expected) in [
            (
                "companions",
                session.companions_relpath.as_deref(),
                session.companion_files.as_slice(),
            ),
            (
                "tasks",
                session.tasks_relpath.as_deref(),
                session.task_files.as_slice(),
            ),
        ] {
            if let Some(root) = root {
                let root = backup.join(paths::checked_relative_path(root)?);
                path_safety::validate_tree(backup, &root, &format!("Claude 备份 {label}"))?;
                if manifest.version >= 5 {
                    let actual = collect_manifest_artifacts(&root)?;
                    if actual.len() != expected.len()
                        || actual.iter().zip(expected).any(|(left, right)| {
                            left.relpath != right.relpath
                                || left.bytes != right.bytes
                                || left.sha256 != right.sha256
                        })
                    {
                        return Err(AppError::Other(format!(
                            "Claude {label} 文件清单或 sha256 校验失败: {}",
                            session.id
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

fn load_backup_manifest(path: &Path) -> AppResult<Manifest> {
    let manifest_path = path.join("manifest.json");
    let metadata = fs::symlink_metadata(&manifest_path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::Path(format!(
            "备份 manifest 必须是普通文件且不能是符号链接: {}",
            manifest_path.to_string_lossy()
        )));
    }
    let manifest: Manifest = serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;
    validate_manifest_paths(&manifest)?;
    Ok(manifest)
}

fn validated_backup_path(backup_dir: &Path, backup_path: &Path) -> AppResult<PathBuf> {
    let root_metadata = fs::symlink_metadata(backup_dir)?;
    if path_safety::metadata_is_link_or_reparse(&root_metadata) || !root_metadata.is_dir() {
        return Err(AppError::Path(format!(
            "备份根目录必须是普通目录且不能是链接或 junction: {}",
            backup_dir.to_string_lossy()
        )));
    }
    let target_metadata = fs::symlink_metadata(backup_path)?;
    if path_safety::metadata_is_link_or_reparse(&target_metadata) || !target_metadata.is_dir() {
        return Err(AppError::Path(format!(
            "备份目标必须是普通目录且不能是符号链接: {}",
            backup_path.to_string_lossy()
        )));
    }
    let root = backup_dir.canonicalize()?;
    let target = backup_path.canonicalize()?;
    if target == root || target.parent() != Some(root.as_path()) {
        return Err(AppError::Path(format!(
            "备份目标必须是备份根目录的直接子目录: {}",
            backup_path.to_string_lossy()
        )));
    }
    load_backup_manifest(&target)?;
    Ok(target)
}

fn summarize_backup(path: &Path) -> AppResult<BackupSummary> {
    let manifest = load_backup_manifest(path)?;
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
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id) {
            continue;
        }
        if let Some(rows) = history_index.get(id) {
            lines.extend(rows.iter().cloned());
        }
    }
    let path = backup_dir.join("history.jsonl");
    let written = crate::history::write_lines(&path, &lines)?;
    if !path.exists() {
        let file = File::create(&path)?;
        file.sync_all()?;
    }
    Ok(written)
}

fn append_backup_history_if_present(destination: &Path, backup: &Path, id: &str) -> AppResult<u32> {
    let source = backup.join("history.jsonl");
    match fs::symlink_metadata(&source) {
        Ok(_) => crate::history::append_from_file(destination, &source, id),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

pub fn list_backups(backup_dir: String, provider: Option<String>) -> AppResult<Vec<BackupSummary>> {
    let root = PathBuf::from(&backup_dir);
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&root)? {
        let e = entry?;
        let p = e.path();
        let file_type = e.file_type()?;
        let metadata = fs::symlink_metadata(&p)?;
        if !file_type.is_dir() || path_safety::metadata_is_link_or_reparse(&metadata) {
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
            let s = summarize_backup(&p)?;
            if let Some(provider) = provider.as_deref() {
                let backup_provider = backup_provider(&s.provider);
                if backup_provider != provider {
                    continue;
                }
            }
            out.push(s);
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

pub fn open_backup(backup_dir: String, backup_path: String) -> AppResult<BackupDetail> {
    let p = validated_backup_path(Path::new(&backup_dir), Path::new(&backup_path))?;
    let summary = summarize_backup(&p)?;
    let manifest = load_backup_manifest(&p)?;
    validate_backup_payload(&p, &manifest, RolloutPresence::AllowMissing)?;
    Ok(BackupDetail { summary, manifest })
}

pub fn delete_backup(backup_dir: String, backup_path: String) -> AppResult<()> {
    let p = validated_backup_path(Path::new(&backup_dir), Path::new(&backup_path))?;
    let root = Path::new(&backup_dir).canonicalize()?;
    path_safety::remove_path(&root, &p, EntryKind::Directory, "备份目录")?;
    Ok(())
}

pub fn verify_backup(backup_dir: String, backup_path: String) -> AppResult<VerifyReport> {
    let p = validated_backup_path(Path::new(&backup_dir), Path::new(&backup_path))?;
    let manifest = load_backup_manifest(&p)?;
    validate_backup_payload(&p, &manifest, RolloutPresence::AllowMissing)?;
    let mut items = Vec::new();
    // v1-v3 没有覆盖 threads/logs/history 的哈希，不能把“rollout 校验通过”误报成
    // “整个备份完整性通过”。旧备份仍可逐项查看和还原，但整体状态保持未通过。
    let mut all_ok = manifest.version >= 4;
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

pub fn restore_session(
    provider: Option<String>,
    backup_dir: String,
    backup_path: String,
    codex_dir: String,
    claude_dir: Option<String>,
    id: String,
    backup_rollout_relpath: Option<String>,
    overwrite: bool,
) -> AppResult<RestoreResult> {
    restore_session_with_opencode(
        provider,
        backup_dir,
        backup_path,
        codex_dir,
        claude_dir,
        None,
        id,
        backup_rollout_relpath,
        overwrite,
    )
}

pub fn restore_session_with_opencode(
    provider: Option<String>,
    backup_dir: String,
    backup_path: String,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    id: String,
    backup_rollout_relpath: Option<String>,
    overwrite: bool,
) -> AppResult<RestoreResult> {
    restore_session_with_dirs(
        provider,
        backup_dir,
        backup_path,
        ProviderDirs {
            codex_dir,
            claude_dir,
            opencode_dir,
            ..ProviderDirs::default()
        },
        id,
        backup_rollout_relpath,
        overwrite,
    )
}

pub fn restore_session_with_dirs(
    provider: Option<String>,
    backup_dir: String,
    backup_path: String,
    dirs: ProviderDirs,
    id: String,
    backup_rollout_relpath: Option<String>,
    overwrite: bool,
) -> AppResult<RestoreResult> {
    let backup = validated_backup_path(Path::new(&backup_dir), Path::new(&backup_path))?;
    let codex = dirs.codex_path();
    let claude = dirs.claude_path();
    let opencode = dirs.opencode_path();
    let cursor = dirs.cursor_path();
    let manifest = load_backup_manifest(&backup)?;
    validate_backup_payload(&backup, &manifest, RolloutPresence::Required)?;
    let matches = manifest
        .sessions
        .iter()
        .filter(|session| session.id == id)
        .collect::<Vec<_>>();
    let target = if let Some(requested) = backup_rollout_relpath.as_deref() {
        let requested = paths::checked_relative_path(requested)?;
        matches
            .into_iter()
            .find(|session| {
                paths::checked_relative_path(&session.rollout_relpath)
                    .map(|relative| relative == requested)
                    .unwrap_or(false)
            })
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "manifest 中未找到精确备份会话: id={id} rollout_relpath={}",
                    requested.to_string_lossy()
                ))
            })?
    } else {
        match matches.as_slice() {
            [target] => *target,
            [] => return Err(AppError::NotFound(format!("manifest 中未找到 id: {id}"))),
            duplicates => {
                return Err(AppError::Other(format!(
                "manifest 中存在 {} 个同 ID 会话，还原必须提供精确 backup_rollout_relpath: {id}",
                duplicates.len()
            )))
            }
        }
    };
    let manifest_provider = manifest_session_provider(&manifest, target);
    if let Some(requested) = provider.as_deref() {
        if requested != manifest_provider {
            return Err(AppError::Other(format!(
                "备份 provider 为 {manifest_provider}，当前页面却请求按 {requested} 还原，已拒绝"
            )));
        }
    }
    let provider = provider.as_deref().unwrap_or(manifest_provider);
    match provider {
        PROVIDER_CLAUDE => restore_one_claude(&backup, &claude, target, overwrite),
        PROVIDER_OPENCODE => restore_one_opencode(&backup, &opencode, target, overwrite),
        PROVIDER_CURSOR => restore_one_cursor(&backup, &cursor, target, overwrite),
        _ => restore_one(&backup, &codex, target, overwrite),
    }
}

/// 产品入口必须持有共享 FamilyLock，覆盖完整 restore，避免归档来源账本并发丢写。
pub fn restore_session_with_lock(
    provider: Option<String>,
    backup_dir: String,
    backup_path: String,
    dirs: ProviderDirs,
    id: String,
    backup_rollout_relpath: Option<String>,
    overwrite: bool,
    lock: &crate::family::FamilyLock,
) -> AppResult<RestoreResult> {
    crate::family::with_lock(lock, |_guard| {
        restore_session_with_dirs(
            provider,
            backup_dir,
            backup_path,
            dirs,
            id,
            backup_rollout_relpath,
            overwrite,
        )
    })
}

pub fn restore_all(
    provider: Option<String>,
    backup_dir: String,
    backup_path: String,
    codex_dir: String,
    claude_dir: Option<String>,
    overwrite: bool,
) -> AppResult<Vec<RestoreResult>> {
    restore_all_with_opencode(
        provider,
        backup_dir,
        backup_path,
        codex_dir,
        claude_dir,
        None,
        overwrite,
    )
}

pub fn restore_all_with_opencode(
    provider: Option<String>,
    backup_dir: String,
    backup_path: String,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    overwrite: bool,
) -> AppResult<Vec<RestoreResult>> {
    restore_all_with_dirs(
        provider,
        backup_dir,
        backup_path,
        ProviderDirs {
            codex_dir,
            claude_dir,
            opencode_dir,
            ..ProviderDirs::default()
        },
        overwrite,
    )
}

pub fn restore_all_with_dirs(
    provider: Option<String>,
    backup_dir: String,
    backup_path: String,
    dirs: ProviderDirs,
    overwrite: bool,
) -> AppResult<Vec<RestoreResult>> {
    restore_selected_with_dirs(provider, backup_dir, backup_path, dirs, None, overwrite)
}

pub fn restore_selected_with_dirs(
    provider: Option<String>,
    backup_dir: String,
    backup_path: String,
    dirs: ProviderDirs,
    targets: Option<Vec<BackupRestoreTarget>>,
    overwrite: bool,
) -> AppResult<Vec<RestoreResult>> {
    let backup = validated_backup_path(Path::new(&backup_dir), Path::new(&backup_path))?;
    let codex = dirs.codex_path();
    let claude = dirs.claude_path();
    let opencode = dirs.opencode_path();
    let cursor = dirs.cursor_path();
    let manifest = load_backup_manifest(&backup)?;
    let selected = select_manifest_sessions(&manifest, targets.as_deref())?;
    validate_backup_payload_sessions(&backup, &manifest, &selected, RolloutPresence::Required)?;
    if let Some(requested) = provider.as_deref() {
        for session in &selected {
            let actual = manifest_session_provider(&manifest, session);
            if actual != requested {
                return Err(AppError::Other(format!(
                    "备份会话 {} 的 provider 为 {actual}，当前页面请求按 {requested} 还原，已拒绝",
                    session.id
                )));
            }
        }
    }
    let mut out = Vec::new();
    for s in selected {
        let session_provider = provider
            .as_deref()
            .unwrap_or_else(|| manifest_session_provider(&manifest, s));
        out.push(
            (match session_provider {
                PROVIDER_CLAUDE => restore_one_claude(&backup, &claude, s, overwrite),
                PROVIDER_OPENCODE => restore_one_opencode(&backup, &opencode, s, overwrite),
                PROVIDER_CURSOR => restore_one_cursor(&backup, &cursor, s, overwrite),
                _ => restore_one(&backup, &codex, s, overwrite),
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

/// 产品入口必须持有共享 FamilyLock，覆盖完整 restore-all，避免归档来源账本并发丢写。
pub fn restore_all_with_lock(
    provider: Option<String>,
    backup_dir: String,
    backup_path: String,
    dirs: ProviderDirs,
    overwrite: bool,
    lock: &crate::family::FamilyLock,
) -> AppResult<Vec<RestoreResult>> {
    restore_selected_with_lock(
        provider,
        backup_dir,
        backup_path,
        dirs,
        None,
        overwrite,
        lock,
    )
}

pub fn restore_selected_with_lock(
    provider: Option<String>,
    backup_dir: String,
    backup_path: String,
    dirs: ProviderDirs,
    targets: Option<Vec<BackupRestoreTarget>>,
    overwrite: bool,
    lock: &crate::family::FamilyLock,
) -> AppResult<Vec<RestoreResult>> {
    crate::family::with_lock(lock, |_guard| {
        restore_selected_with_dirs(provider, backup_dir, backup_path, dirs, targets, overwrite)
    })
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

fn select_manifest_sessions<'a>(
    manifest: &'a Manifest,
    targets: Option<&[BackupRestoreTarget]>,
) -> AppResult<Vec<&'a ManifestSession>> {
    let Some(targets) = targets else {
        return Ok(manifest.sessions.iter().collect());
    };
    if targets.is_empty() {
        return Err(AppError::Other("至少选择一个要还原的会话".to_string()));
    }

    let mut seen = HashSet::with_capacity(targets.len());
    let mut selected = Vec::with_capacity(targets.len());
    for target in targets {
        let key = (target.id.as_str(), target.backup_rollout_relpath.as_str());
        if !seen.insert(key) {
            return Err(AppError::Other(format!(
                "还原目标重复: id={} rollout_relpath={}",
                target.id, target.backup_rollout_relpath
            )));
        }
        let mut matches = manifest.sessions.iter().filter(|session| {
            session.id == target.id && session.rollout_relpath == target.backup_rollout_relpath
        });
        let session = matches.next().ok_or_else(|| {
            AppError::NotFound(format!(
                "备份中不存在精确还原目标: id={} rollout_relpath={}",
                target.id, target.backup_rollout_relpath
            ))
        })?;
        if matches.next().is_some() {
            return Err(AppError::Other(format!(
                "备份包含重复的精确还原目标: id={} rollout_relpath={}",
                target.id, target.backup_rollout_relpath
            )));
        }
        selected.push(session);
    }
    Ok(selected)
}

fn read_backup_log_rows(
    backup: &Path,
    target: &ManifestSession,
) -> AppResult<Vec<serde_json::Map<String, serde_json::Value>>> {
    let path = backup.join("logs.ndjson");
    if !path.is_file() {
        if target.logs_count == 0 {
            return Ok(Vec::new());
        }
        return Err(AppError::NotFound(format!(
            "备份缺少 logs.ndjson，会话 {} 声明有 {} 行日志",
            target.id, target.logs_count
        )));
    }
    let mut out = Vec::new();
    for (line_no, line) in BufReader::new(File::open(path)?).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line).map_err(|error| {
            AppError::Other(format!("logs.ndjson 第 {} 行损坏: {error}", line_no + 1))
        })?;
        let object = value.as_object().ok_or_else(|| {
            AppError::Other(format!("logs.ndjson 第 {} 行不是 JSON 对象", line_no + 1))
        })?;
        if object.get("thread_id").and_then(serde_json::Value::as_str) == Some(target.id.as_str()) {
            out.push(object.clone());
        }
    }
    if out.len() as u32 != target.logs_count {
        return Err(AppError::Other(format!(
            "logs.ndjson 会话 {} 行数与 manifest 不一致: expected={} actual={}",
            target.id,
            target.logs_count,
            out.len()
        )));
    }
    Ok(out)
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
    let target_rel = validate_claude_source_relpath(source_rel, &target.id)?;
    let backup_rel = paths::checked_relative_path(&target.rollout_relpath)?;
    let src = backup.join(&backup_rel);
    let dest = paths::claude_projects_dir(claude).join(&target_rel);
    let sidecar_dest = crate::claude_sessions::sidecar_path_for(&dest)
        .ok_or_else(|| AppError::Path("Claude 会话路径无法计算 sidecar".into()))?;
    let tasks_dest = crate::claude_sessions::task_path_for(claude, &target.id);

    fs::create_dir_all(claude)?;
    path_safety::validate_descendant(claude, &dest, EntryKind::File, true, "Claude 会话还原目标")?;
    path_safety::validate_descendant(
        claude,
        &sidecar_dest,
        EntryKind::FileOrDirectory,
        true,
        "Claude sidecar 还原目标",
    )?;
    path_safety::validate_descendant(
        claude,
        &tasks_dest,
        EntryKind::Directory,
        true,
        "Claude tasks 还原目标",
    )?;
    let companion_destinations = if dest.parent().is_some_and(Path::exists) {
        crate::claude_sessions::companion_files_for(&dest)?
    } else {
        Vec::new()
    };
    verify_restore_source(backup, &src, target, PROVIDER_CLAUDE)?;

    if (dest.exists()
        || sidecar_dest.exists()
        || !companion_destinations.is_empty()
        || tasks_dest.exists())
        && !overwrite
    {
        result.conflict = true;
        return Ok(result);
    }
    let sidecar_src = target
        .sidecar_relpath
        .as_deref()
        .map(paths::checked_relative_path)
        .transpose()?
        .map(|relative| backup.join(relative));
    let companions_src = target
        .companions_relpath
        .as_deref()
        .map(paths::checked_relative_path)
        .transpose()?
        .map(|relative| backup.join(relative));
    let tasks_src = target
        .tasks_relpath
        .as_deref()
        .map(paths::checked_relative_path)
        .transpose()?
        .map(|relative| backup.join(relative));
    crate::bundle::replace_claude_snapshot_with_extras_verified(
        &src,
        &dest,
        None,
        sidecar_src.as_deref(),
        companions_src.as_deref(),
        tasks_src.as_deref(),
        &tasks_dest,
        Some(&target.sha256_rollout),
    )?;
    result.rollout_copied = true;
    match append_backup_history_if_present(&paths::history_path(claude), backup, &target.id) {
        Ok(appended) => result.history_appended = appended,
        Err(error) => {
            result.error = Some(format!(
                "Claude transcript/sidecar 已还原，但 history 追加失败: {error}"
            ));
            return Ok(result);
        }
    }

    result.ok = true;
    Ok(result)
}

fn restore_one_opencode(
    backup: &Path,
    data_dir: &Path,
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
    let relative = validate_opencode_snapshot_relpath(&target.rollout_relpath, &target.id)?;
    let source = backup.join(relative);
    verify_restore_source(backup, &source, target, PROVIDER_OPENCODE)?;
    let snapshot = crate::opencode_transfer::read_snapshot(&source, &target.id)?;
    let outcome = crate::opencode_transfer::restore_snapshot(data_dir, &snapshot, overwrite)?;
    if !outcome.written {
        result.conflict = true;
        result.error = outcome.skipped_reason;
        return Ok(result);
    }
    result.rollout_copied = true;
    result.threads_inserted = true;
    result.ok = true;
    Ok(result)
}

fn restore_one_cursor(
    backup: &Path,
    cursor_dir: &Path,
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
    let relative = validate_cursor_snapshot_relpath(&target.rollout_relpath, &target.id)?;
    let source = backup.join(relative);
    verify_restore_source(backup, &source, target, PROVIDER_CURSOR)?;
    let snapshot = crate::cursor_transfer::read_snapshot(&source, &target.id)?;
    if !crate::cursor_transfer::import_snapshot(cursor_dir, &snapshot, overwrite)? {
        result.conflict = true;
        result.error = Some("目标库中已存在同 ID 会话".into());
        return Ok(result);
    }
    result.rollout_copied = true;
    result.threads_inserted = true;
    result.ok = true;
    Ok(result)
}

fn insert_restore_thread(
    transaction: &rusqlite::Transaction<'_>,
    codex: &Path,
    target_rel: &Path,
    row: &serde_json::Value,
) -> AppResult<()> {
    // 按目标表实际列还原：备份可能来自新/旧不同 schema，两个方向都取交集。
    let table_cols = crate::repair::threads_table_columns(transaction)?;
    if !table_cols.iter().any(|name| name == "id") {
        return Err(AppError::Other(
            "threads 表缺少 id 列，无法还原会话记录".into(),
        ));
    }
    let mut columns: Vec<String> = Vec::with_capacity(table_cols.len());
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(table_cols.len());
    for name in &table_cols {
        if name == "rollout_path" {
            columns.push(name.clone());
            values.push(Box::new(
                codex.join(target_rel).to_string_lossy().into_owned(),
            ));
            continue;
        }
        // 旧备份未捕获 preview/thread_source；目标库有列而备份缺值时按 App 规则补齐，
        // 否则还原出的行会因 preview = '' 在官方 App 列表中不可见。
        let value = match name.as_str() {
            "preview" => match row.get(name.as_str()) {
                Some(serde_json::Value::String(text)) if !text.is_empty() => {
                    serde_json::Value::String(text.clone())
                }
                _ => row
                    .get("first_user_message")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            },
            "thread_source" => match row.get(name.as_str()) {
                Some(serde_json::Value::String(text)) if !text.is_empty() => {
                    serde_json::Value::String(text.clone())
                }
                _ => {
                    let source = row.get("source").and_then(serde_json::Value::as_str);
                    serde_json::Value::String(
                        if crate::repair::is_subagent_source(source) {
                            "subagent"
                        } else {
                            "user"
                        }
                        .to_string(),
                    )
                }
            },
            _ => row
                .get(name.as_str())
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        };
        // 备份未捕获或为 NULL 的列交给表默认值/触发器，避免对 NOT NULL 列写显式 NULL。
        if value.is_null() {
            continue;
        }
        columns.push(name.clone());
        let parameter: Box<dyn rusqlite::ToSql> = match &value {
            serde_json::Value::Bool(value) => Box::new(if *value { 1i64 } else { 0i64 }),
            serde_json::Value::Number(number) => {
                if let Some(value) = number.as_i64() {
                    Box::new(value)
                } else if let Some(value) = number.as_f64() {
                    Box::new(value)
                } else {
                    Box::new(number.to_string())
                }
            }
            serde_json::Value::String(value) => Box::new(value.clone()),
            other => Box::new(other.to_string()),
        };
        values.push(parameter);
    }
    let columns_sql = columns.join(", ");
    let placeholders = (0..columns.len())
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("INSERT OR REPLACE INTO threads ({columns_sql}) VALUES ({placeholders})");
    let parameters = values
        .iter()
        .map(|value| value.as_ref())
        .collect::<Vec<_>>();
    let affected = transaction.execute(&sql, parameters.as_slice())?;
    if affected != 1 {
        return Err(AppError::Other(format!(
            "threads 还原写入行数异常: expected=1 actual={affected}"
        )));
    }
    Ok(())
}

fn portable_sql_parameter(value: &PortableSqliteValue) -> AppResult<Box<dyn rusqlite::ToSql>> {
    Ok(match value {
        PortableSqliteValue::Null => Box::new(Option::<String>::None),
        PortableSqliteValue::Integer(value) => Box::new(*value),
        PortableSqliteValue::Real(value) => Box::new(*value),
        PortableSqliteValue::Text(value) => Box::new(value.clone()),
        PortableSqliteValue::Blob(value) => Box::new(hex::decode(value).map_err(|error| {
            AppError::Other(format!(
                "thread_history.ndjson 包含无效十六进制 BLOB: {error}"
            ))
        })?),
    })
}

fn quoted_sql_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn attach_codex_thread_history_for_restore(
    state: &rusqlite::Connection,
    codex: &Path,
    backup_rows: &[CodexThreadHistoryBackupRow],
) -> AppResult<Option<HashMap<String, HashSet<String>>>> {
    let history_path = codex.join("thread_history_1.sqlite");
    let metadata = match fs::symlink_metadata(&history_path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if metadata.is_none() {
        if backup_rows.is_empty() {
            return Ok(None);
        }
        return Err(AppError::NotFound(format!(
            "备份包含 Codex 分页历史投影，但目标 thread_history_1.sqlite 不存在: {}",
            history_path.to_string_lossy()
        )));
    }
    let metadata = metadata.expect("checked above");
    if path_safety::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(AppError::Path(format!(
            "Codex thread history 数据库不是普通文件: {}",
            history_path.to_string_lossy()
        )));
    }
    state.execute(
        "ATTACH DATABASE ?1 AS restore_history",
        [history_path.to_string_lossy().into_owned()],
    )?;
    Ok(Some(validate_codex_thread_history_schema(
        state,
        "restore_history",
    )?))
}

fn codex_projection_owned_ids(session: &ManifestSession) -> AppResult<HashSet<String>> {
    let mut ids = HashSet::from([session.id.clone()]);
    let relative = paths::checked_relative_path(&session.rollout_relpath)?;
    if let Some(rollout_id) = codex_rollout_filename_id(&relative) {
        ids.insert(rollout_id);
    }
    Ok(ids)
}

fn insert_restore_thread_history_rows(
    transaction: &rusqlite::Transaction<'_>,
    rows: &[CodexThreadHistoryBackupRow],
    owned_ids: &HashSet<String>,
    overwrite: bool,
    allowed_columns: &HashMap<String, HashSet<String>>,
) -> AppResult<u32> {
    for table in CODEX_THREAD_HISTORY_TABLES {
        if overwrite {
            for thread_id in owned_ids {
                transaction.execute(
                    &format!("DELETE FROM restore_history.{table} WHERE thread_id = ?1"),
                    [thread_id],
                )?;
            }
        } else {
            for thread_id in owned_ids {
                let exists: bool = transaction.query_row(
                    &format!(
                        "SELECT EXISTS(SELECT 1 FROM restore_history.{table} WHERE thread_id = ?1)"
                    ),
                    [thread_id],
                    |row| row.get(0),
                )?;
                if exists {
                    return Err(AppError::Other(format!(
                        "目标 Codex home 已有孤立的分页历史投影，拒绝非覆盖还原: table={table} thread_id={thread_id}"
                    )));
                }
            }
        }
    }

    let mut inserted = 0u32;
    for row in rows {
        let allowed = allowed_columns
            .get(&row.table)
            .ok_or_else(|| AppError::Other(format!("目标 thread history 缺少表: {}", row.table)))?;
        if let Some(unknown) = row.values.keys().find(|column| !allowed.contains(*column)) {
            return Err(AppError::Other(format!(
                "thread_history.ndjson 包含目标 {} 表不存在的列: {unknown}",
                row.table
            )));
        }
        let columns = row.values.keys().cloned().collect::<Vec<_>>();
        let quoted_columns = columns
            .iter()
            .map(|column| quoted_sql_identifier(column))
            .collect::<Vec<_>>();
        let placeholders = (0..columns.len())
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let parameters = columns
            .iter()
            .map(|column| portable_sql_parameter(&row.values[column]))
            .collect::<AppResult<Vec<_>>>()?;
        let bound = parameters
            .iter()
            .map(|parameter| parameter.as_ref())
            .collect::<Vec<_>>();
        let affected = transaction.execute(
            &format!(
                "INSERT OR IGNORE INTO restore_history.{} ({}) VALUES ({placeholders})",
                row.table,
                quoted_columns.join(",")
            ),
            bound.as_slice(),
        )?;
        if affected == 1 {
            inserted = inserted
                .checked_add(1)
                .ok_or_else(|| AppError::Other("Codex thread history 还原行数溢出".into()))?;
            continue;
        }

        let predicates = quoted_columns
            .iter()
            .map(|column| format!("{column} IS ?"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let identical: bool = transaction.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM restore_history.{} WHERE {predicates})",
                row.table
            ),
            bound.as_slice(),
            |existing| existing.get(0),
        )?;
        if !identical {
            return Err(AppError::Other(format!(
                "目标 Codex home 已有主键相同但内容不同的分页历史投影: table={} thread_id={}",
                row.table,
                codex_thread_history_row_thread_id(row)?
            )));
        }
    }
    Ok(inserted)
}

fn insert_restore_logs(
    transaction: &rusqlite::Transaction<'_>,
    thread_id: &str,
    rows: &[serde_json::Map<String, serde_json::Value>],
    overwrite: bool,
) -> AppResult<u32> {
    if overwrite {
        transaction.execute(
            "DELETE FROM restore_logs.logs WHERE thread_id = ?",
            [thread_id],
        )?;
    }
    let mut inserted = 0u32;
    for object in rows {
        let keys = object.keys().cloned().collect::<Vec<_>>();
        let placeholders = (0..keys.len()).map(|_| "?").collect::<Vec<_>>().join(",");
        let quoted_keys = keys
            .iter()
            .map(|key| format!("\"{}\"", key.replace('"', "\"\"")))
            .collect::<Vec<_>>();
        let sql = format!(
            "INSERT INTO restore_logs.logs ({}) VALUES ({placeholders})",
            quoted_keys.join(",")
        );
        let mut parameters: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(keys.len());
        for key in &keys {
            let value = &object[key];
            let parameter: Box<dyn rusqlite::ToSql> = match value {
                serde_json::Value::Null => Box::new(Option::<String>::None),
                serde_json::Value::Bool(value) => Box::new(if *value { 1i64 } else { 0i64 }),
                serde_json::Value::Number(number) => {
                    if let Some(value) = number.as_i64() {
                        Box::new(value)
                    } else if let Some(value) = number.as_f64() {
                        Box::new(value)
                    } else {
                        Box::new(number.to_string())
                    }
                }
                serde_json::Value::String(value) => Box::new(value.clone()),
                other => Box::new(other.to_string()),
            };
            parameters.push(parameter);
        }
        let bound = parameters
            .iter()
            .map(|value| value.as_ref())
            .collect::<Vec<_>>();
        let affected = transaction.execute(&sql, bound.as_slice())?;
        if affected != 1 {
            return Err(AppError::Other(format!(
                "logs 还原写入行数异常: session={thread_id} expected=1 actual={affected}"
            )));
        }
        inserted = inserted
            .checked_add(1)
            .ok_or_else(|| AppError::Other("logs 还原行数溢出".into()))?;
    }
    Ok(inserted)
}

fn restore_thread_cwd<'a>(row: &'a serde_json::Value, target: &'a ManifestSession) -> &'a str {
    row.get("cwd")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&target.cwd)
}

fn restore_thread_host_cwd(
    codex: &Path,
    row: &serde_json::Value,
    target: &ManifestSession,
) -> String {
    paths::host_path_string_from_codex_record(codex, restore_thread_cwd(row, target))
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
    let target_rel = validate_codex_rollout_relpath(&target.rollout_relpath, &target.id)?;
    let src = backup.join(&target_rel);
    let dest = codex.join(&target_rel);

    path_safety::validate_descendant(
        codex,
        &dest,
        EntryKind::File,
        true,
        "Codex rollout 还原目标",
    )?;
    verify_restore_source(backup, &src, target, PROVIDER_CODEX)?;
    if dest.exists() && !overwrite {
        result.conflict = true;
        return Ok(result);
    }
    let history_base_restore_files =
        prepare_codex_history_base_restore_files(backup, codex, target)?;
    let backup_thread_history_rows = codex_thread_history_rows_for_session(backup, target)?;
    if crate::codex_projects::desktop_state_initialized(codex)? {
        crate::codex_projects::ensure_desktop_not_running(codex)?;
    }

    // 1) 在打开任何目标数据库前先解析备份行并验证 Desktop 项目状态。SQLite 打开
    // 本身可能更新 header/WAL，因此所有无需目标数据库的严格预检必须先完成。
    let threads_raw = fs::read_to_string(backup.join("threads.json"))?;
    let threads: Vec<serde_json::Value> = serde_json::from_str(&threads_raw)?;
    let matching_threads = threads
        .iter()
        .filter(|value| {
            value.get("id").and_then(|value| value.as_str()) == Some(target.id.as_str())
        })
        .collect::<Vec<_>>();
    let row = match matching_threads.as_slice() {
        [row] => *row,
        [] => {
            return Err(AppError::NotFound(format!(
                "threads.json 中缺 id: {}",
                target.id
            )))
        }
        duplicates => {
            return Err(AppError::Other(format!(
                "threads.json 中 id={} 存在 {} 行，拒绝不确定还原",
                target.id,
                duplicates.len()
            )))
        }
    };
    if crate::codex_projects::desktop_state_initialized(codex)? {
        crate::codex_projects::validate_thread_project_assignment(
            codex,
            &target.id,
            &restore_thread_host_cwd(codex, row, target),
        )?;
    }

    // 2) 冲突检测及依赖目标 schema 的日志校验。
    let mut state = state_db::open(codex)?;
    let thread_exists =
        match state.query_row("SELECT 1 FROM threads WHERE id = ?", [&target.id], |_| {
            Ok(true)
        }) {
            Ok(exists) => exists,
            Err(rusqlite::Error::QueryReturnedNoRows) => false,
            Err(error) => return Err(error.into()),
        };
    if (thread_exists || dest.exists()) && !overwrite {
        result.conflict = true;
        return Ok(result);
    }
    let backup_log_rows = read_backup_log_rows(backup, target)?;
    let logs_path = codex.join("logs_2.sqlite");
    let logs_exist = match fs::symlink_metadata(&logs_path) {
        Ok(metadata) => {
            if path_safety::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
                return Err(AppError::Path(format!(
                    "Codex logs 数据库不是普通文件: {}",
                    logs_path.to_string_lossy()
                )));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let restore_logs = !backup_log_rows.is_empty() || (overwrite && logs_exist);
    let allowed_log_columns = if restore_logs {
        if !logs_exist {
            return Err(AppError::NotFound(format!(
                "备份包含 Codex 日志，但目标 logs_2.sqlite 不存在: {}",
                logs_path.to_string_lossy()
            )));
        }
        let logs_connection = logs_db::open(codex)?;
        drop(logs_connection);
        let logs_path_string = logs_path.to_string_lossy().into_owned();
        state.execute("ATTACH DATABASE ?1 AS restore_logs", [&logs_path_string])?;
        let columns = {
            let mut statement = state.prepare("PRAGMA restore_logs.table_info(logs)")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
            rows.collect::<Result<HashSet<_>, _>>()?
        };
        if !columns.contains("thread_id") {
            return Err(AppError::Other("logs 表缺少还原所需 thread_id 列".into()));
        }
        columns
    } else {
        HashSet::new()
    };
    for object in &backup_log_rows {
        if object.get("thread_id").and_then(serde_json::Value::as_str) != Some(target.id.as_str()) {
            return Err(AppError::Other(format!(
                "logs.ndjson 含 thread_id 不匹配的待还原行: expected={}",
                target.id
            )));
        }
        if let Some(unknown) = object
            .keys()
            .find(|key| !allowed_log_columns.contains(*key))
        {
            return Err(AppError::Other(format!(
                "logs.ndjson 包含 logs 表不存在的列: {unknown}"
            )));
        }
    }
    let allowed_thread_history_columns =
        attach_codex_thread_history_for_restore(&state, codex, &backup_thread_history_rows)?;
    let projection_owned_ids = codex_projection_owned_ids(target)?;
    // 3) threads、logs 与分页历史先在同一 SQLite 事务中实际执行所有约束，但暂不提交。
    let transaction = state.transaction()?;
    let database_stage = (|| -> AppResult<()> {
        insert_restore_thread(&transaction, codex, &target_rel, row)?;
        result.threads_inserted = true;
        if restore_logs {
            result.logs_inserted =
                insert_restore_logs(&transaction, &target.id, &backup_log_rows, overwrite)?;
        }
        if let Some(allowed_columns) = allowed_thread_history_columns.as_ref() {
            insert_restore_thread_history_rows(
                &transaction,
                &backup_thread_history_rows,
                &projection_owned_ids,
                overwrite,
                allowed_columns,
            )?;
        }
        Ok(())
    })();
    if let Err(error) = database_stage {
        let rollback_error = transaction.rollback().err();
        if rollback_error.is_none() {
            result.threads_inserted = false;
            result.logs_inserted = 0;
        }
        result.error = Some(restore_failure_message(
            format!("Codex 数据库还原约束失败，事务已回滚: {error}"),
            rollback_error,
            Vec::new(),
            None,
        ));
        return Ok(result);
    }

    // 4) 为所有即将改写的文件创建一致快照。后续任一步失败均按变更后指纹补偿。
    let history_path = paths::history_path(codex);
    let index_path = paths::session_index_path(codex);
    let global_state_path = paths::codex_global_state_json_path(codex);
    let mut snapshot_paths = vec![("rollout".to_string(), dest.clone())];
    snapshot_paths.extend(
        history_base_restore_files
            .iter()
            .filter(|dependency| dependency.copy_required)
            .map(|dependency| {
                (
                    dependency.label.clone(),
                    dependency.destination_path.clone(),
                )
            }),
    );
    snapshot_paths.extend([
        ("history".to_string(), history_path.clone()),
        ("session index".to_string(), index_path.clone()),
        ("Codex global state".to_string(), global_state_path.clone()),
    ]);
    let mut snapshots = match RestoreFileSnapshots::capture_owned(&snapshot_paths) {
        Ok(snapshots) => snapshots,
        Err(error) => {
            let rollback_error = transaction.rollback().err();
            if rollback_error.is_none() {
                result.threads_inserted = false;
                result.logs_inserted = 0;
            }
            result.error = Some(restore_failure_message(
                format!("创建 Codex 还原补偿快照失败: {error}"),
                rollback_error,
                Vec::new(),
                None,
            ));
            return Ok(result);
        }
    };

    let mut project_state_receipt = None;
    let file_stage = (|| -> AppResult<()> {
        for dependency in history_base_restore_files
            .iter()
            .filter(|dependency| dependency.copy_required)
        {
            snapshots.start(&dependency.label)?;
            if let Err(error) = copy_restore_file_atomically(
                codex,
                &dependency.source_path,
                &dependency.destination_path,
                &dependency.sha256,
                "Codex history_base 还原目标",
            ) {
                snapshots.record_failure(&dependency.label, &error)?;
                return Err(error);
            }
            snapshots.finish(&dependency.label)?;
        }

        snapshots.start("rollout")?;
        if let Err(error) = copy_restore_file_atomically(
            codex,
            &src,
            &dest,
            &target.sha256_rollout,
            "Codex rollout 还原目标",
        ) {
            snapshots.record_failure("rollout", &error)?;
            return Err(error);
        }
        result.rollout_copied = true;
        snapshots.finish("rollout")?;

        snapshots.start("history")?;
        result.history_appended =
            match append_backup_history_if_present(&history_path, backup, &target.id) {
                Ok(appended) => appended,
                Err(error) => {
                    snapshots.record_failure("history", &error)?;
                    return Err(error);
                }
            };
        if result.history_appended > 0 {
            snapshots.finish("history")?;
        }

        snapshots.start("session index")?;
        if let Err(error) = inject_restore_file_fault("session index") {
            snapshots.record_failure("session index", &error)?;
            return Err(error);
        }
        if let Err(error) =
            crate::repair::append_index_line(codex, &target.id, &target.title, &dest)
        {
            snapshots.record_failure("session index", &error)?;
            return Err(error);
        }
        snapshots.finish("session index")?;

        // 全局状态在一致快照时尚不存在，说明 Desktop 未初始化该 Codex home。
        // 此时必须保持 no-op；若文件随后由 Desktop 并发创建，也不能把它误当作
        // 本次还原生成的文件并在补偿时删除。
        if snapshots.was_present("Codex global state")? {
            snapshots.start("Codex global state")?;
            project_state_receipt =
                crate::codex_projects::sync_thread_project_assignment_with_receipt(
                    codex,
                    &target.id,
                    &restore_thread_host_cwd(codex, row, target),
                )?;
        }
        Ok(())
    })();
    if let Err(error) = file_stage {
        let rollback_error = transaction.rollback().err();
        let mut compensation_errors = Vec::new();
        if let Some(receipt) = project_state_receipt.as_ref() {
            if let Err(error) = receipt.compensate() {
                compensation_errors.push(format!("补偿 Codex global state 失败: {error}"));
            }
        }
        compensation_errors.extend(snapshots.compensate_except(&["Codex global state"]));
        let cleanup_error = snapshots.cleanup().err();
        if rollback_error.is_none() {
            result.threads_inserted = false;
            result.logs_inserted = 0;
        }
        if compensation_errors.is_empty() {
            result.rollout_copied = false;
            result.history_appended = 0;
        }
        result.error = Some(restore_failure_message(
            format!("Codex 文件还原未完成，已执行补偿: {error}"),
            rollback_error,
            compensation_errors,
            cleanup_error,
        ));
        return Ok(result);
    }

    // 5) 文件全部落盘后一次提交 threads 与附加的 logs 数据库。
    if let Err(error) = transaction.commit() {
        let mut compensation_errors = Vec::new();
        if let Some(receipt) = project_state_receipt.as_ref() {
            if let Err(error) = receipt.compensate() {
                compensation_errors.push(format!("补偿 Codex global state 失败: {error}"));
            }
        }
        compensation_errors.extend(snapshots.compensate_except(&["Codex global state"]));
        let cleanup_error = snapshots.cleanup().err();
        if compensation_errors.is_empty() {
            result.rollout_copied = false;
            result.history_appended = 0;
        }
        result.error = Some(restore_failure_message(
            format!("提交 Codex 数据库还原事务失败，数据库最终状态可能不确定，已补偿文件: {error}"),
            None,
            compensation_errors,
            cleanup_error,
        ));
        return Ok(result);
    }

    result.ok = true;
    // 归档来源账本：还原到 archived_sessions/ 的会话记录为 Restore（D10）。
    // 无 MutationJournal（RestoreFileSnapshots 补偿），因此放在 commit 之后最后一步，
    // 失败仅记入 result.error，不回滚已成功的主流程（与 cleanup 失败同模式）。
    if target_rel.starts_with("archived_sessions") {
        if let Err(error) = crate::archive_ledger::record(
            codex,
            &target.id,
            ArchiveOrigin::Restore,
            Some(chrono::Utc::now().timestamp()),
            Some(dest.to_string_lossy().into_owned()),
            Some(target.sha256_rollout.clone()),
        ) {
            result.error = Some(format!("Codex 会话已完整还原，但登记归档来源失败: {error}"));
        }
    }
    if let Err(error) = snapshots.cleanup() {
        result.error = Some(format!("Codex 会话已完整还原，但 {error}"));
    }
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
    for entry in walkdir::WalkDir::new(from).follow_links(false) {
        let entry = entry.map_err(|error| {
            AppError::Other(format!(
                "遍历待复制路径失败 {}: {error}",
                from.to_string_lossy()
            ))
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "待复制路径包含符号链接或 junction，已拒绝: {}",
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
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dest)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &dest)?;
        } else {
            return Err(AppError::Path(format!(
                "待复制路径包含不支持的文件类型: {}",
                entry.path().to_string_lossy()
            )));
        }
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
    use super::restore_snapshot::RestoreFileTestFaultGuard;
    use super::*;

    fn selection_manifest_session(id: &str, rollout_relpath: &str) -> ManifestSession {
        ManifestSession {
            provider: Some(PROVIDER_CODEX.to_string()),
            id: id.to_string(),
            rollout_relpath: rollout_relpath.to_string(),
            history_base_rollouts: Vec::new(),
            source_relpath: None,
            sidecar_relpath: None,
            sidecar_files: Vec::new(),
            companions_relpath: None,
            companion_files: Vec::new(),
            tasks_relpath: None,
            task_files: Vec::new(),
            title: id.to_string(),
            cwd: String::new(),
            created_at: 0,
            updated_at: 0,
            tokens_used: 0,
            model: None,
            bytes_rollout: 0,
            logs_count: 0,
            history_rows: 0,
            sha256_rollout: String::new(),
        }
    }

    fn selection_manifest(sessions: Vec<ManifestSession>) -> Manifest {
        Manifest {
            version: 5,
            provider: Some(PROVIDER_CODEX.to_string()),
            created_at: "2026-09-01T00:00:00Z".to_string(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            codex_dir: String::new(),
            claude_dir: None,
            opencode_dir: None,
            note: None,
            artifacts: Vec::new(),
            sessions,
        }
    }

    #[test]
    fn selective_restore_matches_duplicate_ids_by_rollout_relpath() -> AppResult<()> {
        let first_relpath = "sessions/2026/09/01/rollout-shared-first.jsonl";
        let second_relpath = "sessions/2026/09/01/rollout-shared-second.jsonl";
        let manifest = selection_manifest(vec![
            selection_manifest_session("shared", first_relpath),
            selection_manifest_session("shared", second_relpath),
        ]);
        let targets = vec![BackupRestoreTarget {
            id: "shared".to_string(),
            backup_rollout_relpath: second_relpath.to_string(),
        }];

        let selected = select_manifest_sessions(&manifest, Some(&targets))?;

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].rollout_relpath, second_relpath);
        Ok(())
    }

    #[test]
    fn selective_restore_rejects_empty_or_unknown_targets() {
        let manifest = selection_manifest(vec![selection_manifest_session(
            "known",
            "sessions/rollout-known.jsonl",
        )]);

        assert!(select_manifest_sessions(&manifest, Some(&[]))
            .expect_err("empty selection must be rejected")
            .to_string()
            .contains("至少选择"));
        let unknown = vec![BackupRestoreTarget {
            id: "known".to_string(),
            backup_rollout_relpath: "sessions/rollout-other.jsonl".to_string(),
        }];
        assert!(select_manifest_sessions(&manifest, Some(&unknown))
            .expect_err("unknown exact target must be rejected")
            .to_string()
            .contains("不存在精确还原目标"));
    }

    #[test]
    fn selective_restore_does_not_require_unselected_rollout_payloads() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-selective-restore-payload-test");
        let backup_root = root.join("backups");
        let backup = backup_root.join("full-backup");
        let codex = root.join("codex");
        let selected_id = "selected-session";
        let selected_relpath = PathBuf::from("sessions/2026/09/01/rollout-selected-session.jsonl");
        let selected = write_codex_restore_backup(
            &backup,
            selected_id,
            &selected_relpath,
            serde_json::json!({
                "id": selected_id,
                "rollout_path": backup.join(&selected_relpath).to_string_lossy(),
                "cwd": r"F:\work\selected",
                "title": "selected"
            }),
            r"F:\work\selected",
        )?;
        let mut unselected = selection_manifest_session(
            "unselected-session",
            "sessions/2026/09/01/rollout-unselected-session.jsonl",
        );
        unselected.sha256_rollout = "missing-on-purpose".to_string();
        let manifest = Manifest {
            version: 3,
            provider: Some(PROVIDER_CODEX.to_string()),
            created_at: "2026-09-01T00:00:00Z".to_string(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            codex_dir: String::new(),
            claude_dir: None,
            opencode_dir: None,
            note: None,
            artifacts: Vec::new(),
            sessions: vec![selected, unselected],
        };
        fs::write(
            backup.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        fs::create_dir_all(&codex)?;
        let state = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        state.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT)",
            [],
        )?;
        drop(state);

        let results = restore_selected_with_dirs(
            Some(PROVIDER_CODEX.to_string()),
            backup_root.to_string_lossy().into_owned(),
            backup.to_string_lossy().into_owned(),
            ProviderDirs::new(codex.to_string_lossy().into_owned()),
            Some(vec![BackupRestoreTarget {
                id: selected_id.to_string(),
                backup_rollout_relpath: selected_relpath.to_string_lossy().replace('\\', "/"),
            }]),
            false,
        )?;

        assert_eq!(results.len(), 1);
        assert!(results[0].ok, "{:?}", results[0].error);
        assert_eq!(results[0].id, selected_id);
        assert!(!codex
            .join("sessions/2026/09/01/rollout-unselected-session.jsonl")
            .exists());
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    fn assert_waits_for_family_lock<T: Send + 'static>(
        run: impl FnOnce(std::sync::Arc<crate::family::FamilyLock>) -> AppResult<T> + Send + 'static,
    ) {
        let lock = std::sync::Arc::new(crate::family::FamilyLock::default());
        let guard = lock.0.lock().unwrap();
        let worker_lock = std::sync::Arc::clone(&lock);
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender.send(run(worker_lock).map(|_| ())).unwrap();
        });

        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(guard);
        assert!(receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("restore should continue after releasing FamilyLock")
            .is_err());
        worker.join().unwrap();
    }

    #[test]
    fn restore_product_entrypoints_wait_for_family_lock() {
        assert_waits_for_family_lock(|lock| {
            restore_session_with_lock(
                None,
                "missing-backup-root".into(),
                "missing-backup".into(),
                ProviderDirs::new("missing-codex".into()),
                "missing-session".into(),
                None,
                false,
                &lock,
            )
        });
        assert_waits_for_family_lock(|lock| {
            restore_all_with_lock(
                None,
                "missing-backup-root".into(),
                "missing-backup".into(),
                ProviderDirs::new("missing-codex".into()),
                false,
                &lock,
            )
        });
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ))
    }

    fn create_directory_link(target: &Path, link: &Path) -> AppResult<()> {
        #[cfg(windows)]
        {
            match std::os::windows::fs::symlink_dir(target, link) {
                Ok(()) => return Ok(()),
                Err(error) if error.raw_os_error() == Some(1314) => {
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
                        return Ok(());
                    }
                    return Err(AppError::Other(format!(
                        "无法创建 junction 测试夹具: {}",
                        String::from_utf8_lossy(&output.stderr)
                    )));
                }
                Err(error) => return Err(error.into()),
            }
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)?;
            Ok(())
        }
    }

    fn remove_directory_link(link: &Path) -> AppResult<()> {
        #[cfg(windows)]
        {
            fs::remove_dir(link)?;
        }
        #[cfg(unix)]
        {
            fs::remove_file(link)?;
        }
        Ok(())
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

    fn write_opencode_database(root: &Path, cwd: &Path, title: &str) -> AppResult<()> {
        fs::create_dir_all(root)?;
        fs::create_dir_all(cwd)?;
        let connection = rusqlite::Connection::open(crate::opencode_sessions::database_path(root))?;
        connection.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL, vcs TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, sandboxes TEXT NOT NULL);
             INSERT INTO project VALUES ('global','/',NULL,1,1,'[]');
             CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES project(id), parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER, path TEXT, workspace_id TEXT);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE, session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE todo (session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE, position INTEGER NOT NULL, content TEXT NOT NULL, PRIMARY KEY(session_id, position));
             CREATE TABLE session_share (session_id TEXT PRIMARY KEY REFERENCES session(id) ON DELETE CASCADE, id TEXT NOT NULL, secret TEXT NOT NULL, url TEXT NOT NULL);",
        )?;
        connection.execute(
            "INSERT INTO session (id,project_id,parent_id,slug,directory,title,version,time_created,time_updated,time_archived,path,workspace_id) VALUES ('ses_backup','global',NULL,'backup',?1,?2,'1.0',1000,4000,NULL,NULL,NULL)",
            rusqlite::params![cwd.to_string_lossy().as_ref(), title],
        )?;
        connection.execute(
            "INSERT INTO message VALUES ('msg_backup','ses_backup',1000,1000,?1)",
            [serde_json::json!({"role":"user"}).to_string()],
        )?;
        connection.execute(
            "INSERT INTO part VALUES ('part_backup','msg_backup','ses_backup',1000,1000,?1)",
            [serde_json::json!({"type":"text","text":"portable opencode content"}).to_string()],
        )?;
        connection.execute(
            "INSERT INTO todo VALUES ('ses_backup',0,'portable task')",
            [],
        )?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn codex_backup_exact_target_accepts_equivalent_windows_path_forms() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-windows-path-identity-test");
        fs::create_dir_all(&root)?;
        let rollout = root.join("Rollout-Path-Identity.jsonl");
        fs::write(&rollout, b"fixture")?;
        let verbatim = PathBuf::from(format!(r"\\?\{}", rollout.to_string_lossy()));
        let case_variant = PathBuf::from(rollout.to_string_lossy().to_uppercase());

        assert!(same_existing_path(&verbatim, &case_variant)?);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    fn write_codex_restore_backup(
        backup: &Path,
        id: &str,
        relative: &Path,
        thread_row: serde_json::Value,
        manifest_cwd: &str,
    ) -> AppResult<ManifestSession> {
        let source = backup.join(relative);
        fs::create_dir_all(source.parent().unwrap_or(backup))?;
        let rollout_cwd = thread_row
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(manifest_cwd);
        let session_meta = serde_json::json!({
            "timestamp": "2026-07-10T00:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "model_provider": "openai",
                "cwd": rollout_cwd
            }
        });
        fs::write(
            &source,
            format!("{}\n", serde_json::to_string(&session_meta)?),
        )?;
        fs::write(
            backup.join("threads.json"),
            serde_json::to_vec_pretty(&vec![thread_row.clone()])?,
        )?;
        Ok(ManifestSession {
            provider: Some(PROVIDER_CODEX.to_string()),
            id: id.to_string(),
            rollout_relpath: relative.to_string_lossy().replace('\\', "/"),
            history_base_rollouts: Vec::new(),
            source_relpath: None,
            sidecar_relpath: None,
            sidecar_files: Vec::new(),
            companions_relpath: None,
            companion_files: Vec::new(),
            tasks_relpath: None,
            task_files: Vec::new(),
            title: thread_row
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            cwd: manifest_cwd.to_string(),
            created_at: 0,
            updated_at: 0,
            tokens_used: 0,
            model: None,
            bytes_rollout: fs::metadata(&source)?.len(),
            logs_count: 0,
            history_rows: 0,
            sha256_rollout: sha256_file(&source)?,
        })
    }

    fn attach_history_base_to_restore_backup(
        backup: &Path,
        target: &mut ManifestSession,
        history_base_id: &str,
        history_base_relative: &Path,
    ) -> AppResult<Vec<u8>> {
        let primary = backup.join(&target.rollout_relpath);
        let mut meta: serde_json::Value =
            serde_json::from_str(fs::read_to_string(&primary)?.trim())?;
        meta["payload"]["history_mode"] = serde_json::Value::String("paginated".to_string());
        meta["payload"]["history_base"] = serde_json::json!({"thread_id": history_base_id});
        fs::write(&primary, format!("{meta}\n"))?;
        target.bytes_rollout = fs::metadata(&primary)?.len();
        target.sha256_rollout = sha256_file(&primary)?;

        let history_base = backup.join(history_base_relative);
        fs::create_dir_all(history_base.parent().unwrap_or(backup))?;
        fs::write(
            &history_base,
            format!(
                "{}\n",
                serde_json::json!({
                    "timestamp": "2026-07-09T00:00:00Z",
                    "type": "session_meta",
                    "payload": {
                        "id": history_base_id,
                        "model_provider": "openai",
                        "cwd": r"F:\work\restored",
                        "history_mode": "paginated"
                    }
                })
            ),
        )?;
        let bytes = fs::read(&history_base)?;
        target.history_base_rollouts = vec![ManifestArtifact {
            relpath: history_base_relative.to_string_lossy().replace('\\', "/"),
            bytes: bytes.len() as u64,
            sha256: sha256_file(&history_base)?,
        }];
        Ok(bytes)
    }

    fn create_minimal_codex_restore_state(codex: &Path) -> AppResult<()> {
        fs::create_dir_all(codex)?;
        let state = rusqlite::Connection::open(paths::state_db_path(codex))?;
        state.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT)",
            [],
        )?;
        Ok(())
    }

    fn create_codex_thread_history_fixture(codex: &Path) -> AppResult<rusqlite::Connection> {
        let connection = rusqlite::Connection::open(codex.join("thread_history_1.sqlite"))?;
        connection.execute_batch(
            "CREATE TABLE thread_turns (
                thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, rollout_ordinal INTEGER NOT NULL,
                status TEXT NOT NULL, rollout_byte_offset INTEGER, rollout_end_ordinal INTEGER,
                rollout_end_byte_offset INTEGER, PRIMARY KEY(thread_id, turn_id)
             );
             CREATE TABLE thread_items (
                thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, item_id TEXT NOT NULL,
                rollout_ordinal INTEGER NOT NULL, created_at_ms INTEGER NOT NULL,
                item_json TEXT NOT NULL, item_type TEXT NOT NULL DEFAULT '',
                updated_at_ordinal INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(thread_id, turn_id, item_id)
             );
             CREATE TABLE thread_history_projection_state (
                thread_id TEXT PRIMARY KEY, next_rollout_byte_offset INTEGER NOT NULL,
                next_rollout_ordinal INTEGER NOT NULL
             );
             CREATE TABLE thread_realtime_items (
                thread_id TEXT NOT NULL, item_id TEXT NOT NULL, rollout_ordinal INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL, item_type TEXT NOT NULL, item_json TEXT NOT NULL,
                PRIMARY KEY(thread_id, item_id)
             );",
        )?;
        Ok(connection)
    }

    #[test]
    fn modified_rollouts_omit_projection_rows_for_official_reprojection() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-modified-projection-backup-test");
        fs::create_dir_all(&root)?;
        let modified_id = "modified-rollout";
        let preserved_id = "preserved-rollout";
        let history = create_codex_thread_history_fixture(&root)?;
        history.execute(
            "INSERT INTO thread_turns
                (thread_id, turn_id, rollout_ordinal, status, rollout_byte_offset,
                 rollout_end_ordinal, rollout_end_byte_offset)
             VALUES (?1, 'turn-1', 1, 'completed', 10, 2, 20)",
            [modified_id],
        )?;
        history.execute(
            "INSERT INTO thread_items
                (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json,
                 item_type, updated_at_ordinal)
             VALUES (?1, 'turn-1', 'item-1', 1, 1000, '{}', 'userMessage', 1)",
            [modified_id],
        )?;
        history.execute(
            "INSERT INTO thread_history_projection_state VALUES (?1, 30, 3)",
            [modified_id],
        )?;
        history.execute(
            "INSERT INTO thread_realtime_items
                (thread_id, item_id, rollout_ordinal, created_at_ms, item_type, item_json)
             VALUES (?1, 'realtime-1', 2, 1001, 'realtime_session_started', '{}')",
            [modified_id],
        )?;
        history.execute(
            "INSERT INTO thread_history_projection_state VALUES (?1, 40, 4)",
            [preserved_id],
        )?;
        drop(history);

        let destination = root.join(CODEX_THREAD_HISTORY_FILE);
        let projection_ids = HashSet::from([modified_id.to_string(), preserved_id.to_string()]);
        let modified_ids = HashSet::from([modified_id.to_string()]);
        assert_eq!(
            export_codex_thread_history(&root, &destination, &projection_ids, &modified_ids,)?,
            1
        );
        let rows = read_codex_thread_history_backup_rows(&root)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(codex_thread_history_row_thread_id(&rows[0])?, preserved_id);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn restore_thread_host_cwd_maps_wsl_record_before_desktop_assignment() {
        let codex = Path::new(r"\\wsl.localhost\Ubuntu\home\alice\.codex");
        let row = serde_json::json!({"cwd": "/home/alice/work/repo"});
        let target = ManifestSession {
            provider: Some(PROVIDER_CODEX.to_string()),
            id: "wsl-restore".to_string(),
            rollout_relpath: "sessions/rollout-wsl-restore.jsonl".to_string(),
            history_base_rollouts: Vec::new(),
            source_relpath: None,
            sidecar_relpath: None,
            sidecar_files: Vec::new(),
            companions_relpath: None,
            companion_files: Vec::new(),
            tasks_relpath: None,
            task_files: Vec::new(),
            title: String::new(),
            cwd: "/manifest/fallback".to_string(),
            created_at: 0,
            updated_at: 0,
            tokens_used: 0,
            model: None,
            bytes_rollout: 0,
            logs_count: 0,
            history_rows: 0,
            sha256_rollout: String::new(),
        };

        assert_eq!(
            restore_thread_host_cwd(codex, &row, &target),
            r"\\wsl.localhost\Ubuntu\home\alice\work\repo"
        );
    }

    #[test]
    fn codex_backup_materializes_pending_desktop_move_without_source_project_id() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-codex-pending-move-backup-test");
        let codex = root.join("source-codex");
        let backups = root.join("backups");
        let id = "pending-desktop-move-backup";
        let source_rollout = codex
            .join("sessions/2026/08/13")
            .join(format!("rollout-2026-08-13T10-00-00-{id}.jsonl"));
        fs::create_dir_all(source_rollout.parent().unwrap_or(&codex))?;
        let stale_core_cwd = r"F:\old-project";
        let pending_host_cwd = r"F:\new-project";
        let source_lines = [
            serde_json::json!({
                "timestamp": "2026-08-13T10:00:00Z",
                "type": "session_meta",
                "payload": {"id": id, "cwd": stale_core_cwd, "model_provider": "openai"}
            }),
            serde_json::json!({
                "timestamp": "2026-08-13T10:00:01Z",
                "type": "session_meta",
                "payload": {"id": "ancestor-thread", "cwd": r"F:\ancestor"}
            }),
            serde_json::json!({
                "timestamp": "2026-08-13T10:00:01.100Z",
                "type": "turn_context",
                "payload": {"cwd": r"F:\historical-project"}
            }),
            serde_json::json!({
                "timestamp": "2026-08-13T10:00:01.200Z",
                "type": "turn_context",
                "payload": {"cwd": stale_core_cwd}
            }),
            serde_json::json!({
                "timestamp": "2026-08-13T10:00:02Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "keep"}
            }),
        ];
        fs::write(
            &source_rollout,
            format!(
                "{}\n",
                source_lines
                    .iter()
                    .map(serde_json::Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )?;
        let source_before = fs::read(&source_rollout)?;

        let state = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        state.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT,
                created_at INTEGER, updated_at INTEGER, tokens_used INTEGER, model TEXT
            );",
        )?;
        state.execute(
            "INSERT INTO threads VALUES (?1, ?2, ?3, 'pending move', 1, 2, 3, 'gpt-5')",
            rusqlite::params![
                id,
                source_rollout.to_string_lossy().into_owned(),
                pending_host_cwd
            ],
        )?;
        drop(state);
        fs::write(
            paths::codex_global_state_json_path(&codex),
            serde_json::to_vec_pretty(&serde_json::json!({
                "local-projects": {
                    "source-machine-project-id": {
                        "id": "source-machine-project-id",
                        "name": "new-project",
                        "rootPaths": [pending_host_cwd]
                    }
                },
                "thread-project-assignments": {
                    (id): {
                        "projectKind": "local",
                        "projectId": "source-machine-project-id",
                        "cwd": pending_host_cwd,
                        "pendingCoreUpdate": true
                    }
                }
            }))?,
        )?;

        let summary = create_backup(
            Some(PROVIDER_CODEX.to_string()),
            codex.to_string_lossy().into_owned(),
            None,
            backups.to_string_lossy().into_owned(),
            vec![id.to_string()],
            None,
            Some("pending-move".to_string()),
            None,
        )?;
        let backup = PathBuf::from(&summary.path);
        let manifest = load_backup_manifest(&backup)?;
        assert_eq!(manifest.sessions[0].cwd, pending_host_cwd);

        let threads: Vec<serde_json::Value> =
            serde_json::from_slice(&fs::read(backup.join("threads.json"))?)?;
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0]["cwd"], pending_host_cwd);
        assert!(
            !serde_json::to_string(&threads[0])?.contains("source-machine-project-id"),
            "threads.json must never carry the source projectId"
        );

        let backed_up_rollout = backup.join(&manifest.sessions[0].rollout_relpath);
        let metas = fs::read_to_string(&backed_up_rollout)?
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|value| value["type"] == "session_meta")
            .collect::<Vec<_>>();
        assert_eq!(metas[0]["payload"]["id"], id);
        assert_eq!(metas[0]["payload"]["cwd"], pending_host_cwd);
        assert_eq!(metas[1]["payload"]["id"], "ancestor-thread");
        assert_eq!(metas[1]["payload"]["cwd"], r"F:\ancestor");
        let turns = fs::read_to_string(&backed_up_rollout)?
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|value| value["type"] == "turn_context")
            .collect::<Vec<_>>();
        assert_eq!(turns[0]["payload"]["cwd"], r"F:\historical-project");
        assert_eq!(turns[1]["payload"]["cwd"], pending_host_cwd);
        assert_eq!(fs::read(&source_rollout)?, source_before);

        // Restore on a different Codex home: membership is re-derived from the target machine's
        // local-project root, and the source machine projectId is not reused.
        let target_codex = root.join("target-codex");
        fs::create_dir_all(&target_codex)?;
        let target_state = rusqlite::Connection::open(paths::state_db_path(&target_codex))?;
        target_state.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT)",
            [],
        )?;
        drop(target_state);
        fs::write(
            paths::codex_global_state_json_path(&target_codex),
            serde_json::to_vec_pretty(&serde_json::json!({
                "local-projects": {
                    "target-machine-project-id": {
                        "id": "target-machine-project-id",
                        "name": "new-project",
                        "rootPaths": [pending_host_cwd]
                    }
                },
                "project-order": []
            }))?,
        )?;

        let restored = restore_one(&backup, &target_codex, &manifest.sessions[0], false)?;
        assert!(restored.ok, "restore failed: {:?}", restored.error);
        let restored_global: serde_json::Value = serde_json::from_slice(&fs::read(
            paths::codex_global_state_json_path(&target_codex),
        )?)?;
        let restored_assignment = &restored_global["thread-project-assignments"][id];
        assert_eq!(
            restored_assignment["projectId"],
            "target-machine-project-id"
        );
        assert_eq!(restored_assignment["cwd"], pending_host_cwd);
        assert_eq!(restored_assignment["pendingCoreUpdate"], false);
        let restored_state = state_db::open_ro(&target_codex)?;
        let restored_cwd: String =
            restored_state.query_row("SELECT cwd FROM threads WHERE id = ?", [id], |row| {
                row.get(0)
            })?;
        assert_eq!(restored_cwd, pending_host_cwd);
        let restored_rollout = target_codex.join(&manifest.sessions[0].rollout_relpath);
        let restored_meta = crate::family::read_session_meta(&restored_rollout)?;
        assert_eq!(restored_meta["payload"]["cwd"], pending_host_cwd);

        drop(restored_state);
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn codex_backup_and_restore_preserve_paginated_history_base_chain() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-paginated-rollout-identity-test");
        let codex = root.join("source-codex");
        let backups = root.join("backups");
        let logical_id = "logical-paginated-thread";
        let rollout_id = "019d0000-1111-7000-8000-000000000010";
        let history_base_id = "019d0000-1111-7000-8000-000000000009";
        let rollout = codex
            .join("sessions/2026/08/31")
            .join(format!("rollout-2026-08-31T10-00-00-{rollout_id}.jsonl"));
        fs::create_dir_all(rollout.parent().unwrap_or(&codex))?;
        let history_base = codex.join("sessions/2026/08/31").join(format!(
            "rollout-2026-08-31T09-00-00-{history_base_id}.jsonl"
        ));
        fs::write(
            &history_base,
            format!(
                "{}\n",
                serde_json::json!({
                    "timestamp": "2026-08-31T09:00:00Z",
                    "type": "session_meta",
                    "payload": {
                        "id": history_base_id,
                        "cwd": r"F:\work\paginated",
                        "model_provider": "openai",
                        "history_mode": "paginated"
                    }
                })
            ),
        )?;
        let history_base_bytes = fs::read(&history_base)?;
        let lines = [
            serde_json::json!({
                "timestamp": "2026-08-31T10:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": logical_id,
                    "cwd": r"F:\work\paginated",
                    "model_provider": "openai",
                    "history_mode": "paginated",
                    "history_base": {"thread_id": history_base_id}
                }
            }),
            serde_json::json!({
                "timestamp": "2026-08-31T10:00:01Z",
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "thread_id": logical_id,
                    "item": {
                        "type": "UserMessage",
                        "content": [{"type": "text", "text": "paginated backup"}]
                    }
                }
            }),
        ];
        fs::write(
            &rollout,
            format!(
                "{}\n",
                lines
                    .iter()
                    .map(serde_json::Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )?;
        let state = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        state.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT,
                first_user_message TEXT, preview TEXT, created_at INTEGER, updated_at INTEGER,
                tokens_used INTEGER, model TEXT, history_mode TEXT
            );",
        )?;
        state.execute(
            "INSERT INTO threads VALUES (?1, ?2, 'F:\\work\\paginated', 'Paginated',
                'paginated backup', 'paginated backup', 1, 2, 3, 'gpt-5', 'paginated')",
            rusqlite::params![logical_id, rollout.to_string_lossy().into_owned()],
        )?;
        drop(state);
        let history = create_codex_thread_history_fixture(&codex)?;
        history.execute(
            "INSERT INTO thread_turns
                (thread_id, turn_id, rollout_ordinal, status, rollout_byte_offset,
                 rollout_end_ordinal, rollout_end_byte_offset)
             VALUES (?1, 'turn-current', 10, 'completed', 100, 20, 200)",
            [logical_id],
        )?;
        history.execute(
            "INSERT INTO thread_items
                (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json,
                 item_type, updated_at_ordinal)
             VALUES (?1, 'turn-current', 'item-current', 11, 1000, '{}', 'userMessage', 11)",
            [logical_id],
        )?;
        history.execute(
            "INSERT INTO thread_history_projection_state VALUES (?1, 300, 30)",
            [logical_id],
        )?;
        history.execute(
            "INSERT INTO thread_history_projection_state VALUES (?1, 50, 5)",
            [history_base_id],
        )?;
        drop(history);

        let summary = create_backup(
            Some(PROVIDER_CODEX.to_string()),
            codex.to_string_lossy().into_owned(),
            None,
            backups.to_string_lossy().into_owned(),
            vec![logical_id.to_string()],
            None,
            Some("paginated".to_string()),
            None,
        )?;
        let backup = PathBuf::from(summary.path);
        let detail = open_backup(
            backups.to_string_lossy().into_owned(),
            backup.to_string_lossy().into_owned(),
        )?;
        let target = &detail.manifest.sessions[0];
        assert_eq!(detail.manifest.version, 6);
        assert_eq!(target.id, logical_id);
        assert_eq!(
            crate::family::read_session_meta(&backup.join(&target.rollout_relpath))?["payload"]
                ["id"],
            logical_id
        );
        assert_eq!(target.history_base_rollouts.len(), 1);
        let history_base_artifact = &target.history_base_rollouts[0];
        assert!(history_base_artifact
            .relpath
            .ends_with(&format!("{history_base_id}.jsonl")));
        assert_eq!(
            fs::read(backup.join(&history_base_artifact.relpath))?,
            history_base_bytes
        );

        let restored_codex = root.join("restored-codex");
        fs::create_dir_all(&restored_codex)?;
        let restored_state = rusqlite::Connection::open(paths::state_db_path(&restored_codex))?;
        restored_state.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT,
                preview TEXT, history_mode TEXT
            );",
        )?;
        drop(restored_state);
        drop(create_codex_thread_history_fixture(&restored_codex)?);
        let restored = restore_one(&backup, &restored_codex, target, false)?;
        assert!(restored.ok, "restore failed: {:?}", restored.error);
        let restored_state = state_db::open_ro(&restored_codex)?;
        let restored_history_mode: String = restored_state.query_row(
            "SELECT history_mode FROM threads WHERE id = ?1",
            [logical_id],
            |row| row.get(0),
        )?;
        assert_eq!(restored_history_mode, "paginated");
        let restored_meta =
            crate::family::read_session_meta(&restored_codex.join(&target.rollout_relpath))?;
        assert_eq!(restored_meta["payload"]["id"], logical_id);
        assert_eq!(
            fs::read(restored_codex.join(&history_base_artifact.relpath))?,
            history_base_bytes
        );
        let restored_history =
            rusqlite::Connection::open(restored_codex.join("thread_history_1.sqlite"))?;
        let restored_turns: i64 = restored_history.query_row(
            "SELECT COUNT(*) FROM thread_turns WHERE thread_id = ?1",
            [logical_id],
            |row| row.get(0),
        )?;
        let restored_items: i64 = restored_history.query_row(
            "SELECT COUNT(*) FROM thread_items WHERE thread_id = ?1",
            [logical_id],
            |row| row.get(0),
        )?;
        let restored_base_state: i64 = restored_history.query_row(
            "SELECT COUNT(*) FROM thread_history_projection_state WHERE thread_id = ?1",
            [history_base_id],
            |row| row.get(0),
        )?;
        assert_eq!(
            (restored_turns, restored_items, restored_base_state),
            (1, 1, 1)
        );
        drop(restored_history);

        drop(restored_state);
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn empty_projection_restore_rejects_existing_orphan_rows() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-empty-projection-orphan-test");
        let backup = root.join("backup");
        let codex = root.join("codex");
        let id = "empty-projection-restore";
        let relative = PathBuf::from(format!("sessions/2026/08/31/rollout-{id}.jsonl"));
        let target = write_codex_restore_backup(
            &backup,
            id,
            &relative,
            serde_json::json!({
                "id": id,
                "rollout_path": relative.to_string_lossy(),
                "cwd": r"F:\work\restored",
                "title": "empty projection restore"
            }),
            r"F:\work\restored",
        )?;
        create_minimal_codex_restore_state(&codex)?;
        let history = create_codex_thread_history_fixture(&codex)?;
        history.execute(
            "INSERT INTO thread_history_projection_state VALUES (?1, 99, 9)",
            [id],
        )?;
        drop(history);

        let restored = restore_one(&backup, &codex, &target, false)?;

        assert!(!restored.ok);
        assert!(
            restored
                .error
                .as_deref()
                .is_some_and(|error| error.contains("孤立的分页历史投影")),
            "{:?}",
            restored.error
        );
        assert!(!codex.join(&relative).exists());
        let state = state_db::open_ro(&codex)?;
        let thread_count: i64 =
            state.query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))?;
        assert_eq!(thread_count, 0);
        drop(state);
        let history = rusqlite::Connection::open(codex.join("thread_history_1.sqlite"))?;
        let projection_count: i64 = history.query_row(
            "SELECT COUNT(*) FROM thread_history_projection_state WHERE thread_id = ?1",
            [id],
            |row| row.get(0),
        )?;
        assert_eq!(projection_count, 1);
        drop(history);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn codex_backup_rejects_missing_history_base_without_partial_output() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-missing-history-base-test");
        let codex = root.join("source-codex");
        let backups = root.join("backups");
        let logical_id = "logical-missing-history-base";
        let rollout_id = "019d0000-1111-7000-8000-000000000020";
        let missing_id = "019d0000-1111-7000-8000-000000000019";
        let rollout = codex
            .join("sessions/2026/08/31")
            .join(format!("rollout-2026-08-31T10-00-00-{rollout_id}.jsonl"));
        fs::create_dir_all(rollout.parent().unwrap_or(&codex))?;
        fs::write(
            &rollout,
            format!(
                "{}\n",
                serde_json::json!({
                    "timestamp": "2026-08-31T10:00:00Z",
                    "type": "session_meta",
                    "payload": {
                        "id": logical_id,
                        "cwd": r"F:\work\paginated",
                        "model_provider": "openai",
                        "history_mode": "paginated",
                        "history_base": {"thread_id": missing_id}
                    }
                })
            ),
        )?;
        let state = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        state.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT,
                created_at INTEGER, updated_at INTEGER, tokens_used INTEGER, model TEXT,
                history_mode TEXT
            );",
        )?;
        state.execute(
            "INSERT INTO threads VALUES (?1, ?2, 'F:\\work\\paginated', 'Missing base',
                1, 2, 3, 'gpt-5', 'paginated')",
            rusqlite::params![logical_id, rollout.to_string_lossy().into_owned()],
        )?;
        drop(state);

        let error = create_backup(
            Some(PROVIDER_CODEX.to_string()),
            codex.to_string_lossy().into_owned(),
            None,
            backups.to_string_lossy().into_owned(),
            vec![logical_id.to_string()],
            None,
            Some("missing-base".to_string()),
            None,
        )
        .expect_err("a non-self-contained paginated backup must fail closed");

        assert!(error.to_string().contains(missing_id), "{error}");
        assert!(!backups.join("missing-base").exists());
        assert!(!backups.join(".missing-base.partial").exists());
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn codex_history_base_restore_reuses_rejects_and_compensates() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-history-base-restore-test");
        let backup = root.join("backup");
        let logical_id = "logical-history-base-restore";
        let rollout_id = "019d0000-1111-7000-8000-000000000030";
        let history_base_id = "019d0000-1111-7000-8000-000000000029";
        let relative = PathBuf::from(format!(
            "sessions/2026/08/31/rollout-2026-08-31T10-00-00-{rollout_id}.jsonl"
        ));
        let history_base_relative = PathBuf::from(format!(
            "sessions/2026/08/31/rollout-2026-08-31T09-00-00-{history_base_id}.jsonl"
        ));
        let mut target = write_codex_restore_backup(
            &backup,
            logical_id,
            &relative,
            serde_json::json!({
                "id": logical_id,
                "rollout_path": relative.to_string_lossy(),
                "cwd": r"F:\work\restored",
                "title": "history base restore"
            }),
            r"F:\work\restored",
        )?;
        let history_base_bytes = attach_history_base_to_restore_backup(
            &backup,
            &mut target,
            history_base_id,
            &history_base_relative,
        )?;

        let reuse_codex = root.join("reuse-codex");
        create_minimal_codex_restore_state(&reuse_codex)?;
        let reuse_base = reuse_codex.join(&history_base_relative);
        fs::create_dir_all(reuse_base.parent().unwrap_or(&reuse_codex))?;
        fs::write(&reuse_base, &history_base_bytes)?;
        let reused = restore_one(&backup, &reuse_codex, &target, false)?;
        assert!(reused.ok, "restore failed: {:?}", reused.error);
        assert_eq!(fs::read(&reuse_base)?, history_base_bytes);

        let conflict_codex = root.join("conflict-codex");
        create_minimal_codex_restore_state(&conflict_codex)?;
        let conflict_base = conflict_codex.join(&history_base_relative);
        fs::create_dir_all(conflict_base.parent().unwrap_or(&conflict_codex))?;
        fs::write(&conflict_base, b"different rollout with the same UUID\n")?;
        let error = restore_one(&backup, &conflict_codex, &target, false)
            .expect_err("a same-UUID dependency conflict must be rejected");
        assert!(error.to_string().contains("同 UUID 但内容不同"), "{error}");
        assert!(!conflict_codex.join(&relative).exists());

        let rollback_codex = root.join("rollback-codex");
        create_minimal_codex_restore_state(&rollback_codex)?;
        let rollback_index = paths::session_index_path(&rollback_codex);
        fs::write(&rollback_index, b"original index\n")?;
        let fault = RestoreFileTestFaultGuard::replace_and_conflict(
            "session index",
            rollback_index.clone(),
            b"concurrent index\n".to_vec(),
        );
        let rolled_back = restore_one(&backup, &rollback_codex, &target, false)?;
        drop(fault);
        assert!(!rolled_back.ok);
        assert!(!rollback_codex.join(&relative).exists());
        assert!(!rollback_codex.join(&history_base_relative).exists());
        assert_eq!(fs::read(&rollback_index)?, b"concurrent index\n");
        let state = state_db::open_ro(&rollback_codex)?;
        let rows: i64 = state.query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))?;
        assert_eq!(rows, 0);
        drop(state);

        fs::write(
            backup.join(&history_base_relative),
            b"tampered dependency\n",
        )?;
        let error = validate_codex_history_base_payload(&backup, &target)
            .expect_err("tampered dependency must fail validation");
        assert!(error.to_string().contains("sha256"), "{error}");

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn codex_backup_ignores_settled_stale_desktop_assignment_cwd() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-codex-stale-assignment-backup-test");
        let codex = root.join("source-codex");
        let backups = root.join("backups");
        let id = "settled-stale-desktop-assignment-backup";
        let core_cwd = r"F:\core-project";
        let stale_host_cwd = r"F:\stale-desktop-project";
        let source_rollout = codex
            .join("sessions/2026/08/13")
            .join(format!("rollout-2026-08-13T10-00-00-{id}.jsonl"));
        fs::create_dir_all(source_rollout.parent().unwrap_or(&codex))?;
        fs::write(
            &source_rollout,
            format!(
                "{}\n",
                serde_json::to_string(&serde_json::json!({
                    "timestamp": "2026-08-13T10:00:00Z",
                    "type": "session_meta",
                    "payload": {"id": id, "cwd": core_cwd, "model_provider": "openai"}
                }))?
            ),
        )?;

        let state = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        state.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT,
                created_at INTEGER, updated_at INTEGER, tokens_used INTEGER, model TEXT
            );",
        )?;
        state.execute(
            "INSERT INTO threads VALUES (?1, ?2, ?3, 'settled move', 1, 2, 3, 'gpt-5')",
            rusqlite::params![id, source_rollout.to_string_lossy().into_owned(), core_cwd],
        )?;
        drop(state);
        fs::write(
            paths::codex_global_state_json_path(&codex),
            serde_json::to_vec(&serde_json::json!({
                "local-projects": {
                    "stale-project-id": {
                        "id": "stale-project-id",
                        "rootPaths": [stale_host_cwd]
                    }
                },
                "thread-project-assignments": {
                    (id): {
                        "projectKind": "local",
                        "projectId": "stale-project-id",
                        "cwd": stale_host_cwd,
                        "pendingCoreUpdate": false
                    }
                }
            }))?,
        )?;

        let summary = create_backup(
            Some(PROVIDER_CODEX.to_string()),
            codex.to_string_lossy().into_owned(),
            None,
            backups.to_string_lossy().into_owned(),
            vec![id.to_string()],
            None,
            Some("settled-stale-assignment".to_string()),
            None,
        )?;
        let backup = PathBuf::from(&summary.path);
        let manifest = load_backup_manifest(&backup)?;
        assert_eq!(manifest.sessions[0].cwd, core_cwd);
        let threads: Vec<serde_json::Value> =
            serde_json::from_slice(&fs::read(backup.join("threads.json"))?)?;
        assert_eq!(threads[0]["cwd"], core_cwd);
        let backed_up_meta =
            crate::family::read_session_meta(&backup.join(&manifest.sessions[0].rollout_relpath))?;
        assert_eq!(backed_up_meta["payload"]["cwd"], core_cwd);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn backs_up_and_restores_claude_session() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-claude-backup-test");
        let source_claude = root.join("source-claude");
        let restore_claude = root.join("restore-claude");
        let backup_dir = root.join("backups");
        write_claude_session(&source_claude, "claude-backup-1")?;
        let source_sidecar = source_claude
            .join("projects")
            .join("sample-project")
            .join("claude-backup-1");
        fs::create_dir_all(source_sidecar.join("subagents"))?;
        fs::write(
            source_sidecar.join("subagents").join("agent.jsonl"),
            "sidecar-content",
        )?;
        fs::write(
            source_claude
                .join("projects")
                .join("sample-project")
                .join("claude-backup-1.claudinal.json"),
            "companion-content",
        )?;
        let source_tasks = source_claude.join("tasks").join("claude-backup-1");
        fs::create_dir_all(&source_tasks)?;
        fs::write(source_tasks.join("task.json"), "task-content")?;

        let summary = create_backup(
            Some(PROVIDER_CLAUDE.to_string()),
            String::new(),
            Some(source_claude.to_string_lossy().into_owned()),
            backup_dir.to_string_lossy().into_owned(),
            vec!["claude-backup-1".to_string()],
            None,
            Some("claude-backup".to_string()),
            Some("test".to_string()),
        )?;
        assert_eq!(summary.provider.as_deref(), Some(PROVIDER_CLAUDE));
        let backup_path = summary.path.clone();
        let backup_root = backup_dir.to_string_lossy().into_owned();
        let detail = open_backup(backup_root.clone(), backup_path.clone())?;
        assert_eq!(detail.manifest.sessions[0].history_rows, 2);
        assert_eq!(detail.manifest.sessions[0].sidecar_files.len(), 1);
        assert_eq!(detail.manifest.sessions[0].companion_files.len(), 1);
        assert_eq!(detail.manifest.sessions[0].task_files.len(), 1);
        let backup_history = fs::read_to_string(PathBuf::from(&backup_path).join("history.jsonl"))?;
        assert!(backup_history.contains("keep one"));
        assert!(backup_history.contains("keep two"));
        assert!(!backup_history.contains("ignore"));

        let restored = restore_session(
            Some(PROVIDER_CLAUDE.to_string()),
            backup_root,
            backup_path.clone(),
            String::new(),
            Some(restore_claude.to_string_lossy().into_owned()),
            "claude-backup-1".to_string(),
            None,
            false,
        )?;

        assert!(restored.ok);
        assert_eq!(restored.history_appended, 2);
        assert!(paths::claude_projects_dir(&restore_claude)
            .join("sample-project")
            .join("claude-backup-1.jsonl")
            .is_file());
        assert_eq!(
            fs::read_to_string(
                paths::claude_projects_dir(&restore_claude)
                    .join("sample-project")
                    .join("claude-backup-1")
                    .join("subagents")
                    .join("agent.jsonl")
            )?,
            "sidecar-content"
        );
        assert_eq!(
            fs::read_to_string(
                paths::claude_projects_dir(&restore_claude)
                    .join("sample-project")
                    .join("claude-backup-1.claudinal.json")
            )?,
            "companion-content"
        );
        assert_eq!(
            fs::read_to_string(
                restore_claude
                    .join("tasks")
                    .join("claude-backup-1")
                    .join("task.json")
            )?,
            "task-content"
        );
        let restored_history = fs::read_to_string(restore_claude.join("history.jsonl"))?;
        assert!(restored_history.contains("keep one"));
        assert!(restored_history.contains("keep two"));
        assert!(!restored_history.contains("ignore"));

        let backup_sidecar = PathBuf::from(&summary.path)
            .join("sidecars")
            .join("claude-backup-1")
            .join("subagents")
            .join("agent.jsonl");
        fs::write(&backup_sidecar, "corrupted")?;
        let error = verify_backup(backup_dir.to_string_lossy().into_owned(), summary.path)
            .expect_err("sidecar corruption must fail backup verification");
        assert!(error.to_string().contains("sidecar"));
        fs::write(&backup_sidecar, "sidecar-content")?;
        fs::write(
            PathBuf::from(&backup_path).join("history.jsonl"),
            "tampered\n",
        )?;
        let error = verify_backup(backup_dir.to_string_lossy().into_owned(), backup_path)
            .expect_err("support-file corruption must fail backup verification");
        assert!(error.to_string().contains("辅助文件"));
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn backs_up_verifies_and_restores_opencode_snapshot() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-opencode-backup-test");
        let source = root.join("source-opencode");
        let target = root.join("target-opencode");
        let cwd = root.join("project");
        let backup_dir = root.join("backups");
        write_opencode_database(&source, &cwd, "OpenCode portable")?;
        write_opencode_database(&target, &cwd, "placeholder")?;
        let target_connection =
            rusqlite::Connection::open(crate::opencode_sessions::database_path(&target))?;
        target_connection.execute_batch("PRAGMA foreign_keys=ON")?;
        target_connection.execute("DELETE FROM session WHERE id='ses_backup'", [])?;
        drop(target_connection);

        let summary = create_backup_with_opencode(
            Some(PROVIDER_OPENCODE.to_string()),
            String::new(),
            None,
            Some(source.to_string_lossy().into_owned()),
            backup_dir.to_string_lossy().into_owned(),
            vec!["ses_backup".to_string()],
            None,
            Some("opencode-backup".to_string()),
            Some("portable sqlite snapshot".to_string()),
        )?;
        assert_eq!(summary.provider.as_deref(), Some(PROVIDER_OPENCODE));
        let verified = verify_backup(
            backup_dir.to_string_lossy().into_owned(),
            summary.path.clone(),
        )?;
        assert!(verified.all_ok);

        let restored = restore_session_with_opencode(
            Some(PROVIDER_OPENCODE.to_string()),
            backup_dir.to_string_lossy().into_owned(),
            summary.path,
            String::new(),
            None,
            Some(target.to_string_lossy().into_owned()),
            "ses_backup".to_string(),
            None,
            false,
        )?;
        assert!(restored.ok);
        let sessions = crate::opencode_sessions::list_sessions(&target)?;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "OpenCode portable");
        let preview = crate::opencode_sessions::preview_range(&sessions[0].rollout_path, 0, 10)?;
        assert_eq!(preview.len(), 1);
        assert_eq!(
            crate::rollout::preview_event_text(&preview[0]),
            "portable opencode content"
        );

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn claude_backup_uses_exact_rollout_path_for_duplicate_ids() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-claude-backup-duplicate-id-test");
        let claude = root.join("claude");
        let backup_dir = root.join("backups");
        let id = "duplicate-claude-id";
        let first = claude
            .join("projects")
            .join("project-a")
            .join(format!("{id}.jsonl"));
        let second = claude
            .join("projects")
            .join("project-b")
            .join(format!("{id}.jsonl"));
        fs::create_dir_all(first.parent().unwrap_or(&claude))?;
        fs::create_dir_all(second.parent().unwrap_or(&claude))?;
        fs::write(
            &first,
            format!(
                "{{\"sessionId\":\"{id}\",\"cwd\":\"F:\\\\a\",\"timestamp\":\"2026-07-10T00:00:00Z\",\"marker\":\"first\"}}\n"
            ),
        )?;
        fs::write(
            &second,
            format!(
                "{{\"sessionId\":\"{id}\",\"cwd\":\"F:\\\\b\",\"timestamp\":\"2026-07-10T00:00:01Z\",\"marker\":\"second\"}}\n"
            ),
        )?;

        let summary = create_backup(
            Some(PROVIDER_CLAUDE.to_string()),
            String::new(),
            Some(claude.to_string_lossy().into_owned()),
            backup_dir.to_string_lossy().into_owned(),
            vec![id.to_string()],
            Some(vec![BundleExportTarget {
                id: id.to_string(),
                rollout_path: Some(second.to_string_lossy().into_owned()),
            }]),
            Some("exact-duplicate".to_string()),
            None,
        )?;
        let manifest = load_backup_manifest(Path::new(&summary.path))?;
        assert_eq!(manifest.sessions.len(), 1);
        assert_eq!(
            manifest.sessions[0].source_relpath.as_deref(),
            Some("project-b/duplicate-claude-id.jsonl")
        );
        let copied = PathBuf::from(&summary.path).join(&manifest.sessions[0].rollout_relpath);
        assert!(fs::read_to_string(copied)?.contains("\"marker\":\"second\""));

        let ambiguous = create_backup(
            Some(PROVIDER_CLAUDE.to_string()),
            String::new(),
            Some(claude.to_string_lossy().into_owned()),
            backup_dir.to_string_lossy().into_owned(),
            vec![id.to_string()],
            None,
            Some("ambiguous-duplicate".to_string()),
            None,
        )
        .expect_err("an id-only request must not choose one duplicate arbitrarily");
        assert!(ambiguous.to_string().contains("必须提供精确 rollout_path"));

        let both = create_backup(
            Some(PROVIDER_CLAUDE.to_string()),
            String::new(),
            Some(claude.to_string_lossy().into_owned()),
            backup_dir.to_string_lossy().into_owned(),
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
            Some("both-duplicates".to_string()),
            None,
        )?;
        let both_manifest = load_backup_manifest(Path::new(&both.path))?;
        assert_eq!(both_manifest.sessions.len(), 2);
        let restore_claude = root.join("restore-claude");
        let ambiguous_restore = restore_session(
            Some(PROVIDER_CLAUDE.to_string()),
            backup_dir.to_string_lossy().into_owned(),
            both.path.clone(),
            String::new(),
            Some(restore_claude.to_string_lossy().into_owned()),
            id.to_string(),
            None,
            false,
        )
        .expect_err("an id-only restore must not choose one duplicate arbitrarily");
        assert!(ambiguous_restore
            .to_string()
            .contains("必须提供精确 backup_rollout_relpath"));

        let second_manifest = both_manifest
            .sessions
            .iter()
            .find(|session| {
                session.source_relpath.as_deref() == Some("project-b/duplicate-claude-id.jsonl")
            })
            .expect("second duplicate manifest entry");
        let restored = restore_session(
            Some(PROVIDER_CLAUDE.to_string()),
            backup_dir.to_string_lossy().into_owned(),
            both.path,
            String::new(),
            Some(restore_claude.to_string_lossy().into_owned()),
            id.to_string(),
            Some(second_manifest.rollout_relpath.clone()),
            false,
        )?;
        assert!(restored.ok);
        assert!(fs::read_to_string(
            restore_claude
                .join("projects")
                .join("project-b")
                .join(format!("{id}.jsonl"))
        )?
        .contains("\"marker\":\"second\""));

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn codex_restore_without_overwrite_preserves_orphan_rollout() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-codex-orphan-restore-test");
        let backup = root.join("backup");
        let codex = root.join("codex");
        let relative = PathBuf::from("sessions/2026/07/10/rollout-orphan.jsonl");
        let source = backup.join(&relative);
        let destination = codex.join(&relative);
        fs::create_dir_all(source.parent().unwrap_or(&backup))?;
        fs::create_dir_all(destination.parent().unwrap_or(&codex))?;
        let source_line = serde_json::json!({
            "timestamp": "2026-07-10T00:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": "orphan",
                "model_provider": "openai",
                "cwd": "F:\\work\\sample"
            }
        });
        fs::write(
            &source,
            format!("{}\n", serde_json::to_string(&source_line)?),
        )?;
        fs::write(&destination, "newer local copy\n")?;

        let state = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        state.execute("CREATE TABLE threads (id TEXT PRIMARY KEY)", [])?;
        drop(state);

        let target = ManifestSession {
            provider: Some(PROVIDER_CODEX.to_string()),
            id: "orphan".to_string(),
            rollout_relpath: relative.to_string_lossy().replace('\\', "/"),
            history_base_rollouts: Vec::new(),
            source_relpath: None,
            sidecar_relpath: None,
            sidecar_files: Vec::new(),
            companions_relpath: None,
            companion_files: Vec::new(),
            tasks_relpath: None,
            task_files: Vec::new(),
            title: "orphan".to_string(),
            cwd: String::new(),
            created_at: 0,
            updated_at: 0,
            tokens_used: 0,
            model: None,
            bytes_rollout: fs::metadata(&source)?.len(),
            logs_count: 0,
            history_rows: 0,
            sha256_rollout: sha256_file(&source)?,
        };

        let restored = restore_one(&backup, &codex, &target, false)?;
        assert!(restored.conflict);
        assert!(!restored.ok);
        assert!(!restored.rollout_copied);
        assert_eq!(fs::read_to_string(&destination)?, "newer local copy\n");

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn codex_restore_syncs_desktop_project_from_thread_row_cwd() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-codex-project-restore-test");
        let backup = root.join("backup");
        let codex = root.join("codex");
        let id = "restore-project-assignment";
        let relative = PathBuf::from(format!("sessions/2026/07/10/rollout-{id}.jsonl"));
        fs::create_dir_all(&codex)?;

        let state = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        state.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT)",
            [],
        )?;
        drop(state);

        let row_cwd = r"F:\work\repo\src";
        let target = write_codex_restore_backup(
            &backup,
            id,
            &relative,
            serde_json::json!({
                "id": id,
                "rollout_path": backup.join(&relative).to_string_lossy(),
                "cwd": row_cwd,
                "title": "restored project thread"
            }),
            r"C:\manifest-fallback-must-not-win",
        )?;
        let global_state_path = paths::codex_global_state_json_path(&codex);
        fs::write(
            &global_state_path,
            serde_json::to_vec(&serde_json::json!({
                "local-projects": {
                    "project-existing": {
                        "id": "project-existing",
                        "name": "Repo",
                        "rootPaths": [r"F:\work\repo"]
                    }
                },
                "thread-project-assignments": {
                    id: {"projectKind": "local", "projectId": "old-project", "cwd": r"C:\old"}
                },
                "projectless-thread-ids": [id]
            }))?,
        )?;

        let restored = restore_one(&backup, &codex, &target, false)?;

        assert!(restored.ok, "restore failed: {:?}", restored.error);
        let global_state: serde_json::Value =
            serde_json::from_slice(&fs::read(&global_state_path)?)?;
        assert_eq!(
            global_state["thread-project-assignments"][id],
            serde_json::json!({
                "projectKind": "local",
                "projectId": "project-existing",
                "cwd": row_cwd,
                "pendingCoreUpdate": false
            })
        );
        assert!(global_state["projectless-thread-ids"]
            .as_array()
            .is_some_and(Vec::is_empty));
        let state = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        let restored_cwd: String =
            state.query_row("SELECT cwd FROM threads WHERE id = ?", [id], |row| {
                row.get(0)
            })?;
        assert_eq!(restored_cwd, row_cwd);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn codex_restore_to_archived_sessions_records_restore_origin() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-codex-restore-archived-ledger-test");
        let backup = root.join("backup");
        let codex = root.join("codex");
        let id = "restore-archived-origin";
        let relative = PathBuf::from(format!("archived_sessions/rollout-{id}.jsonl"));
        fs::create_dir_all(&codex)?;

        let state = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        state.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT, archived INTEGER, archived_at INTEGER)",
            [],
        )?;
        drop(state);

        let target = write_codex_restore_backup(
            &backup,
            id,
            &relative,
            serde_json::json!({
                "id": id,
                "rollout_path": backup.join(&relative).to_string_lossy(),
                "cwd": r"F:\work\sample",
                "title": "restored archived thread",
                "archived": 1,
                "archived_at": 1770000400
            }),
            r"F:\work\sample",
        )?;

        let restored = restore_one(&backup, &codex, &target, false)?;
        assert!(restored.ok, "restore failed: {:?}", restored.error);

        // 归档来源账本：还原到 archived_sessions/ 应记录 Restore，校验和与备份一致
        let ledger = crate::archive_ledger::load(&codex)?;
        let entry = ledger
            .entries
            .get(id)
            .expect("归档路径还原应记录 ArchiveOrigin::Restore");
        assert_eq!(entry.origin, ArchiveOrigin::Restore);
        assert!(entry.archived_at.is_some());
        assert_eq!(
            entry.sha256.as_deref(),
            Some(target.sha256_rollout.as_str())
        );
        assert!(entry
            .source_path
            .as_deref()
            .unwrap_or_default()
            .contains("archived_sessions"));

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn codex_restore_rejects_malformed_project_state_before_core_mutation() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-codex-restore-project-preflight-test");
        let backup = root.join("backup");
        let codex = root.join("codex");
        let id = "restore-project-preflight";
        let relative = PathBuf::from(format!("sessions/2026/07/10/rollout-{id}.jsonl"));
        fs::create_dir_all(&codex)?;
        let state = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        state.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT)",
            [],
        )?;
        drop(state);
        let target = write_codex_restore_backup(
            &backup,
            id,
            &relative,
            serde_json::json!({
                "id": id,
                "rollout_path": backup.join(&relative).to_string_lossy(),
                "cwd": r"F:\work\repo",
                "title": "must not restore"
            }),
            r"F:\work\repo",
        )?;
        let state_path = paths::state_db_path(&codex);
        let state_before = fs::read(&state_path)?;
        let global_path = paths::codex_global_state_json_path(&codex);
        fs::write(
            &global_path,
            serde_json::to_vec(&serde_json::json!({"project-order": null}))?,
        )?;
        let global_before = fs::read(&global_path)?;

        let error = restore_one(&backup, &codex, &target, false)
            .expect_err("malformed Desktop project state must reject restore preflight");

        assert!(error.to_string().contains("project-order"), "{error}");
        assert_eq!(fs::read(&state_path)?, state_before);
        assert_eq!(fs::read(&global_path)?, global_before);
        assert!(!codex.join(&relative).exists());
        assert!(!paths::session_index_path(&codex).exists());
        assert!(!paths::history_path(&codex).exists());
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn codex_restore_keeps_missing_desktop_global_state_absent() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-codex-missing-project-state-restore-test");
        let backup = root.join("backup");
        let codex = root.join("codex");
        let id = "restore-without-global-state";
        let relative = PathBuf::from(format!("sessions/2026/07/10/rollout-{id}.jsonl"));
        fs::create_dir_all(&codex)?;
        let state = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        state.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT)",
            [],
        )?;
        drop(state);
        let target = write_codex_restore_backup(
            &backup,
            id,
            &relative,
            serde_json::json!({
                "id": id,
                "rollout_path": backup.join(&relative).to_string_lossy(),
                "cwd": r"F:\work\repo",
                "title": "legacy Desktop restore"
            }),
            r"F:\work\repo",
        )?;
        let global_state_path = paths::codex_global_state_json_path(&codex);
        assert!(!global_state_path.exists());

        let restored = restore_one(&backup, &codex, &target, false)?;

        assert!(restored.ok, "restore failed: {:?}", restored.error);
        assert!(!global_state_path.exists());
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn codex_restore_while_desktop_running_preserves_every_target_store() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-codex-restore-desktop-running-test");
        let backup_root = root.join("backups");
        let backup = backup_root.join("desktop-running");
        let codex = root.join("codex");
        let id = "restore-desktop-running";
        let relative = PathBuf::from(format!("sessions/2026/07/10/rollout-{id}.jsonl"));
        let destination = codex.join(&relative);
        fs::create_dir_all(destination.parent().unwrap_or(&codex))?;
        fs::create_dir_all(&backup)?;

        let target = write_codex_restore_backup(
            &backup,
            id,
            &relative,
            serde_json::json!({
                "id": id,
                "rollout_path": backup.join(&relative).to_string_lossy(),
                "cwd": r"F:\work\restored",
                "title": "restored title"
            }),
            r"F:\work\restored",
        )?;
        let manifest = Manifest {
            version: 3,
            provider: Some(PROVIDER_CODEX.to_string()),
            created_at: "2026-07-10T00:00:00Z".to_string(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            codex_dir: String::new(),
            claude_dir: None,
            opencode_dir: None,
            note: None,
            artifacts: Vec::new(),
            sessions: vec![target],
        };
        fs::write(
            backup.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;

        let state = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        state.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT)",
            [],
        )?;
        state.execute(
            "INSERT INTO threads VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                id,
                destination.to_string_lossy().into_owned(),
                r"C:\local-before",
                "local title"
            ],
        )?;
        drop(state);
        fs::write(
            &destination,
            b"local rollout must remain byte-for-byte unchanged\n",
        )?;
        let index_path = paths::session_index_path(&codex);
        let history_path = paths::history_path(&codex);
        let state_path = paths::state_db_path(&codex);
        let global_state_path = paths::codex_global_state_json_path(&codex);
        fs::write(&index_path, b"index-before\r\n")?;
        fs::write(&history_path, b"history-before\r\n")?;
        fs::write(
            &global_state_path,
            serde_json::to_vec(&serde_json::json!({
                "local-projects": {
                    "local-before": {
                        "id": "local-before",
                        "rootPaths": [r"C:\local-before"]
                    }
                },
                "thread-project-assignments": {
                    id: {
                        "projectKind": "local",
                        "projectId": "local-before",
                        "cwd": r"C:\local-before",
                        "pendingCoreUpdate": false
                    }
                }
            }))?,
        )?;

        let rollout_before = fs::read(&destination)?;
        let state_before = fs::read(&state_path)?;
        let index_before = fs::read(&index_path)?;
        let history_before = fs::read(&history_path)?;
        let global_state_before = fs::read(&global_state_path)?;

        let _desktop_running = crate::codex_projects::DesktopTestProbeGuard::running();
        let error = restore_session(
            Some(PROVIDER_CODEX.to_string()),
            backup_root.to_string_lossy().into_owned(),
            backup.to_string_lossy().into_owned(),
            codex.to_string_lossy().into_owned(),
            None,
            id.to_string(),
            Some(relative.to_string_lossy().replace('\\', "/")),
            true,
        )
        .expect_err("running Desktop must reject restore before any target mutation");
        let error = error.to_string();
        assert!(error.contains("Codex/ChatGPT 桌面应用正在运行"), "{error}");
        assert!(
            error.contains("请完全退出桌面应用（包括后台进程）后重试"),
            "{error}"
        );
        assert_eq!(fs::read(&destination)?, rollout_before);
        assert_eq!(fs::read(&state_path)?, state_before);
        assert_eq!(fs::read(&index_path)?, index_before);
        assert_eq!(fs::read(&history_path)?, history_before);
        assert_eq!(fs::read(&global_state_path)?, global_state_before);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn codex_restore_commit_failure_compensates_desktop_project_state() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-codex-project-compensation-test");
        let backup = root.join("backup");
        let codex = root.join("codex");
        let id = "restore-project-rollback";
        let relative = PathBuf::from(format!("sessions/2026/07/10/rollout-{id}.jsonl"));
        let destination = codex.join(&relative);
        fs::create_dir_all(&codex)?;

        // The deferred constraint lets all files, including Desktop global state, be staged
        // before SQLite rejects COMMIT. This exercises the real post-sync compensation path.
        let state = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        state.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE parents (id TEXT PRIMARY KEY);
             CREATE TABLE threads (
                 id TEXT PRIMARY KEY,
                 rollout_path TEXT,
                 cwd TEXT,
                 title TEXT,
                 parent_id TEXT REFERENCES parents(id) DEFERRABLE INITIALLY DEFERRED
             );",
        )?;
        drop(state);

        let cwd = r"F:\work\repo";
        let target = write_codex_restore_backup(
            &backup,
            id,
            &relative,
            serde_json::json!({
                "id": id,
                "rollout_path": backup.join(&relative).to_string_lossy(),
                "cwd": cwd,
                "title": "must roll back",
                "parent_id": "missing-parent"
            }),
            cwd,
        )?;
        let history_path = paths::history_path(&codex);
        let index_path = paths::session_index_path(&codex);
        fs::write(&history_path, b"old history\n")?;
        fs::write(&index_path, b"old index\n")?;
        let global_state_path = paths::codex_global_state_json_path(&codex);
        fs::write(
            &global_state_path,
            serde_json::to_vec(&serde_json::json!({
                "local-projects": {
                    "project-existing": {
                        "id": "project-existing",
                        "name": "Repo",
                        "rootPaths": [cwd]
                    }
                },
                "project-order": ["project-existing"],
                "untouched": {"value": 1}
            }))?,
        )?;
        let history_before = fs::read(&history_path)?;
        let index_before = fs::read(&index_path)?;
        let global_state_before = fs::read(&global_state_path)?;

        let restored = restore_one(&backup, &codex, &target, false)?;

        assert!(!restored.ok);
        assert!(!restored.rollout_copied);
        assert!(restored
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("提交 Codex 数据库还原事务失败"));
        assert!(!destination.exists());
        assert_eq!(fs::read(&history_path)?, history_before);
        assert_eq!(fs::read(&index_path)?, index_before);
        assert_eq!(fs::read(&global_state_path)?, global_state_before);
        let state = state_db::open(&codex)?;
        let restored_rows: i64 =
            state.query_row("SELECT COUNT(*) FROM threads WHERE id = ?", [id], |row| {
                row.get(0)
            })?;
        assert_eq!(restored_rows, 0);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn codex_restore_never_overwrites_concurrent_index_change_during_compensation() -> AppResult<()>
    {
        let root = temp_dir("cc-session-manager-codex-restore-index-race-test");
        let backup = root.join("backup");
        let codex = root.join("codex");
        let id = "restore-index-race";
        let relative = PathBuf::from(format!("sessions/2026/07/10/rollout-{id}.jsonl"));
        fs::create_dir_all(&codex)?;

        let state = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        state.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT)",
            [],
        )?;
        drop(state);
        let target = write_codex_restore_backup(
            &backup,
            id,
            &relative,
            serde_json::json!({
                "id": id,
                "rollout_path": backup.join(&relative).to_string_lossy(),
                "cwd": r"F:\work\repo",
                "title": "restore index race"
            }),
            r"F:\work\repo",
        )?;
        let history_path = paths::history_path(&codex);
        let index_path = paths::session_index_path(&codex);
        fs::write(&history_path, b"history before\n")?;
        fs::write(&index_path, b"index before\n")?;
        let concurrent_index = b"index written concurrently\n".to_vec();
        let _fault = RestoreFileTestFaultGuard::replace_and_conflict(
            "session index",
            index_path.clone(),
            concurrent_index.clone(),
        );

        let restored = restore_one(&backup, &codex, &target, false)?;

        assert!(!restored.ok);
        assert!(!restored.rollout_copied);
        assert!(restored
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("并发写入冲突"));
        assert_eq!(fs::read(&index_path)?, concurrent_index);
        assert_eq!(fs::read(&history_path)?, b"history before\n");
        assert!(!codex.join(&relative).exists());
        let state = state_db::open_ro(&codex)?;
        let rows: i64 =
            state.query_row("SELECT COUNT(*) FROM threads WHERE id = ?", [id], |row| {
                row.get(0)
            })?;
        assert_eq!(rows, 0);

        drop(state);
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn restore_snapshot_never_blindly_compensates_without_a_post_write_fingerprint() -> AppResult<()>
    {
        let root = temp_dir("cc-session-manager-restore-unobserved-commit-test");
        fs::create_dir_all(&root)?;
        let path = root.join("session_index.jsonl");
        fs::write(&path, b"before\n")?;
        let mut snapshots =
            RestoreFileSnapshots::capture_owned(&[("session index".to_string(), path.clone())])?;
        snapshots.start("session index")?;

        // Model a successful writer followed by a failure while observing the resulting path.
        fs::write(&path, b"committed but unobserved\n")?;
        snapshots.mark_committed_without_observation("session index")?;

        let errors = snapshots.compensate_except(&[]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("拒绝盲目覆盖"), "{}", errors[0]);
        assert_eq!(fs::read(&path)?, b"committed but unobserved\n");

        snapshots.cleanup()?;
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn codex_restore_log_constraint_failure_preserves_all_existing_state() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-codex-restore-rollback-test");
        let backup = root.join("backup");
        let codex = root.join("codex");
        let id = "restore-rollback";
        let relative = PathBuf::from(format!("sessions/2026/07/10/rollout-{id}.jsonl"));
        let source = backup.join(&relative);
        let destination = codex.join(&relative);
        fs::create_dir_all(source.parent().unwrap_or(&backup))?;
        fs::create_dir_all(destination.parent().unwrap_or(&codex))?;
        let source_line = serde_json::json!({
            "timestamp": "2026-07-10T00:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "model_provider": "openai",
                "cwd": "F:\\work\\restored"
            }
        });
        fs::write(
            &source,
            format!("{}\n", serde_json::to_string(&source_line)?),
        )?;
        fs::write(&destination, "old rollout must remain\n")?;

        let thread_columns = "
            id TEXT PRIMARY KEY, rollout_path TEXT, created_at INTEGER, updated_at INTEGER,
            source TEXT, model_provider TEXT, cwd TEXT, title TEXT, sandbox_policy TEXT,
            approval_mode TEXT, tokens_used INTEGER, has_user_event INTEGER, archived INTEGER,
            archived_at INTEGER, git_sha TEXT, git_branch TEXT, git_origin_url TEXT,
            cli_version TEXT, first_user_message TEXT, agent_nickname TEXT, agent_role TEXT,
            memory_mode TEXT, model TEXT, reasoning_effort TEXT, agent_path TEXT,
            created_at_ms INTEGER, updated_at_ms INTEGER";
        let state = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        state.execute(&format!("CREATE TABLE threads ({thread_columns})"), [])?;
        state.execute(
            "INSERT INTO threads (id, rollout_path, title) VALUES (?1, ?2, 'old thread')",
            rusqlite::params![id, destination.to_string_lossy().into_owned()],
        )?;
        drop(state);

        let logs = rusqlite::Connection::open(codex.join("logs_2.sqlite"))?;
        logs.execute(
            "CREATE TABLE logs (id INTEGER PRIMARY KEY, thread_id TEXT NOT NULL, message TEXT)",
            [],
        )?;
        logs.execute(
            "INSERT INTO logs (id, thread_id, message) VALUES (1, ?1, 'old log')",
            [id],
        )?;
        drop(logs);

        fs::create_dir_all(&backup)?;
        fs::write(
            backup.join("threads.json"),
            serde_json::to_vec_pretty(&vec![serde_json::json!({
                "id": id,
                "rollout_path": source.to_string_lossy(),
                "title": "new thread",
                "cwd": "F:\\work\\restored"
            })])?,
        )?;
        let duplicate_logs = [
            serde_json::json!({"id": 2, "thread_id": id, "message": "first"}),
            serde_json::json!({"id": 2, "thread_id": id, "message": "duplicate"}),
        ];
        let logs_ndjson = duplicate_logs
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");
        fs::write(backup.join("logs.ndjson"), format!("{logs_ndjson}\n"))?;
        fs::write(
            backup.join("history.jsonl"),
            format!("{{\"session_id\":\"{id}\",\"text\":\"new history\"}}\n"),
        )?;
        let history_path = paths::history_path(&codex);
        let index_path = paths::session_index_path(&codex);
        fs::write(&history_path, "old history must remain\n")?;
        fs::write(&index_path, "old index must remain\n")?;

        let rollout_before = fs::read(&destination)?;
        let history_before = fs::read(&history_path)?;
        let index_before = fs::read(&index_path)?;
        let target = ManifestSession {
            provider: Some(PROVIDER_CODEX.to_string()),
            id: id.to_string(),
            rollout_relpath: relative.to_string_lossy().replace('\\', "/"),
            history_base_rollouts: Vec::new(),
            source_relpath: None,
            sidecar_relpath: None,
            sidecar_files: Vec::new(),
            companions_relpath: None,
            companion_files: Vec::new(),
            tasks_relpath: None,
            task_files: Vec::new(),
            title: "new thread".to_string(),
            cwd: "F:\\work\\restored".to_string(),
            created_at: 0,
            updated_at: 0,
            tokens_used: 0,
            model: None,
            bytes_rollout: fs::metadata(&source)?.len(),
            logs_count: 2,
            history_rows: 1,
            sha256_rollout: sha256_file(&source)?,
        };

        let restored = restore_one(&backup, &codex, &target, true)?;
        assert!(!restored.ok);
        assert!(!restored.threads_inserted);
        assert_eq!(restored.logs_inserted, 0);
        assert!(!restored.rollout_copied);
        let error = restored.error.as_deref().unwrap_or_default();
        assert!(error.contains("数据库还原约束失败"));
        assert!(error.contains("UNIQUE") || error.contains("constraint"));
        assert_eq!(fs::read(&destination)?, rollout_before);
        assert_eq!(fs::read(&history_path)?, history_before);
        assert_eq!(fs::read(&index_path)?, index_before);

        let state = state_db::open(&codex)?;
        let old_title: String =
            state.query_row("SELECT title FROM threads WHERE id = ?", [id], |row| {
                row.get(0)
            })?;
        assert_eq!(old_title, "old thread");
        drop(state);
        let logs = logs_db::open(&codex)?;
        let rows = logs
            .prepare("SELECT id, message FROM logs WHERE thread_id = ? ORDER BY id")?
            .query_map([id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(rows, vec![(1, "old log".to_string())]);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn codex_restore_with_logs_requires_existing_logs_database_without_creating_it() -> AppResult<()>
    {
        let root = temp_dir("cc-session-manager-codex-restore-missing-logs-db-test");
        let backup = root.join("backup");
        let codex = root.join("codex");
        let id = "restore-missing-logs-db";
        let relative = PathBuf::from(format!("sessions/2026/07/10/rollout-{id}.jsonl"));
        fs::create_dir_all(&codex)?;
        let state = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        state.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT)",
            [],
        )?;
        drop(state);

        let mut target = write_codex_restore_backup(
            &backup,
            id,
            &relative,
            serde_json::json!({
                "id": id,
                "rollout_path": relative.to_string_lossy(),
                "cwd": r"F:\work\restored",
                "title": "restore with logs"
            }),
            r"F:\work\restored",
        )?;
        target.logs_count = 1;
        fs::write(
            backup.join("logs.ndjson"),
            format!("{{\"thread_id\":\"{id}\",\"message\":\"restored\"}}\n"),
        )?;
        let logs_path = codex.join("logs_2.sqlite");
        assert!(!logs_path.exists());

        let error = restore_one(&backup, &codex, &target, false)
            .expect_err("logs restore must require an existing target schema");

        assert!(
            error.to_string().contains("logs_2.sqlite 不存在"),
            "{error}"
        );
        assert!(!logs_path.exists());
        assert!(!codex.join(&relative).exists());
        let state = state_db::open_ro(&codex)?;
        let thread_count: i64 =
            state.query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))?;
        assert_eq!(thread_count, 0);
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn atomic_restore_copy_rejects_unverified_source_bytes() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-restore-copy-sha-test");
        let codex = root.join("codex");
        let source = root.join("source.jsonl");
        let destination = codex.join("sessions").join("target.jsonl");
        fs::create_dir_all(destination.parent().unwrap_or(&codex))?;
        fs::write(&source, b"unverified source\n")?;
        fs::write(&destination, b"local data\n")?;

        let error = copy_restore_file_atomically(
            &codex,
            &source,
            &destination,
            &hex::encode(Sha256::digest(b"different expected bytes\n")),
            "测试还原目标",
        )
        .expect_err("a source whose streamed hash differs must not be committed");

        assert!(error.to_string().contains("还原期间发生变化"));
        assert_eq!(fs::read(&destination)?, b"local data\n");
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn restore_rejects_manifest_targeting_codex_core_file() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-backup-core-path-test");
        let backup_root = root.join("backups");
        let backup = backup_root.join("malicious");
        let codex = root.join("codex");
        fs::create_dir_all(&backup)?;
        fs::create_dir_all(&codex)?;
        fs::write(backup.join("config.toml"), "attacker-controlled")?;
        fs::write(codex.join("config.toml"), "trusted-config")?;
        let manifest = Manifest {
            version: 2,
            provider: Some(PROVIDER_CODEX.to_string()),
            created_at: "2026-07-10T00:00:00Z".to_string(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            codex_dir: codex.to_string_lossy().into_owned(),
            claude_dir: None,
            opencode_dir: None,
            note: None,
            artifacts: Vec::new(),
            sessions: vec![ManifestSession {
                provider: Some(PROVIDER_CODEX.to_string()),
                id: "malicious".to_string(),
                rollout_relpath: "config.toml".to_string(),
                history_base_rollouts: Vec::new(),
                source_relpath: None,
                sidecar_relpath: None,
                sidecar_files: Vec::new(),
                companions_relpath: None,
                companion_files: Vec::new(),
                tasks_relpath: None,
                task_files: Vec::new(),
                title: String::new(),
                cwd: String::new(),
                created_at: 0,
                updated_at: 0,
                tokens_used: 0,
                model: None,
                bytes_rollout: fs::metadata(backup.join("config.toml"))?.len(),
                logs_count: 0,
                history_rows: 0,
                sha256_rollout: sha256_file(&backup.join("config.toml"))?,
            }],
        };
        fs::write(
            backup.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;

        let error = restore_session(
            Some(PROVIDER_CODEX.to_string()),
            backup_root.to_string_lossy().into_owned(),
            backup.to_string_lossy().into_owned(),
            codex.to_string_lossy().into_owned(),
            None,
            "malicious".to_string(),
            None,
            true,
        )
        .expect_err("core paths must never be accepted as rollout destinations");

        assert!(error.to_string().contains("sessions"));
        assert_eq!(
            fs::read_to_string(codex.join("config.toml"))?,
            "trusted-config"
        );
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn missing_rollout_is_visible_but_restore_remains_strict() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-backup-missing-rollout-test");
        let backup_root = root.join("backups");
        let backup = backup_root.join("missing-rollout");
        let codex = root.join("codex");
        fs::create_dir_all(&backup)?;
        fs::write(backup.join("threads.json"), "[]")?;

        let manifest = Manifest {
            version: 3,
            provider: Some(PROVIDER_CODEX.to_string()),
            created_at: "2026-07-10T00:00:00Z".to_string(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            codex_dir: String::new(),
            claude_dir: None,
            opencode_dir: None,
            note: None,
            artifacts: Vec::new(),
            sessions: vec![ManifestSession {
                provider: Some(PROVIDER_CODEX.to_string()),
                id: "missing".to_string(),
                rollout_relpath: "sessions/2026/07/10/rollout-missing.jsonl".to_string(),
                history_base_rollouts: Vec::new(),
                source_relpath: None,
                sidecar_relpath: None,
                sidecar_files: Vec::new(),
                companions_relpath: None,
                companion_files: Vec::new(),
                tasks_relpath: None,
                task_files: Vec::new(),
                title: "missing rollout".to_string(),
                cwd: String::new(),
                created_at: 0,
                updated_at: 0,
                tokens_used: 0,
                model: None,
                bytes_rollout: 0,
                logs_count: 0,
                history_rows: 0,
                sha256_rollout: "not-present".to_string(),
            }],
        };
        fs::write(
            backup.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;

        let backup_root_string = backup_root.to_string_lossy().into_owned();
        let backup_string = backup.to_string_lossy().into_owned();
        let detail = open_backup(backup_root_string.clone(), backup_string.clone())?;
        assert_eq!(detail.manifest.sessions.len(), 1);

        let report = verify_backup(backup_root_string.clone(), backup_string.clone())?;
        assert!(!report.all_ok);
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].id, "missing");
        assert!(report.items[0].missing);
        assert!(!report.items[0].ok);

        let error = restore_session(
            Some(PROVIDER_CODEX.to_string()),
            backup_root_string,
            backup_string,
            codex.to_string_lossy().into_owned(),
            None,
            "missing".to_string(),
            None,
            true,
        )
        .expect_err("restore must reject a backup whose rollout is missing");
        assert!(error.to_string().contains("不存在"));
        assert!(!codex.exists());

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn open_backup_rejects_rollout_beneath_linked_directory() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-backup-linked-payload-test");
        let backup_root = root.join("backups");
        let backup = backup_root.join("linked-payload");
        let external_sessions = root.join("external-sessions");
        fs::create_dir_all(&backup)?;
        fs::create_dir_all(&external_sessions)?;
        let source = external_sessions.join("rollout-linked.jsonl");
        let line = serde_json::json!({
            "timestamp": "2026-07-10T00:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": "linked",
                "model_provider": "openai",
                "cwd": "F:\\work\\sample"
            }
        });
        fs::write(&source, format!("{}\n", serde_json::to_string(&line)?))?;
        create_directory_link(&external_sessions, &backup.join("sessions"))?;
        fs::write(backup.join("threads.json"), "[]")?;
        let manifest = Manifest {
            version: 3,
            provider: Some(PROVIDER_CODEX.to_string()),
            created_at: "2026-07-10T00:00:00Z".to_string(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            codex_dir: String::new(),
            claude_dir: None,
            opencode_dir: None,
            note: None,
            artifacts: Vec::new(),
            sessions: vec![ManifestSession {
                provider: Some(PROVIDER_CODEX.to_string()),
                id: "linked".to_string(),
                rollout_relpath: "sessions/rollout-linked.jsonl".to_string(),
                history_base_rollouts: Vec::new(),
                source_relpath: None,
                sidecar_relpath: None,
                sidecar_files: Vec::new(),
                companions_relpath: None,
                companion_files: Vec::new(),
                tasks_relpath: None,
                task_files: Vec::new(),
                title: String::new(),
                cwd: String::new(),
                created_at: 0,
                updated_at: 0,
                tokens_used: 0,
                model: None,
                bytes_rollout: fs::metadata(&source)?.len(),
                logs_count: 0,
                history_rows: 0,
                sha256_rollout: sha256_file(&source)?,
            }],
        };
        fs::write(
            backup.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;

        let error = open_backup(
            backup_root.to_string_lossy().into_owned(),
            backup.to_string_lossy().into_owned(),
        )
        .expect_err("backup payload links must be rejected");
        assert!(error.to_string().contains("junction") || error.to_string().contains("链接"));

        let error = verify_backup(
            backup_root.to_string_lossy().into_owned(),
            backup.to_string_lossy().into_owned(),
        )
        .expect_err("backup verification must reject payload links");
        assert!(error.to_string().contains("junction") || error.to_string().contains("链接"));

        remove_directory_link(&backup.join("sessions"))?;
        assert!(source.is_file(), "external payload must remain untouched");
        fs::remove_dir_all(root).ok();
        Ok(())
    }
}
