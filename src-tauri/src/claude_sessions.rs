use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset};
use serde_json::Value;

use crate::error::AppResult;
use crate::models::{PreviewEvent, SessionMetaBrief, SessionSummary};
use crate::paths;

const PROVIDER: &str = "claude";
const SUBAGENT_SOURCE: &str = "subagent";
const TITLE_MAX_CHARS: usize = 80;
const PREVIEW_CAPACITY_HINT_MAX: usize = 1024;

pub fn scan_sessions(claude_dir: &Path) -> AppResult<Vec<SessionSummary>> {
    let root = paths::claude_projects_dir(claude_dir);
    if !root.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_jsonl_files(&root, &mut files)?;

    let mut sessions = Vec::new();
    for file in files {
        if let Some(session) = parse_session(&file)? {
            sessions.push(session);
        }
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at.max(s.created_at)));
    Ok(sessions)
}

pub fn preview_range(path: &str, offset: usize, limit: usize) -> AppResult<Vec<PreviewEvent>> {
    let f = File::open(PathBuf::from(path))?;
    let reader = BufReader::new(f);
    let mut out = Vec::with_capacity(preview_capacity_hint(limit));
    let mut event_index = 0usize;
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(raw) = serde_json::from_str::<Value>(&line) {
            if let Some(event) = classify_preview(i, raw) {
                if event_index < offset {
                    event_index += 1;
                    continue;
                }
                out.push(event);
                event_index += 1;
                if out.len() >= limit {
                    break;
                }
            }
        }
    }
    Ok(out)
}

fn preview_capacity_hint(limit: usize) -> usize {
    limit.min(PREVIEW_CAPACITY_HINT_MAX)
}

pub fn preview_meta(path: &str) -> AppResult<SessionMetaBrief> {
    let f = File::open(PathBuf::from(path))?;
    let reader = BufReader::new(f);
    let path_ref = Path::new(path);
    let is_subagent = is_agent_session(path_ref);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let raw: Value = serde_json::from_str(&line)?;
        return Ok(SessionMetaBrief {
            id: if is_subagent {
                infer_session_id_from_filename(path_ref)
            } else {
                raw.get("sessionId")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| infer_session_id_from_filename(path_ref))
            },
            timestamp: raw
                .get("timestamp")
                .and_then(Value::as_str)
                .map(String::from),
            cwd: raw.get("cwd").and_then(Value::as_str).map(String::from),
            originator: None,
            cli_version: raw.get("version").and_then(Value::as_str).map(String::from),
            source: Some(
                if is_subagent {
                    SUBAGENT_SOURCE
                } else {
                    PROVIDER
                }
                .to_string(),
            ),
            model_provider: Some(PROVIDER.to_string()),
        });
    }
    Ok(SessionMetaBrief {
        id: infer_session_id_from_filename(Path::new(path)),
        timestamp: None,
        cwd: None,
        originator: None,
        cli_version: None,
        source: Some(
            if is_subagent {
                SUBAGENT_SOURCE
            } else {
                PROVIDER
            }
            .to_string(),
        ),
        model_provider: Some(PROVIDER.to_string()),
    })
}

pub fn session_relpath(claude_dir: &Path, source_path: &Path) -> PathBuf {
    let projects = paths::claude_projects_dir(claude_dir);
    source_path
        .strip_prefix(&projects)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| {
            source_path
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("unknown.jsonl"))
        })
}

pub fn sidecar_path_for(source_path: &Path) -> Option<PathBuf> {
    let stem = source_path.file_stem()?;
    Some(source_path.with_file_name(stem))
}

