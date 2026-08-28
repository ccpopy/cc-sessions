//! 会话导出为人眼可读的 Markdown。
//!
//! 与 bundle.rs / backup.rs 的区别：那些是面向迁移/校验的 JSONL + manifest，
//! 这里产出的是给**人阅读**或**另一个 AI 当上下文**用的纯 Markdown。
//!
//! 设计取舍（详见 issue #7 讨论）：
//! - 默认只保留 user / assistant 对话，工具调用与模型推理默认关闭；
//! - 同一条 Codex 消息在 rollout 里既有 `event_msg` 又有 `response_item`，
//!   这里只取 `response_item`（与预览的"仅看对话消息"一致）以避免重复；
//! - Claude 的 assistant 回合常把 text 与 tool_use 混在一条消息里，
//!   这里会保留其中的正文，不会因为含 tool_use 就整条丢弃；
//! - 用户中途的"引导"消息会保留（它是任务意图的高价值信号），
//!   但 Codex 注入的 AGENTS.md / environment_context 这类内部上下文会被过滤。

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::error::AppResult;
use crate::models::{
    MarkdownExportHeader, MarkdownExportOptions, MarkdownExportReport, PreviewEvent,
};
use crate::rollout::preview_session_range;

/// 一条事件归一化后的语义类别。
enum Segment {
    Message { role: &'static str, text: String },
    Reasoning(String),
    ToolCall(String),
    ToolResult(String),
    Skip,
}

pub fn export_session_markdown(
    provider: Option<String>,
    rollout_path: String,
    out_path: Option<String>,
    header: MarkdownExportHeader,
    options: MarkdownExportOptions,
) -> AppResult<MarkdownExportReport> {
    let events = preview_session_range(provider, rollout_path, 0, usize::MAX)?;
    let markdown = render_markdown(&events, &header, &options);
    let (markdown, message_count) = markdown;

    let bytes = markdown.len() as u64;
    let written = match out_path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(out) => {
            let path = Path::new(out);
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)?;
                }
            }
            fs::write(path, markdown.as_bytes())?;
            Some(out.to_string())
        }
        None => None,
    };

    Ok(MarkdownExportReport {
        ok: true,
        out_path: written,
        markdown,
        message_count,
        bytes,
    })
}

/// 返回 (markdown, 对话条数)。
fn render_markdown(
    events: &[PreviewEvent],
    header: &MarkdownExportHeader,
    options: &MarkdownExportOptions,
) -> (String, u32) {
    let selected: Option<HashSet<usize>> = options
        .selected_indices
        .as_ref()
        .map(|v| v.iter().copied().collect());

    let mut body = String::new();
    let mut message_count: u32 = 0;
    let mut first_user_message: Option<String> = None;

    for e in events {
        if let Some(sel) = &selected {
            if !sel.contains(&e.index) {
                continue;
            }
        }
        let chunk = match segment(e) {
            Segment::Message { role, text } => {
                message_count += 1;
                if role == "user" && first_user_message.is_none() {
                    first_user_message = Some(text.clone());
                }
                let label = if role == "user" {
                    "👤 User"
                } else {
                    "🤖 Assistant"
                };
                let time = format_event_time(&e.timestamp);
                let heading = if time.is_empty() {
                    format!("## {label}")
                } else {
                    format!("## {label} · {time}")
                };
                let body_text = if text.trim().is_empty() {
                    "_(空消息)_".to_string()
                } else {
                    text
                };
                format!("{heading}\n\n{body_text}")
            }
            Segment::Reasoning(text) if options.include_reasoning => {
                if text.trim().is_empty() {
                    continue;
                }
                format!("<details>\n<summary>🧠 推理过程</summary>\n\n{text}\n\n</details>")
            }
            Segment::ToolCall(label) if options.include_tools => {
                format!("> 🔧 工具调用：{label}")
            }
            Segment::ToolResult(label) if options.include_tools => {
                format!("> ↩️ 工具返回：{label}")
            }
            _ => continue,
        };
        body.push_str(&chunk);
        body.push_str("\n\n");
    }

    let mut md = String::new();
    if options.include_front_matter {
        md.push_str(&render_front_matter(header));
        md.push('\n');
    }
    if options.ai_handoff_preamble {
        md.push_str(&render_preamble(header, first_user_message.as_deref()));
        md.push('\n');
    }
    md.push_str(body.trim_end());
    md.push('\n');

    (md, message_count)
}

