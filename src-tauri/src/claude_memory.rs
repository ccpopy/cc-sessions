use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::atomic_file;
use crate::error::{AppError, AppResult};
use crate::models::{ClaudeMemoryDocument, ClaudeMemoryFile, ClaudeMemoryProject};
use crate::path_safety::{self, EntryKind};
use crate::paths;

const MAX_MEMORY_BYTES: usize = 1024 * 1024;
const PREVIEW_CHARS: usize = 160;

pub fn list_projects(claude_dir: String) -> AppResult<Vec<ClaudeMemoryProject>> {
    let claude = PathBuf::from(claude_dir);
    let projects_root = paths::claude_projects_dir(&claude);
    if !projects_root.is_dir() {
        return Ok(Vec::new());
    }
    let configured_paths = configured_project_paths(&claude);
    let mut projects = Vec::new();
    for entry in fs::read_dir(&projects_root)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir() || path_safety::metadata_is_link_or_reparse(&metadata) {
            continue;
        }
        let project_key = entry.file_name().to_string_lossy().into_owned();
        let memory_dir = entry.path().join("memory");
        let mut file_count = 0u32;
        let mut total_bytes = 0u64;
        let mut updated_at = 0i64;
        let mut has_index = false;
        if memory_dir.is_dir() {
            for file in list_markdown_paths(&projects_root, &memory_dir)? {
                let metadata = fs::metadata(&file)?;
                file_count += 1;
                total_bytes += metadata.len();
                updated_at = updated_at.max(modified_seconds(&metadata));
                has_index |= file
                    .file_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case("MEMORY.md"));
            }
        }
        let project_path = infer_project_path(&entry.path())
            .or_else(|| configured_project_path(&configured_paths, &project_key, &entry.path()))
            .unwrap_or_else(|| project_key.clone());
        projects.push(ClaudeMemoryProject {
            project_key,
            project_path,
            memory_dir: memory_dir.to_string_lossy().into_owned(),
            file_count,
            total_bytes,
            updated_at,
            has_index,
        });
    }
    projects.sort_by(|left, right| {
        right
            .file_count
            .cmp(&left.file_count)
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| left.project_path.cmp(&right.project_path))
    });
    Ok(projects)
}

pub fn list_files(claude_dir: String, project_key: String) -> AppResult<Vec<ClaudeMemoryFile>> {
    let context = memory_context(&claude_dir, &project_key, false)?;
    if !context.memory_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = list_markdown_paths(&context.projects_root, &context.memory_dir)?
        .into_iter()
        .map(|path| summarize_file(&project_key, &path))
        .collect::<AppResult<Vec<_>>>()?;
    files.sort_by(|left, right| {
        right
            .is_index
            .cmp(&left.is_index)
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| left.file_name.cmp(&right.file_name))
    });
    Ok(files)
}

pub fn read_file(
    claude_dir: String,
    project_key: String,
    file_name: String,
) -> AppResult<ClaudeMemoryDocument> {
    let context = memory_context(&claude_dir, &project_key, false)?;
    let path = memory_file_path(&context, &file_name, false)?;
    let content = fs::read_to_string(&path)?;
    Ok(ClaudeMemoryDocument {
        file: summarize_file(&project_key, &path)?,
        content,
    })
}

