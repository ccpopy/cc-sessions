use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::AppResult;
use crate::models::{
    PreviewEvent, SessionMetaBrief, TimelineMessageBrief, UserPromptBrief, UserPromptList,
};

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
        ("event_msg", "sub_agent_activity") => (
            "subagent".into(),
            "sub_agent_activity".into(),
            subagent_activity_summary(&raw),
        ),
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
        ("response_item", "agent_message") => (
            "subagent".into(),
            "agent_message".into(),
            subagent_message_summary(&raw),
        ),
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

fn subagent_activity_summary(raw: &Value) -> String {
    let payload = raw.get("payload");
    let agent = payload
        .and_then(|value| value.get("agent_path"))
        .and_then(Value::as_str)
        .map(short_agent_name)
        .filter(|value| !value.is_empty())
        .unwrap_or("子智能体");
    let kind = payload
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let activity = match kind {
        "started" => "已启动".to_string(),
        "interacted" => "有新活动".to_string(),
        "interrupted" => "已中断".to_string(),
        other => format!("活动：{other}"),
    };
    format!("{agent} {activity}")
}

fn subagent_message_summary(raw: &Value) -> String {
    let payload = raw.get("payload");
    let author = payload
        .and_then(|value| value.get("author"))
        .and_then(Value::as_str)
        .map(short_agent_name)
        .filter(|value| !value.is_empty())
        .unwrap_or("子智能体");
    let message_type = flatten_content(payload.and_then(|value| value.get("content")));
    if message_type.contains("Message Type: FINAL_ANSWER") {
        format!("{author} 已完成")
    } else {
        format!("{author} 发来更新")
    }
}

fn short_agent_name(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
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
    is_internal_codex_context_text(&first_line, &text)
}