fn render_front_matter(h: &MarkdownExportHeader) -> String {
    let mut out = String::from("---\n");
    out.push_str(&yaml_line("title", &h.title));
    out.push_str(&yaml_line("session_id", &h.session_id));
    out.push_str(&yaml_line("provider", &h.provider));
    if let Some(model) = &h.model {
        let model = match &h.reasoning_effort {
            Some(effort) if !effort.is_empty() => format!("{model} · {effort}"),
            _ => model.clone(),
        };
        out.push_str(&yaml_line("model", &model));
    }
    if !h.cwd.is_empty() {
        out.push_str(&yaml_line("cwd", &h.cwd));
    }
    if h.created_at > 0 {
        out.push_str(&yaml_line("created", &format_epoch(h.created_at)));
    }
    if h.updated_at > 0 {
        out.push_str(&yaml_line("updated", &format_epoch(h.updated_at)));
    }
    if h.tokens_used > 0 {
        out.push_str(&format!("tokens: {}\n", h.tokens_used));
    }
    if !h.resume_command.is_empty() {
        out.push_str(&yaml_line("resume", &h.resume_command));
    }
    out.push_str("---\n");
    out
}

fn render_preamble(h: &MarkdownExportHeader, first_user_message: Option<&str>) -> String {
    let provider_label = match h.provider.as_str() {
        "claude" => "Claude",
        "codex" => "Codex",
        other => other,
    };
    let mut out = String::new();
    out.push_str(&format!(
        "> 以下是用户与 {provider_label} 的一段历史会话记录，供你作为背景上下文。\n"
    ));
    out.push_str("> - 第一条 User 消息是最初诉求；其后的 User 消息是用户追加的纠正与引导。\n");
    out.push_str("> - 工具调用与模型推理可能已被省略，只保留对话本身。\n");
    out.push_str("> 请先通读，并用一两句话向我复述你对当前任务状态的理解，再继续。\n");
    if let Some(req) = first_user_message.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("\n**原始诉求：**\n\n");
        out.push_str(req);
        out.push('\n');
    }
    out
}