fn parse_session(path: &Path) -> AppResult<Option<SessionSummary>> {
    let is_subagent = is_agent_session(path);
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut session_id: Option<String> = None;
    let mut agent_id = infer_agent_id_from_filename(path);
    let mut agent_role: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut created_at: Option<i64> = None;
    let mut updated_at: Option<i64> = None;
    let mut first_user_message: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut custom_title: Option<String> = None;
    let mut session_title: Option<String> = None;
    let mut summary_title: Option<String> = None;
    let mut last_prompt: Option<String> = None;
    let mut tail_summary: Option<String> = None;
    let mut model: Option<String> = None;
    let mut tokens_used = 0i64;
    let mut reasoning_effort: Option<String> = None;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if session_id.is_none() {
            session_id = value
                .get("sessionId")
                .and_then(Value::as_str)
                .map(String::from);
        }
        if is_subagent {
            if agent_id.is_none() {
                agent_id = string_field(&value, "agentId");
            }
            assign_if_some(
                &mut agent_role,
                first_string_field(&value, &["attributionAgent", "attributionSkill"]),
            );
        }
        if cwd.is_none() {
            cwd = value.get("cwd").and_then(Value::as_str).map(String::from);
        }
        if let Some(ts) = value.get("timestamp").and_then(parse_timestamp_to_seconds) {
            created_at.get_or_insert(ts);
            updated_at = Some(ts);
        }

        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        assign_if_some(&mut session_title, hook_session_title(&value));
        match event_type {
            "ai-title" => {
                assign_if_some(
                    &mut ai_title,
                    first_string_field(&value, &["aiTitle", "title"]),
                );
            }
            "custom-title" => {
                assign_if_some(
                    &mut custom_title,
                    first_string_field(&value, &["customTitle", "title"]),
                );
            }
            "summary" => {
                assign_if_some(&mut summary_title, string_field(&value, "summary"));
            }
            "last-prompt" => {
                assign_if_some(&mut last_prompt, string_field(&value, "lastPrompt"));
            }
            _ => {}
        }

        if let Some(message) = value.get("message") {
            if model.is_none() {
                model = message
                    .get("model")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| value.get("model").and_then(Value::as_str).map(String::from));
            }
            tokens_used += usage_tokens(message.get("usage"));
            tokens_used += usage_tokens(value.get("usage"));

            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let text = message.get("content").map(extract_text).unwrap_or_default();
            let trimmed = text.trim();
            let is_user = event_type == "user" || role == "user";
            let is_meta = value.get("isMeta").and_then(Value::as_bool) == Some(true);
            let is_sidechain = value.get("isSidechain").and_then(Value::as_bool) == Some(true);
            let visible_message = !is_meta && (is_subagent || !is_sidechain);
            if first_user_message.is_none()
                && is_user
                && visible_message
                && !trimmed.is_empty()
                && !is_generated_user_prompt(trimmed)
            {
                first_user_message = Some(trimmed.to_string());
            }
            if visible_message && !trimmed.is_empty() {
                tail_summary = Some(trimmed.to_string());
            }
            if is_user {
                if let Some(level) = parse_effort_level(trimmed) {
                    reasoning_effort = Some(level);
                }
            }
        }
    }

    let parent_session_id = session_id.clone();
    let id = if is_subagent {
        infer_session_id_from_filename(path)
    } else {
        session_id.or_else(|| infer_session_id_from_filename(path))
    };
    let Some(id) = id else {
        return Ok(None);
    };
    let cwd_value = cwd.unwrap_or_default();
    let title = custom_title
        .or(session_title)
        .or(ai_title)
        .or(summary_title)
        .or_else(|| first_user_message.clone())
        .or(last_prompt)
        .or(tail_summary)
        .or_else(|| path_basename(&cwd_value))
        .unwrap_or_else(|| id.clone());
    let bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    Ok(Some(SessionSummary {
        provider: PROVIDER.to_string(),
        id: id.clone(),
        rollout_path: path.to_string_lossy().into_owned(),
        cwd: paths::strip_verbatim(&cwd_value),
        cwd_display: paths::basename_display(&cwd_value),
        title: truncate_summary(&title, TITLE_MAX_CHARS),
        first_user_message: first_user_message.unwrap_or_default(),
        model,
        reasoning_effort,
        source: if is_subagent {
            Some(SUBAGENT_SOURCE.to_string())
        } else {
            None
        },
        agent_nickname: if is_subagent { agent_id } else { None },
        agent_role: if is_subagent { agent_role } else { None },
        tokens_used,
        created_at: created_at.unwrap_or(0),
        updated_at: updated_at.or(created_at).unwrap_or(0),
        archived: false,
        git_branch: None,
        rollout_bytes: bytes,
        logs_count: 0,
        has_backup: false,
        resume_command: format!(
            "claude --resume {}",
            if is_subagent {
                parent_session_id.as_deref().unwrap_or(&id)
            } else {
                id.as_str()
            }
        ),
    }))
}

