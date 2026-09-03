//! 会话导出为人眼可读的 Markdown。
//!
//! 与 bundle.rs / backup.rs 的区别：那些是面向迁移/校验的 JSONL + manifest，
//! 这里产出的是给**人阅读**或**另一个 AI 当上下文**用的纯 Markdown。
//!
//! 设计取舍（详见 issue #7 / #39 讨论）：
//! - 默认只保留 user / assistant 对话，工具调用与模型推理默认关闭；
//! - 同一条 Codex 消息在 rollout 里既有 `event_msg` 又有 `response_item`，
//!   这里只取 `response_item`（与预览的"仅看对话消息"一致）以避免重复；
//! - Claude 的 assistant 回合常把 text 与 tool_use 混在一条消息里，
//!   这里会保留其中的正文，不会因为含 tool_use 就整条丢弃；
//! - 用户中途的"引导"消息会保留（它是任务意图的高价值信号），
//!   但 Codex 注入的 AGENTS.md / environment_context 这类内部上下文会被过滤；
//! - 片段勾选（`selected_indices`）与时间范围（`time_from` / `time_to`）只作用于
//!   对话消息；推理 / 工具事件跟随它们所服务的那条回复一起进出，避免孤立的工具行。

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::error::AppResult;
use crate::models::{
    MarkdownExportHeader, MarkdownExportOptions, MarkdownExportReport, PreviewEvent,
};
use crate::rollout::preview_session_range;

/// 工具调用 / 返回摘要的最大字符数（单行）。
const TOOL_DETAIL_MAX_CHARS: usize = 120;

/// 一条事件归一化后的语义类别。
enum Segment {
    Message { role: &'static str, text: String },
    Reasoning(String),
    ToolCall(String),
    ToolResult(String),
    Skip,
}

struct Rendered {
    markdown: String,
    message_count: u32,
    total_message_count: u32,
}

pub fn export_session_markdown(
    provider: Option<String>,
    rollout_path: String,
    out_path: Option<String>,
    header: MarkdownExportHeader,
    options: MarkdownExportOptions,
) -> AppResult<MarkdownExportReport> {
    let events = preview_session_range(provider, rollout_path, 0, usize::MAX)?;
    let rendered = render_markdown(&events, &header, &options);

    let bytes = rendered.markdown.len() as u64;
    let written = match out_path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(out) => {
            let path = Path::new(out);
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)?;
                }
            }
            fs::write(path, rendered.markdown.as_bytes())?;
            Some(out.to_string())
        }
        None => None,
    };

    Ok(MarkdownExportReport {
        ok: true,
        out_path: written,
        markdown: rendered.markdown,
        message_count: rendered.message_count,
        total_message_count: rendered.total_message_count,
        bytes,
    })
}

fn render_markdown(
    events: &[PreviewEvent],
    header: &MarkdownExportHeader,
    options: &MarkdownExportOptions,
) -> Rendered {
    let segments: Vec<Segment> = events.iter().map(segment).collect();
    let (included, total_message_count) = plan_inclusion(events, &segments, options);

    let mut body = String::new();
    let mut message_count: u32 = 0;
    let mut first_user_message: Option<String> = None;

    for ((e, seg), include) in events.iter().zip(segments).zip(included) {
        if !include {
            continue;
        }
        let chunk = match seg {
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
            Segment::ToolResult(brief) if options.include_tools => {
                if brief.is_empty() {
                    "> ↩️ 工具返回".to_string()
                } else {
                    format!("> ↩️ 工具返回：{brief}")
                }
            }
            _ => continue,
        };
        body.push_str(&chunk);
        body.push_str("\n\n");
    }

    let excerpt = ExcerptInfo {
        message_count,
        total_message_count,
        time_from: options.time_from,
        time_to: options.time_to,
    };
    let mut md = String::new();
    if options.include_front_matter {
        md.push_str(&render_front_matter(header, &excerpt));
        md.push('\n');
    }
    if options.ai_handoff_preamble {
        md.push_str(&render_preamble(
            header,
            first_user_message.as_deref(),
            &excerpt,
        ));
        md.push('\n');
    }
    md.push_str(body.trim_end());
    md.push('\n');

    Rendered {
        markdown: md,
        message_count,
        total_message_count,
    }
}

