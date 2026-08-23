//! Claude Code 会话重命名与项目目录迁移。

use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::models::MoveSessionCwdReport;
use crate::{claude_sessions, paths};

static MOVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct MoveArtifact {
    source: PathBuf,
    destination: PathBuf,
    stage: PathBuf,
    backup: PathBuf,
}

pub fn rename_session(
    claude_dir: &Path,
    session_id: &str,
    rollout_path: Option<&str>,
    title: &str,
) -> AppResult<u32> {
    let title = title.trim();
    if title.is_empty() {
        return Err(AppError::Other("会话名称不能为空".into()));
    }
    if title.chars().count() > 120 {
        return Err(AppError::Other("会话名称过长（最多 120 个字符）".into()));
    }
    let session = claude_sessions::resolve_session_summary(claude_dir, session_id, rollout_path)?;
    crate::repair::append_custom_title(Path::new(&session.rollout_path), session_id, title)?;
    Ok(1)
}

pub fn move_session_cwd(
    claude_dir: &Path,
    session_id: &str,
    rollout_path: Option<&str>,
    target_cwd: &str,
) -> AppResult<MoveSessionCwdReport> {
    move_session_cwd_with_options(claude_dir, session_id, rollout_path, target_cwd, false)
}

pub fn move_session_cwd_with_options(
    claude_dir: &Path,
    session_id: &str,
    rollout_path: Option<&str>,
    target_cwd: &str,
    preserve_path_case: bool,
) -> AppResult<MoveSessionCwdReport> {
    let target_cwd = normalize_target_cwd(target_cwd, preserve_path_case)?;
    let session = claude_sessions::resolve_session_summary(claude_dir, session_id, rollout_path)?;
    let source_transcript = PathBuf::from(&session.rollout_path);
    claude_sessions::validate_main_transcript(claude_dir, &source_transcript, session_id)?;
    let destination_project = claude_sessions::project_dir_for_cwd(claude_dir, &target_cwd);
    let destination_transcript = destination_project.join(format!("{session_id}.jsonl"));
    let transcript_stays_in_place =
        same_existing_entry(&source_transcript, &destination_transcript)?;
    if transcript_stays_in_place && session.cwd == target_cwd {
        return Ok(MoveSessionCwdReport {
            old_cwd: session.cwd,
            new_cwd: target_cwd,
            threads_updated: 0,
            rollout_rewritten: false,
            desktop_project_synced: false,
            artifacts_moved: 0,
            history_rows_updated: 0,
            target_project_id: None,
            requires_project_open: false,
        });
    }

    ensure_plain_directory(&paths::claude_projects_dir(claude_dir), "Claude projects")?;
    ensure_plain_directory_path(&destination_project, "Claude 目标项目目录")?;
    let sequence = MOVE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stage_root = destination_project.join(format!(
        ".ccsm-move-stage-{}-{sequence}",
        std::process::id()
    ));
    if stage_root.exists() {
        return Err(AppError::Other(format!(
            "Claude 迁移暂存目录已存在: {}",
            stage_root.to_string_lossy()
        )));
    }
    fs::create_dir(&stage_root)?;

    let source_sidecar = claude_sessions::sidecar_path_for(&source_transcript)
        .ok_or_else(|| AppError::Path("Claude transcript 无法计算 sidecar".into()))?;
    let destination_sidecar = claude_sessions::sidecar_path_for(&destination_transcript)
        .ok_or_else(|| AppError::Path("Claude 目标 transcript 无法计算 sidecar".into()))?;
    let companions = claude_sessions::companion_files_for(&source_transcript)?;
    let mut artifacts = Vec::new();
    artifacts.push(MoveArtifact {
        source: source_transcript.clone(),
        destination: destination_transcript.clone(),
        stage: stage_root.join("transcript.jsonl"),
        backup: source_transcript.with_file_name(format!(
            ".ccsm-move-source-{}-{sequence}-transcript",
            std::process::id()
        )),
    });
    if path_exists(&source_sidecar)? {
        artifacts.push(MoveArtifact {
            source: source_sidecar.clone(),
            destination: destination_sidecar,
            stage: stage_root.join("sidecar"),
            backup: source_sidecar.with_file_name(format!(
                ".ccsm-move-source-{}-{sequence}-sidecar",
                std::process::id()
            )),
        });
    }
    for (index, source) in companions.iter().enumerate() {
        let name = source.file_name().ok_or_else(|| {
            AppError::Path(format!(
                "Claude companion 文件名无效: {}",
                source.to_string_lossy()
            ))
        })?;
        artifacts.push(MoveArtifact {
            source: source.clone(),
            destination: destination_project.join(name),
            stage: stage_root.join("companions").join(name),
            backup: source.with_file_name(format!(
                ".ccsm-move-source-{}-{sequence}-companion-{index}",
                std::process::id()
            )),
        });
    }

    let operation = (|| -> AppResult<(u32, u32)> {
        for artifact in &artifacts {
            if path_exists(&artifact.destination)?
                && !same_existing_entry(&artifact.source, &artifact.destination)?
            {
                return Err(AppError::Other(format!(
                    "Claude 目标项目中已存在同名会话资产: {}",
                    artifact.destination.to_string_lossy()
                )));
            }
            if path_exists(&artifact.backup)? {
                return Err(AppError::Other(format!(
                    "Claude 源目录存在未清理的迁移备份: {}",
                    artifact.backup.to_string_lossy()
                )));
            }
        }

        rewrite_jsonl(
            &source_transcript,
            &artifacts[0].stage,
            &target_cwd,
            Some(session_id),
        )?;
        for artifact in artifacts.iter().skip(1) {
            if artifact.source == source_sidecar {
                copy_tree_with_cwd(&artifact.source, &artifact.stage, &target_cwd)?;
            } else {
                copy_regular_file(&artifact.source, &artifact.stage)?;
            }
        }

        for (backed_up, artifact) in artifacts.iter().enumerate() {
            if let Err(error) = fs::rename(&artifact.source, &artifact.backup) {
                let rollback = rollback_source_backups(&artifacts[..backed_up]);
                return Err(with_rollback(error.into(), rollback));
            }
        }

        let mut published = 0usize;
        for artifact in &artifacts {
            if let Some(parent) = artifact.destination.parent() {
                if let Err(error) = fs::create_dir_all(parent) {
                    let rollback = rollback_published(&artifacts[..published], &artifacts);
                    return Err(with_rollback(error.into(), rollback));
                }
            }
            if let Err(error) = fs::rename(&artifact.stage, &artifact.destination) {
                let rollback = rollback_published(&artifacts[..published], &artifacts);
                return Err(with_rollback(error.into(), rollback));
            }
            published += 1;
        }

        let verify_path = if transcript_stays_in_place {
            &source_transcript
        } else {
            &destination_transcript
        };
        let verify = match claude_sessions::resolve_session_summary(
            claude_dir,
            session_id,
            Some(verify_path.to_string_lossy().as_ref()),
        ) {
            Ok(verify) => verify,
            Err(error) => {
                let rollback = rollback_published(&artifacts[..published], &artifacts);
                return Err(with_rollback(error, rollback));
            }
        };
        if verify.cwd != target_cwd {
            let rollback = rollback_published(&artifacts[..published], &artifacts);
            return Err(with_rollback(
                AppError::Other(format!(
                    "Claude 迁移后 transcript cwd 校验失败: 期望 {target_cwd}，实际 {}",
                    verify.cwd
                )),
                rollback,
            ));
        }

        let history_rows = match crate::history::rewrite_project_for_session(
            &paths::history_path(claude_dir),
            session_id,
            &target_cwd,
        ) {
            Ok(updated) => updated,
            Err(error) => {
                let rollback = rollback_published(&artifacts[..published], &artifacts);
                return Err(with_rollback(error, rollback));
            }
        };
        Ok((artifacts.len() as u32, history_rows))
    })();

    let result = match operation {
        Ok(result) => result,
        Err(error) => {
            remove_path(&stage_root).ok();
            return Err(error);
        }
    };

    for artifact in &artifacts {
        remove_path(&artifact.backup).ok();
    }
    remove_path(&stage_root).ok();
    if let Some(source_project) = source_transcript.parent() {
        remove_empty_directory(source_project).ok();
    }

    Ok(MoveSessionCwdReport {
        old_cwd: session.cwd,
        new_cwd: target_cwd,
        threads_updated: 0,
        rollout_rewritten: true,
        desktop_project_synced: false,
        artifacts_moved: if transcript_stays_in_place {
            0
        } else {
            result.0
        },
        history_rows_updated: result.1,
        target_project_id: None,
        requires_project_open: false,
    })
}