fn classify_preview(index: usize, raw: Value) -> Option<PreviewEvent> {
    let timestamp = raw
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let kind = raw
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
        .to_string();
    if raw.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return Some(PreviewEvent {
            index,
            timestamp,
            role: "meta".into(),
            kind,
            text_summary: claude_non_message_summary(&raw),
            raw,
        });
    }
    let Some(message) = raw.get("message") else {
        return Some(PreviewEvent {
            index,
            timestamp,
            role: if matches!(kind.as_str(), "summary" | "custom-title") {
                "meta".into()
            } else {
                "other".into()
            },
            kind,
            text_summary: claude_non_message_summary(&raw),
            raw,
        });
    };
    let mut role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let content = message.get("content");
    if role == "user" && content_is_all_tool_results(content) {
        role = "tool_result".into();
    } else if role == "assistant" && content_has_tool_use(content) {
        role = "tool_call".into();
    } else if role == "assistant" && content_is_only_thinking(content) {
        role = "reasoning".into();
    }
    let text = if role == "reasoning" {
        extract_thinking_text(content)
    } else {
        content.map(extract_text).unwrap_or_default()
    };
    Some(PreviewEvent {
        index,
        timestamp,
        role,
        kind,
        text_summary: truncate_summary(&text, 120),
        raw,
    })
}

fn claude_non_message_summary(raw: &Value) -> String {
    for key in [
        "customTitle",
        "aiTitle",
        "sessionTitle",
        "summary",
        "content",
        "text",
        "stdout",
        "stderr",
        "command",
    ] {
        if let Some(text) = raw.get(key).and_then(Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return truncate_summary(trimmed, 120);
            }
        }
    }
    raw.get("type")
        .and_then(Value::as_str)
        .unwrap_or("事件")
        .to_string()
}

fn collect_jsonl_files(root: &Path, files: &mut Vec<PathBuf>) -> AppResult<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
    Ok(())
}

fn infer_session_id_from_filename(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(String::from)
}

fn infer_agent_id_from_filename(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.strip_prefix("agent-"))
        .map(String::from)
}

fn is_agent_session(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with("agent-"))
        .unwrap_or(false)
}

fn is_generated_user_prompt(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("Caveat:")
        || trimmed.starts_with('/')
        || trimmed.starts_with("# AGENTS.md")
        || trimmed.starts_with("<command-name>")
        || trimmed.starts_with("<command-message>")
        || trimmed.starts_with("<command-args>")
        || trimmed.starts_with("<local-command-caveat>")
        || trimmed.starts_with("<local-command-stdout>")
        || trimmed.starts_with("<local-command-stderr>")
        || trimmed.starts_with("<bash-input>")
        || trimmed.starts_with("<bash-stdout>")
        || trimmed.starts_with("<bash-stderr>")
}

fn parse_effort_level(text: &str) -> Option<String> {
    parse_effort_command_args(text).or_else(|| parse_effort_stdout(text))
}

fn parse_effort_command_args(text: &str) -> Option<String> {
    if tag_content(text, "command-name")?.trim() != "/effort" {
        return None;
    }
    let args = tag_content(text, "command-args")?;
    normalize_effort_level(args)
}

fn parse_effort_stdout(text: &str) -> Option<String> {
    let marker = "Set effort level to ";
    let idx = text.find(marker)?;
    let rest = &text[idx + marker.len()..];
    let level = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect::<String>();
    normalize_effort_level(&level)
}