/// 决定每条事件是否导出，并返回会话内对话消息总数。
///
/// 对话消息按 `selected_indices` 与时间范围各自判断；推理 / 工具事件没有独立的取舍
/// 意义，跟随它们所服务的那条消息——文件顺序上的下一条对话消息（Codex 的推理与
/// 工具调用都发生在最终回复之前），末尾没有后继消息的残留则跟随上一条。这样勾选
/// 一条回答或框定一段时间时，配套的推理与工具调用一并进出，不会出现孤立的工具行。
fn plan_inclusion(
    events: &[PreviewEvent],
    segments: &[Segment],
    options: &MarkdownExportOptions,
) -> (Vec<bool>, u32) {
    let n = events.len();
    let is_message: Vec<bool> = segments
        .iter()
        .map(|seg| matches!(seg, Segment::Message { .. }))
        .collect();
    let total_messages = is_message.iter().filter(|flag| **flag).count() as u32;

    let selected: Option<HashSet<usize>> = options
        .selected_indices
        .as_ref()
        .map(|v| v.iter().copied().collect());
    let has_time_filter = options.time_from.is_some() || options.time_to.is_some();
    if selected.is_none() && !has_time_filter {
        return (vec![true; n], total_messages);
    }

    let mut included = vec![false; n];
    for i in 0..n {
        if !is_message[i] {
            continue;
        }
        let by_selection = selected
            .as_ref()
            .is_none_or(|set| set.contains(&events[i].index));
        included[i] = by_selection && within_time_range(&events[i].timestamp, options);
    }

    let mut owner: Vec<Option<usize>> = vec![None; n];
    let mut next_message = None;
    for i in (0..n).rev() {
        if is_message[i] {
            next_message = Some(i);
        } else {
            owner[i] = next_message;
        }
    }
    let mut previous_message = None;
    for i in 0..n {
        if is_message[i] {
            previous_message = Some(i);
        } else if owner[i].is_none() {
            owner[i] = previous_message;
        }
    }
    for i in 0..n {
        if !is_message[i] {
            included[i] = owner[i].is_some_and(|owner_index| included[owner_index]);
        }
    }
    (included, total_messages)
}

/// 无时间戳（或无法解析）的事件无法定位，不受时间范围限制，与前端列表行为一致。
fn within_time_range(timestamp: &str, options: &MarkdownExportOptions) -> bool {
    let Some(epoch) = parse_event_epoch(timestamp) else {
        return true;
    };
    options.time_from.is_none_or(|from| epoch >= from)
        && options.time_to.is_none_or(|to| epoch < to)
}

struct ExcerptInfo {
    message_count: u32,
    total_message_count: u32,
    time_from: Option<i64>,
    time_to: Option<i64>,
}

impl ExcerptInfo {
    fn is_excerpt(&self) -> bool {
        self.message_count < self.total_message_count
    }

    /// 时间范围的本地时间描述；上界按"不含"回退一秒，显示为最后一个被包含的分钟。
    fn range_label(&self) -> Option<String> {
        let from = self.time_from.map(format_epoch).unwrap_or_default();
        let to = self
            .time_to
            .map(|to| format_epoch(to.saturating_sub(1)))
            .unwrap_or_default();
        match (from.is_empty(), to.is_empty()) {
            (false, false) => Some(format!("{from} ~ {to}")),
            (false, true) => Some(format!("{from} 起")),
            (true, false) => Some(format!("至 {to}")),
            (true, true) => None,
        }
    }
}

fn render_front_matter(h: &MarkdownExportHeader, excerpt: &ExcerptInfo) -> String {
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
    out.push_str(&format!("messages: {}\n", excerpt.message_count));
    if excerpt.is_excerpt() {
        out.push_str(&yaml_line(
            "excerpt",
            &format!(
                "{}/{} 条对话",
                excerpt.message_count, excerpt.total_message_count
            ),
        ));
        if let Some(range) = excerpt.range_label() {
            out.push_str(&yaml_line("range", &range));
        }
    }
    out.push_str(&yaml_line(
        "exported",
        &chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
    ));
    out.push_str("---\n");
    out
}

