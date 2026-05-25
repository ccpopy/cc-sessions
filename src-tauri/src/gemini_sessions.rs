use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::{DateTime, FixedOffset};
use serde::Serialize;
use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::models::{PreviewEvent, SessionMetaBrief, SessionSummary};
use crate::paths;

const PROVIDER: &str = "gemini";
const TITLE_MAX_CHARS: usize = 80;
const SUMMARY_PROTO: &str = "agyhub_summaries_proto.pb";
const ANTIGRAVITY_SIDECAR_DIRS: [&str; 10] = [
    "brain",
    "browser_recordings",
    "code_tracker",
    "context_state",
    "html_artifacts",
    "implicit",
    "knowledge",
    "playground",
    "prompting",
    "scratch",
];

#[derive(Debug, Clone, Default)]
struct Summary {
    title: Option<String>,
    cwd: Option<String>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
}

#[derive(Debug, Clone)]
struct SummaryEntry {
    id: String,
    linked_ids: BTreeSet<String>,
    summary: Summary,
}

#[derive(Debug, Clone)]
struct AntigravityEntry {
    id: String,
    primary_path: PathBuf,
    source_labels: BTreeSet<String>,
    summary: Option<Summary>,
    bytes: u64,
    updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct AssociatedPath {
    pub abs: PathBuf,
    pub rel: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeminiOrphanSummary {
    pub surface: String,
    pub id: String,
    pub linked_ids: Vec<String>,
    pub title: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeminiOrphanReport {
    pub scanned_summaries: u32,
    pub orphan_summaries: u32,
    pub removed_summaries: u32,
    pub dry_run: bool,
    pub items: Vec<GeminiOrphanSummary>,
}

pub fn scan_sessions(gemini_dir: &Path) -> AppResult<Vec<SessionSummary>> {
    if !gemini_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    sessions.extend(scan_cli_sessions(gemini_dir)?);
    sessions.extend(scan_antigravity_sessions(gemini_dir)?);
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at.max(s.created_at)));
    Ok(sessions)
}

fn scan_cli_sessions(gemini_dir: &Path) -> AppResult<Vec<SessionSummary>> {
    let tmp = gemini_dir.join("tmp");
    if !tmp.is_dir() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for project in fs::read_dir(&tmp)? {
        let project = project?;
        let project_path = project.path();
        if !project_path.is_dir() {
            continue;
        }
        let project_hash = project_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_string();
        let chats = project_path.join("chats");
        if !chats.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&chats)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() || !is_json_session_file(&path) {
                continue;
            }
            if let Some(session) = parse_cli_session(&path, &project_hash)? {
                out.push(session);
            }
        }
    }
    Ok(out)
}

fn is_json_session_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("json") | Some("jsonl")
    )
}

fn parse_cli_session(path: &Path, project_hash: &str) -> AppResult<Option<SessionSummary>> {
    let raw = fs::read_to_string(path)?;
    let first = raw.trim_start().chars().next();
    match first {
        Some('{') | Some('[') => parse_cli_json_session(path, project_hash, &raw),
        _ => parse_cli_jsonl_session(path, project_hash),
    }
}

fn parse_cli_json_session(
    path: &Path,
    project_hash: &str,
    raw: &str,
) -> AppResult<Option<SessionSummary>> {
    let value: Value = serde_json::from_str(raw)?;
    let messages = match &value {
        Value::Object(map) => map.get("messages").and_then(Value::as_array).cloned(),
        Value::Array(items) => Some(items.clone()),
        _ => None,
    }
    .unwrap_or_default();

    let id = value
        .get("sessionId")
        .or_else(|| value.get("session_id"))
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| infer_session_id_from_filename(path));
    let Some(id) = id else {
        return Ok(None);
    };

    let mut created_at = value
        .get("startTime")
        .or_else(|| value.get("createdAt"))
        .and_then(parse_timestamp_value);
    let mut updated_at = value
        .get("lastUpdated")
        .or_else(|| value.get("updatedAt"))
        .and_then(parse_timestamp_value);
    let mut first_user_message = None;
    let mut tail_summary = None;
    let mut model = value.get("model").and_then(Value::as_str).map(String::from);
    let mut tokens_used = usage_tokens(value.get("usage"));

    for message in messages {
        ingest_cli_message(
            &message,
            &mut created_at,
            &mut updated_at,
            &mut first_user_message,
            &mut tail_summary,
            &mut model,
            &mut tokens_used,
        );
    }

    let bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let cwd = cli_project_cwd(project_hash);
    let title = first_user_message
        .clone()
        .or_else(|| tail_summary.clone())
        .or_else(|| value.get("title").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| id.clone());
    let created = created_at.unwrap_or_else(|| file_mtime_seconds(path));
    let updated = updated_at.or(created_at).unwrap_or(created);

    Ok(Some(SessionSummary {
        provider: PROVIDER.to_string(),
        id: id.clone(),
        rollout_path: path.to_string_lossy().into_owned(),
        cwd: cwd.clone(),
        cwd_display: cli_project_display(project_hash),
        title: truncate_summary(&title, TITLE_MAX_CHARS),
        first_user_message: first_user_message.unwrap_or_default(),
        model,
        reasoning_effort: None,
        source: Some("gemini-cli".into()),
        agent_nickname: None,
        agent_role: None,
        tokens_used,
        created_at: created,
        updated_at: updated,
        archived: false,
        git_branch: None,
        rollout_bytes: bytes,
        logs_count: 0,
        has_backup: false,
        resume_command: format!("gemini --resume {id}"),
    }))
}