pub fn save_file(
    claude_dir: String,
    project_key: String,
    file_name: String,
    content: String,
    expected_sha256: Option<String>,
) -> AppResult<ClaudeMemoryDocument> {
    if content.len() > MAX_MEMORY_BYTES {
        return Err(AppError::Other(format!(
            "Claude Memory 文件过大（最多 {} MiB）",
            MAX_MEMORY_BYTES / 1024 / 1024
        )));
    }
    let context = memory_context(&claude_dir, &project_key, true)?;
    let path = memory_file_path(&context, &file_name, true)?;
    if path.is_file() {
        let current_sha = sha256_file(&path)?;
        let expected = expected_sha256.as_deref().ok_or_else(|| {
            AppError::Other(format!(
                "Memory 文件已存在，请先读取后再保存: {}",
                path.to_string_lossy()
            ))
        })?;
        if current_sha != expected {
            return Err(AppError::Other(format!(
                "Memory 文件已被 Claude 或其他进程修改，请重新加载后再保存: {}",
                path.to_string_lossy()
            )));
        }
        let fingerprint = atomic_file::fingerprint(&path)?;
        atomic_file::replace_with_writer_if_unchanged(&path, &fingerprint, |out| {
            out.write_all(content.as_bytes())?;
            Ok(())
        })?;
    } else {
        if expected_sha256.is_some() {
            return Err(AppError::NotFound(format!(
                "待更新的 Memory 文件不存在: {}",
                path.to_string_lossy()
            )));
        }
        atomic_file::create_with_writer_if_absent(&path, |out| {
            out.write_all(content.as_bytes())?;
            Ok(())
        })?;
    }
    read_file(claude_dir, project_key, file_name)
}

pub fn rename_file(
    claude_dir: String,
    project_key: String,
    file_name: String,
    new_file_name: String,
    expected_sha256: String,
) -> AppResult<ClaudeMemoryDocument> {
    let document = read_file(claude_dir.clone(), project_key.clone(), file_name.clone())?;
    if document.file.sha256 != expected_sha256 {
        return Err(AppError::Other(
            "Memory 文件已发生变化，请重新加载后再重命名".into(),
        ));
    }
    validate_memory_file_name(&new_file_name)?;
    if file_name == new_file_name {
        return Ok(document);
    }

    let context = memory_context(&claude_dir, &project_key, false)?;
    let source = memory_file_path(&context, &file_name, false)?;
    let destination = memory_file_path(&context, &new_file_name, true)?;
    let destination_exists = destination.try_exists()?;
    let destination_is_source = destination_exists
        && fs::canonicalize(&source)
            .and_then(|source_path| {
                fs::canonicalize(&destination)
                    .map(|destination_path| source_path == destination_path)
            })
            .unwrap_or(false);
    if destination_exists && !destination_is_source {
        return Err(AppError::Other(format!(
            "Memory 文件已存在，无法重命名: {}",
            destination.to_string_lossy()
        )));
    }

    if destination_is_source || file_name.eq_ignore_ascii_case(&new_file_name) {
        rename_file_via_temporary(&source, &destination)?;
    } else {
        fs::rename(&source, &destination)?;
    }
    read_file(claude_dir, project_key, new_file_name)
}

pub fn delete_file(
    claude_dir: String,
    project_key: String,
    file_name: String,
    expected_sha256: String,
) -> AppResult<bool> {
    let context = memory_context(&claude_dir, &project_key, false)?;
    let path = memory_file_path(&context, &file_name, false)?;
    if sha256_file(&path)? != expected_sha256 {
        return Err(AppError::Other(
            "Memory 文件已发生变化，请重新加载后再删除".into(),
        ));
    }
    path_safety::remove_path(
        &context.projects_root,
        &path,
        EntryKind::File,
        "Claude Memory 文件",
    )
}

struct MemoryContext {
    projects_root: PathBuf,
    memory_dir: PathBuf,
}

fn memory_context(
    claude_dir: &str,
    project_key: &str,
    create_memory: bool,
) -> AppResult<MemoryContext> {
    validate_component(project_key, "Claude 项目标识")?;
    let claude = PathBuf::from(claude_dir);
    let projects_root = paths::claude_projects_dir(&claude);
    let project_dir = projects_root.join(project_key);
    path_safety::validate_descendant(
        &projects_root,
        &project_dir,
        EntryKind::Directory,
        false,
        "Claude 项目目录",
    )?;
    let memory_dir = project_dir.join("memory");
    let exists = path_safety::validate_descendant(
        &projects_root,
        &memory_dir,
        EntryKind::Directory,
        true,
        "Claude Memory 目录",
    )?;
    if !exists && create_memory {
        fs::create_dir(&memory_dir)?;
    }
    Ok(MemoryContext {
        projects_root,
        memory_dir,
    })
}