fn render_preamble(
    h: &MarkdownExportHeader,
    first_user_message: Option<&str>,
    excerpt: &ExcerptInfo,
) -> String {
    let provider_label = match h.provider.as_str() {
        "claude" => "Claude",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "cursor" => "Cursor",
        other => other,
    };
    let mut out = String::new();
    out.push_str(&format!(
        "> 以下是用户与 {provider_label} 的一段历史会话记录，供你作为背景上下文。\n"
    ));
    out.push_str("> - 第一条 User 消息是最初诉求；其后的 User 消息是用户追加的纠正与引导。\n");
    out.push_str("> - 工具调用与模型推理可能已被省略，只保留对话本身。\n");
    if excerpt.is_excerpt() {
        let range = excerpt
            .range_label()
            .map(|range| format!("，时间范围 {range}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "> - 这只是会话的节选（{}/{} 条对话{range}），前后文可能缺失，不要假设这里就是全部。\n",
            excerpt.message_count, excerpt.total_message_count
        ));
    }
    out.push_str("> 请先通读，并用一两句话向我复述你对当前任务状态的理解，再继续。\n");
    if let Some(req) = first_user_message.map(str::trim).filter(|s| !s.is_empty()) {
        let label = if excerpt.is_excerpt() {
            "**节选内的首条诉求：**"
        } else {
            "**原始诉求：**"
        };
        out.push_str(&format!("\n{label}\n\n"));
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
                    Segment::ToolResult(claude_tool_result_brief(content))
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
                let detail = payload
                    .and_then(|p| p.get("arguments"))
                    .and_then(|arguments| match arguments {
                        Value::String(raw_arguments) => {
                            serde_json::from_str::<Value>(raw_arguments).ok()
                        }
                        other => Some(other.clone()),
                    })
                    .and_then(|arguments| tool_input_detail(&arguments));
                Segment::ToolCall(tool_label(name, detail))
            }
            "custom_tool_call" => {
                let name = payload
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let detail = payload
                    .and_then(|p| p.get("input"))
                    .and_then(Value::as_str)
                    .and_then(first_line_brief);
                Segment::ToolCall(tool_label(name, detail))
            }
            "local_shell_call" => {
                let detail = payload
                    .and_then(|p| p.get("action"))
                    .and_then(|action| action.get("command"))
                    .and_then(command_detail);
                Segment::ToolCall(tool_label("shell", detail))
            }
            "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
                Segment::ToolResult(codex_tool_output_brief(
                    payload.and_then(|p| p.get("output")),
                ))
            }
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

/// Claude tool_use 摘要："Bash: git status; Edit: src/a.ts"。
fn claude_tool_use_label(content: Option<&Value>) -> String {
    let labels: Vec<String> = match content {
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"))
            .map(|item| {
                let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
                tool_label(name, item.get("input").and_then(tool_input_detail))
            })
            .collect(),
        _ => Vec::new(),
    };
    if labels.is_empty() {
        "工具调用".into()
    } else {
        truncate_line(&labels.join("; "), TOOL_DETAIL_MAX_CHARS)
    }
}

/// Claude tool_result 的首行摘要（content 为字符串或 text 块数组）。
fn claude_tool_result_brief(content: Option<&Value>) -> String {
    let Some(Value::Array(items)) = content else {
        return String::new();
    };
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
        .find_map(|item| match item.get("content") {
            Some(Value::String(text)) => first_line_brief(text),
            Some(Value::Array(blocks)) => blocks.iter().find_map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .and_then(first_line_brief)
            }),
            _ => None,
        })
        .unwrap_or_default()
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

/// Codex 工具输出摘要。output 有三种形态：纯文本；`{"output": "...", "metadata": …}` 的
/// JSON 串；新版 exec 工具的 `[{"type":"input_text","text":"…"}, …]` 内容块数组。
/// 块数组会先拼成文本；若带有 "Output:" 标记行，则跳过前面的执行统计只取真正的输出。
fn codex_tool_output_brief(output: Option<&Value>) -> String {
    let Some(output) = output else {
        return String::new();
    };
    let text = match output {
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(parsed @ (Value::Object(_) | Value::Array(_))) => {
                structured_output_text(&parsed).unwrap_or_else(|| raw.clone())
            }
            _ => raw.clone(),
        },
        structured @ (Value::Object(_) | Value::Array(_)) => {
            structured_output_text(structured).unwrap_or_default()
        }
        other => other.to_string(),
    };
    let after_marker = text
        .split_once("Output:\n")
        .map(|(_, rest)| rest)
        .filter(|rest| !rest.trim().is_empty());
    first_line_brief(after_marker.unwrap_or(&text)).unwrap_or_default()
}