fn parse_cli_jsonl_session(path: &Path, project_hash: &str) -> AppResult<Option<SessionSummary>> {
    let f = File::open(path)?;
    let mut id = infer_session_id_from_filename(path);
    let mut created_at = None;
    let mut updated_at = None;
    let mut first_user_message = None;
    let mut tail_summary = None;
    let mut model = None;
    let mut tokens_used = 0i64;

    for line in BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if id.is_none() {
            id = value
                .get("sessionId")
                .or_else(|| value.get("session_id"))
                .or_else(|| value.get("id"))
                .and_then(Value::as_str)
                .map(String::from);
        }
        ingest_cli_message(
            &value,
            &mut created_at,
            &mut updated_at,
            &mut first_user_message,
            &mut tail_summary,
            &mut model,
            &mut tokens_used,
        );
    }

    let Some(id) = id else {
        return Ok(None);
    };
    let bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let cwd = cli_project_cwd(project_hash);
    let title = first_user_message
        .clone()
        .or(tail_summary)
        .unwrap_or_else(|| id.clone());
    let created = created_at.unwrap_or_else(|| file_mtime_seconds(path));
    let updated = updated_at.or(created_at).unwrap_or(created);

    Ok(Some(SessionSummary {
        provider: PROVIDER.to_string(),
        id: id.clone(),
        rollout_path: path.to_string_lossy().into_owned(),
        cwd,
        cwd_display: cli_project_display(project_hash),
        title: truncate_summary(&title, TITLE_MAX_CHARS),
        first_user_message: first_user_message.unwrap_or_default(),
        model,
        reasoning_effort: None,
        source: Some("gemini-cli".into()),
        agent_nickname: None,
        agent_role: None,
        tokens_used,
        created_at: created,
        updated_at: updated,
        archived: false,
        git_branch: None,
        rollout_bytes: bytes,
        logs_count: 0,
        has_backup: false,
        resume_command: format!("gemini --resume {id}"),
    }))
}

fn ingest_cli_message(
    value: &Value,
    created_at: &mut Option<i64>,
    updated_at: &mut Option<i64>,
    first_user_message: &mut Option<String>,
    tail_summary: &mut Option<String>,
    model: &mut Option<String>,
    tokens_used: &mut i64,
) {
    if let Some(ts) = value
        .get("timestamp")
        .or_else(|| value.get("time"))
        .and_then(parse_timestamp_value)
    {
        created_at.get_or_insert(ts);
        *updated_at = Some(ts);
    }

    if model.is_none() {
        *model = value
            .get("model")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| {
                value
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(Value::as_str)
                    .map(String::from)
            });
    }

    *tokens_used += usage_tokens(value.get("usage"));
    *tokens_used += value
        .get("message")
        .map(|m| usage_tokens(m.get("usage")))
        .unwrap_or(0);

    let role = value
        .get("role")
        .or_else(|| value.get("type"))
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("message")
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
        })
        .unwrap_or("");
    let text = cli_message_text(value);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    if first_user_message.is_none() && role == "user" {
        *first_user_message = Some(trimmed.to_string());
    }
    if !matches!(role, "info" | "system") {
        *tail_summary = Some(trimmed.to_string());
    } else if tail_summary.is_none() {
        *tail_summary = Some(trimmed.to_string());
    }
}

fn cli_message_text(value: &Value) -> String {
    value
        .get("content")
        .map(extract_text)
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            value
                .get("message")
                .and_then(|m| m.get("content"))
                .map(extract_text)
                .filter(|text| !text.trim().is_empty())
        })
        .or_else(|| value.get("text").and_then(Value::as_str).map(String::from))
        .unwrap_or_default()
}

fn scan_antigravity_sessions(gemini_dir: &Path) -> AppResult<Vec<SessionSummary>> {
    let mut summary_index = HashMap::<String, Summary>::new();
    for surface in antigravity_surfaces() {
        let dir = gemini_dir.join(surface);
        if dir.is_dir() {
            summary_index.extend(read_summary_index(&dir)?);
        }
    }

    let mut by_id: HashMap<String, AntigravityEntry> = HashMap::new();
    for surface in antigravity_surfaces() {
        let surface_dir = gemini_dir.join(surface);
        let conversations = surface_dir.join("conversations");
        if !conversations.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&conversations)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("pb") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(String::from)
            else {
                continue;
            };
            let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let mtime = file_mtime_seconds(&path);
            let item = by_id.entry(id.clone()).or_insert_with(|| AntigravityEntry {
                id: id.clone(),
                primary_path: path.clone(),
                source_labels: BTreeSet::new(),
                summary: summary_index.get(&id).cloned(),
                bytes: 0,
                updated_at: 0,
            });
            item.source_labels.insert(surface.to_string());
            item.bytes += bytes;
            if mtime >= item.updated_at {
                item.updated_at = mtime;
                item.primary_path = path;
            }
            if item.summary.is_none() {
                item.summary = summary_index.get(&id).cloned();
            }
        }
    }

    Ok(by_id
        .into_values()
        .map(antigravity_summary)
        .collect::<Vec<_>>())
}

fn antigravity_summary(item: AntigravityEntry) -> SessionSummary {
    let source = item.source_labels.into_iter().collect::<Vec<_>>().join("+");
    let summary = item.summary.unwrap_or_default();
    let title = summary
        .title
        .clone()
        .unwrap_or_else(|| format!("Antigravity 会话 {}", short_id(&item.id)));
    let cwd = summary.cwd.unwrap_or_else(|| "antigravity".to_string());
    let created_at = summary.created_at.unwrap_or(item.updated_at);
    let updated_at = summary
        .updated_at
        .unwrap_or(item.updated_at)
        .max(item.updated_at);
    SessionSummary {
        provider: PROVIDER.to_string(),
        id: item.id.clone(),
        rollout_path: item.primary_path.to_string_lossy().into_owned(),
        cwd: cwd.clone(),
        cwd_display: paths::basename_display(&cwd),
        title: truncate_summary(&title, TITLE_MAX_CHARS),
        first_user_message: String::new(),
        model: None,
        reasoning_effort: None,
        source: Some(source),
        agent_nickname: None,
        agent_role: None,
        tokens_used: 0,
        created_at,
        updated_at,
        archived: false,
        git_branch: None,
        rollout_bytes: item.bytes,
        logs_count: 0,
        has_backup: false,
        resume_command: "agy".to_string(),
    }
}