/// 把一条事件归一化为语义类别。逻辑独立于预览的 role，
/// 以便正确处理 Codex 去重与 Claude 的 text+tool_use 混排。
fn segment(e: &PreviewEvent) -> Segment {
    let raw = &e.raw;

    // Claude 形态：顶层带 message
    if let Some(message) = raw.get("message") {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let content = message.get("content");
        let text = collect_claude_text(content);
        match role {
            "assistant" => {
                if !text.trim().is_empty() {
                    Segment::Message {
                        role: "assistant",
                        text,
                    }
                } else {
                    let thinking = collect_claude_thinking(content);
                    if !thinking.trim().is_empty() {
                        Segment::Reasoning(thinking)
                    } else if content_has_type(content, "tool_use") {
                        Segment::ToolCall(claude_tool_use_label(content))
                    } else {
                        Segment::Skip
                    }
                }
            }
            "user" => {
                if !text.trim().is_empty() {
                    if is_internal_user_text(&text) {
                        Segment::Skip
                    } else {
                        Segment::Message { role: "user", text }
                    }
                } else if content_has_type(content, "tool_result") {
                    Segment::ToolResult("工具结果".into())
                } else {
                    Segment::Skip
                }
            }
            _ => Segment::Skip,
        }
    } else {
        // Codex 形态：顶层 type + payload
        let outer = raw.get("type").and_then(Value::as_str).unwrap_or("");
        if outer != "response_item" {
            // event_msg 与 Codex 的 response_item/message 内容重复，这里只取后者去重
            return Segment::Skip;
        }
        let payload = raw.get("payload");
        let ptype = payload
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match ptype {
            "message" => {
                let role = payload
                    .and_then(|p| p.get("role"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let text = collect_codex_text(payload.and_then(|p| p.get("content")));
                if text.trim().is_empty() {
                    return Segment::Skip;
                }
                match role {
                    "assistant" => Segment::Message {
                        role: "assistant",
                        text,
                    },
                    "user" => {
                        if is_internal_user_text(&text) {
                            Segment::Skip
                        } else {
                            Segment::Message { role: "user", text }
                        }
                    }
                    _ => Segment::Skip,
                }
            }
            "reasoning" => {
                Segment::Reasoning(collect_codex_text(payload.and_then(|p| p.get("content"))))
            }
            "function_call" => {
                let name = payload
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                Segment::ToolCall(name.to_string())
            }
            "function_call_output" => Segment::ToolResult("工具返回".into()),
            _ => Segment::Skip,
        }
    }
}

/// 收集 Claude message.content 里的纯文本块（忽略 thinking / tool_use / tool_result）。
fn collect_claude_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                if let Some(s) = item.as_str() {
                    return Some(s.to_string());
                }
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => item.get("text").and_then(Value::as_str).map(String::from),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn collect_claude_thinking(content: Option<&Value>) -> String {
    match content {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item.get("type").and_then(Value::as_str) {
                Some("thinking") => item
                    .get("thinking")
                    .and_then(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .or_else(|| Some("(加密推理)".into())),
                Some("redacted_thinking") => Some("(加密推理)".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn claude_tool_use_label(content: Option<&Value>) -> String {
    let names: Vec<String> = match content {
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"))
            .filter_map(|item| item.get("name").and_then(Value::as_str).map(String::from))
            .collect(),
        _ => Vec::new(),
    };
    if names.is_empty() {
        "工具调用".into()
    } else {
        names.join(", ")
    }
}

fn content_has_type(content: Option<&Value>, ty: &str) -> bool {
    matches!(content, Some(Value::Array(items))
        if items
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some(ty)))
}

/// 收集 Codex response_item content 里的文本（input_text / output_text / text 等）。
fn collect_codex_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                if let Some(s) = item.as_str() {
                    return Some(s.to_string());
                }
                item.get("text").and_then(Value::as_str).map(String::from)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 识别 Codex 注入的内部上下文（AGENTS.md 指令 / environment_context），它们不是真实用户输入。
fn is_internal_user_text(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let first_line = trimmed
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('#')
        .trim_start();
    (first_line.starts_with("AGENTS.md instructions for ") && trimmed.contains("<INSTRUCTIONS>"))
        || (first_line == "<environment_context>" && trimmed.contains("</environment_context>"))
}

fn yaml_line(key: &str, value: &str) -> String {
    format!("{key}: {}\n", yaml_quote(value))
}

/// YAML 标量做最小转义：单行用双引号包裹并转义 `\` 与 `"`。
fn yaml_quote(value: &str) -> String {
    let one_line = value.replace(['\n', '\r'], " ");
    let escaped = one_line.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// epoch 秒 → 本地时间 "YYYY-MM-DD HH:MM"。
fn format_epoch(secs: i64) -> String {
    match chrono::DateTime::from_timestamp(secs, 0) {
        Some(dt) => dt
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
        None => String::new(),
    }
}

/// ISO8601 时间戳 → 本地 "YYYY-MM-DD HH:MM:SS"，解析失败返回空串。
fn format_event_time(ts: &str) -> String {
    if ts.is_empty() {
        return String::new();
    }
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(dt) => dt
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::PreviewEvent;
    use serde_json::json;
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::PathBuf;

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{name}-{}-{}.jsonl",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ))
    }

    fn ev(index: usize, raw: Value) -> PreviewEvent {
        PreviewEvent {
            index,
            timestamp: String::new(),
            role: String::new(),
            kind: String::new(),
            text_summary: String::new(),
            raw,
        }
    }

    fn default_options() -> MarkdownExportOptions {
        MarkdownExportOptions {
            include_front_matter: false,
            include_reasoning: false,
            include_tools: false,
            ai_handoff_preamble: false,
            selected_indices: None,
        }
    }

    fn header() -> MarkdownExportHeader {
        MarkdownExportHeader {
            title: "t".into(),
            session_id: "id".into(),
            provider: "claude".into(),
            model: None,
            reasoning_effort: None,
            cwd: String::new(),
            created_at: 0,
            updated_at: 0,
            tokens_used: 0,
            resume_command: String::new(),
        }
    }

    #[test]
    fn message_headings_include_the_full_local_timestamp() {
        let mut event = ev(
            0,
            json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "带日期导出"}]
                }
            }),
        );
        event.timestamp = "2026-08-27T12:34:56Z".into();
        let expected = chrono::DateTime::parse_from_rfc3339(&event.timestamp)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        let (markdown, count) = render_markdown(&[event], &header(), &default_options());

        assert_eq!(count, 1);
        assert!(
            markdown.contains(&format!("## 👤 User · {expected}")),
            "导出的消息标题缺少完整日期时间: {markdown}"
        );
    }

    #[test]
    fn keeps_assistant_text_alongside_tool_use() {
        let events = vec![ev(
            0,
            json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "我来改一下文件"},
                        {"type": "tool_use", "name": "Edit", "input": {}}
                    ]
                }
            }),
        )];
        let (md, count) = render_markdown(&events, &header(), &default_options());
        assert_eq!(count, 1);
        assert!(md.contains("我来改一下文件"));
        assert!(md.contains("🤖 Assistant"));
    }

    #[test]
    fn dedupes_codex_event_msg() {
        let events = vec![
            ev(
                0,
                json!({"type": "event_msg", "payload": {"type": "agent_message", "message": "hi"}}),
            ),
            ev(
                1,
                json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hi"}]}}),
            ),
        ];
        let (md, count) = render_markdown(&events, &header(), &default_options());
        assert_eq!(count, 1, "event_msg 应被去重，只保留 response_item");
        assert_eq!(md.matches("hi").count(), 1);
    }

    #[test]
    fn filters_internal_codex_context() {
        let events = vec![ev(
            0,
            json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>\nfoo\n</environment_context>"}]}}),
        )];
        let (_, count) = render_markdown(&events, &header(), &default_options());
        assert_eq!(count, 0);
    }

    #[test]
    fn tools_and_reasoning_hidden_by_default() {
        let events = vec![
            ev(
                0,
                json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell"}}),
            ),
            ev(
                1,
                json!({"type": "response_item", "payload": {"type": "reasoning", "content": [{"type": "text", "text": "thinking"}]}}),
            ),
        ];
        let (md, _) = render_markdown(&events, &header(), &default_options());
        assert!(!md.contains("shell"));
        assert!(!md.contains("thinking"));

        let mut opts = default_options();
        opts.include_tools = true;
        opts.include_reasoning = true;
        let (md2, _) = render_markdown(&events, &header(), &opts);
        assert!(md2.contains("shell"));
        assert!(md2.contains("thinking"));
    }

    #[test]
    fn respects_selected_indices() {
        let events = vec![
            ev(
                3,
                json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "keep me"}]}}),
            ),
            ev(
                7,
                json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "drop me"}]}}),
            ),
        ];
        let mut opts = default_options();
        opts.selected_indices = Some(vec![3]);
        let (md, count) = render_markdown(&events, &header(), &opts);
        assert_eq!(count, 1);
        assert!(md.contains("keep me"));
        assert!(!md.contains("drop me"));
    }

    #[test]
    fn export_session_markdown_reads_full_codex_session_without_unbounded_preallocation(
    ) -> AppResult<()> {
        let file = temp_file("cc-session-manager-markdown-export-unbounded-limit-test");
        {
            let mut out = File::create(&file)?;
            for value in [
                json!({"type": "session_meta", "payload": {"id": "id"}}),
                json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "question"}]}}),
                json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]}}),
            ] {
                writeln!(out, "{}", serde_json::to_string(&value)?)?;
            }
        }

        let report = export_session_markdown(
            Some("codex".to_string()),
            file.to_string_lossy().into_owned(),
            None,
            header(),
            default_options(),
        )?;
        fs::remove_file(file).ok();

        assert_eq!(report.message_count, 2);
        assert!(report.markdown.contains("question"));
        assert!(report.markdown.contains("answer"));
        Ok(())
    }
}
