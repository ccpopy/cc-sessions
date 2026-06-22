use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::AppResult;
use crate::models::{PreviewEvent, SessionMetaBrief};

const PREVIEW_CAPACITY_HINT_MAX: usize = 1024;

fn classify(index: usize, raw: Value) -> PreviewEvent {
    let timestamp = raw
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let outer_type = raw
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let payload_type = raw
        .get("payload")
        .and_then(|p| p.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let (role, kind, text_summary) = match (outer_type.as_str(), payload_type.as_str()) {
        ("session_meta", _) => ("meta".into(), "session_meta".into(), "会话元数据".into()),
        ("event_msg", "task_started") => ("meta".into(), "task_started".into(), "任务开始".into()),
        ("event_msg", "token_count") => {
            let total = token_total_from_value(&raw).unwrap_or(0);
            (
                "meta".into(),
                "token_count".into(),
                format!("tokens: {}", total),
            )
        }
        ("event_msg", "agent_message") => {
            let text = raw
                .get("payload")
                .and_then(|p| p.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            ("assistant".into(), "agent_message".into(), trim(&text, 120))
        }
        ("event_msg", "user_message") => {
            let text = raw
                .get("payload")
                .and_then(|p| p.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            ("user".into(), "user_message".into(), trim(&text, 120))
        }
        ("response_item", "message") => {
            let role_name = raw
                .get("payload")
                .and_then(|p| p.get("role"))
                .and_then(|v| v.as_str())
                .unwrap_or("assistant")
                .to_string();
            let text = flatten_content(raw.get("payload").and_then(|p| p.get("content")));
            (role_name, "message".into(), trim(&text, 120))
        }
        ("response_item", "reasoning") => {
            let text = flatten_content(raw.get("payload").and_then(|p| p.get("content")));
            ("reasoning".into(), "reasoning".into(), trim(&text, 80))
        }
        ("response_item", "function_call") => {
            let name = raw
                .get("payload")
                .and_then(|p| p.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            ("tool_call".into(), "function_call".into(), name)
        }
        ("response_item", "function_call_output") => (
            "tool_result".into(),
            "function_call_output".into(),
            "工具返回".into(),
        ),
        _ => (
            "other".into(),
            format!("{}/{}", outer_type, payload_type),
            String::new(),
        ),
    };

    PreviewEvent {
        index,
        timestamp,
        role,
        kind,
        text_summary,
        raw,
    }
}

fn flatten_content(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|x| {
                x.get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| x.as_str().map(|s| s.to_string()))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn trim(s: &str, n: usize) -> String {
    let flat: String = s.chars().filter(|c| *c != '\n').collect();
    if flat.chars().count() <= n {
        flat
    } else {
        let mut out: String = flat.chars().take(n).collect();
        out.push('…');
        out
    }
}

pub fn preview_event_is_conversation(event: &PreviewEvent) -> bool {
    if is_internal_codex_context_message(event) {
        return false;
    }
    if !matches!(event.role.as_str(), "user" | "assistant") {
        return false;
    }

    let raw_message_role = event
        .raw
        .get("message")
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str);
    if raw_message_role.is_some() {
        return true;
    }

    raw_type(event) == "response_item" && payload_type(event) == "message"
}

pub fn preview_event_is_conversation_or_reasoning(event: &PreviewEvent) -> bool {
    preview_event_is_conversation(event) || event.role == "reasoning"
}

fn is_internal_codex_context_message(event: &PreviewEvent) -> bool {
    if event.role != "user" {
        return false;
    }
    let text = preview_event_text(event).trim().to_string();
    if text.is_empty() {
        return false;
    }
    let first_line = normalize_prompt_heading(text.lines().next().unwrap_or(""));
    (first_line.starts_with("AGENTS.md instructions for ") && text.contains("<INSTRUCTIONS>"))
        || (first_line == "<environment_context>" && text.contains("</environment_context>"))
}

fn normalize_prompt_heading(line: &str) -> String {
    line.trim().trim_start_matches('#').trim_start().to_string()
}

fn raw_type(event: &PreviewEvent) -> &str {
    event.raw.get("type").and_then(Value::as_str).unwrap_or("")
}

fn payload_type(event: &PreviewEvent) -> &str {
    event
        .raw
        .get("payload")
        .and_then(|payload| payload.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

pub fn preview_event_text(event: &PreviewEvent) -> String {
    if let Some(message) = event.raw.get("message") {
        let content = message.get("content");
        let text = flatten_rich_content(content);
        if !text.is_empty() {
            return text;
        }
    }

    let payload = event.raw.get("payload");
    if let Some(message) = payload
        .and_then(|payload| payload.get("message"))
        .and_then(Value::as_str)
    {
        return message.to_string();
    }
    if let Some(text) = payload
        .and_then(|payload| payload.get("text"))
        .and_then(Value::as_str)
    {
        return text.to_string();
    }

    let text = flatten_rich_content(payload.and_then(|payload| payload.get("content")));
    if text.is_empty() {
        event.text_summary.clone()
    } else {
        text
    }
}

fn flatten_rich_content(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(flatten_rich_content_item)
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn flatten_rich_content_item(item: &Value) -> Option<String> {
    if let Some(text) = item.as_str() {
        return Some(text.to_string());
    }
    if let Some(text) = item.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    if let Some(text) = item.get("content").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    let nested = flatten_rich_content(item.get("content"));
    if nested.is_empty() {
        None
    } else {
        Some(nested)
    }
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn preview_session_head(
    provider: Option<String>,
    rollout_path: String,
    limit: usize,
) -> AppResult<Vec<PreviewEvent>> {
    preview_range_by_provider(provider, &rollout_path, 0, limit)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn preview_session_range(
    provider: Option<String>,
    rollout_path: String,
    offset: usize,
    limit: usize,
) -> AppResult<Vec<PreviewEvent>> {
    preview_range_by_provider(provider, &rollout_path, offset, limit)
}

fn preview_range_by_provider(
    provider: Option<String>,
    path: &str,
    offset: usize,
    limit: usize,
) -> AppResult<Vec<PreviewEvent>> {
    match provider.as_deref().unwrap_or("codex") {
        "codex" => preview_range_impl(path, offset, limit),
        "claude" => crate::claude_sessions::preview_range(path, offset, limit),
        other => Err(crate::error::AppError::Other(format!(
            "不支持的 provider: {other}"
        ))),
    }
}

fn preview_range_impl(path: &str, offset: usize, limit: usize) -> AppResult<Vec<PreviewEvent>> {
    let f = File::open(PathBuf::from(path))?;
    let reader = BufReader::new(f);
    let mut out = Vec::with_capacity(preview_capacity_hint(limit));
    let mut event_index = 0usize;
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // 预览允许跳过损坏行，完整修复功能会负责诊断这类文件。
        if let Ok(raw) = serde_json::from_str::<Value>(&line) {
            if event_index < offset {
                event_index += 1;
                continue;
            }
            out.push(classify(i, raw));
            event_index += 1;
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

fn preview_capacity_hint(limit: usize) -> usize {
    limit.min(PREVIEW_CAPACITY_HINT_MAX)
}

pub fn read_rollout_token_total(path: &Path) -> AppResult<i64> {
    let f = File::open(path)?;
    let reader = BufReader::new(f);
    let mut total = 0i64;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(next) = token_total_from_value(&raw) {
            total = next;
        }
    }
    Ok(total)
}

pub fn token_total_from_value(raw: &Value) -> Option<i64> {
    if raw.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = raw.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }

    nonnegative_i64(
        payload
            .get("info")
            .and_then(|info| info.get("total_token_usage"))
            .and_then(|usage| usage.get("total_tokens")),
    )
    .or_else(|| {
        nonnegative_i64(
            payload
                .get("info")
                .and_then(|info| info.get("total_tokens")),
        )
    })
    .or_else(|| nonnegative_i64(payload.get("total_tokens")))
    .or_else(|| nonnegative_i64(raw.get("total_tokens")))
}

fn nonnegative_i64(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(n) = value.as_i64() {
        return (n >= 0).then_some(n);
    }
    if let Some(n) = value.as_u64() {
        return i64::try_from(n).ok();
    }
    None
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn preview_session_meta(
    provider: Option<String>,
    rollout_path: String,
) -> AppResult<SessionMetaBrief> {
    if provider.as_deref().unwrap_or("codex") == "claude" {
        return crate::claude_sessions::preview_meta(&rollout_path);
    }
    let f = File::open(PathBuf::from(&rollout_path))?;
    let mut reader = BufReader::new(f);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let raw: Value = serde_json::from_str(first.trim())?;
    let payload = raw.get("payload");
    let brief = SessionMetaBrief {
        id: payload
            .and_then(|p| p.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from),
        timestamp: payload
            .and_then(|p| p.get("timestamp"))
            .and_then(|v| v.as_str())
            .map(String::from),
        cwd: payload
            .and_then(|p| p.get("cwd"))
            .and_then(|v| v.as_str())
            .map(String::from),
        originator: payload
            .and_then(|p| p.get("originator"))
            .and_then(|v| v.as_str())
            .map(String::from),
        cli_version: payload
            .and_then(|p| p.get("cli_version"))
            .and_then(|v| v.as_str())
            .map(String::from),
        source: payload
            .and_then(|p| p.get("source"))
            .and_then(|v| v.as_str())
            .map(String::from),
        model_provider: payload
            .and_then(|p| p.get("model_provider"))
            .and_then(|v| v.as_str())
            .map(String::from),
    };
    Ok(brief)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{name}-{}-{}.jsonl",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ))
    }

    #[test]
    fn reads_latest_nested_token_count() -> AppResult<()> {
        let file = temp_file("cc-session-manager-rollout-token-test");
        {
            let mut out = File::create(&file)?;
            for value in [
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "total_tokens": 12
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
                                "total_tokens": 3456789
                            }
                        }
                    }
                }),
            ] {
                writeln!(out, "{}", serde_json::to_string(&value)?)?;
            }
        }

        let total = read_rollout_token_total(&file)?;
        fs::remove_file(file).ok();

        assert_eq!(total, 3_456_789);
        Ok(())
    }

    #[test]
    fn token_preview_uses_nested_total_token_usage() {
        let raw = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "total_tokens": 42
                    }
                }
            }
        });

        let event = classify(0, raw);

        assert_eq!(event.text_summary, "tokens: 42");
    }

    #[test]
    fn preview_range_accepts_unbounded_limit_without_unbounded_preallocation() -> AppResult<()> {
        let file = temp_file("cc-session-manager-rollout-unbounded-limit-test");
        {
            let mut out = File::create(&file)?;
            for value in [
                serde_json::json!({
                    "type": "session_meta",
                    "payload": {"id": "session-1"}
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "hello"}]
                    }
                }),
            ] {
                writeln!(out, "{}", serde_json::to_string(&value)?)?;
            }
        }

        let events = preview_session_range(
            Some("codex".to_string()),
            file.to_string_lossy().into_owned(),
            0,
            usize::MAX,
        )?;
        fs::remove_file(file).ok();

        assert_eq!(events.len(), 2);
        assert_eq!(
            events.last().map(|event| event.text_summary.as_str()),
            Some("hello")
        );
        Ok(())
    }
}