fn antigravity_surfaces() -> [&'static str; 3] {
    ["antigravity-cli", "antigravity", "antigravity-ide"]
}

pub fn preview_range(path: &str, offset: usize, limit: usize) -> AppResult<Vec<PreviewEvent>> {
    let p = PathBuf::from(path);
    if p.extension().and_then(|ext| ext.to_str()) == Some("pb") {
        return preview_antigravity(path, offset, limit);
    }

    if is_json_session_file(&p) {
        return preview_cli_json(path, offset, limit);
    }

    Err(AppError::Other(format!(
        "不支持的 Gemini 会话文件: {}",
        p.to_string_lossy()
    )))
}

fn preview_antigravity(path: &str, offset: usize, limit: usize) -> AppResult<Vec<PreviewEvent>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let p = PathBuf::from(path);
    let id = infer_session_id_from_filename(&p).unwrap_or_else(|| "unknown".into());
    let bytes = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
    let mut events = antigravity_brain_events(&p, &id)?;
    if events.is_empty() {
        if let Some(summary_event) = antigravity_summary_event(&p, &id, bytes)? {
            events.push(summary_event);
        } else {
            events.push(PreviewEvent {
                index: 0,
                timestamp: file_mtime_seconds(&p).to_string(),
                role: "assistant".into(),
                kind: "antigravity_protobuf".into(),
                text_summary: format!(
                    "Antigravity 会话主体是 protobuf 二进制文件，未找到可展示的 brain 产物。id={id}, bytes={bytes}"
                ),
                raw: serde_json::json!({
                    "provider": PROVIDER,
                    "message": {
                        "role": "assistant",
                        "content": format!("Antigravity 会话主体是 protobuf 二进制文件，未找到可展示的 brain 产物。\n\nid: {id}\nbytes: {bytes}")
                    },
                    "id": id,
                    "path": path,
                    "bytes": bytes,
                    "format": "protobuf"
                }),
            });
        }
    }
    for (index, event) in events.iter_mut().enumerate() {
        event.index = index;
    }
    Ok(events.into_iter().skip(offset).take(limit).collect())
}

fn antigravity_summary_event(path: &Path, id: &str, bytes: u64) -> AppResult<Option<PreviewEvent>> {
    let Some((gemini_dir, surface, _)) = antigravity_path_parts(path) else {
        return Ok(None);
    };
    let surface_dir = gemini_dir.join(surface);
    let index = read_summary_index(&surface_dir)?;
    let Some(summary) = index.get(id) else {
        return Ok(None);
    };
    let title = summary.title.as_deref().unwrap_or("(无标题)");
    let cwd = summary.cwd.as_deref().unwrap_or("(未知项目)");
    let created = summary
        .created_at
        .map(|v| v.to_string())
        .unwrap_or_else(|| "(未知)".to_string());
    let updated = summary
        .updated_at
        .map(|v| v.to_string())
        .unwrap_or_else(|| "(未知)".to_string());
    let content = format!(
        "### Antigravity history summary\n\n\
         这个会话没有可展示的 `brain/<session-id>` 产物，`.pb` 主体也不是可直接读取的文本 transcript。下面是从 `agyhub_summaries_proto.pb` 解析出的历史摘要。\n\n\
         - 标题：{title}\n\
         - 项目：{cwd}\n\
         - 会话 ID：{id}\n\
         - protobuf 大小：{bytes} bytes\n\
         - 创建时间戳：{created}\n\
         - 更新时间戳：{updated}"
    );
    Ok(Some(PreviewEvent {
        index: 0,
        timestamp: summary
            .updated_at
            .unwrap_or_else(|| file_mtime_seconds(path))
            .to_string(),
        role: "assistant".into(),
        kind: "antigravity_summary".into(),
        text_summary: truncate_summary(&content, 120),
        raw: serde_json::json!({
            "provider": PROVIDER,
            "type": "antigravity_summary",
            "id": id,
            "path": path.to_string_lossy(),
            "bytes": bytes,
            "title": title,
            "cwd": cwd,
            "message": {
                "role": "assistant",
                "content": content
            }
        }),
    }))
}

fn antigravity_brain_events(path: &Path, id: &str) -> AppResult<Vec<PreviewEvent>> {
    let Some((gemini_dir, _, _)) = antigravity_path_parts(path) else {
        return Ok(Vec::new());
    };

    let mut files = Vec::<PathBuf>::new();
    for surface in antigravity_surfaces() {
        let brain = gemini_dir.join(surface).join("brain").join(id);
        if !brain.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&brain)? {
            let entry = entry?;
            let p = entry.path();
            if !p.is_file() || !is_previewable_brain_file(&p) {
                continue;
            }
            files.push(p);
        }
    }
    files.sort_by(|a, b| {
        brain_file_rank(a)
            .cmp(&brain_file_rank(b))
            .then_with(|| a.file_name().cmp(&b.file_name()))
            .then_with(|| a.cmp(b))
    });

    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for file in files {
        let text = fs::read_to_string(&file)?;
        if text.trim().is_empty() {
            continue;
        }
        let name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artifact")
            .to_string();
        let fingerprint = format!("{name}\n{text}");
        if !seen.insert(fingerprint) {
            continue;
        }
        let title = brain_artifact_title(&name);
        let content = format!("### {title}\n\n{text}");
        out.push(PreviewEvent {
            index: 0,
            timestamp: file_mtime_seconds(&file).to_string(),
            role: "assistant".into(),
            kind: "antigravity_artifact".into(),
            text_summary: truncate_summary(&content, 120),
            raw: serde_json::json!({
                "provider": PROVIDER,
                "type": "antigravity_artifact",
                "artifact": name,
                "path": file.to_string_lossy(),
                "message": {
                    "role": "assistant",
                    "content": content
                }
            }),
        });
    }
    Ok(out)
}