/// 从结构化的工具输出里取文本：对象取 `output` 字段，数组拼接各块的 `text`。
fn structured_output_text(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => map
            .get("output")
            .and_then(Value::as_str)
            .map(str::to_string),
        Value::Array(blocks) => {
            let joined = blocks
                .iter()
                .filter_map(|block| {
                    block
                        .as_str()
                        .or_else(|| block.get("text").and_then(Value::as_str))
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

/// 从工具入参里挑一个最能说明"做了什么"的字段：命令优先，其次是路径 / 模式 / 查询。
fn tool_input_detail(input: &Value) -> Option<String> {
    const KEYS: [&str; 9] = [
        "cmd",
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "description",
        "prompt",
    ];
    let object = input.as_object()?;
    KEYS.iter()
        .find_map(|key| object.get(*key).and_then(command_detail))
}

/// 命令既可能是字符串也可能是 argv 数组。
fn command_detail(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => first_line_brief(text),
        Value::Array(parts) => {
            let joined = parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            first_line_brief(&joined)
        }
        _ => None,
    }
}

fn tool_label(name: &str, detail: Option<String>) -> String {
    match detail {
        Some(detail) => truncate_line(&format!("{name}: {detail}"), TOOL_DETAIL_MAX_CHARS),
        None => name.to_string(),
    }
}

/// 取首个非空行并截断；用于工具入参 / 输出的一行摘要。
fn first_line_brief(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(truncate_line(line, TOOL_DETAIL_MAX_CHARS))
}

/// 折叠空白并按字符（而非字节）截断，超出部分以省略号结尾。
fn truncate_line(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    let mut out: String = normalized
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect();
    out.push('…');
    out
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

/// 事件时间戳 → 本地时间。优先 RFC3339；无时区的 ISO 字符串按本地时间解释。
fn parse_event_datetime(ts: &str) -> Option<chrono::DateTime<chrono::Local>> {
    let ts = ts.trim();
    if ts.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return Some(dt.with_timezone(&chrono::Local));
    }
    ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
        .iter()
        .find_map(|pattern| chrono::NaiveDateTime::parse_from_str(ts, pattern).ok())
        .and_then(|naive| naive.and_local_timezone(chrono::Local).single())
}

fn parse_event_epoch(ts: &str) -> Option<i64> {
    parse_event_datetime(ts).map(|dt| dt.timestamp())
}

/// 事件时间戳 → 本地 "YYYY-MM-DD HH:MM:SS"，解析失败返回空串。
fn format_event_time(ts: &str) -> String {
    parse_event_datetime(ts)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
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

    fn ev_at(index: usize, timestamp: &str, raw: Value) -> PreviewEvent {
        let mut event = ev(index, raw);
        event.timestamp = timestamp.to_string();
        event
    }

    fn epoch(timestamp: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .timestamp()
    }

    fn user(text: &str) -> Value {
        json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}})
    }

    fn assistant(text: &str) -> Value {
        json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}})
    }

    fn reasoning(text: &str) -> Value {
        json!({"type": "response_item", "payload": {"type": "reasoning", "content": [{"type": "text", "text": text}]}})
    }

    fn shell_call(command: &[&str]) -> Value {
        json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell", "arguments": json!({"cmd": command}).to_string()}})
    }

    fn default_options() -> MarkdownExportOptions {
        MarkdownExportOptions {
            include_front_matter: false,
            include_reasoning: false,
            include_tools: false,
            ai_handoff_preamble: false,
            selected_indices: None,
            time_from: None,
            time_to: None,
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
        let event = ev_at(0, "2026-08-27T12:34:56Z", user("带日期导出"));
        let expected = chrono::DateTime::parse_from_rfc3339(&event.timestamp)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        let rendered = render_markdown(&[event], &header(), &default_options());

        assert_eq!(rendered.message_count, 1);
        assert!(
            rendered
                .markdown
                .contains(&format!("## 👤 User · {expected}")),
            "导出的消息标题缺少完整日期时间: {}",
            rendered.markdown
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
        let rendered = render_markdown(&events, &header(), &default_options());
        assert_eq!(rendered.message_count, 1);
        assert!(rendered.markdown.contains("我来改一下文件"));
        assert!(rendered.markdown.contains("🤖 Assistant"));
    }

    #[test]
    fn dedupes_codex_event_msg() {
        let events = vec![
            ev(
                0,
                json!({"type": "event_msg", "payload": {"type": "agent_message", "message": "hi"}}),
            ),
            ev(1, assistant("hi")),
        ];
        let rendered = render_markdown(&events, &header(), &default_options());
        assert_eq!(
            rendered.message_count, 1,
            "event_msg 应被去重，只保留 response_item"
        );
        assert_eq!(rendered.markdown.matches("hi").count(), 1);
    }

    #[test]
    fn filters_internal_codex_context() {
        let events = vec![ev(
            0,
            user("<environment_context>\nfoo\n</environment_context>"),
        )];
        let rendered = render_markdown(&events, &header(), &default_options());
        assert_eq!(rendered.message_count, 0);
        assert_eq!(rendered.total_message_count, 0);
    }

    #[test]
    fn tools_and_reasoning_hidden_by_default() {
        let events = vec![
            ev(
                0,
                json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell"}}),
            ),
            ev(1, reasoning("thinking")),
        ];
        let rendered = render_markdown(&events, &header(), &default_options());
        assert!(!rendered.markdown.contains("shell"));
        assert!(!rendered.markdown.contains("thinking"));

        let mut opts = default_options();
        opts.include_tools = true;
        opts.include_reasoning = true;
        let rendered = render_markdown(&events, &header(), &opts);
        assert!(rendered.markdown.contains("shell"));
        assert!(rendered.markdown.contains("thinking"));
    }

    #[test]
    fn respects_selected_indices() {
        let events = vec![ev(3, user("keep me")), ev(7, assistant("drop me"))];
        let mut opts = default_options();
        opts.selected_indices = Some(vec![3]);
        let rendered = render_markdown(&events, &header(), &opts);
        assert_eq!(rendered.message_count, 1);
        assert_eq!(rendered.total_message_count, 2);
        assert!(rendered.markdown.contains("keep me"));
        assert!(!rendered.markdown.contains("drop me"));
    }

    #[test]
    fn selected_reply_carries_its_reasoning_and_tools() {
        let events = vec![
            ev(0, user("question one")),
            ev(1, reasoning("plan one")),
            ev(2, shell_call(&["git", "status"])),
            ev(3, assistant("answer one")),
            ev(4, user("question two")),
            ev(5, reasoning("plan two")),
            ev(6, assistant("answer two")),
        ];
        let mut opts = default_options();
        opts.include_tools = true;
        opts.include_reasoning = true;
        opts.selected_indices = Some(vec![3]);

        let rendered = render_markdown(&events, &header(), &opts);

        assert_eq!(rendered.message_count, 1);
        assert!(rendered.markdown.contains("plan one"));
        assert!(rendered.markdown.contains("shell: git status"));
        assert!(rendered.markdown.contains("answer one"));
        assert!(!rendered.markdown.contains("question one"));
        assert!(!rendered.markdown.contains("plan two"));
        assert!(!rendered.markdown.contains("answer two"));
    }

    #[test]
    fn time_range_keeps_messages_in_window_and_their_tool_activity() {
        let events = vec![
            ev_at(0, "2026-09-02T02:00:00Z", user("early question")),
            ev_at(1, "2026-09-02T02:00:30Z", reasoning("early plan")),
            ev_at(2, "2026-09-02T02:01:00Z", shell_call(&["ls"])),
            ev_at(3, "2026-09-02T02:02:00Z", assistant("early answer")),
            ev_at(4, "2026-09-02T03:00:00Z", user("late question")),
            ev_at(5, "2026-09-02T03:00:10Z", reasoning("late plan")),
            ev_at(6, "2026-09-02T03:01:00Z", assistant("late answer")),
        ];
        let mut opts = default_options();
        opts.include_tools = true;
        opts.include_reasoning = true;
        opts.time_from = Some(epoch("2026-09-02T02:00:00Z"));
        opts.time_to = Some(epoch("2026-09-02T02:30:00Z"));

        let rendered = render_markdown(&events, &header(), &opts);

        assert_eq!(rendered.message_count, 2);
        assert_eq!(rendered.total_message_count, 4);
        for expected in ["early question", "early plan", "shell: ls", "early answer"] {
            assert!(rendered.markdown.contains(expected), "缺少 {expected}");
        }
        for unexpected in ["late question", "late plan", "late answer"] {
            assert!(
                !rendered.markdown.contains(unexpected),
                "不应包含 {unexpected}"
            );
        }
    }

    #[test]
    fn time_range_upper_bound_is_exclusive_and_lower_bound_inclusive() {
        let events = vec![
            ev_at(0, "2026-09-02T02:00:00Z", user("at start")),
            ev_at(1, "2026-09-02T02:30:00Z", assistant("at end")),
        ];
        let mut opts = default_options();
        opts.time_from = Some(epoch("2026-09-02T02:00:00Z"));
        opts.time_to = Some(epoch("2026-09-02T02:30:00Z"));

        let rendered = render_markdown(&events, &header(), &opts);

        assert!(rendered.markdown.contains("at start"));
        assert!(!rendered.markdown.contains("at end"));
    }

    #[test]
    fn events_without_timestamp_ignore_time_range() {
        let events = vec![
            ev(0, user("undated question")),
            ev_at(1, "2026-09-02T05:00:00Z", assistant("dated answer")),
        ];
        let mut opts = default_options();
        opts.time_from = Some(epoch("2026-09-02T01:00:00Z"));
        opts.time_to = Some(epoch("2026-09-02T02:00:00Z"));

        let rendered = render_markdown(&events, &header(), &opts);

        assert_eq!(rendered.message_count, 1);
        assert!(rendered.markdown.contains("undated question"));
        assert!(!rendered.markdown.contains("dated answer"));
    }

    #[test]
    fn trailing_tool_activity_follows_previous_message() {
        let events = vec![
            ev(0, user("question")),
            ev(1, assistant("answer")),
            ev(2, shell_call(&["cargo", "test"])),
        ];
        let mut opts = default_options();
        opts.include_tools = true;
        opts.selected_indices = Some(vec![1]);
        let rendered = render_markdown(&events, &header(), &opts);
        assert!(rendered.markdown.contains("shell: cargo test"));

        opts.selected_indices = Some(vec![0]);
        let rendered = render_markdown(&events, &header(), &opts);
        assert!(!rendered.markdown.contains("shell: cargo test"));
    }

    #[test]
    fn front_matter_and_preamble_mark_excerpts() {
        let events = vec![
            ev_at(0, "2026-09-02T02:00:00Z", user("first ask")),
            ev_at(1, "2026-09-02T02:05:00Z", assistant("first answer")),
            ev_at(2, "2026-09-02T03:00:00Z", user("second ask")),
            ev_at(3, "2026-09-02T03:05:00Z", assistant("second answer")),
        ];
        let mut opts = default_options();
        opts.include_front_matter = true;
        opts.ai_handoff_preamble = true;
        opts.time_from = Some(epoch("2026-09-02T02:50:00Z"));
        opts.time_to = Some(epoch("2026-09-02T03:10:00Z"));

        let rendered = render_markdown(&events, &header(), &opts);

        assert_eq!(rendered.message_count, 2);
        assert!(rendered.markdown.contains("messages: 2\n"));
        assert!(rendered.markdown.contains("excerpt: \"2/4 条对话\""));
        let range_line = format!(
            "range: \"{} ~ {}\"",
            format_epoch(epoch("2026-09-02T02:50:00Z")),
            format_epoch(epoch("2026-09-02T03:09:59Z"))
        );
        assert!(
            rendered.markdown.contains(&range_line),
            "缺少时间范围行 {range_line}: {}",
            rendered.markdown
        );
        assert!(rendered.markdown.contains("这只是会话的节选（2/4 条对话"));
        assert!(rendered
            .markdown
            .contains("**节选内的首条诉求：**\n\nsecond ask"));

        let full = render_markdown(
            &events,
            &header(),
            &MarkdownExportOptions {
                include_front_matter: true,
                ai_handoff_preamble: true,
                ..default_options()
            },
        );
        assert!(full.markdown.contains("messages: 4\n"));
        assert!(!full.markdown.contains("excerpt:"));
        assert!(!full.markdown.contains("节选"));
        assert!(full.markdown.contains("**原始诉求：**\n\nfirst ask"));
    }

    #[test]
    fn tool_labels_include_command_and_output_brief() {
        let events = vec![
            ev(0, user("run it")),
            ev(1, shell_call(&["git", "status", "--short"])),
            ev(
                2,
                json!({"type": "response_item", "payload": {"type": "function_call_output", "output": json!({"output": " M src/main.rs\n?? notes.md\n", "metadata": {"exit_code": 0}}).to_string()}}),
            ),
            ev(
                3,
                json!({"type": "response_item", "payload": {"type": "local_shell_call", "action": {"type": "exec", "command": ["cargo", "build"]}}}),
            ),
            ev(
                4,
                json!({"type": "response_item", "payload": {"type": "custom_tool_call", "name": "apply_patch", "input": "*** Begin Patch\n*** Update File: a.rs"}}),
            ),
            ev(
                5,
                json!({
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [
                            {"type": "tool_use", "name": "Bash", "input": {"command": "pnpm test", "description": "run tests"}},
                            {"type": "tool_use", "name": "Edit", "input": {"file_path": "src/app.tsx"}}
                        ]
                    }
                }),
            ),
            ev(
                6,
                json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [{"type": "tool_result", "content": [{"type": "text", "text": "\n\nall 12 tests passed\nmore"}]}]
                    }
                }),
            ),
            ev(7, assistant("done")),
        ];
        let mut opts = default_options();
        opts.include_tools = true;

        let rendered = render_markdown(&events, &header(), &opts);

        for expected in [
            "> 🔧 工具调用：shell: git status --short",
            "> ↩️ 工具返回：M src/main.rs",
            "> 🔧 工具调用：shell: cargo build",
            "> 🔧 工具调用：apply_patch: *** Begin Patch",
            "> 🔧 工具调用：Bash: pnpm test; Edit: src/app.tsx",
            "> ↩️ 工具返回：all 12 tests passed",
        ] {
            assert!(
                rendered.markdown.contains(expected),
                "缺少 {expected}: {}",
                rendered.markdown
            );
        }
    }

    #[test]
    fn exec_tool_output_blocks_are_summarized_after_the_output_marker() {
        let blocks = json!([
            {"type": "input_text", "text": "Script completed\nWall time 0.6 seconds\nOutput:\n"},
            {"type": "input_text", "text": "\r\nId ProcessName\r\n62872 v2rayN\r\n"}
        ]);
        assert_eq!(
            codex_tool_output_brief(Some(&Value::String(blocks.to_string()))),
            "Id ProcessName"
        );
        assert_eq!(codex_tool_output_brief(Some(&blocks)), "Id ProcessName");

        let stats_only = json!([{"type": "input_text", "text": "Script completed\nWall time 0.6 seconds\nOutput:\n"}]);
        assert_eq!(
            codex_tool_output_brief(Some(&Value::String(stats_only.to_string()))),
            "Script completed"
        );

        let plain = Value::String("plain first line\nsecond".into());
        assert_eq!(codex_tool_output_brief(Some(&plain)), "plain first line");
        assert_eq!(codex_tool_output_brief(None), "");
    }

    #[test]
    fn long_tool_details_are_truncated_by_chars() {
        let long = "字".repeat(300);
        let label = tool_label("shell", Some(long));
        assert!(label.ends_with('…'));
        assert_eq!(label.chars().count(), TOOL_DETAIL_MAX_CHARS);
    }

    #[test]
    fn naive_timestamps_are_parsed_as_local_time() {
        assert!(parse_event_epoch("2026-09-02T10:10:36.123").is_some());
        assert!(parse_event_epoch("2026-09-02 10:10:36").is_some());
        assert!(parse_event_epoch("").is_none());
        assert!(parse_event_epoch("not a time").is_none());
        assert_eq!(
            parse_event_epoch("2026-09-02T02:00:00Z"),
            Some(epoch("2026-09-02T02:00:00Z"))
        );
    }

    #[test]
    fn export_session_markdown_reads_full_codex_session_without_unbounded_preallocation(
    ) -> AppResult<()> {
        let file = temp_file("cc-session-manager-markdown-export-unbounded-limit-test");
        {
            let mut out = File::create(&file)?;
            for value in [
                json!({"type": "session_meta", "payload": {"id": "id"}}),
                user("question"),
                assistant("answer"),
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
        assert_eq!(report.total_message_count, 2);
        assert!(report.markdown.contains("question"));
        assert!(report.markdown.contains("answer"));
        Ok(())
    }
}