fn normalize_target_cwd(raw: &str, preserve_path_case: bool) -> AppResult<String> {
    let raw = paths::strip_verbatim(raw.trim());
    if raw.is_empty() || raw.chars().any(char::is_control) {
        return Err(AppError::Path("Claude 目标工作目录无效".into()));
    }
    let path = PathBuf::from(&raw);
    if !path.is_absolute() {
        return Err(AppError::Path(format!("工作目录必须是绝对路径: {raw}")));
    }
    let canonical = path.canonicalize().map_err(|error| {
        AppError::NotFound(format!("工作目录不存在或无法访问: {raw} ({error})"))
    })?;
    if !canonical.is_dir() {
        return Err(AppError::Path(format!(
            "目标工作目录不是文件夹: {}",
            canonical.to_string_lossy()
        )));
    }
    if preserve_path_case {
        normalize_preserved_target_cwd(&raw)
    } else {
        Ok(paths::strip_verbatim(&canonical.to_string_lossy()))
    }
}

fn normalize_preserved_target_cwd(raw: &str) -> AppResult<String> {
    let path = Path::new(raw);
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(AppError::Path(
            "保留路径大小写时不能包含父目录片段 '..'".into(),
        ));
    }
    let normalized = path.components().collect::<PathBuf>();
    let mut stored = paths::strip_verbatim(&normalized.to_string_lossy());
    if looks_like_windows_path(&stored) {
        stored = stored.replace('/', "\\");
    }
    trim_trailing_separators(&mut stored);
    Ok(stored)
}