fn antigravity_path_parts(path: &Path) -> Option<(PathBuf, String, String)> {
    let id = infer_session_id_from_filename(path)?;
    let conversations = path.parent()?;
    if conversations.file_name().and_then(|name| name.to_str()) != Some("conversations") {
        return None;
    }
    let surface_dir = conversations.parent()?;
    let surface = surface_dir.file_name()?.to_str()?.to_string();
    if !antigravity_surfaces().contains(&surface.as_str()) {
        return None;
    }
    let gemini_dir = surface_dir.parent()?.to_path_buf();
    Some((gemini_dir, surface, id))
}

fn is_previewable_brain_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if name.ends_with(".metadata.json") {
        return false;
    }
    name.ends_with(".md") || name.contains(".md.resolved")
}

fn brain_file_rank(path: &Path) -> u8 {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name == "task.md" {
        0
    } else if name == "implementation_plan.md" {
        1
    } else if name.ends_with(".md") {
        2
    } else {
        3
    }
}

fn brain_artifact_title(name: &str) -> String {
    let trimmed = name
        .trim_end_matches(".resolved")
        .trim_end_matches(".resolved.0")
        .trim_end_matches(".resolved.1")
        .trim_end_matches(".md");
    trimmed.replace('_', " ").replace('-', " ")
}

fn preview_cli_json(path: &str, offset: usize, limit: usize) -> AppResult<Vec<PreviewEvent>> {
    let raw = fs::read_to_string(path)?;
    let first = raw.trim_start().chars().next();
    let mut events = Vec::new();
    if matches!(first, Some('{') | Some('[')) {
        let value: Value = serde_json::from_str(&raw)?;
        let messages = match &value {
            Value::Object(map) => map.get("messages").and_then(Value::as_array).cloned(),
            Value::Array(items) => Some(items.clone()),
            _ => None,
        }
        .unwrap_or_default();
        for (index, message) in messages.into_iter().enumerate() {
            events.push(classify_cli_event(index, message));
        }
    } else {
        for (index, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                events.push(classify_cli_event(index, value));
            }
        }
    }
    Ok(events.into_iter().skip(offset).take(limit).collect())
}

fn classify_cli_event(index: usize, raw: Value) -> PreviewEvent {
    let timestamp = raw
        .get("timestamp")
        .or_else(|| raw.get("time"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let kind = raw
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
        .to_string();
    let role = raw
        .get("role")
        .and_then(Value::as_str)
        .or_else(|| raw.get("type").and_then(Value::as_str))
        .unwrap_or("other")
        .to_string();
    let text = cli_message_text(&raw);
    PreviewEvent {
        index,
        timestamp,
        role: role.clone(),
        kind,
        text_summary: truncate_summary(&text, 120),
        raw: serde_json::json!({
            "provider": PROVIDER,
            "message": {
                "role": role,
                "content": text
            },
            "raw": raw
        }),
    }
}

pub fn preview_meta(path: &str) -> AppResult<SessionMetaBrief> {
    let p = PathBuf::from(path);
    let id = infer_session_id_from_filename(&p);
    let source = if p.extension().and_then(|ext| ext.to_str()) == Some("pb") {
        "antigravity"
    } else {
        "gemini-cli"
    };
    Ok(SessionMetaBrief {
        id,
        timestamp: Some(file_mtime_seconds(&p).to_string()),
        cwd: None,
        originator: None,
        cli_version: None,
        source: Some(source.to_string()),
        model_provider: Some(PROVIDER.to_string()),
    })
}

pub fn session_relpath(gemini_dir: &Path, source_path: &Path) -> PathBuf {
    source_path
        .strip_prefix(gemini_dir)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| {
            source_path
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("unknown"))
        })
}

pub fn associated_paths(gemini_dir: &Path, id: &str) -> AppResult<Vec<AssociatedPath>> {
    let mut out = Vec::new();
    if !gemini_dir.is_dir() {
        return Ok(out);
    }

    for surface in antigravity_surfaces() {
        let dir = gemini_dir.join(surface);
        for rel in [
            PathBuf::from("conversations").join(format!("{id}.pb")),
            PathBuf::from("annotations").join(format!("{id}.pbtxt")),
        ] {
            let abs = dir.join(&rel);
            if abs.exists() {
                out.push(AssociatedPath {
                    rel: PathBuf::from(surface).join(rel),
                    abs,
                });
            }
        }
        for sidecar in ANTIGRAVITY_SIDECAR_DIRS {
            let rel = PathBuf::from(sidecar).join(id);
            let abs = dir.join(&rel);
            if abs.exists() {
                out.push(AssociatedPath {
                    rel: PathBuf::from(surface).join(rel),
                    abs,
                });
            }
        }
    }

    let tmp = gemini_dir.join("tmp");
    if tmp.is_dir() {
        for entry in walkdir::WalkDir::new(&tmp)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            let path = entry.path();
            if entry.file_type().is_file()
                && is_json_session_file(path)
                && path.file_name().and_then(|name| name.to_str()) != Some("logs.json")
            {
                if file_contains_session_id(path, id)? {
                    out.push(AssociatedPath {
                        abs: path.to_path_buf(),
                        rel: session_relpath(gemini_dir, path),
                    });
                }
            } else if entry.file_type().is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name == id)
                    .unwrap_or(false)
            {
                out.push(AssociatedPath {
                    abs: path.to_path_buf(),
                    rel: session_relpath(gemini_dir, path),
                });
            }
        }
    }

    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out.dedup_by(|a, b| a.abs == b.abs);
    Ok(out)
}