fn normalize_effort_level(level: &str) -> Option<String> {
    let trimmed = level.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn tag_content<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(&text[start..end])
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn first_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| string_field(value, key))
}

fn assign_if_some(slot: &mut Option<String>, value: Option<String>) {
    if let Some(value) = value {
        *slot = Some(value);
    }
}

fn hook_session_title(value: &Value) -> Option<String> {
    hook_output_title(value)
        .or_else(|| value.get("attachment").and_then(hook_output_title))
        .or_else(|| {
            value
                .get("attachment")
                .and_then(|attachment| attachment.get("stdout"))
                .and_then(Value::as_str)
                .and_then(parse_hook_stdout_session_title)
        })
        .or_else(|| {
            value
                .get("stdout")
                .and_then(Value::as_str)
                .and_then(parse_hook_stdout_session_title)
        })
}

fn hook_output_title(value: &Value) -> Option<String> {
    string_field(value, "sessionTitle").or_else(|| {
        value
            .get("hookSpecificOutput")
            .and_then(|output| string_field(output, "sessionTitle"))
    })
}

fn parse_hook_stdout_session_title(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    hook_output_title(&value)
}

fn parse_timestamp_to_seconds(value: &Value) -> Option<i64> {
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
    ]
    .iter()
    .filter_map(|key| map.get(*key).and_then(Value::as_i64))
    .sum()
}

fn content_is_all_tool_results(content: Option<&Value>) -> bool {
    match content {
        Some(Value::Array(items)) if !items.is_empty() => items
            .iter()
            .all(|item| item.get("type").and_then(Value::as_str) == Some("tool_result")),
        _ => false,
    }
}

fn content_has_tool_use(content: Option<&Value>) -> bool {
    match content {
        Some(Value::Array(items)) => items
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("tool_use")),
        _ => false,
    }
}

fn content_is_only_thinking(content: Option<&Value>) -> bool {
    match content {
        Some(Value::Array(items)) if !items.is_empty() => {
            let mut has_thinking = false;
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("thinking") | Some("redacted_thinking") => has_thinking = true,
                    _ => return false,
                }
            }
            has_thinking
        }
        _ => false,
    }
}

fn extract_thinking_text(content: Option<&Value>) -> String {
    let Some(Value::Array(items)) = content else {
        return String::new();
    };
    let mut parts = Vec::new();
    for item in items {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "thinking" => {
                if let Some(text) = item.get("thinking").and_then(Value::as_str) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        parts.push(trimmed.to_string());
                    }
                }
            }
            "redacted_thinking" => {
                parts.push("(加密推理)".to_string());
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        "(加密推理)".to_string()
    } else {
        parts.join("\n\n")
    }
}