fn looks_like_windows_path(path: &str) -> bool {
    (path.as_bytes().get(1) == Some(&b':')
        && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic))
        || path.starts_with("\\\\")
        || path.starts_with("//")
}

fn trim_trailing_separators(path: &mut String) {
    let is_unix_root = path == "/";
    let is_drive_root = path.len() == 3
        && path.as_bytes().get(1) == Some(&b':')
        && matches!(path.as_bytes().get(2), Some(b'\\') | Some(b'/'));
    if !is_unix_root && !is_drive_root {
        while path.len() > 1 && (path.ends_with('/') || path.ends_with('\\')) {
            path.pop();
        }
    }
}

fn rewrite_jsonl(
    source: &Path,
    destination: &Path,
    target_cwd: &str,
    expected_session_id: Option<&str>,
) -> AppResult<()> {
    let metadata = fs::symlink_metadata(source)?;
    if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        return Err(AppError::Path(format!(
            "Claude JSONL 不是普通文件: {}",
            source.to_string_lossy()
        )));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut writer = BufWriter::new(File::create(destination)?);
    let mut found_session_id = expected_session_id.is_none();
    for (line_number, line) in BufReader::new(File::open(source)?).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            writeln!(writer)?;
            continue;
        }
        let mut value: Value = serde_json::from_str(&line).map_err(|error| {
            AppError::Other(format!(
                "Claude JSONL 第 {} 行损坏 {}: {error}",
                line_number + 1,
                source.to_string_lossy()
            ))
        })?;
        if let Some(actual) = value.get("sessionId").and_then(Value::as_str) {
            if let Some(expected) = expected_session_id {
                if actual != expected {
                    return Err(AppError::Other(format!(
                        "Claude transcript sessionId 不匹配: 期望 {expected}，实际 {actual}"
                    )));
                }
                found_session_id = true;
            }
        }
        if let Some(object) = value.as_object_mut() {
            if object.contains_key("cwd") {
                object.insert("cwd".into(), Value::String(target_cwd.to_string()));
            }
        }
        serde_json::to_writer(&mut writer, &value)?;
        writeln!(writer)?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    if !found_session_id {
        return Err(AppError::Other(format!(
            "Claude transcript 缺少 sessionId: {}",
            source.to_string_lossy()
        )));
    }
    Ok(())
}