fn memory_file_path(
    context: &MemoryContext,
    file_name: &str,
    allow_missing: bool,
) -> AppResult<PathBuf> {
    validate_memory_file_name(file_name)?;
    let path = context.memory_dir.join(file_name);
    path_safety::validate_descendant(
        &context.projects_root,
        &path,
        EntryKind::File,
        allow_missing,
        "Claude Memory 文件",
    )?;
    Ok(path)
}

fn validate_component(value: &str, label: &str) -> AppResult<()> {
    let path = Path::new(value);
    if value.trim().is_empty()
        || value != value.trim()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(AppError::Path(format!("{label} 非法: {value}")));
    }
    Ok(())
}

fn validate_memory_file_name(file_name: &str) -> AppResult<()> {
    validate_component(file_name, "Memory 文件名")?;
    if Path::new(file_name)
        .extension()
        .and_then(|value| value.to_str())
        .is_none_or(|extension| !extension.eq_ignore_ascii_case("md"))
    {
        return Err(AppError::Path("Memory 文件必须使用 .md 扩展名".into()));
    }
    Ok(())
}

fn list_markdown_paths(projects_root: &Path, memory_dir: &Path) -> AppResult<Vec<PathBuf>> {
    path_safety::validate_descendant(
        projects_root,
        memory_dir,
        EntryKind::Directory,
        false,
        "Claude Memory 目录",
    )?;
    let mut out = Vec::new();
    for entry in fs::read_dir(memory_dir)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if path_safety::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
            continue;
        }
        if entry
            .path()
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            out.push(entry.path());
        }
    }
    Ok(out)
}

fn summarize_file(project_key: &str, path: &Path) -> AppResult<ClaudeMemoryFile> {
    let content = fs::read_to_string(path)?;
    let metadata = fs::metadata(path)?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| AppError::Path("Memory 文件名不是有效 UTF-8".into()))?
        .to_string();
    let title = content
        .lines()
        .find_map(|line| line.trim().strip_prefix('#').map(str::trim))
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            Path::new(&file_name)
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or(&file_name)
                .to_string()
        });
    let preview_source = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or("");
    Ok(ClaudeMemoryFile {
        project_key: project_key.to_string(),
        file_name: file_name.clone(),
        path: path.to_string_lossy().into_owned(),
        title,
        preview: truncate(preview_source, PREVIEW_CHARS),
        bytes: metadata.len(),
        updated_at: modified_seconds(&metadata),
        is_index: file_name.eq_ignore_ascii_case("MEMORY.md"),
        sha256: sha256_bytes(content.as_bytes()),
    })
}

fn infer_project_path(project_dir: &Path) -> Option<String> {
    let mut candidates = fs::read_dir(project_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl"))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|entry| {
        std::cmp::Reverse(
            entry
                .metadata()
                .map(|metadata| modified_seconds(&metadata))
                .unwrap_or_default(),
        )
    });
    for entry in candidates.into_iter().take(3) {
        let Ok(file) = File::open(entry.path()) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok).take(100) {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
                if !cwd.trim().is_empty() {
                    return Some(paths::strip_verbatim(cwd));
                }
            }
        }
    }
    None
}

#[derive(Clone)]
struct ConfiguredProjectPath {
    path: String,
    last_session_id: Option<String>,
}