fn extract_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(extract_text_from_item)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn extract_text_from_item(item: &Value) -> Option<String> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    if item_type == "tool_use" {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Some(format!("[Tool: {name}]"));
    }
    if item_type == "tool_result" {
        if let Some(content) = item.get("content") {
            let text = extract_text(content);
            if !text.is_empty() {
                return Some(text);
            }
        }
        return None;
    }
    if let Some(text) = item.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    if let Some(content) = item.get("content") {
        let text = extract_text(content);
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
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

fn path_basename(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed
        .trim_end_matches(['/', '\\'])
        .split(['/', '\\'])
        .next_back()
        .filter(|segment| !segment.is_empty())
        .map(String::from)
}

#[allow(dead_code)]
fn read_head_tail_lines(
    path: &Path,
    head_n: usize,
    tail_n: usize,
) -> AppResult<(Vec<String>, Vec<String>)> {
    let file = File::open(path)?;
    let mut head = Vec::new();
    let mut tail = VecDeque::with_capacity(tail_n);
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if idx < head_n {
            head.push(line.clone());
        }
        if tail.len() == tail_n {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    Ok((head, tail.into_iter().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ))
    }

    fn write_sample_session(claude: &Path) -> AppResult<PathBuf> {
        let session_dir = claude.join("projects").join("sample-project");
        fs::create_dir_all(&session_dir)?;
        let file = session_dir.join("claude-1.jsonl");
        let mut out = File::create(&file)?;
        for value in [
            serde_json::json!({
                "sessionId": "claude-1",
                "cwd": "F:\\work\\sample-project",
                "timestamp": "2026-04-20T10:00:00Z",
                "type": "user",
                "isMeta": true,
                "message": {
                    "role": "user",
                    "content": "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>"
                }
            }),
            serde_json::json!({
                "sessionId": "claude-1",
                "cwd": "F:\\work\\sample-project",
                "timestamp": "2026-04-20T10:00:30Z",
                "type": "user",
                "message": {
                    "role": "user",
                    "content": "<command-name>/effort</command-name>\n            <command-message>effort</command-message>\n            <command-args>max</command-args>"
                }
            }),
            serde_json::json!({
                "sessionId": "claude-1",
                "cwd": "F:\\work\\sample-project",
                "timestamp": "2026-04-20T10:01:00Z",
                "type": "user",
                "message": {
                    "role": "user",
                    "content": "<local-command-stdout>Set effort level to max (this session only): Maximum capability with deepest reasoning</local-command-stdout>"
                }
            }),
            serde_json::json!({
                "sessionId": "claude-1",
                "cwd": "F:\\work\\sample-project",
                "timestamp": "2026-04-20T10:01:30Z",
                "type": "user",
                "message": {"role": "user", "content": "hello claude"}
            }),
            serde_json::json!({
                "sessionId": "claude-1",
                "cwd": "F:\\work\\sample-project",
                "timestamp": "2026-04-20T10:02:00Z",
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "model": "claude-3-5-sonnet",
                    "usage": {"input_tokens": 10, "output_tokens": 5},
                    "content": [{"type": "text", "text": "answer"}]
                }
            }),
            serde_json::json!({
                "sessionId": "claude-1",
                "timestamp": "2026-04-20T10:02:30Z",
                "type": "ai-title",
                "aiTitle": "AI Claude Title"
            }),
            serde_json::json!({
                "sessionId": "claude-1",
                "timestamp": "2026-04-20T10:02:40Z",
                "type": "last-prompt",
                "lastPrompt": "hello claude"
            }),
            serde_json::json!({
                "sessionId": "claude-1",
                "timestamp": "2026-04-20T10:03:00Z",
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "content": "tool ok"}]
                }
            }),
        ] {
            writeln!(out, "{}", serde_json::to_string(&value)?)?;
        }
        Ok(file)
    }

    fn write_session_values(
        claude: &Path,
        file_name: &str,
        values: Vec<Value>,
    ) -> AppResult<PathBuf> {
        let session_dir = claude.join("projects").join("sample-project");
        fs::create_dir_all(&session_dir)?;
        let file = session_dir.join(file_name);
        let mut out = File::create(&file)?;
        for value in values {
            writeln!(out, "{}", serde_json::to_string(&value)?)?;
        }
        Ok(file)
    }

    fn write_agent_session(claude: &Path) -> AppResult<PathBuf> {
        let session_dir = claude
            .join("projects")
            .join("sample-project")
            .join("parent-session")
            .join("subagents");
        fs::create_dir_all(&session_dir)?;
        let file = session_dir.join("agent-aabbccddeeff0011.jsonl");
        let mut out = File::create(&file)?;
        for value in [
            serde_json::json!({
                "sessionId": "parent-session",
                "agentId": "aabbccddeeff0011",
                "attributionAgent": "general-purpose",
                "cwd": "F:\\work\\sample-project",
                "timestamp": "2026-04-20T10:00:00Z",
                "type": "user",
                "isSidechain": true,
                "message": {"role": "user", "content": "inspect this subsystem"}
            }),
            serde_json::json!({
                "sessionId": "parent-session",
                "agentId": "aabbccddeeff0011",
                "attributionAgent": "general-purpose",
                "cwd": "F:\\work\\sample-project",
                "timestamp": "2026-04-20T10:00:10Z",
                "type": "assistant",
                "isSidechain": true,
                "message": {
                    "role": "assistant",
                    "model": "claude-3-5-sonnet",
                    "usage": {"input_tokens": 4, "output_tokens": 6},
                    "content": "subagent result"
                }
            }),
        ] {
            writeln!(out, "{}", serde_json::to_string(&value)?)?;
        }
        Ok(file)
    }

    #[test]
    fn skips_slash_command_wrapper_in_first_user_message() {
        let raw = "<command-name>/effort</command-name>\n            <command-message>effort</command-message>\n            <command-args>max</command-args>";
        assert!(is_generated_user_prompt(raw));
        assert!(is_generated_user_prompt(
            "<local-command-caveat>Caveat: generated by local commands.</local-command-caveat>"
        ));
        assert!(is_generated_user_prompt(
            "<local-command-stdout>Set effort level to max</local-command-stdout>"
        ));
        assert!(is_generated_user_prompt("<bash-input>ls</bash-input>"));
        assert!(!is_generated_user_prompt("hello claude"));
    }

    #[test]
    fn extracts_text_from_tool_result() {
        let value = serde_json::json!([
            {"type": "tool_result", "content": "File written"}
        ]);
        assert_eq!(extract_text(&value), "File written");
    }

    #[test]
    fn extracts_effort_level_from_local_command_stdout() {
        let command = "<command-name>/effort</command-name>\n            <command-message>effort</command-message>\n            <command-args>xhigh</command-args>";
        assert_eq!(parse_effort_level(command).as_deref(), Some("xhigh"));

        let text = "<local-command-stdout>Set effort level to max (this session only): Maximum capability with deepest reasoning</local-command-stdout>";
        assert_eq!(parse_effort_level(text).as_deref(), Some("max"));

        let text2 = "<local-command-stdout>Set effort level to xhigh (this session only): Extra capability</local-command-stdout>";
        assert_eq!(parse_effort_level(text2).as_deref(), Some("xhigh"));

        assert_eq!(parse_effort_level("hello world"), None);
    }

    #[test]
    fn classifies_thinking_only_assistant_as_reasoning() {
        let raw = serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-04-28T04:22:06.430Z",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "let me think about this", "signature": "x"}
                ]
            }
        });
        let event = classify_preview(0, raw).expect("event");
        assert_eq!(event.role, "reasoning");
        assert_eq!(event.text_summary, "let me think about this");
    }

    #[test]
    fn keeps_assistant_when_thinking_mixed_with_text() {
        let raw = serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-04-28T04:22:06.430Z",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "...", "signature": "x"},
                    {"type": "text", "text": "final answer"}
                ]
            }
        });
        let event = classify_preview(0, raw).expect("event");
        assert_eq!(event.role, "assistant");
        assert!(event.text_summary.contains("final answer"));
    }

    #[test]
    fn redacted_thinking_is_reasoning_with_placeholder() {
        let raw = serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-04-28T04:22:06.430Z",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "redacted_thinking", "data": "encrypted"}
                ]
            }
        });
        let event = classify_preview(0, raw).expect("event");
        assert_eq!(event.role, "reasoning");
        assert_eq!(event.text_summary, "(加密推理)");
    }

    #[test]
    fn parses_rfc3339_to_seconds() {
        assert_eq!(
            parse_timestamp_to_seconds(&serde_json::json!("1970-01-01T00:00:01Z")),
            Some(1)
        );
    }

    #[test]
    fn scans_claude_jsonl_and_builds_preview() -> AppResult<()> {
        let claude = temp_dir("cc-session-manager-claude-scan-test");
        let file = write_sample_session(&claude)?;

        let sessions = scan_sessions(&claude)?;

        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.provider, PROVIDER);
        assert_eq!(session.id, "claude-1");
        assert_eq!(session.title, "AI Claude Title");
        assert_eq!(session.first_user_message, "hello claude");
        assert_eq!(session.model.as_deref(), Some("claude-3-5-sonnet"));
        assert_eq!(session.reasoning_effort.as_deref(), Some("max"));
        assert_eq!(session.tokens_used, 15);
        assert_eq!(session.resume_command, "claude --resume claude-1");

        let events = preview_range(&file.to_string_lossy(), 0, 10)?;
        fs::remove_dir_all(&claude).ok();
        assert!(events.iter().any(|event| event.role == "tool_result"));
        Ok(())
    }

    #[test]
    fn scans_claude_agent_jsonl_as_subagent_session() -> AppResult<()> {
        let claude = temp_dir("cc-session-manager-claude-subagent-scan-test");
        let file = write_agent_session(&claude)?;

        let sessions = scan_sessions(&claude)?;

        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.provider, PROVIDER);
        assert_eq!(session.id, "agent-aabbccddeeff0011");
        assert_eq!(session.source.as_deref(), Some(SUBAGENT_SOURCE));
        assert_eq!(session.agent_nickname.as_deref(), Some("aabbccddeeff0011"));
        assert_eq!(session.agent_role.as_deref(), Some("general-purpose"));
        assert_eq!(session.first_user_message, "inspect this subsystem");
        assert_eq!(session.title, "inspect this subsystem");
        assert_eq!(session.tokens_used, 10);
        assert_eq!(session.resume_command, "claude --resume parent-session");

        let events = preview_range(&file.to_string_lossy(), 0, 10)?;
        fs::remove_dir_all(&claude).ok();
        assert!(events.iter().any(|event| event.role == "assistant"));
        Ok(())
    }

    #[test]
    fn preserves_ai_title_when_later_title_event_is_empty() -> AppResult<()> {
        let claude = temp_dir("cc-session-manager-claude-ai-title-test");
        write_session_values(
            &claude,
            "claude-title.jsonl",
            vec![
                serde_json::json!({
                    "sessionId": "claude-title",
                    "cwd": "F:\\work\\sample-project",
                    "timestamp": "2026-04-20T10:00:00Z",
                    "type": "user",
                    "message": {"role": "user", "content": "first user prompt"}
                }),
                serde_json::json!({
                    "sessionId": "claude-title",
                    "timestamp": "2026-04-20T10:00:10Z",
                    "type": "ai-title",
                    "aiTitle": "Generated Claude Title"
                }),
                serde_json::json!({
                    "sessionId": "claude-title",
                    "timestamp": "2026-04-20T10:00:20Z",
                    "type": "ai-title"
                }),
            ],
        )?;

        let sessions = scan_sessions(&claude)?;
        fs::remove_dir_all(&claude).ok();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "Generated Claude Title");
        Ok(())
    }

    #[test]
    fn custom_title_overrides_ai_title() -> AppResult<()> {
        let claude = temp_dir("cc-session-manager-claude-custom-title-test");
        write_session_values(
            &claude,
            "claude-custom.jsonl",
            vec![
                serde_json::json!({
                    "sessionId": "claude-custom",
                    "cwd": "F:\\work\\sample-project",
                    "timestamp": "2026-04-20T10:00:00Z",
                    "type": "user",
                    "message": {"role": "user", "content": "first user prompt"}
                }),
                serde_json::json!({
                    "sessionId": "claude-custom",
                    "timestamp": "2026-04-20T10:00:10Z",
                    "type": "ai-title",
                    "aiTitle": "Generated Claude Title"
                }),
                serde_json::json!({
                    "sessionId": "claude-custom",
                    "timestamp": "2026-04-20T10:00:20Z",
                    "type": "custom-title",
                    "customTitle": "Manual Claude Title"
                }),
            ],
        )?;

        let sessions = scan_sessions(&claude)?;
        fs::remove_dir_all(&claude).ok();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "Manual Claude Title");
        Ok(())
    }

    #[test]
    fn reads_hook_session_title_from_stdout() -> AppResult<()> {
        let claude = temp_dir("cc-session-manager-claude-hook-title-test");
        let stdout = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "sessionTitle": "Hook Claude Title"
            }
        })
        .to_string();
        write_session_values(
            &claude,
            "claude-hook-title.jsonl",
            vec![
                serde_json::json!({
                    "sessionId": "claude-hook-title",
                    "cwd": "F:\\work\\sample-project",
                    "timestamp": "2026-04-20T10:00:00Z",
                    "type": "user",
                    "message": {"role": "user", "content": "first user prompt"}
                }),
                serde_json::json!({
                    "sessionId": "claude-hook-title",
                    "timestamp": "2026-04-20T10:00:10Z",
                    "type": "user",
                    "attachment": {
                        "type": "hook_success",
                        "hookEvent": "UserPromptSubmit",
                        "stdout": stdout
                    }
                }),
            ],
        )?;

        let sessions = scan_sessions(&claude)?;
        fs::remove_dir_all(&claude).ok();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "Hook Claude Title");
        Ok(())
    }

    #[test]
    fn falls_back_to_first_user_message_without_explicit_title() -> AppResult<()> {
        let claude = temp_dir("cc-session-manager-claude-first-user-title-test");
        write_session_values(
            &claude,
            "claude-first-user.jsonl",
            vec![serde_json::json!({
                "sessionId": "claude-first-user",
                "cwd": "F:\\work\\sample-project",
                "timestamp": "2026-04-20T10:00:00Z",
                "type": "user",
                "message": {"role": "user", "content": "first user prompt"}
            })],
        )?;

        let sessions = scan_sessions(&claude)?;
        fs::remove_dir_all(&claude).ok();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "first user prompt");
        Ok(())
    }

    #[test]
    fn paginates_preview_by_events_not_raw_lines() -> AppResult<()> {
        let claude = temp_dir("cc-session-manager-claude-pagination-test");
        let session_dir = claude.join("projects").join("sample-project");
        fs::create_dir_all(&session_dir)?;
        let file = session_dir.join("claude-page.jsonl");
        let mut out = File::create(&file)?;
        for i in 0..226 {
            let value = if i % 10 == 0 {
                serde_json::json!({
                    "sessionId": "claude-page",
                    "timestamp": "2026-04-20T10:00:00Z",
                    "type": "custom-title",
                    "customTitle": format!("title {i}")
                })
            } else {
                serde_json::json!({
                    "sessionId": "claude-page",
                    "timestamp": "2026-04-20T10:00:00Z",
                    "type": "assistant",
                    "message": {"role": "assistant", "content": format!("event {i}")}
                })
            };
            writeln!(out, "{}", serde_json::to_string(&value)?)?;
        }

        let first = preview_range(&file.to_string_lossy(), 0, 200)?;
        let second = preview_range(&file.to_string_lossy(), first.len(), 200)?;
        fs::remove_dir_all(&claude).ok();

        assert_eq!(first.len(), 200);
        assert_eq!(second.len(), 26);
        assert_eq!(second.last().map(|event| event.index), Some(225));
        Ok(())
    }

    #[test]
    fn preview_range_accepts_unbounded_limit_without_unbounded_preallocation() -> AppResult<()> {
        let claude = temp_dir("cc-session-manager-claude-unbounded-limit-test");
        let file = write_session_values(
            &claude,
            "claude-unbounded.jsonl",
            vec![
                serde_json::json!({
                    "sessionId": "claude-unbounded",
                    "timestamp": "2026-04-20T10:00:00Z",
                    "type": "user",
                    "message": {"role": "user", "content": "hello"}
                }),
                serde_json::json!({
                    "sessionId": "claude-unbounded",
                    "timestamp": "2026-04-20T10:00:10Z",
                    "type": "assistant",
                    "message": {"role": "assistant", "content": "answer"}
                }),
            ],
        )?;

        let events = preview_range(&file.to_string_lossy(), 0, usize::MAX)?;
        fs::remove_dir_all(&claude).ok();

        assert_eq!(events.len(), 2);
        assert_eq!(
            events.last().map(|event| event.text_summary.as_str()),
            Some("answer")
        );
        Ok(())
    }
}