fn file_contains_session_id(path: &Path, id: &str) -> AppResult<bool> {
    let raw = fs::read_to_string(path)?;
    if !raw.contains(id) {
        return Ok(false);
    }
    if let Ok(value) = serde_json::from_str::<Value>(&raw) {
        return Ok(json_has_session_id(&value, id));
    }
    for line in raw.lines() {
        if line.contains(id) {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                if json_has_session_id(&value, id) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn json_has_session_id(value: &Value, id: &str) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "sessionId" | "session_id" | "conversationId" | "conversation_id" | "id"
            ) && value.as_str() == Some(id)
                || json_has_session_id(value, id)
        }),
        Value::Array(items) => items.iter().any(|value| json_has_session_id(value, id)),
        _ => false,
    }
}

pub fn delete_session(gemini_dir: &Path, id: &str) -> AppResult<crate::models::DeleteResult> {
    let mut result = crate::models::DeleteResult {
        id: id.to_string(),
        threads_rows_deleted: 0,
        logs_rows_deleted: 0,
        history_rows_deleted: 0,
        rollout_deleted: false,
        rollout_missing: false,
        ok: false,
        error: None,
    };

    let associated = associated_paths(gemini_dir, id)?;
    if associated.is_empty() {
        result.rollout_missing = true;
    }
    for item in associated {
        let removed = if item.abs.is_dir() {
            fs::remove_dir_all(&item.abs)
        } else {
            fs::remove_file(&item.abs)
        };
        match removed {
            Ok(_) => {
                result.rollout_deleted = true;
                result.threads_rows_deleted += 1;
                prune_empty_parents_until(&item.abs, gemini_dir);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                result.rollout_missing = true;
            }
            Err(e) => append_error(&mut result, format!("remove failed: {}", e)),
        }
    }

    let logs_removed = prune_cli_logs(gemini_dir, id)?;
    result.logs_rows_deleted = logs_removed;

    let summaries_removed = prune_antigravity_summaries(gemini_dir, id)?;
    result.history_rows_deleted = summaries_removed;

    result.ok =
        result.rollout_deleted || result.logs_rows_deleted > 0 || result.history_rows_deleted > 0;
    Ok(result)
}