fn configured_project_paths(claude_dir: &Path) -> HashMap<String, Vec<ConfiguredProjectPath>> {
    let Some(parent) = claude_dir.parent() else {
        return HashMap::new();
    };
    let Ok(content) = fs::read_to_string(parent.join(".claude.json")) else {
        return HashMap::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&content) else {
        return HashMap::new();
    };
    let Some(projects) = value.get("projects").and_then(Value::as_object) else {
        return HashMap::new();
    };

    let mut paths_by_key = HashMap::<String, Vec<ConfiguredProjectPath>>::new();
    for (raw_path, project) in projects {
        let path = raw_path.trim();
        if path.is_empty() {
            continue;
        }
        let encoded = encode_claude_project_dir(path).to_ascii_lowercase();
        let candidates = paths_by_key.entry(encoded).or_default();
        if !candidates.iter().any(|candidate| candidate.path == path) {
            candidates.push(ConfiguredProjectPath {
                path: paths::strip_verbatim(path),
                last_session_id: project
                    .get("lastSessionId")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|session_id| !session_id.is_empty())
                    .map(str::to_string),
            });
        }
    }
    paths_by_key
}

fn configured_project_path(
    configured_paths: &HashMap<String, Vec<ConfiguredProjectPath>>,
    project_key: &str,
    project_dir: &Path,
) -> Option<String> {
    let candidates = configured_paths.get(&project_key.to_ascii_lowercase())?;
    if candidates.len() == 1 {
        return Some(candidates[0].path.clone());
    }
    let matched = candidates
        .iter()
        .filter(|candidate| {
            candidate
                .last_session_id
                .as_deref()
                .is_some_and(|session_id| {
                    project_dir.join(format!("{session_id}.jsonl")).is_file()
                        || project_dir
                            .join(format!("{session_id}.claudinal.json"))
                            .is_file()
                })
        })
        .collect::<Vec<_>>();
    (matched.len() == 1).then(|| matched[0].path.clone())
}