fn is_internal_codex_context_text(first_line: &str, text: &str) -> bool {
    (first_line.starts_with("AGENTS.md instructions") && text.contains("<INSTRUCTIONS>"))
        || (first_line == "<environment_context>" && text.contains("</environment_context>"))
        || (first_line == "<recommended_plugins>" && text.contains("</recommended_plugins>"))
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

pub fn preview_session_head(
    provider: Option<String>,
    rollout_path: String,
    limit: usize,
) -> AppResult<Vec<PreviewEvent>> {
    preview_range_by_provider(provider, &rollout_path, 0, limit)
}

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

/// 时间线消息预览的最大字符数：悬浮卡片/列表只需要开头片段。
const TIMELINE_MESSAGE_PREVIEW_CHARS: usize = 400;

/// 扫描整个会话文件，以真实用户提问作为时间线刻度，并附带其后最后一条 Agent 回复摘要。
/// 不携带 raw，负载远小于全量 preview_session_range。
pub fn preview_session_user_prompts(
    provider: Option<String>,
    rollout_path: String,
) -> AppResult<UserPromptList> {
    match provider.as_deref().unwrap_or("codex") {
        "codex" => user_prompts_impl(&rollout_path, |index, raw| Some(classify(index, raw))),
        "claude" => user_prompts_impl(&rollout_path, crate::claude_sessions::classify_preview),
        other => Err(crate::error::AppError::Other(format!(
            "不支持的 provider: {other}"
        ))),
    }
}

fn user_prompts_impl(
    path: &str,
    classify_line: impl Fn(usize, Value) -> Option<PreviewEvent>,
) -> AppResult<UserPromptList> {
    let f = File::open(PathBuf::from(path))?;
    let reader = BufReader::new(f);
    let mut prompts = Vec::new();
    let mut total_events = 0usize;
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(event) = classify_line(i, raw) else {
            continue;
        };
        // offset 必须与 preview_session_range 的事件计数保持一致，前端靠它分页加载到目标
        let offset = total_events;
        total_events += 1;
        if !preview_event_is_conversation(&event) {
            continue;
        }
        let event_text = preview_event_text(&event);
        let display_text = if event.role == "user" {
            timeline_user_text(&event_text)
        } else {
            event_text
        };
        let text = clip_timeline_preview(&display_text);
        match event.role.as_str() {
            "user" => prompts.push(UserPromptBrief {
                index: event.index,
                offset,
                timestamp: event.timestamp.clone(),
                text,
                response: None,
            }),
            // 一轮里可能出现多条 Agent 消息；保留最后一条，更接近最终答复摘要。
            "assistant" if !text.is_empty() => {
                if let Some(prompt) = prompts.last_mut() {
                    prompt.response = Some(TimelineMessageBrief {
                        index: event.index,
                        offset,
                        timestamp: event.timestamp.clone(),
                        text,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(UserPromptList {
        prompts,
        total_events,
    })
}

/// 保留换行的截断：与 trim() 的单行摘要不同，悬浮卡片需要多行预览。
fn clip_timeline_preview(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= TIMELINE_MESSAGE_PREVIEW_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed
        .chars()
        .take(TIMELINE_MESSAGE_PREVIEW_CHARS)
        .collect();
    out.push('…');
    out
}

/// Codex 带附件的用户消息会把文件清单和真实请求包装在同一段文本中。
/// 时间线只展示 `My request for Codex:` 标题后的正文，避免附件元数据占满卡片。
fn timeline_user_text(text: &str) -> String {
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let heading = line.trim().trim_start_matches('#').trim();
        if heading.eq_ignore_ascii_case("My request for Codex:") {
            return lines
                .filter(|line| {
                    let trimmed = line.trim_start();
                    !trimmed.starts_with("<image") && !trimmed.starts_with("</image")
                })
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
        }
    }
    text.to_string()
}

pub fn read_rollout_token_total(path: &Path) -> AppResult<i64> {
    const REVERSE_SCAN_CHUNK_BYTES: usize = 64 * 1024;

    let mut file = File::open(path)?;
    let mut scan_end = file.metadata()?.len();
    let mut line_end = scan_end;
    let mut chunk = vec![0u8; REVERSE_SCAN_CHUNK_BYTES];

    while scan_end > 0 {
        let chunk_start = scan_end.saturating_sub(REVERSE_SCAN_CHUNK_BYTES as u64);
        let chunk_len = usize::try_from(scan_end - chunk_start).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "rollout 扫描窗口过大")
        })?;
        file.seek(SeekFrom::Start(chunk_start))?;
        file.read_exact(&mut chunk[..chunk_len])?;

        for offset in (0..chunk_len).rev() {
            if chunk[offset] != b'\n' {
                continue;
            }
            let separator = chunk_start + offset as u64;
            if let Some(total) = read_token_total_from_range(&mut file, separator + 1, line_end)? {
                return Ok(total);
            }
            line_end = separator;
        }
        scan_end = chunk_start;
    }

    Ok(read_token_total_from_range(&mut file, 0, line_end)?.unwrap_or(0))
}

fn read_token_total_from_range(file: &mut File, start: u64, end: u64) -> AppResult<Option<i64>> {
    if end <= start {
        return Ok(None);
    }
    let line_len = usize::try_from(end - start).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "rollout 单行长度超出平台限制",
        )
    })?;
    let mut bytes = vec![0u8; line_len];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut bytes)?;
    let line = std::str::from_utf8(&bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("rollout 行不是有效 UTF-8: {error}"),
        )
    })?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let Ok(raw) = serde_json::from_str::<Value>(trimmed) else {
        return Ok(None);
    };
    Ok(token_total_from_value(&raw))
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
    fn latest_token_count_does_not_require_parsing_earlier_rollout_bytes() -> AppResult<()> {
        let file = temp_file("cc-session-manager-rollout-token-tail-test");
        {
            let mut out = File::create(&file)?;
            out.write_all(&vec![0xff; 2 * 1024 * 1024])?;
            out.write_all(b"\n")?;
            writeln!(
                out,
                "{}",
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "total_tokens": 7_654_321
                            }
                        }
                    }
                })
            )?;
        }

        let total = read_rollout_token_total(&file)?;
        fs::remove_file(file).ok();

        assert_eq!(total, 7_654_321);
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
    fn classifies_subagent_activity_as_non_conversation_event() {
        let raw = serde_json::json!({
            "timestamp": "2026-07-10T00:00:00Z",
            "type": "event_msg",
            "payload": {
                "type": "sub_agent_activity",
                "event_id": "call-activity",
                "occurred_at_ms": 1_783_626_366_052_i64,
                "agent_thread_id": "019f486a-54b2-77a2-8576-ec5148028d3b",
                "agent_path": "/root/audit_backend",
                "kind": "started"
            }
        });

        let event = classify(0, raw);

        assert_eq!(event.role, "subagent");
        assert_eq!(event.kind, "sub_agent_activity");
        assert_eq!(event.text_summary, "audit_backend 已启动");
        assert!(!preview_event_is_conversation(&event));
    }

    #[test]
    fn classifies_subagent_message_without_exposing_encrypted_content() {
        let raw = serde_json::json!({
            "timestamp": "2026-07-10T00:00:00Z",
            "type": "response_item",
            "payload": {
                "type": "agent_message",
                "author": "/root/audit_backend",
                "recipient": "/root",
                "content": [
                    {
                        "type": "input_text",
                        "text": "Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/audit_backend\nPayload:\n"
                    },
                    {
                        "type": "encrypted_content",
                        "encrypted_content": "encrypted-secret"
                    }
                ]
            }
        });

        let event = classify(0, raw);

        assert_eq!(event.role, "subagent");
        assert_eq!(event.kind, "agent_message");
        assert_eq!(event.text_summary, "audit_backend 已完成");
        assert!(!event.text_summary.contains("encrypted-secret"));
        assert!(!preview_event_is_conversation(&event));
    }

    #[test]
    fn preserves_unknown_subagent_activity_kind_in_summary() {
        let raw = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "sub_agent_activity",
                "agent_path": "/root/audit_backend",
                "kind": "future_kind"
            }
        });

        let event = classify(0, raw);

        assert_eq!(event.text_summary, "audit_backend 活动：future_kind");
    }

    #[test]
    fn recommended_plugins_context_is_not_a_conversation_message() {
        let raw = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "<recommended_plugins>\nInternal plugin catalog\n</recommended_plugins>"
                    },
                    {
                        "type": "input_text",
                        "text": "# AGENTS.md instructions\n<INSTRUCTIONS>internal</INSTRUCTIONS>"
                    }
                ]
            }
        });

        let event = classify(0, raw);

        assert!(!preview_event_is_conversation(&event));
    }

    #[test]
    fn user_prompts_collects_only_real_user_questions() -> AppResult<()> {
        let file = temp_file("cc-session-manager-rollout-user-prompts-test");
        {
            let mut out = File::create(&file)?;
            for value in [
                serde_json::json!({
                    "type": "session_meta",
                    "payload": {"id": "session-1"}
                }),
                // 内部上下文消息：不应计入提问
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "<environment_context>\nfoo\n</environment_context>"}]
                    }
                }),
                // 展示层事件消息：与「仅看对话消息」一致，不计入提问
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {"type": "user_message", "message": "第一个问题"}
                }),
                serde_json::json!({
                    "type": "response_item",
                    "timestamp": "2026-07-21T10:00:00Z",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": "# Files mentioned by the user:\n\n## My request for Codex:\n第一个问题\n<image name=[Image #1]>\n</image>"
                        }]
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "回答"}]
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "第二个问题"}]
                    }
                }),
            ] {
                writeln!(out, "{}", serde_json::to_string(&value)?)?;
            }
        }

        let list = preview_session_user_prompts(
            Some("codex".to_string()),
            file.to_string_lossy().into_owned(),
        )?;
        fs::remove_file(file).ok();

        assert_eq!(list.total_events, 6);
        assert_eq!(list.prompts.len(), 2);
        assert_eq!(list.prompts[0].text, "第一个问题");
        assert_eq!(list.prompts[0].index, 3);
        assert_eq!(list.prompts[0].offset, 3);
        assert_eq!(list.prompts[0].timestamp, "2026-07-21T10:00:00Z");
        let response = list.prompts[0]
            .response
            .as_ref()
            .expect("第一轮应包含 Agent 回复");
        assert_eq!(response.text, "回答");
        assert_eq!(response.index, 4);
        assert_eq!(response.offset, 4);
        assert_eq!(list.prompts[1].text, "第二个问题");
        assert_eq!(list.prompts[1].index, 5);
        assert_eq!(list.prompts[1].offset, 5);
        assert!(list.prompts[1].response.is_none());
        Ok(())
    }

    #[test]
    fn user_prompts_pair_claude_question_with_last_agent_reply() -> AppResult<()> {
        let file = temp_file("cc-session-manager-claude-timeline-test");
        {
            let mut out = File::create(&file)?;
            for value in [
                serde_json::json!({
                    "type": "user",
                    "timestamp": "2026-07-21T10:00:00Z",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "帮我检查这个问题"}]
                    }
                }),
                serde_json::json!({
                    "type": "assistant",
                    "timestamp": "2026-07-21T10:00:01Z",
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "text", "text": "我先检查代码。"}]
                    }
                }),
                serde_json::json!({
                    "type": "assistant",
                    "timestamp": "2026-07-21T10:00:02Z",
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "text", "text": "已经定位并修复。"}]
                    }
                }),
            ] {
                writeln!(out, "{}", serde_json::to_string(&value)?)?;
            }
        }

        let list = preview_session_user_prompts(
            Some("claude".to_string()),
            file.to_string_lossy().into_owned(),
        )?;
        fs::remove_file(file).ok();

        assert_eq!(list.total_events, 3);
        assert_eq!(list.prompts.len(), 1);
        assert_eq!(list.prompts[0].text, "帮我检查这个问题");
        let response = list.prompts[0]
            .response
            .as_ref()
            .expect("Claude 对话轮次应包含回复");
        assert_eq!(response.text, "已经定位并修复。");
        assert_eq!(response.index, 2);
        assert_eq!(response.offset, 2);
        Ok(())
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