fn prune_empty_parents_until(path: &Path, root: &Path) {
    let mut cur = path.parent();
    while let Some(dir) = cur {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}

fn prune_cli_logs(gemini_dir: &Path, id: &str) -> AppResult<u32> {
    let mut removed = 0u32;
    let tmp = gemini_dir.join("tmp");
    if !tmp.is_dir() {
        return Ok(removed);
    }
    for entry in walkdir::WalkDir::new(&tmp)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        if !entry.file_type().is_file()
            || path.file_name().and_then(|n| n.to_str()) != Some("logs.json")
        {
            continue;
        }
        let raw = fs::read_to_string(path)?;
        let Ok(mut value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Value::Array(items) = &mut value {
            let before = items.len();
            items.retain(|item| !json_has_session_id(item, id));
            let delta = before.saturating_sub(items.len());
            if delta > 0 {
                fs::write(path, serde_json::to_vec_pretty(&value)?)?;
                removed += delta as u32;
            }
        }
    }
    Ok(removed)
}

fn prune_antigravity_summaries(gemini_dir: &Path, id: &str) -> AppResult<u32> {
    let mut removed = 0u32;
    for surface in antigravity_surfaces() {
        let path = gemini_dir.join(surface).join(SUMMARY_PROTO);
        if path.is_file() {
            removed += remove_summary_entry(&path, id)?;
        }
    }
    Ok(removed)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn diagnose_gemini_orphans(gemini_dir: String) -> AppResult<GeminiOrphanReport> {
    prune_gemini_orphans(gemini_dir, true)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn prune_gemini_orphans(gemini_dir: String, dry_run: bool) -> AppResult<GeminiOrphanReport> {
    let gemini = PathBuf::from(gemini_dir);
    let mut report = GeminiOrphanReport {
        scanned_summaries: 0,
        orphan_summaries: 0,
        removed_summaries: 0,
        dry_run,
        items: Vec::new(),
    };
    if !gemini.is_dir() {
        return Ok(report);
    }

    for surface in antigravity_surfaces() {
        let surface_dir = gemini.join(surface);
        let path = surface_dir.join(SUMMARY_PROTO);
        if !path.is_file() {
            continue;
        }
        let data = fs::read(&path)?;
        let fields = parse_proto_fields(&data);
        let mut next = Vec::with_capacity(data.len());
        let mut cursor = 0usize;
        let mut removed = 0u32;

        for field in fields {
            if field.start > cursor {
                next.extend_from_slice(&data[cursor..field.start]);
            }
            let orphan = field.number == 1
                && field.wire_type == 2
                && parse_summary_entry(field.value)
                    .map(|entry| {
                        report.scanned_summaries += 1;
                        if summary_entry_is_orphan(&surface_dir, &entry) {
                            report.orphan_summaries += 1;
                            report.items.push(GeminiOrphanSummary {
                                surface: surface.to_string(),
                                id: entry.id.clone(),
                                linked_ids: entry.linked_ids.iter().cloned().collect(),
                                title: entry
                                    .summary
                                    .title
                                    .clone()
                                    .unwrap_or_else(|| entry.id.clone()),
                            });
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
            if orphan {
                removed += 1;
            } else {
                next.extend_from_slice(&data[field.start..field.end]);
            }
            cursor = field.end;
        }
        if cursor < data.len() {
            next.extend_from_slice(&data[cursor..]);
        }
        if removed > 0 && !dry_run {
            write_summary_bytes(&path, &next)?;
            report.removed_summaries += removed;
        }
    }

    Ok(report)
}

fn summary_entry_is_orphan(surface_dir: &Path, entry: &SummaryEntry) -> bool {
    if summary_conversation_exists(surface_dir, &entry.id) {
        return false;
    }
    !entry
        .linked_ids
        .iter()
        .any(|id| summary_conversation_exists(surface_dir, id))
}

fn read_summary_index(surface_dir: &Path) -> AppResult<HashMap<String, Summary>> {
    let path = surface_dir.join(SUMMARY_PROTO);
    if !path.is_file() {
        return Ok(HashMap::new());
    }
    let data = fs::read(&path)?;
    let mut out = HashMap::new();
    for field in parse_proto_fields(&data) {
        if field.number == 1 && field.wire_type == 2 {
            if let Some(entry) = parse_summary_entry(field.value) {
                out.insert(entry.id.clone(), entry.summary.clone());
                for linked_id in &entry.linked_ids {
                    if summary_conversation_exists(surface_dir, linked_id) {
                        out.entry(linked_id.clone())
                            .or_insert_with(|| entry.summary.clone());
                    }
                }
            }
        }
    }
    Ok(out)
}

fn remove_summary_entry(path: &Path, id: &str) -> AppResult<u32> {
    let data = fs::read(path)?;
    let fields = parse_proto_fields(&data);
    let mut next = Vec::with_capacity(data.len());
    let mut removed = 0u32;
    let mut cursor = 0usize;
    for field in fields {
        if field.start > cursor {
            next.extend_from_slice(&data[cursor..field.start]);
        }
        let remove = field.number == 1
            && field.wire_type == 2
            && parse_summary_entry(field.value)
                .map(|entry| entry.matches_id(id))
                .unwrap_or(false);
        if remove {
            removed += 1;
        } else {
            next.extend_from_slice(&data[field.start..field.end]);
        }
        cursor = field.end;
    }
    if cursor < data.len() {
        next.extend_from_slice(&data[cursor..]);
    }
    if removed > 0 {
        write_summary_bytes(path, &next)?;
    }
    Ok(removed)
}

fn write_summary_bytes(path: &Path, data: &[u8]) -> AppResult<()> {
    let tmp = path.with_extension("pb.tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(data)?;
        f.flush()?;
    }
    fs::rename(tmp, path)?;
    Ok(())
}

fn parse_summary_entry(data: &[u8]) -> Option<SummaryEntry> {
    let mut id = None;
    let mut summary = Summary::default();
    let mut linked_ids = BTreeSet::new();
    for field in parse_proto_fields(data) {
        match (field.number, field.wire_type) {
            (1, 2) => id = decode_utf8(field.value),
            (2, 2) => parse_summary_detail(field.value, &mut summary, &mut linked_ids),
            _ => {}
        }
    }
    id.map(|id| SummaryEntry {
        id,
        linked_ids,
        summary,
    })
}

impl SummaryEntry {
    fn matches_id(&self, id: &str) -> bool {
        self.id == id || self.linked_ids.contains(id)
    }
}

fn parse_summary_detail(data: &[u8], summary: &mut Summary, linked_ids: &mut BTreeSet<String>) {
    for field in parse_proto_fields(data) {
        match (field.number, field.wire_type) {
            (1, 2) => summary.title = decode_utf8(field.value).filter(|s| !s.trim().is_empty()),
            (4, 2) => {
                if let Some(id) = decode_utf8(field.value).filter(|s| is_uuid_like(s)) {
                    linked_ids.insert(id);
                }
            }
            (3 | 7 | 10, 2) => {
                if let Some(ts) = parse_proto_timestamp(field.value) {
                    summary.created_at =
                        Some(summary.created_at.map(|cur| cur.min(ts)).unwrap_or(ts));
                    summary.updated_at =
                        Some(summary.updated_at.map(|cur| cur.max(ts)).unwrap_or(ts));
                }
            }
            (9, 2) => {
                if summary.cwd.is_none() {
                    summary.cwd = decode_utf8(field.value).and_then(|s| extract_file_uri_path(&s));
                }
            }
            (17, 2) => parse_workspace_detail(field.value, summary),
            _ => {}
        }
    }
}

fn summary_conversation_exists(surface_dir: &Path, id: &str) -> bool {
    surface_dir
        .join("conversations")
        .join(format!("{id}.pb"))
        .is_file()
}

fn is_uuid_like(value: &str) -> bool {
    value.len() == 36
        && value.chars().enumerate().all(|(idx, ch)| {
            matches!(idx, 8 | 13 | 18 | 23) && ch == '-'
                || !matches!(idx, 8 | 13 | 18 | 23) && ch.is_ascii_hexdigit()
        })
}

fn parse_workspace_detail(data: &[u8], summary: &mut Summary) {
    for field in parse_proto_fields(data) {
        if field.wire_type != 2 {
            continue;
        }
        if matches!(field.number, 1 | 7) {
            if let Some(cwd) = decode_utf8(field.value).and_then(|s| extract_file_uri_path(&s)) {
                summary.cwd = Some(cwd);
                return;
            }
        }
    }
}

fn parse_proto_timestamp(data: &[u8]) -> Option<i64> {
    let mut seconds = None;
    for field in parse_proto_fields(data) {
        if field.number == 1 && field.wire_type == 0 {
            seconds = Some(field.varint as i64);
        }
    }
    seconds
}

#[derive(Debug, Clone, Copy)]
struct ProtoField<'a> {
    number: u32,
    wire_type: u8,
    start: usize,
    end: usize,
    value: &'a [u8],
    varint: u64,
}

fn parse_proto_fields(data: &[u8]) -> Vec<ProtoField<'_>> {
    let mut fields = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let start = pos;
        let Some(key) = read_varint(data, &mut pos) else {
            break;
        };
        let number = (key >> 3) as u32;
        let wire_type = (key & 7) as u8;
        match wire_type {
            0 => {
                let Some(varint) = read_varint(data, &mut pos) else {
                    break;
                };
                fields.push(ProtoField {
                    number,
                    wire_type,
                    start,
                    end: pos,
                    value: &[],
                    varint,
                });
            }
            1 => {
                if pos + 8 > data.len() {
                    break;
                }
                let value = &data[pos..pos + 8];
                pos += 8;
                fields.push(ProtoField {
                    number,
                    wire_type,
                    start,
                    end: pos,
                    value,
                    varint: 0,
                });
            }
            2 => {
                let Some(len) = read_varint(data, &mut pos).map(|n| n as usize) else {
                    break;
                };
                if pos + len > data.len() {
                    break;
                }
                let value = &data[pos..pos + len];
                pos += len;
                fields.push(ProtoField {
                    number,
                    wire_type,
                    start,
                    end: pos,
                    value,
                    varint: 0,
                });
            }
            5 => {
                if pos + 4 > data.len() {
                    break;
                }
                let value = &data[pos..pos + 4];
                pos += 4;
                fields.push(ProtoField {
                    number,
                    wire_type,
                    start,
                    end: pos,
                    value,
                    varint: 0,
                });
            }
            _ => break,
        }
    }
    fields
}

fn read_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut shift = 0u32;
    let mut value = 0u64;
    while *pos < data.len() && shift < 64 {
        let byte = data[*pos];
        *pos += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

fn decode_utf8(data: &[u8]) -> Option<String> {
    String::from_utf8(data.to_vec()).ok()
}

fn extract_file_uri_path(raw: &str) -> Option<String> {
    let marker = "file:///";
    let start = raw.find(marker)? + marker.len();
    let tail = &raw[start..];
    let path = tail
        .trim_start_matches('<')
        .split(|c: char| c == '>' || c.is_control() || c.is_whitespace())
        .next()
        .unwrap_or("")
        .trim();
    if path.is_empty() {
        return None;
    }
    let decoded = percent_decode(path);
    let windows = if decoded.len() >= 2 && decoded.as_bytes()[1] == b':' {
        decoded.replace('/', "\\")
    } else {
        format!("/{}", decoded)
    };
    Some(paths::strip_verbatim(&windows))
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(a), Some(b)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((a << 4) | b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_timestamp_value(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(if n > 1_000_000_000_000 { n / 1000 } else { n });
    }
    if let Some(n) = value.as_f64() {
        let n = n as i64;
        return Some(if n > 1_000_000_000_000 { n / 1000 } else { n });
    }
    let raw = value.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt: DateTime<FixedOffset>| dt.timestamp())
}

fn usage_tokens(value: Option<&Value>) -> i64 {
    let Some(Value::Object(map)) = value else {
        return 0;
    };
    [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "total_tokens",
        "promptTokenCount",
        "candidatesTokenCount",
        "totalTokenCount",
    ]
    .iter()
    .filter_map(|key| map.get(*key).and_then(Value::as_i64))
    .sum()
}

fn extract_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(extract_text_item)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => map
            .get("text")
            .or_else(|| map.get("content"))
            .map(extract_text)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn extract_text_item(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(text) = value.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    if let Some(content) = value.get("content") {
        let text = extract_text(content);
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    if let Some(name) = value.get("name").and_then(Value::as_str) {
        return Some(format!("[Tool: {name}]"));
    }
    None
}

fn infer_session_id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    if let Some(rest) = stem.strip_prefix("session-") {
        let suffix = rest.rsplit('-').next().unwrap_or(rest);
        if suffix.len() >= 8 {
            return Some(suffix.to_string());
        }
    }
    Some(stem.to_string())
}

fn cli_project_cwd(project_hash: &str) -> String {
    if project_hash.is_empty() {
        "gemini-cli".into()
    } else {
        format!("gemini-cli/{project_hash}")
    }
}

fn cli_project_display(project_hash: &str) -> String {
    if project_hash.is_empty() {
        "gemini-cli".into()
    } else {
        format!("gemini-cli {}", short_id(project_hash))
    }
}

fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

fn file_mtime_seconds(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn truncate_summary(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

fn append_error(result: &mut crate::models::DeleteResult, msg: String) {
    result.error = Some(match result.error.take() {
        Some(prev) => format!("{prev}; {msg}"),
        None => msg,
    });
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

    #[test]
    fn scans_gemini_cli_json_session() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-gemini-cli-test");
        let chats = root.join("tmp").join("projecthash").join("chats");
        fs::create_dir_all(&chats)?;
        fs::write(
            chats.join("session-2026-01-13T09-11-abcd1234.json"),
            serde_json::to_string(&serde_json::json!({
                "sessionId": "gemini-cli-1",
                "projectHash": "projecthash",
                "startTime": "2026-01-13T09:11:39Z",
                "lastUpdated": "2026-01-13T09:17:39Z",
                "messages": [
                    {"type": "user", "timestamp": "2026-01-13T09:11:39Z", "content": "hello gemini"},
                    {"type": "assistant", "timestamp": "2026-01-13T09:12:39Z", "model": "gemini-3", "content": "answer", "usage": {"total_tokens": 7}}
                ]
            }))?,
        )?;

        let sessions = scan_sessions(&root)?;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "gemini-cli-1");
        assert_eq!(sessions[0].first_user_message, "hello gemini");
        assert_eq!(sessions[0].tokens_used, 7);
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn removes_summary_entry_without_touching_other_entries() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-gemini-summary-test");
        fs::create_dir_all(&root)?;
        let path = root.join(SUMMARY_PROTO);
        let entry_a = summary_entry_bytes("a", "Title A");
        let entry_b = summary_entry_bytes("b", "Title B");
        let mut raw = Vec::new();
        write_len_field(&mut raw, 1, &entry_a);
        write_len_field(&mut raw, 1, &entry_b);
        fs::write(&path, raw)?;

        let removed = remove_summary_entry(&path, "a")?;
        assert_eq!(removed, 1);
        let index = read_summary_index(&root)?;
        assert!(!index.contains_key("a"));
        assert_eq!(
            index.get("b").and_then(|s| s.title.as_deref()),
            Some("Title B")
        );
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn delete_removes_antigravity_sidecars_logs_and_summary() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-gemini-delete-test");
        let surface = root.join("antigravity");
        let conversations = surface.join("conversations");
        let annotations = surface.join("annotations");
        let brain = surface.join("brain").join("cleanup-target");
        fs::create_dir_all(&conversations)?;
        fs::create_dir_all(&annotations)?;
        fs::create_dir_all(&brain)?;

        let id = "cleanup-target";
        fs::write(conversations.join(format!("{id}.pb")), b"pb")?;
        fs::write(annotations.join(format!("{id}.pbtxt")), b"note")?;
        fs::write(brain.join("task.md"), b"task")?;

        let mut summaries = Vec::new();
        write_len_field(&mut summaries, 1, &summary_entry_bytes(id, "Remove Me"));
        write_len_field(
            &mut summaries,
            1,
            &summary_entry_bytes("keep-target", "Keep Me"),
        );
        fs::write(surface.join(SUMMARY_PROTO), summaries)?;

        let project = root.join("tmp").join("projecthash");
        fs::create_dir_all(&project)?;
        let logs_path = project.join("logs.json");
        fs::write(
            &logs_path,
            serde_json::to_vec(&serde_json::json!([
                {"sessionId": id, "message": "remove"},
                {"sessionId": "keep-target", "message": "keep"}
            ]))?,
        )?;
        let raw_logs: Value = serde_json::from_slice(&fs::read(&logs_path)?)?;
        assert!(json_has_session_id(&raw_logs[0], id));

        let result = delete_session(&root, id)?;
        assert!(result.ok);
        assert!(result.rollout_deleted);
        assert_eq!(result.logs_rows_deleted, 1);
        assert_eq!(result.history_rows_deleted, 1);
        assert!(!conversations.join(format!("{id}.pb")).exists());
        assert!(!annotations.join(format!("{id}.pbtxt")).exists());
        assert!(!brain.exists());

        let index = read_summary_index(&surface)?;
        assert!(!index.contains_key(id));
        assert_eq!(
            index.get("keep-target").and_then(|s| s.title.as_deref()),
            Some("Keep Me")
        );

        let logs: Value = serde_json::from_slice(&fs::read(&logs_path)?)?;
        let rows = logs.as_array().expect("logs should stay an array");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("sessionId").and_then(Value::as_str),
            Some("keep-target")
        );

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn summary_removal_matches_linked_conversation_id() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-gemini-linked-summary-test");
        fs::create_dir_all(&root)?;
        let path = root.join(SUMMARY_PROTO);
        let mut raw = Vec::new();
        write_len_field(
            &mut raw,
            1,
            &summary_entry_bytes_with_link(
                "summary-id",
                "Hello",
                "11111111-1111-4111-8111-111111111111",
            ),
        );
        fs::write(&path, raw)?;

        let removed = remove_summary_entry(&path, "11111111-1111-4111-8111-111111111111")?;
        assert_eq!(removed, 1);
        assert!(read_summary_index(&root)?.is_empty());
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn prune_gemini_orphans_removes_summary_without_conversation() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-gemini-orphan-test");
        let surface = root.join("antigravity");
        let conversations = surface.join("conversations");
        fs::create_dir_all(&conversations)?;
        fs::write(conversations.join("keep-id.pb"), b"pb")?;

        let mut raw = Vec::new();
        write_len_field(&mut raw, 1, &summary_entry_bytes("orphan-id", "Orphan"));
        write_len_field(&mut raw, 1, &summary_entry_bytes("keep-id", "Keep"));
        fs::write(surface.join(SUMMARY_PROTO), raw)?;

        let dry = prune_gemini_orphans(root.to_string_lossy().into_owned(), true)?;
        assert_eq!(dry.orphan_summaries, 1);
        assert_eq!(dry.removed_summaries, 0);

        let report = prune_gemini_orphans(root.to_string_lossy().into_owned(), false)?;
        assert_eq!(report.orphan_summaries, 1);
        assert_eq!(report.removed_summaries, 1);
        let index = read_summary_index(&surface)?;
        assert!(!index.contains_key("orphan-id"));
        assert_eq!(
            index.get("keep-id").and_then(|s| s.title.as_deref()),
            Some("Keep")
        );
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    fn summary_entry_bytes(id: &str, title: &str) -> Vec<u8> {
        summary_entry_bytes_with_link(id, title, "")
    }

    fn summary_entry_bytes_with_link(id: &str, title: &str, linked_id: &str) -> Vec<u8> {
        let mut detail = Vec::new();
        write_len_field(&mut detail, 1, title.as_bytes());
        if !linked_id.is_empty() {
            write_len_field(&mut detail, 4, linked_id.as_bytes());
        }
        let mut entry = Vec::new();
        write_len_field(&mut entry, 1, id.as_bytes());
        write_len_field(&mut entry, 2, &detail);
        entry
    }

    fn write_len_field(out: &mut Vec<u8>, number: u32, bytes: &[u8]) {
        write_varint(out, ((number as u64) << 3) | 2);
        write_varint(out, bytes.len() as u64);
        out.extend_from_slice(bytes);
    }

    fn write_varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }
}