fn encode_claude_project_dir(path: &str) -> String {
    paths::strip_verbatim(path)
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn rename_file_via_temporary(source: &Path, destination: &Path) -> AppResult<()> {
    let parent = source
        .parent()
        .ok_or_else(|| AppError::Path("Memory 文件缺少父目录".into()))?;
    let temporary = (0..1000u32)
        .map(|attempt| {
            parent.join(format!(
                ".cc-sessions-rename-{}-{attempt}.tmp",
                std::process::id()
            ))
        })
        .find(|candidate| !candidate.exists())
        .ok_or_else(|| AppError::Other("无法分配 Memory 重命名临时文件".into()))?;

    fs::rename(source, &temporary)?;
    if let Err(rename_error) = fs::rename(&temporary, destination) {
        if let Err(rollback_error) = fs::rename(&temporary, source) {
            return Err(AppError::Other(format!(
                "Memory 重命名失败且回滚失败；临时文件位于 {}。重命名错误: {rename_error}；回滚错误: {rollback_error}",
                temporary.to_string_lossy()
            )));
        }
        return Err(rename_error.into());
    }
    Ok(())
}

fn modified_seconds(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

fn sha256_file(path: &Path) -> AppResult<String> {
    Ok(sha256_bytes(&fs::read(path)?))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut out = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> AppResult<(PathBuf, String)> {
        let claude = std::env::temp_dir().join(format!(
            "cc-sessions-memory-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let key = "F--project-memory-test".to_string();
        let project = claude.join("projects").join(&key);
        fs::create_dir_all(&project)?;
        fs::write(
            project.join("session.jsonl"),
            serde_json::json!({"cwd":"F:\\project\\memory-test"}).to_string(),
        )?;
        Ok((claude, key))
    }

    #[test]
    fn memory_crud_is_atomic_and_detects_conflicts() -> AppResult<()> {
        let (claude, key) = fixture()?;
        let claude_text = claude.to_string_lossy().into_owned();
        let created = save_file(
            claude_text.clone(),
            key.clone(),
            "MEMORY.md".into(),
            "# Index\n\nFirst".into(),
            None,
        )?;
        assert_eq!(created.file.title, "Index");
        assert!(list_projects(claude_text.clone())?[0].has_index);
        assert_eq!(list_files(claude_text.clone(), key.clone())?.len(), 1);

        let updated = save_file(
            claude_text.clone(),
            key.clone(),
            "MEMORY.md".into(),
            "# Index\n\nSecond".into(),
            Some(created.file.sha256.clone()),
        )?;
        assert!(save_file(
            claude_text.clone(),
            key.clone(),
            "MEMORY.md".into(),
            "stale".into(),
            Some(created.file.sha256)
        )
        .is_err());
        assert!(delete_file(
            claude_text.clone(),
            key.clone(),
            "MEMORY.md".into(),
            updated.file.sha256
        )?);
        assert!(list_files(claude_text, key)?.is_empty());
        fs::remove_dir_all(claude).ok();
        Ok(())
    }

    #[test]
    fn memory_paths_reject_traversal() -> AppResult<()> {
        let (claude, key) = fixture()?;
        assert!(save_file(
            claude.to_string_lossy().into_owned(),
            key,
            "..\\escape.md".into(),
            "no".into(),
            None
        )
        .is_err());
        fs::remove_dir_all(claude).ok();
        Ok(())
    }

    #[test]
    fn project_path_uses_claude_config_when_transcript_is_missing() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "cc-sessions-memory-config-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let claude = root.join(".claude");
        let real_path = "F:/hanweb/project/中文项目";
        let key = real_path
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '-'
                }
            })
            .collect::<String>();
        fs::create_dir_all(claude.join("projects").join(&key).join("memory"))?;
        fs::write(
            claude
                .join("projects")
                .join(&key)
                .join("memory")
                .join("MEMORY.md"),
            "# 中文项目",
        )?;
        fs::write(
            root.join(".claude.json"),
            serde_json::json!({"projects": {real_path: {}}}).to_string(),
        )?;

        let projects = list_projects(claude.to_string_lossy().into_owned())?;
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].project_path, real_path);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn project_path_uses_last_session_sidecar_to_resolve_encoded_collisions() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "cc-sessions-memory-config-collision-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let claude = root.join(".claude");
        let stale_path = "F:/hanweb/project/模板转换";
        let current_path = "F:/hanweb/project/国办平台";
        let key = current_path
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '-'
                }
            })
            .collect::<String>();
        let project = claude.join("projects").join(&key);
        fs::create_dir_all(project.join("memory"))?;
        fs::write(project.join("memory").join("MEMORY.md"), "# 国办平台")?;
        fs::write(project.join("current-session.claudinal.json"), "{}")?;
        fs::write(
            root.join(".claude.json"),
            serde_json::json!({
                "projects": {
                    stale_path: {"lastSessionId": "missing-session"},
                    current_path: {"lastSessionId": "current-session"}
                }
            })
            .to_string(),
        )?;

        let projects = list_projects(claude.to_string_lossy().into_owned())?;
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].project_path, current_path);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn memory_rename_supports_regular_and_case_only_names() -> AppResult<()> {
        let (claude, key) = fixture()?;
        let claude_text = claude.to_string_lossy().into_owned();
        let created = save_file(
            claude_text.clone(),
            key.clone(),
            "notes.md".into(),
            "# Notes".into(),
            None,
        )?;
        let renamed = rename_file(
            claude_text.clone(),
            key.clone(),
            "notes.md".into(),
            "project-notes.md".into(),
            created.file.sha256,
        )?;
        assert_eq!(renamed.file.file_name, "project-notes.md");

        let case_renamed = rename_file(
            claude_text.clone(),
            key.clone(),
            "project-notes.md".into(),
            "PROJECT-NOTES.md".into(),
            renamed.file.sha256,
        )?;
        assert_eq!(case_renamed.file.file_name, "PROJECT-NOTES.md");
        assert_eq!(list_files(claude_text, key)?.len(), 1);

        fs::remove_dir_all(claude).ok();
        Ok(())
    }
}