fn copy_tree_with_cwd(source: &Path, destination: &Path, target_cwd: &str) -> AppResult<()> {
    let metadata = fs::symlink_metadata(source)?;
    if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        return Err(AppError::Path(format!(
            "Claude sidecar 包含链接或 junction: {}",
            source.to_string_lossy()
        )));
    }
    if metadata.is_file() {
        if source.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            return rewrite_jsonl(source, destination, target_cwd, None);
        }
        return copy_regular_file(source, destination);
    }
    if !metadata.is_dir() {
        return Err(AppError::Path(format!(
            "Claude sidecar 不是文件或目录: {}",
            source.to_string_lossy()
        )));
    }
    fs::create_dir_all(destination)?;
    for entry in walkdir::WalkDir::new(source)
        .follow_links(false)
        .min_depth(1)
    {
        let entry = entry.map_err(|error| AppError::Other(error.to_string()))?;
        let relative = entry.path().strip_prefix(source).map_err(|_| {
            AppError::Path(format!(
                "Claude sidecar 条目逃出源目录: {}",
                entry.path().to_string_lossy()
            ))
        })?;
        let target = destination.join(relative);
        let metadata = fs::symlink_metadata(entry.path())?;
        if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "Claude sidecar 包含链接或 junction: {}",
                entry.path().to_string_lossy()
            )));
        }
        if metadata.is_dir() {
            fs::create_dir_all(&target)?;
        } else if metadata.is_file() {
            if entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl") {
                rewrite_jsonl(entry.path(), &target, target_cwd, None)?;
            } else {
                copy_regular_file(entry.path(), &target)?;
            }
        }
    }
    Ok(())
}

fn copy_regular_file(source: &Path, destination: &Path) -> AppResult<()> {
    let metadata = fs::symlink_metadata(source)?;
    if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        return Err(AppError::Path(format!(
            "Claude 资产不是普通文件: {}",
            source.to_string_lossy()
        )));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)?;
    File::options().write(true).open(destination)?.sync_all()?;
    Ok(())
}

fn rollback_source_backups(artifacts: &[MoveArtifact]) -> Vec<String> {
    let mut errors = Vec::new();
    for artifact in artifacts.iter().rev() {
        if let Err(error) = fs::rename(&artifact.backup, &artifact.source) {
            errors.push(format!(
                "恢复 Claude 源资产失败 {}: {error}",
                artifact.source.to_string_lossy()
            ));
        }
    }
    errors
}

fn rollback_published(published: &[MoveArtifact], all: &[MoveArtifact]) -> Vec<String> {
    let mut errors = Vec::new();
    for artifact in published.iter().rev() {
        if let Err(error) = remove_path(&artifact.destination) {
            errors.push(format!(
                "移除 Claude 已发布目标失败 {}: {error}",
                artifact.destination.to_string_lossy()
            ));
        }
    }
    errors.extend(rollback_source_backups(all));
    errors
}

fn with_rollback(error: AppError, rollback_errors: Vec<String>) -> AppError {
    if rollback_errors.is_empty() {
        error
    } else {
        AppError::Other(format!(
            "{error}; Claude 迁移补偿失败: {}",
            rollback_errors.join(" | ")
        ))
    }
}

fn path_exists(path: &Path) -> AppResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn same_existing_entry(source: &Path, destination: &Path) -> AppResult<bool> {
    if !path_exists(destination)? {
        return Ok(false);
    }
    same_file::is_same_file(source, destination).map_err(Into::into)
}

fn remove_path(path: &Path) -> AppResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if crate::path_safety::metadata_is_link_or_reparse(&metadata) => {
            Err(AppError::Path(format!(
                "拒绝移除链接或 junction: {}",
                path.to_string_lossy()
            )))
        }
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path).map_err(Into::into),
        Ok(_) => fs::remove_file(path).map_err(Into::into),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn ensure_plain_directory(path: &Path, label: &str) -> AppResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        return Err(AppError::Path(format!(
            "{label} 必须是普通目录且不能是链接或 junction: {}",
            path.to_string_lossy()
        )));
    }
    Ok(())
}

fn ensure_plain_directory_path(path: &Path, label: &str) -> AppResult<()> {
    fs::create_dir_all(path)?;
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if current.as_os_str().is_empty() {
            continue;
        }
        let metadata = fs::symlink_metadata(&current)?;
        if !metadata.is_dir() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "{label} 的父链必须全部是普通目录: {}",
                current.to_string_lossy()
            )));
        }
    }
    Ok(())
}

fn remove_empty_directory(path: &Path) -> AppResult<()> {
    if path.read_dir()?.next().is_none() {
        fs::remove_dir(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> AppResult<(PathBuf, PathBuf, PathBuf)> {
        let root = std::env::temp_dir().join(format!(
            "cc-sessions-claude-move-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let claude = root.join(".claude");
        let old_cwd = root.join("old project");
        let new_cwd = root.join("new project");
        fs::create_dir_all(&old_cwd)?;
        fs::create_dir_all(&new_cwd)?;
        let project =
            claude_sessions::project_dir_for_cwd(&claude, old_cwd.to_string_lossy().as_ref());
        fs::create_dir_all(project.join("session-1/subagents"))?;
        let transcript = project.join("session-1.jsonl");
        fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                serde_json::json!({"type":"user","sessionId":"session-1","cwd":old_cwd,"message":{"role":"user","content":"hello"}}),
                serde_json::json!({"type":"assistant","sessionId":"session-1","cwd":old_cwd,"message":{"role":"assistant","content":[{"type":"text","text":"world"}]}}),
            ),
        )?;
        fs::write(
            project.join("session-1/subagents/agent-a.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({"type":"assistant","sessionId":"session-1","cwd":old_cwd,"message":{"role":"assistant","content":[]}})
            ),
        )?;
        fs::write(
            project.join("session-1.claudinal.json"),
            serde_json::json!({"result":{"session_id":"session-1"}}).to_string(),
        )?;
        fs::write(
            claude.join("history.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({"sessionId":"session-1","project":old_cwd,"display":"hello"})
            ),
        )?;
        Ok((claude, old_cwd, new_cwd))
    }

    #[test]
    fn project_encoding_matches_claude_rules() {
        assert_eq!(
            claude_sessions::encode_project_dir(
                r"F:\project\sessions-management\codex-session-manager"
            ),
            "F--project-sessions-management-codex-session-manager"
        );
    }

    #[test]
    fn move_rewrites_in_place_when_only_the_recorded_cwd_needs_normalizing() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "cc-sessions-claude-normalize-in-place-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let claude = root.join(".claude");
        let target = root.join("same project");
        fs::create_dir_all(&target)?;
        let normalized_target = normalize_target_cwd(target.to_string_lossy().as_ref(), false)?;
        let stale_cwd = format!("{normalized_target}{}", std::path::MAIN_SEPARATOR);
        let project = claude_sessions::project_dir_for_cwd(&claude, &normalized_target);
        fs::create_dir_all(project.join("session-in-place/subagents"))?;
        let transcript = project.join("session-in-place.jsonl");
        fs::write(
            &transcript,
            format!(
                "{}\n",
                serde_json::json!({
                    "type":"user",
                    "sessionId":"session-in-place",
                    "cwd":stale_cwd,
                    "message":{"role":"user","content":"hello"}
                })
            ),
        )?;
        let sidecar = project.join("session-in-place/subagents/agent-a.jsonl");
        fs::write(
            &sidecar,
            format!(
                "{}\n",
                serde_json::json!({
                    "type":"assistant",
                    "sessionId":"session-in-place",
                    "cwd":stale_cwd,
                    "message":{"role":"assistant","content":[]}
                })
            ),
        )?;
        let companion = project.join("session-in-place.claudinal.json");
        fs::write(
            &companion,
            serde_json::json!({"result":{"session_id":"session-in-place"}}).to_string(),
        )?;
        fs::write(
            claude.join("history.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "sessionId":"session-in-place",
                    "project":stale_cwd,
                    "display":"hello"
                })
            ),
        )?;

        let report = move_session_cwd(
            &claude,
            "session-in-place",
            Some(transcript.to_string_lossy().as_ref()),
            target.to_string_lossy().as_ref(),
        )?;

        assert!(report.rollout_rewritten);
        assert_eq!(report.artifacts_moved, 0);
        assert_eq!(report.history_rows_updated, 1);
        assert_eq!(report.new_cwd, normalized_target);
        assert!(transcript.is_file());
        let line = fs::read_to_string(&transcript)?
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        let value: Value = serde_json::from_str(&line)?;
        assert_eq!(value["cwd"], report.new_cwd);
        let sidecar_line = fs::read_to_string(&sidecar)?
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        let sidecar_value: Value = serde_json::from_str(&sidecar_line)?;
        assert_eq!(sidecar_value["cwd"], report.new_cwd);
        assert!(companion.is_file());

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn exact_mode_preserves_drive_letter_case_while_default_mode_normalizes_it() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "cc-sessions-claude-exact-case-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let target = root.join("exact project");
        fs::create_dir_all(&target)?;
        let normalized = normalize_target_cwd(target.to_string_lossy().as_ref(), false)?;
        let mut exact = normalized.clone();
        let drive = exact[..1].to_string();
        let replacement = if drive == drive.to_ascii_lowercase() {
            drive.to_ascii_uppercase()
        } else {
            drive.to_ascii_lowercase()
        };
        exact.replace_range(..1, &replacement);

        assert_ne!(exact, normalized);
        assert_eq!(normalize_target_cwd(&exact, false)?, normalized);
        assert_eq!(normalize_target_cwd(&exact, true)?, exact);

        let claude = root.join(".claude");
        let project = claude_sessions::project_dir_for_cwd(&claude, &normalized);
        fs::create_dir_all(&project)?;
        let transcript = project.join("session-exact.jsonl");
        fs::write(
            &transcript,
            format!(
                "{}\n",
                serde_json::json!({
                    "type":"user",
                    "sessionId":"session-exact",
                    "cwd":normalized,
                    "message":{"role":"user","content":"hello"}
                })
            ),
        )?;

        let report = move_session_cwd_with_options(
            &claude,
            "session-exact",
            Some(transcript.to_string_lossy().as_ref()),
            &exact,
            true,
        )?;
        assert!(report.rollout_rewritten);
        assert_eq!(report.artifacts_moved, 0);
        assert_eq!(report.new_cwd, exact);
        let line = fs::read_to_string(&transcript)?
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        let value: Value = serde_json::from_str(&line)?;
        assert_eq!(value["cwd"], report.new_cwd);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn move_rewrites_transcript_sidecar_and_history_and_moves_companion() -> AppResult<()> {
        let (claude, old_cwd, new_cwd) = fixture()?;
        let source =
            claude_sessions::project_dir_for_cwd(&claude, old_cwd.to_string_lossy().as_ref())
                .join("session-1.jsonl");
        let report = move_session_cwd(
            &claude,
            "session-1",
            Some(source.to_string_lossy().as_ref()),
            new_cwd.to_string_lossy().as_ref(),
        )?;
        assert_eq!(report.artifacts_moved, 3);
        assert_eq!(report.history_rows_updated, 1);
        let normalized_new_cwd = report.new_cwd.clone();
        let destination = claude_sessions::project_dir_for_cwd(&claude, &normalized_new_cwd);
        assert!(destination.join("session-1.jsonl").is_file());
        assert!(destination.join("session-1.claudinal.json").is_file());
        assert!(destination
            .join("session-1/subagents/agent-a.jsonl")
            .is_file());
        assert!(!source.exists());
        let transcript_line = fs::read_to_string(destination.join("session-1.jsonl"))?
            .lines()
            .next()
            .map(str::to_string)
            .unwrap_or_default();
        let transcript: Value = serde_json::from_str(&transcript_line)?;
        assert_eq!(
            transcript.get("cwd").and_then(Value::as_str),
            Some(normalized_new_cwd.as_str())
        );
        let sidecar_line =
            fs::read_to_string(destination.join("session-1/subagents/agent-a.jsonl"))?
                .lines()
                .next()
                .map(str::to_string)
                .unwrap_or_default();
        let sidecar: Value = serde_json::from_str(&sidecar_line)?;
        assert_eq!(
            sidecar.get("cwd").and_then(Value::as_str),
            Some(normalized_new_cwd.as_str())
        );
        let history_line = fs::read_to_string(claude.join("history.jsonl"))?
            .lines()
            .next()
            .map(str::to_string)
            .unwrap_or_default();
        let history: Value = serde_json::from_str(&history_line)?;
        assert_eq!(
            history.get("project").and_then(Value::as_str),
            Some(normalized_new_cwd.as_str())
        );
        fs::remove_dir_all(claude.parent().unwrap_or(&claude)).ok();
        Ok(())
    }

    #[test]
    fn rename_appends_native_custom_title() -> AppResult<()> {
        let (claude, old_cwd, _) = fixture()?;
        let source =
            claude_sessions::project_dir_for_cwd(&claude, old_cwd.to_string_lossy().as_ref())
                .join("session-1.jsonl");
        assert_eq!(
            rename_session(
                &claude,
                "session-1",
                Some(source.to_string_lossy().as_ref()),
                "Renamed"
            )?,
            1
        );
        let tail = fs::read_to_string(&source)?;
        assert!(tail.contains("\"type\":\"custom-title\""));
        assert!(tail.contains("\"customTitle\":\"Renamed\""));
        fs::remove_dir_all(claude.parent().unwrap_or(&claude)).ok();
        Ok(())
    }
}
