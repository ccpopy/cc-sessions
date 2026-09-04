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

use std::collections::{HashMap, HashSet};
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
const TOOL_BLOCK_MAX_LINES: usize = 200;
const TOOL_BLOCK_MAX_CHARS: usize = 8_000;

/// 一条事件归一化后的语义类别。
#[derive(Clone)]
enum Segment {
    Message {
        role: &'static str,
        text: String,
        tool_calls: Vec<ToolCall>,
        tool_results: Vec<ToolResult>,
    },
    Reasoning(String),
    ToolCalls(Vec<ToolCall>),
    ToolResults(Vec<ToolResult>),
    PatchApplied(PatchApplied),
    Skip,
}

#[derive(Clone)]
struct ToolCall {
    id: Option<String>,
    name: String,
    input: Value,
    embedded_result: Option<ToolResult>,
}

#[derive(Clone)]
struct ToolResult {
    call_id: Option<String>,
    text: String,
    truncated: bool,
    is_error: bool,
    metadata: Option<Value>,
    has_image: bool,
}

#[derive(Clone)]
struct PatchApplied {
    call_id: Option<String>,
    success: bool,
    stdout: String,
    stderr: String,
    output_truncated: bool,
    changes: Vec<FileChange>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileChangeKind {
    Update,
    Add,
    Delete,
    Move,
}

#[derive(Clone)]
struct FileChange {
    kind: FileChangeKind,
    path: String,
    move_to: Option<String>,
    diff: String,
    content: String,
    truncated: bool,
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
    let mut tool_chunks = if options.include_tools {
        render_tool_chunks(&segments, &included, &header.cwd)
    } else {
        vec![None; segments.len()]
    };

    let mut body = String::new();
    let mut message_count: u32 = 0;
    let mut first_user_message: Option<String> = None;

    for (position, ((e, seg), include)) in events.iter().zip(segments).zip(included).enumerate() {
        if !include {
            continue;
        }
        let chunk = match seg {
            Segment::Message { role, text, .. } => {
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
                let mut chunk = format!("{heading}\n\n{body_text}");
                if let Some(tools) = tool_chunks[position].take() {
                    chunk.push_str("\n\n");
                    chunk.push_str(&tools);
                }
                chunk
            }
            Segment::Reasoning(text) if options.include_reasoning => {
                if text.trim().is_empty() {
                    continue;
                }
                format!("<details>\n<summary>🧠 推理过程</summary>\n\n{text}\n\n</details>")
            }
            Segment::ToolCalls(_) | Segment::ToolResults(_) | Segment::PatchApplied(_)
                if options.include_tools =>
            {
                let Some(chunk) = tool_chunks[position].take() else {
                    continue;
                };
                chunk
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

    if raw.get("type").and_then(Value::as_str) == Some("event_msg")
        && raw
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str)
            == Some("patch_apply_end")
    {
        return parse_patch_applied(raw);
    }

    // Claude 形态：顶层带 message
    if let Some(message) = raw.get("message") {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let content = message.get("content");
        let text = collect_claude_text(content);
        let tool_calls = collect_claude_tool_calls(content);
        let tool_results = collect_claude_tool_results(raw, content);
        match role {
            "assistant" => {
                if !text.trim().is_empty() {
                    Segment::Message {
                        role: "assistant",
                        text,
                        tool_calls,
                        tool_results,
                    }
                } else {
                    let thinking = collect_claude_thinking(content);
                    if !thinking.trim().is_empty() {
                        Segment::Reasoning(thinking)
                    } else if !tool_calls.is_empty() {
                        Segment::ToolCalls(tool_calls)
                    } else if !tool_results.is_empty() {
                        Segment::ToolResults(tool_results)
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
                        Segment::Message {
                            role: "user",
                            text,
                            tool_calls,
                            tool_results,
                        }
                    }
                } else if !tool_results.is_empty() {
                    Segment::ToolResults(tool_results)
                } else if !tool_calls.is_empty() {
                    Segment::ToolCalls(tool_calls)
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
                        tool_calls: Vec::new(),
                        tool_results: Vec::new(),
                    },
                    "user" => {
                        if is_internal_user_text(&text) {
                            Segment::Skip
                        } else {
                            Segment::Message {
                                role: "user",
                                text,
                                tool_calls: Vec::new(),
                                tool_results: Vec::new(),
                            }
                        }
                    }
                    _ => Segment::Skip,
                }
            }
            "reasoning" => {
                Segment::Reasoning(collect_codex_text(payload.and_then(|p| p.get("content"))))
            }
            "function_call" => {
                Segment::ToolCalls(vec![codex_tool_call(payload, "tool", "arguments")])
            }
            "custom_tool_call" => {
                Segment::ToolCalls(vec![codex_tool_call(payload, "tool", "input")])
            }
            "local_shell_call" => {
                Segment::ToolCalls(vec![codex_tool_call(payload, "shell", "action")])
            }
            "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
                Segment::ToolResults(vec![codex_tool_result(payload)])
            }
            _ => Segment::Skip,
        }
    }
}

fn collect_claude_tool_calls(content: Option<&Value>) -> Vec<ToolCall> {
    let Some(Value::Array(items)) = content else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"))
        .map(|item| {
            let embedded_result = item.get("state").and_then(opencode_embedded_result);
            ToolCall {
                id: string_field(item, &["id", "call_id", "callID"]),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string(),
                input: item.get("input").cloned().unwrap_or(Value::Null),
                embedded_result,
            }
        })
        .collect()
}

fn collect_claude_tool_results(raw: &Value, content: Option<&Value>) -> Vec<ToolResult> {
    let Some(Value::Array(items)) = content else {
        return Vec::new();
    };
    let result_blocks = items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
        .collect::<Vec<_>>();
    let metadata = raw.get("toolUseResult");
    result_blocks
        .iter()
        .enumerate()
        .map(|(index, block)| {
            let call_id = string_field(block, &["tool_use_id", "toolUseId", "call_id"]);
            let metadata = matching_tool_result_metadata(
                metadata,
                call_id.as_deref(),
                index,
                result_blocks.len(),
            );
            let content = block.get("content").unwrap_or(&Value::Null);
            let display_content = if !value_has_display_text(content) {
                metadata.as_ref().unwrap_or(content)
            } else {
                content
            };
            let (text, truncated) = limited_output_text(display_content);
            let has_image =
                value_has_image(content) || metadata.as_ref().is_some_and(value_has_image);
            ToolResult {
                call_id,
                text,
                truncated,
                is_error: block.get("is_error").and_then(Value::as_bool) == Some(true)
                    || metadata.as_ref().is_some_and(tool_value_is_error),
                metadata,
                has_image,
            }
        })
        .collect()
}

fn matching_tool_result_metadata(
    metadata: Option<&Value>,
    call_id: Option<&str>,
    index: usize,
    result_count: usize,
) -> Option<Value> {
    match metadata {
        Some(Value::Array(items)) => call_id
            .and_then(|id| {
                items.iter().find(|item| {
                    string_field(item, &["tool_use_id", "toolUseId", "call_id", "id"]).as_deref()
                        == Some(id)
                })
            })
            .or_else(|| items.get(index))
            .cloned(),
        Some(value) if result_count == 1 => Some(value.clone()),
        _ => None,
    }
}

fn opencode_embedded_result(state: &Value) -> Option<ToolResult> {
    if !state.is_object() {
        return None;
    }
    let status = state.get("status").and_then(Value::as_str).unwrap_or("");
    let is_error = matches!(status, "error" | "failed" | "cancelled")
        || state.get("error").is_some_and(|value| !value.is_null());
    let output = if is_error {
        state
            .get("error")
            .filter(|value| value_has_display_text(value))
            .or_else(|| state.get("output"))
    } else {
        state
            .get("output")
            .filter(|value| value_has_display_text(value))
            .or_else(|| state.get("error"))
    }
    .unwrap_or(&Value::Null);
    let (text, truncated) = limited_output_text(output);
    let has_image = value_has_image(output);
    (!text.is_empty() || has_image || !status.is_empty()).then(|| ToolResult {
        call_id: None,
        text,
        truncated,
        is_error,
        metadata: None,
        has_image,
    })
}

fn codex_tool_call(payload: Option<&Value>, fallback_name: &str, input_key: &str) -> ToolCall {
    let payload = payload.unwrap_or(&Value::Null);
    let input = payload
        .get(input_key)
        .map(parse_embedded_json)
        .unwrap_or(Value::Null);
    ToolCall {
        id: string_field(payload, &["call_id", "id"]),
        name: payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(fallback_name)
            .to_string(),
        input,
        embedded_result: None,
    }
}

fn codex_tool_result(payload: Option<&Value>) -> ToolResult {
    let payload = payload.unwrap_or(&Value::Null);
    let output = payload
        .get("output")
        .or_else(|| payload.get("tools"))
        .unwrap_or(&Value::Null);
    let display = unwrap_tool_output(output);
    let (text, truncated) = limited_output_text(&display);
    ToolResult {
        call_id: string_field(payload, &["call_id", "id"]),
        text,
        truncated,
        is_error: tool_value_is_error(payload),
        metadata: None,
        has_image: value_has_image(&display),
    }
}

fn parse_patch_applied(raw: &Value) -> Segment {
    let payload = raw.get("payload").unwrap_or(&Value::Null);
    let changes = payload
        .get("changes")
        .and_then(Value::as_object)
        .map(|changes| {
            changes
                .iter()
                .filter_map(|(path, change)| patch_file_change(path, change))
                .collect()
        })
        .unwrap_or_default();
    let (stdout, stdout_truncated) =
        limited_output_text(payload.get("stdout").unwrap_or(&Value::Null));
    let (stderr, stderr_truncated) =
        limited_output_text(payload.get("stderr").unwrap_or(&Value::Null));
    Segment::PatchApplied(PatchApplied {
        call_id: string_field(payload, &["call_id", "id"]),
        success: payload.get("success").and_then(Value::as_bool) == Some(true),
        stdout,
        stderr,
        output_truncated: stdout_truncated || stderr_truncated,
        changes,
    })
}

fn patch_file_change(path: &str, change: &Value) -> Option<FileChange> {
    let kind = match change.get("type").and_then(Value::as_str)? {
        "update" if change.get("move_path").and_then(Value::as_str).is_some() => {
            FileChangeKind::Move
        }
        "update" => FileChangeKind::Update,
        "add" => FileChangeKind::Add,
        "delete" => FileChangeKind::Delete,
        "move" => FileChangeKind::Move,
        _ => return None,
    };
    let (diff, diff_truncated) = limit_export_text(
        change
            .get("unified_diff")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let (content, content_truncated) =
        limit_export_text(change.get("content").and_then(Value::as_str).unwrap_or(""));
    Some(FileChange {
        kind,
        path: path.to_string(),
        move_to: string_field(change, &["move_path", "movePath"]),
        diff,
        content,
        truncated: diff_truncated || content_truncated,
    })
}

fn parse_embedded_json(value: &Value) -> Value {
    match value {
        Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| value.clone()),
        _ => value.clone(),
    }
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn render_tool_chunks(segments: &[Segment], included: &[bool], cwd: &str) -> Vec<Option<String>> {
    let mut result_by_id: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    let mut patch_by_id: HashMap<String, Vec<usize>> = HashMap::new();
    for (position, segment) in segments.iter().enumerate() {
        for (index, result) in segment_tool_results(segment).iter().enumerate() {
            if let Some(id) = result.call_id.as_ref() {
                result_by_id
                    .entry(id.clone())
                    .or_default()
                    .push((position, index));
            }
        }
        if let Segment::PatchApplied(patch) = segment {
            if let Some(id) = patch.call_id.as_ref() {
                patch_by_id.entry(id.clone()).or_default().push(position);
            }
        }
    }

    let mut chunks = vec![Vec::<String>::new(); segments.len()];
    let mut consumed_results: HashSet<(usize, usize)> = HashSet::new();
    let mut consumed_patches: HashSet<usize> = HashSet::new();

    for (position, segment) in segments.iter().enumerate() {
        if !included[position] {
            continue;
        }
        for call in segment_tool_calls(segment) {
            let result_ref = matching_result(
                call,
                position,
                segments,
                included,
                &result_by_id,
                &consumed_results,
            );
            let patch_ref = matching_patch(
                call,
                position,
                segments,
                included,
                &patch_by_id,
                &consumed_patches,
            );
            let result = call.embedded_result.clone().or_else(|| {
                result_ref.map(|(result_position, result_index)| {
                    segments_tool_result(segments, result_position, result_index).clone()
                })
            });

            if let Some(patch_position) = patch_ref {
                consumed_patches.insert(patch_position);
                if let Some(reference) = result_ref {
                    consumed_results.insert(reference);
                }
                let Segment::PatchApplied(patch) = &segments[patch_position] else {
                    unreachable!();
                };
                if patch.success && !patch.changes.is_empty() {
                    let status = patch_status_text(patch, result.as_ref());
                    let mut changes = patch.changes.clone();
                    if patch.output_truncated
                        || result.as_ref().is_some_and(|result| result.truncated)
                    {
                        if let Some(change) = changes.first_mut() {
                            change.truncated = true;
                        }
                    }
                    chunks[position].push(render_file_changes(
                        &changes,
                        status.as_deref(),
                        false,
                        cwd,
                    ));
                    continue;
                }
                let patch_result = patch_event_result(patch, result.as_ref(), !patch.success);
                if patch.success {
                    let completed_call = ToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        input: Value::Null,
                        embedded_result: None,
                    };
                    chunks[position]
                        .push(render_generic_tool(&completed_call, Some(&patch_result)));
                } else {
                    chunks[position].push(render_generic_tool(call, Some(&patch_result)));
                }
                continue;
            }

            if let Some(result) = result.as_ref() {
                let changes = claude_file_changes(call, result);
                if !result.is_error && !changes.is_empty() {
                    if let Some(reference) = result_ref {
                        consumed_results.insert(reference);
                    }
                    let mut changes = changes;
                    if result.truncated {
                        if let Some(change) = changes.first_mut() {
                            change.truncated = true;
                        }
                    }
                    chunks[position].push(render_file_changes(
                        &changes,
                        (!result.text.is_empty()).then_some(result.text.as_str()),
                        false,
                        cwd,
                    ));
                    continue;
                }
            }

            if let Some(reference) = result_ref {
                consumed_results.insert(reference);
            }
            chunks[position].push(render_generic_tool(call, result.as_ref()));
        }
    }

    for (position, segment) in segments.iter().enumerate() {
        if !included[position] {
            continue;
        }
        for (index, result) in segment_tool_results(segment).iter().enumerate() {
            if !consumed_results.contains(&(position, index)) {
                chunks[position].push(render_standalone_result(result));
            }
        }
        if let Segment::PatchApplied(patch) = segment {
            if !consumed_patches.contains(&position) {
                if patch.success && !patch.changes.is_empty() {
                    let status = patch_status_text(patch, None);
                    let mut changes = patch.changes.clone();
                    if patch.output_truncated {
                        if let Some(change) = changes.first_mut() {
                            change.truncated = true;
                        }
                    }
                    chunks[position].push(render_file_changes(
                        &changes,
                        status.as_deref(),
                        false,
                        cwd,
                    ));
                } else {
                    chunks[position].push(render_standalone_result(&patch_event_result(
                        patch,
                        None,
                        !patch.success,
                    )));
                }
            }
        }
    }

    chunks
        .into_iter()
        .map(|parts| (!parts.is_empty()).then(|| parts.join("\n\n")))
        .collect()
}

fn segment_tool_calls(segment: &Segment) -> &[ToolCall] {
    match segment {
        Segment::Message { tool_calls, .. } | Segment::ToolCalls(tool_calls) => tool_calls,
        _ => &[],
    }
}

fn segment_tool_results(segment: &Segment) -> &[ToolResult] {
    match segment {
        Segment::Message { tool_results, .. } | Segment::ToolResults(tool_results) => tool_results,
        _ => &[],
    }
}

fn segments_tool_result(segments: &[Segment], position: usize, result_index: usize) -> &ToolResult {
    &segment_tool_results(&segments[position])[result_index]
}

fn matching_result(
    call: &ToolCall,
    call_position: usize,
    segments: &[Segment],
    included: &[bool],
    result_by_id: &HashMap<String, Vec<(usize, usize)>>,
    consumed: &HashSet<(usize, usize)>,
) -> Option<(usize, usize)> {
    if let Some(reference) = call.id.as_ref().and_then(|id| {
        result_by_id.get(id).and_then(|candidates| {
            candidates.iter().copied().find(|reference| {
                included[reference.0]
                    && !consumed.contains(reference)
                    && tool_event_is_in_call_scope(call_position, reference.0, segments)
            })
        })
    }) {
        return Some(reference);
    }

    for position in call_position..segments.len() {
        if position > call_position && matches!(segments[position], Segment::Message { .. }) {
            break;
        }
        if !included[position] {
            continue;
        }
        if let Some(index) = segment_tool_results(&segments[position])
            .iter()
            .enumerate()
            .find(|(index, result)| {
                result.call_id.is_none() && !consumed.contains(&(position, *index))
            })
            .map(|(index, _)| index)
        {
            return Some((position, index));
        }
    }
    None
}

fn matching_patch(
    call: &ToolCall,
    call_position: usize,
    segments: &[Segment],
    included: &[bool],
    patch_by_id: &HashMap<String, Vec<usize>>,
    consumed: &HashSet<usize>,
) -> Option<usize> {
    let id = call.id.as_ref()?;
    patch_by_id.get(id).and_then(|positions| {
        positions.iter().copied().find(|position| {
            included[*position]
                && !consumed.contains(position)
                && tool_event_is_in_call_scope(call_position, *position, segments)
        })
    })
}

fn tool_event_is_in_call_scope(
    call_position: usize,
    event_position: usize,
    segments: &[Segment],
) -> bool {
    if event_position < call_position {
        return false;
    }
    if event_position == call_position {
        return true;
    }
    !segments[call_position + 1..=event_position]
        .iter()
        .any(|segment| matches!(segment, Segment::Message { .. }))
}

fn patch_status_text(patch: &PatchApplied, result: Option<&ToolResult>) -> Option<String> {
    let mut parts = Vec::new();
    if !patch.stdout.trim().is_empty() {
        parts.push(patch.stdout.trim().to_string());
    }
    if !patch.stderr.trim().is_empty() {
        parts.push(patch.stderr.trim().to_string());
    }
    if parts.is_empty() {
        if let Some(result) = result.filter(|result| !result.text.trim().is_empty()) {
            parts.push(result.text.trim().to_string());
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn patch_event_result(
    patch: &PatchApplied,
    result: Option<&ToolResult>,
    is_error: bool,
) -> ToolResult {
    let text = patch_status_text(patch, result).unwrap_or_else(|| {
        if is_error {
            "补丁未应用".into()
        } else {
            "补丁处理完成".into()
        }
    });
    ToolResult {
        call_id: patch.call_id.clone(),
        text,
        truncated: patch.output_truncated || result.is_some_and(|result| result.truncated),
        is_error,
        metadata: None,
        has_image: false,
    }
}

fn claude_file_changes(call: &ToolCall, result: &ToolResult) -> Vec<FileChange> {
    let metadata = result.metadata.as_ref();
    let path = metadata
        .and_then(|value| string_field(value, &["filePath", "file_path", "path"]))
        .or_else(|| string_field(&call.input, &["file_path", "filePath", "path"]));
    let Some(path) = path else {
        return Vec::new();
    };

    if let Some((diff, truncated)) = metadata.and_then(structured_patch_diff) {
        return vec![FileChange {
            kind: FileChangeKind::Update,
            path,
            move_to: None,
            diff,
            content: String::new(),
            truncated,
        }];
    }

    let result_type = metadata
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if result_type == "create" {
        let content = metadata
            .and_then(|value| value.get("content"))
            .and_then(Value::as_str)
            .or_else(|| call.input.get("content").and_then(Value::as_str))
            .unwrap_or("");
        let (content, truncated) = limit_export_text(content);
        return vec![FileChange {
            kind: FileChangeKind::Add,
            path,
            move_to: None,
            diff: String::new(),
            content,
            truncated,
        }];
    }

    if call.name.eq_ignore_ascii_case("edit") {
        let old = string_field_value(&call.input, &["old_string", "oldString"]);
        let new = string_field_value(&call.input, &["new_string", "newString"]);
        if let (Some(old), Some(new)) = (old, new) {
            let fallback = format!(
                "{}\n{}",
                prefix_diff_lines('-', old),
                prefix_diff_lines('+', new)
            );
            let (diff, truncated) = limit_export_text(&fallback);
            return vec![FileChange {
                kind: FileChangeKind::Update,
                path,
                move_to: None,
                diff,
                content: String::new(),
                truncated,
            }];
        }
    }

    Vec::new()
}

fn structured_patch_diff(metadata: &Value) -> Option<(String, bool)> {
    let hunks = metadata.get("structuredPatch")?.as_array()?;
    if hunks.is_empty() {
        return None;
    }
    let mut out = String::new();
    for hunk in hunks {
        let old_start = hunk.get("oldStart").and_then(Value::as_i64)?;
        let old_lines = hunk.get("oldLines").and_then(Value::as_i64).unwrap_or(1);
        let new_start = hunk.get("newStart").and_then(Value::as_i64)?;
        let new_lines = hunk.get("newLines").and_then(Value::as_i64).unwrap_or(1);
        let lines = hunk.get("lines")?.as_array()?;
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "@@ -{old_start},{old_lines} +{new_start},{new_lines} @@\n"
        ));
        out.push_str(
            &lines
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    Some(limit_export_text(&out))
}

fn string_field_value<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
}

fn render_file_changes(
    changes: &[FileChange],
    status: Option<&str>,
    is_error: bool,
    cwd: &str,
) -> String {
    changes
        .iter()
        .enumerate()
        .map(|(index, change)| {
            render_file_change(
                change,
                (index == 0).then_some(status).flatten(),
                is_error,
                cwd,
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_file_change(
    change: &FileChange,
    status: Option<&str>,
    is_error: bool,
    cwd: &str,
) -> String {
    let path = display_path(&change.path, cwd);
    let move_to = change
        .move_to
        .as_deref()
        .map(|path| display_path(path, cwd));
    let action = match change.kind {
        FileChangeKind::Update => "修改",
        FileChangeKind::Add => "新增",
        FileChangeKind::Delete => "删除",
        FileChangeKind::Move => "移动",
    };
    let path_label = move_to
        .as_ref()
        .map(|target| format!("{path} → {target}"))
        .unwrap_or_else(|| path.clone());
    let mut diff = match change.kind {
        FileChangeKind::Update => {
            render_diff_with_headers(&change.diff, &format!("a/{path}"), &format!("b/{path}"))
        }
        FileChangeKind::Add => {
            let count = change.content.lines().count();
            format!(
                "--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{count} @@\n{}",
                prefix_diff_lines('+', &change.content)
            )
        }
        FileChangeKind::Delete => {
            let count = change.content.lines().count();
            format!(
                "--- a/{path}\n+++ /dev/null\n@@ -1,{count} +0,0 @@\n{}",
                prefix_diff_lines('-', &change.content)
            )
        }
        FileChangeKind::Move => {
            let target = move_to.as_deref().unwrap_or(&path);
            let mut rendered =
                format!("similarity index 100%\nrename from {path}\nrename to {target}");
            if !change.diff.trim().is_empty() {
                rendered = render_diff_with_headers(
                    &change.diff,
                    &format!("a/{path}"),
                    &format!("b/{target}"),
                );
            }
            rendered
        }
    };
    if diff.ends_with('\n') {
        diff.pop();
    }
    let mut out = format!(
        "<details open>\n<summary>📝 文件变更：{} · {action}</summary>\n\n{}",
        escape_html(&path_label),
        fenced_block("diff", &diff)
    );
    if change.truncated {
        out.push_str("\n\n_内容已截断，仅保留前 200 行或 8000 个字符。_");
    }
    if let Some(status) = status.filter(|status| !status.trim().is_empty()) {
        let icon = if is_error { "⚠️" } else { "↩️" };
        out.push_str(&format!(
            "\n\n> {icon} {}",
            status.trim().replace('\n', "\n> ")
        ));
    }
    out.push_str("\n\n</details>");
    out
}

fn render_diff_with_headers(diff: &str, old_path: &str, new_path: &str) -> String {
    let diff = diff.trim_end();
    let has_old_header = diff.lines().any(|line| line.starts_with("--- "));
    let has_new_header = diff.lines().any(|line| line.starts_with("+++ "));
    if has_old_header && has_new_header {
        diff.to_string()
    } else {
        format!("--- {old_path}\n+++ {new_path}\n{diff}")
    }
}

fn render_generic_tool(call: &ToolCall, result: Option<&ToolResult>) -> String {
    let summary = tool_label(&call.name, tool_input_detail(&call.input));
    let mut out = format!("<details>\n<summary>🔧 {}</summary>", escape_html(&summary));
    if !call.input.is_null() {
        out.push_str("\n\n**参数**\n\n");
        out.push_str(&render_value_block(&call.input));
    }
    if let Some(result) = result {
        let label = if result.is_error {
            "⚠️ 工具执行失败"
        } else {
            "↩️ 返回"
        };
        out.push_str(&format!("\n\n**{label}**"));
        if !result.text.trim().is_empty() {
            out.push_str("\n\n");
            out.push_str(&render_text_block(&result.text));
        }
        if result.has_image && !result.text.contains("图片数据已省略") {
            out.push_str("\n\n_图片结果已省略。_");
        }
        if result.truncated {
            out.push_str("\n\n_内容已截断，仅保留前 200 行或 8000 个字符。_");
        }
    }
    out.push_str("\n\n</details>");
    out
}

fn render_standalone_result(result: &ToolResult) -> String {
    let placeholder = ToolCall {
        id: result.call_id.clone(),
        name: "孤立工具结果".into(),
        input: Value::Null,
        embedded_result: None,
    };
    render_generic_tool(&placeholder, Some(result))
}

fn render_value_block(value: &Value) -> String {
    let sanitized = sanitize_tool_value(value, None);
    let (text, language) = match sanitized {
        Value::String(text) => (text, "text"),
        value => (
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
            "json",
        ),
    };
    let (text, truncated) = limit_block(&text);
    let mut out = fenced_block(language, &text);
    if truncated {
        out.push_str("\n\n_内容已截断，仅保留前 200 行或 8000 个字符。_");
    }
    out
}

fn render_text_block(text: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return render_value_block(&value);
    }
    fenced_block(
        if looks_like_unified_diff(text) {
            "diff"
        } else {
            "text"
        },
        text,
    )
}

fn display_path(path: &str, cwd: &str) -> String {
    if cwd.trim().is_empty() {
        return path.to_string();
    }
    Path::new(path)
        .strip_prefix(Path::new(cwd))
        .ok()
        .and_then(|relative| {
            let value = relative
                .to_string_lossy()
                .trim_start_matches(['/', '\\'])
                .to_string();
            (!value.is_empty()).then_some(value)
        })
        .unwrap_or_else(|| path.to_string())
}

fn prefix_diff_lines(prefix: char, text: &str) -> String {
    text.split('\n')
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn fenced_block(language: &str, content: &str) -> String {
    let fence = "`".repeat(longest_backtick_run(content).saturating_add(1).max(3));
    format!("{fence}{language}\n{}\n{fence}", content.trim_end())
}

fn longest_backtick_run(text: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for ch in text.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn limited_output_text(value: &Value) -> (String, bool) {
    let display = unwrap_tool_output(value);
    let sanitized = sanitize_tool_value(&display, None);
    let text = output_text(&sanitized);
    let text = strip_exec_preamble(&text).trim_matches(['\r', '\n']);
    let (limited, truncated) = limit_block(text);
    (redact_data_image_uris(&limited), truncated)
}

fn looks_like_unified_diff(text: &str) -> bool {
    let has_old = text.lines().any(|line| line.starts_with("--- "));
    let has_new = text.lines().any(|line| line.starts_with("+++ "));
    let has_hunk = text.lines().any(|line| valid_diff_hunk_header(line.trim()));
    (has_old && has_new && has_hunk) || (text.contains("diff --git ") && has_hunk)
}

fn valid_diff_hunk_header(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("@@ -") else {
        return false;
    };
    let Some((old, rest)) = rest.split_once(" +") else {
        return false;
    };
    let Some((new, _)) = rest.split_once(" @@") else {
        return false;
    };
    range_has_line_number(old) && range_has_line_number(new)
}

fn range_has_line_number(value: &str) -> bool {
    let head = value.split(',').next().unwrap_or("");
    !head.is_empty() && head.chars().all(|ch| ch.is_ascii_digit())
}

fn output_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .or_else(|| item.get("text").and_then(Value::as_str).map(str::to_string))
                    .or_else(|| {
                        item.get("output")
                            .filter(|value| !value.is_null())
                            .map(output_text)
                    })
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(object) => {
            for key in ["output", "stdout", "content", "text", "error"] {
                if let Some(value) = object.get(key).filter(|value| !value.is_null()) {
                    let text = output_text(value);
                    if !text.is_empty() {
                        return text;
                    }
                }
            }
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        }
        other => other.to_string(),
    }
}

fn value_has_display_text(value: &Value) -> bool {
    match value {
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => items.iter().any(value_has_display_text),
        Value::Object(object) => ["output", "stdout", "content", "text", "error"]
            .iter()
            .filter_map(|key| object.get(*key))
            .any(value_has_display_text),
        _ => false,
    }
}

fn unwrap_tool_output(value: &Value) -> Value {
    match value {
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(parsed @ (Value::Object(_) | Value::Array(_))) => unwrap_tool_output(&parsed),
            _ => value.clone(),
        },
        Value::Object(object) => object
            .get("output")
            .filter(|output| !output.is_null())
            .map(unwrap_tool_output)
            .unwrap_or_else(|| value.clone()),
        _ => value.clone(),
    }
}

fn strip_exec_preamble(text: &str) -> &str {
    text.split_once("Output:\n")
        .map(|(_, output)| output)
        .filter(|output| !output.trim().is_empty())
        .unwrap_or(text)
}

fn limit_block(text: &str) -> (String, bool) {
    let mut out = String::new();
    let mut chars = 0usize;
    let mut lines = 1usize;
    let mut truncated = false;
    for ch in text.chars() {
        if chars >= TOOL_BLOCK_MAX_CHARS || (ch == '\n' && lines >= TOOL_BLOCK_MAX_LINES) {
            truncated = true;
            break;
        }
        out.push(ch);
        chars += 1;
        if ch == '\n' {
            lines += 1;
        }
    }
    (out, truncated)
}

fn limit_export_text(text: &str) -> (String, bool) {
    limit_block(&redact_data_image_uris(text))
}

fn redact_data_image_uris(text: &str) -> String {
    const PREFIX: &str = "data:image/";
    let mut remaining = text;
    let mut out = String::new();
    while let Some(start) = remaining.find(PREFIX) {
        out.push_str(&remaining[..start]);
        let tail = &remaining[start..];
        let end = tail
            .char_indices()
            .find_map(|(index, ch)| {
                (index > 0 && (ch.is_whitespace() || matches!(ch, ')' | ']' | '}' | '"' | '\'')))
                    .then_some(index)
            })
            .unwrap_or(tail.len());
        out.push_str("（图片数据已省略）");
        remaining = &tail[end..];
    }
    out.push_str(remaining);
    out
}

fn sanitize_tool_value(value: &Value, key: Option<&str>) -> Value {
    match value {
        Value::String(text) => {
            let image_key = key.is_some_and(|key| {
                matches!(
                    key.to_ascii_lowercase().as_str(),
                    "base64" | "image_url" | "imageurl"
                )
            });
            if image_key || text.starts_with("data:image/") {
                Value::String("（图片数据已省略）".into())
            } else {
                Value::String(redact_data_image_uris(text))
            }
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| sanitize_tool_value(item, key))
                .collect(),
        ),
        Value::Object(object) => {
            let binary_block = matches!(
                object.get("type").and_then(Value::as_str),
                Some("image" | "input_image" | "base64")
            ) || object
                .get("media_type")
                .or_else(|| object.get("mime_type"))
                .and_then(Value::as_str)
                .is_some_and(|media_type| media_type.starts_with("image/"));
            if binary_block {
                Value::String("（图片数据已省略）".into())
            } else {
                Value::Object(
                    object
                        .iter()
                        .map(|(key, value)| (key.clone(), sanitize_tool_value(value, Some(key))))
                        .collect(),
                )
            }
        }
        _ => value.clone(),
    }
}

fn value_has_image(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains("data:image/"),
        Value::Array(items) => items.iter().any(value_has_image),
        Value::Object(object) => {
            matches!(
                object.get("type").and_then(Value::as_str),
                Some("image" | "input_image")
            ) || object.contains_key("image_url")
                || object.contains_key("imageUrl")
                || object.contains_key("base64")
                || object.values().any(value_has_image)
        }
        _ => false,
    }
}

fn tool_value_is_error(value: &Value) -> bool {
    if value.get("is_error").and_then(Value::as_bool) == Some(true)
        || matches!(
            value.get("status").and_then(Value::as_str),
            Some("error" | "failed" | "cancelled")
        )
        || tool_exit_code(value).is_some_and(|code| code != 0)
    {
        return true;
    }
    let raw = value
        .get("output")
        .or_else(|| value.get("tools"))
        .unwrap_or(value);
    match raw {
        Value::String(text) => {
            serde_json::from_str::<Value>(text)
                .ok()
                .is_some_and(|parsed| tool_value_is_error(&parsed))
                || [
                    "apply_patch verification failed",
                    "script failed",
                    "command failed",
                ]
                .iter()
                .any(|needle| text.to_ascii_lowercase().contains(needle))
        }
        _ if !std::ptr::eq(raw, value) => tool_value_is_error(raw),
        _ => false,
    }
}

fn tool_exit_code(value: &Value) -> Option<i64> {
    match value {
        Value::Object(object) => object
            .get("exit_code")
            .or_else(|| object.get("exitCode"))
            .and_then(Value::as_i64)
            .or_else(|| object.get("metadata").and_then(tool_exit_code))
            .or_else(|| object.get("output").and_then(tool_exit_code)),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .as_ref()
            .and_then(tool_exit_code),
        _ => None,
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
    fn structured_tool_blocks_include_parameters_results_and_summaries() {
        let events = vec![
            ev(0, user("run it")),
            ev(
                1,
                json!({"type": "response_item", "payload": {
                    "type": "function_call",
                    "call_id": "status",
                    "name": "shell",
                    "arguments": "{\"cmd\":[\"git\",\"status\",\"--short\"]}"
                }}),
            ),
            ev(
                2,
                json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "status", "output": json!({"output": " M src/main.rs\n?? notes.md\n", "metadata": {"exit_code": 0}}).to_string()}}),
            ),
            ev(
                3,
                json!({"type": "response_item", "payload": {"type": "local_shell_call", "id": "build", "action": {"type": "exec", "command": ["cargo", "build"]}}}),
            ),
            ev(
                4,
                json!({"type": "response_item", "payload": {"type": "local_shell_call_output", "call_id": "build", "output": "all 12 tests passed\nmore"}}),
            ),
            ev(
                5,
                json!({"type": "response_item", "payload": {"type": "custom_tool_call", "name": "apply_patch", "input": "*** Begin Patch\n*** Update File: a.rs"}}),
            ),
            ev(
                6,
                json!({
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [
                            {"type": "tool_use", "id": "bash", "name": "Bash", "input": {"command": "pnpm test", "description": "run tests"}},
                            {"type": "tool_use", "id": "edit", "name": "Edit", "input": {"file_path": "src/app.tsx"}}
                        ]
                    }
                }),
            ),
            ev(
                7,
                json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [
                            {"type": "tool_result", "tool_use_id": "edit", "content": "updated"},
                            {"type": "tool_result", "tool_use_id": "bash", "content": [{"type": "text", "text": "pnpm passed"}]}
                        ]
                    }
                }),
            ),
            ev(8, assistant("done")),
        ];
        let mut opts = default_options();
        opts.include_tools = true;

        let rendered = render_markdown(&events, &header(), &opts);

        for expected in [
            "<summary>🔧 shell: git status --short</summary>",
            "\"cmd\": [",
            "M src/main.rs",
            "<summary>🔧 shell: cargo build</summary>",
            "all 12 tests passed",
            "<summary>🔧 apply_patch</summary>",
            "*** Begin Patch",
            "<summary>🔧 Bash: pnpm test</summary>",
            "pnpm passed",
            "<summary>🔧 Edit: src/app.tsx</summary>",
            "updated",
        ] {
            assert!(
                rendered.markdown.contains(expected),
                "缺少 {expected}: {}",
                rendered.markdown
            );
        }
    }

    #[test]
    fn codex_patch_apply_end_is_the_authoritative_file_diff() {
        let events = vec![
            ev(0, user("change files")),
            ev(
                1,
                json!({"type": "response_item", "payload": {
                    "type": "custom_tool_call",
                    "call_id": "call_patch",
                    "name": "apply_patch",
                    "input": "*** Begin Patch\n*** Update File: guessed.rs\n@@\n-old\n+wrong\n*** End Patch"
                }}),
            ),
            ev(
                2,
                json!({"type": "event_msg", "payload": {
                    "type": "patch_apply_end",
                    "call_id": "call_patch",
                    "stdout": "Success. Updated the following files:\nM src/app.ts\nA notes.md\nD old.txt",
                    "stderr": "",
                    "success": true,
                    "changes": {
                        "src/app.ts": {
                            "type": "update",
                            "unified_diff": "@@ -2,2 +2,2 @@\n keep\n-old\n+new",
                            "move_path": null
                        },
                        "notes.md": {"type": "add", "content": "first\nsecond"},
                        "old.txt": {"type": "delete", "content": "gone"},
                        "before.ts": {
                            "type": "update",
                            "unified_diff": "",
                            "move_path": "after.ts"
                        }
                    }
                }}),
            ),
            ev(
                3,
                json!({"type": "response_item", "payload": {
                    "type": "custom_tool_call_output",
                    "call_id": "call_patch",
                    "output": "Done"
                }}),
            ),
            ev(4, assistant("done")),
        ];
        let mut opts = default_options();
        opts.include_tools = true;

        let markdown = render_markdown(&events, &header(), &opts).markdown;

        for expected in [
            "<details open>",
            "文件变更：src/app.ts · 修改",
            "--- a/src/app.ts",
            "+++ b/src/app.ts",
            "@@ -2,2 +2,2 @@",
            "文件变更：notes.md · 新增",
            "--- /dev/null",
            "+++ b/notes.md",
            "+first",
            "文件变更：old.txt · 删除",
            "--- a/old.txt",
            "+++ /dev/null",
            "-gone",
            "文件变更：before.ts → after.ts · 移动",
            "rename from before.ts",
            "rename to after.ts",
            "Success. Updated the following files",
        ] {
            assert!(markdown.contains(expected), "缺少 {expected}: {markdown}");
        }
        assert!(
            !markdown.contains("guessed.rs"),
            "不得把调用参数冒充真实 diff: {markdown}"
        );
    }

    #[test]
    fn codex_patch_preserves_existing_diff_headers() {
        let change = FileChange {
            kind: FileChangeKind::Update,
            path: "src/app.ts".into(),
            move_to: None,
            diff: "--- a/original.ts\n+++ b/original.ts\n@@ -1 +1 @@\n-old\n+new".into(),
            content: String::new(),
            truncated: false,
        };

        let markdown = render_file_change(&change, None, false, "");

        assert_eq!(markdown.matches("--- ").count(), 1);
        assert_eq!(markdown.matches("+++ ").count(), 1);
        assert!(markdown.contains("--- a/original.ts\n+++ b/original.ts"));
    }

    #[test]
    fn successful_codex_patch_without_changes_is_not_an_error() {
        let events = vec![
            ev(
                0,
                json!({"type": "response_item", "payload": {
                    "type": "custom_tool_call",
                    "call_id": "no_changes",
                    "name": "apply_patch",
                    "input": "*** Begin Patch\n*** Update File: should-not-render.rs\n*** End Patch"
                }}),
            ),
            ev(
                1,
                json!({"type": "event_msg", "payload": {
                    "type": "patch_apply_end",
                    "call_id": "no_changes",
                    "stdout": "Success. No files changed.",
                    "stderr": "",
                    "success": true,
                    "changes": {}
                }}),
            ),
            ev(2, assistant("done")),
        ];
        let mut opts = default_options();
        opts.include_tools = true;

        let markdown = render_markdown(&events, &header(), &opts).markdown;

        assert!(markdown.contains("Success. No files changed."));
        assert!(markdown.contains("↩️ 返回"));
        assert!(!markdown.contains("工具执行失败"));
        assert!(!markdown.contains("should-not-render.rs"));
    }

    #[test]
    fn failed_codex_patch_event_keeps_raw_patch_and_marks_error() {
        let events = vec![
            ev(
                0,
                json!({"type": "response_item", "payload": {
                    "type": "custom_tool_call",
                    "call_id": "failed_patch_event",
                    "name": "apply_patch",
                    "input": "*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch"
                }}),
            ),
            ev(
                1,
                json!({"type": "event_msg", "payload": {
                    "type": "patch_apply_end",
                    "call_id": "failed_patch_event",
                    "stdout": "",
                    "stderr": "apply_patch verification failed",
                    "success": false,
                    "changes": {}
                }}),
            ),
            ev(2, assistant("could not patch")),
        ];
        let mut opts = default_options();
        opts.include_tools = true;

        let markdown = render_markdown(&events, &header(), &opts).markdown;

        assert!(markdown.contains("*** Begin Patch"));
        assert!(markdown.contains("apply_patch verification failed"));
        assert!(markdown.contains("工具执行失败"));
        assert!(!markdown.contains("```diff\n*** Begin Patch"));
    }

    #[test]
    fn failed_codex_patch_falls_back_to_raw_patch_without_diff_label() {
        let events = vec![
            ev(
                0,
                json!({"type": "response_item", "payload": {
                    "type": "custom_tool_call",
                    "call_id": "call_failed",
                    "name": "apply_patch",
                    "input": "*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch"
                }}),
            ),
            ev(
                1,
                json!({"type": "response_item", "payload": {
                    "type": "custom_tool_call_output",
                    "call_id": "call_failed",
                    "output": {"exit_code": 1, "output": "apply_patch verification failed"}
                }}),
            ),
            ev(2, assistant("could not patch")),
        ];
        let mut opts = default_options();
        opts.include_tools = true;

        let markdown = render_markdown(&events, &header(), &opts).markdown;

        assert!(markdown.contains("*** Begin Patch"));
        assert!(markdown.contains("apply_patch verification failed"));
        assert!(markdown.contains("工具执行失败"));
        assert!(!markdown.contains("```diff\n*** Begin Patch"));
    }

    #[test]
    fn claude_structured_patch_wins_over_old_new_fallback() {
        let events = vec![
            ev(
                0,
                json!({"type": "assistant", "message": {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "tool_edit",
                    "name": "Edit",
                    "input": {"file_path": "src/app.ts", "old_string": "fallback old", "new_string": "fallback new"}
                }]}}),
            ),
            ev(
                1,
                json!({
                    "type": "user",
                    "message": {"role": "user", "content": [{
                        "type": "tool_result",
                        "tool_use_id": "tool_edit",
                        "content": "updated",
                        "is_error": false
                    }]},
                    "toolUseResult": {
                        "filePath": "src/app.ts",
                        "structuredPatch": [{
                            "oldStart": 10,
                            "oldLines": 2,
                            "newStart": 10,
                            "newLines": 2,
                            "lines": [" keep", "-actual old", "+actual new"]
                        }]
                    }
                }),
            ),
            ev(2, assistant("done")),
        ];
        let mut opts = default_options();
        opts.include_tools = true;

        let markdown = render_markdown(&events, &header(), &opts).markdown;

        assert!(markdown.contains("@@ -10,2 +10,2 @@"));
        assert!(markdown.contains("-actual old"));
        assert!(markdown.contains("+actual new"));
        assert!(!markdown.contains("fallback old"));
        assert!(!markdown.contains("fallback new"));
    }

    #[test]
    fn claude_edit_and_write_use_safe_file_change_fallbacks() {
        let events = vec![
            ev(
                0,
                json!({"type": "assistant", "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "edit", "name": "Edit", "input": {
                        "file_path": "src/edit.ts", "old_string": "old line", "new_string": "new line"
                    }},
                    {"type": "tool_use", "id": "write", "name": "Write", "input": {
                        "file_path": "src/new.ts", "content": "one\ntwo"
                    }}
                ]}}),
            ),
            ev(
                1,
                json!({
                    "type": "user",
                    "message": {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "write", "content": "created", "is_error": false},
                        {"type": "tool_result", "tool_use_id": "edit", "content": "updated", "is_error": false}
                    ]},
                    "toolUseResult": [
                        {"tool_use_id": "write", "type": "create", "filePath": "src/new.ts"},
                        {"tool_use_id": "edit", "type": "update", "filePath": "src/edit.ts"}
                    ]
                }),
            ),
            ev(2, assistant("done")),
        ];
        let mut opts = default_options();
        opts.include_tools = true;

        let markdown = render_markdown(&events, &header(), &opts).markdown;

        for expected in [
            "文件变更：src/edit.ts · 修改",
            "-old line",
            "+new line",
            "文件变更：src/new.ts · 新增",
            "+one",
            "+two",
        ] {
            assert!(markdown.contains(expected), "缺少 {expected}: {markdown}");
        }
    }

    #[test]
    fn opencode_embedded_tool_state_and_cursor_results_are_preserved() {
        let events = vec![
            ev(
                0,
                json!({
                    "type": "assistant",
                    "message": {"role": "assistant", "content": [{
                        "type": "tool_use",
                        "id": "open_1",
                        "name": "read",
                        "input": {"path": "README.md"},
                        "state": {"status": "completed", "output": "OpenCode output", "error": null}
                    }]},
                    "opencode": {"part_type": "tool"}
                }),
            ),
            ev(
                1,
                json!({"type": "assistant", "message": {"role": "assistant", "content": [{
                    "type": "tool_use", "id": "cursor_1", "name": "read_file_v2", "input": {"target_file": "a.rs"}
                }], "cursor": {"store": "composer"}}}),
            ),
            ev(
                2,
                json!({"type": "user", "message": {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "cursor_1",
                    "content": "{\"lines\":12,\"text\":\"Cursor output\"}",
                    "is_error": false
                }]}}),
            ),
            ev(3, assistant("done")),
        ];
        let mut opts = default_options();
        opts.include_tools = true;

        let markdown = render_markdown(&events, &header(), &opts).markdown;

        assert!(markdown.contains("OpenCode output"));
        assert!(markdown.contains("Cursor output"));
        assert_eq!(markdown.matches("OpenCode output").count(), 1);
    }

    #[test]
    fn opencode_failed_state_prefers_the_error_when_output_is_null() {
        let result = opencode_embedded_result(&json!({
            "status": "failed",
            "output": null,
            "error": "OpenCode failure details"
        }))
        .expect("failed state should produce a result");

        assert!(result.is_error);
        assert_eq!(result.text, "OpenCode failure details");
    }

    #[test]
    fn tool_blocks_use_safe_fences_truncate_and_omit_base64_images() {
        let long_output = format!(
            "before\n```\n{}\ndata:image/png;base64,{}",
            "line\n".repeat(260),
            "A".repeat(10_000)
        );
        let events = vec![
            ev(
                0,
                json!({"type": "response_item", "payload": {
                    "type": "function_call",
                    "call_id": "large",
                    "name": "shell",
                    "arguments": "{\"cmd\":[\"large\"]}"
                }}),
            ),
            ev(
                1,
                json!({"type": "response_item", "payload": {
                    "type": "function_call_output",
                    "call_id": "large",
                    "output": long_output
                }}),
            ),
            ev(2, assistant("done")),
        ];
        let mut opts = default_options();
        opts.include_tools = true;

        let markdown = render_markdown(&events, &header(), &opts).markdown;

        assert!(markdown.contains("````text\nbefore\n```"));
        assert!(markdown.contains("内容已截断"));
        assert!(!markdown.contains(&"A".repeat(1_000)));
    }

    #[test]
    fn matching_results_do_not_cross_message_boundaries() {
        let events = vec![
            ev(
                0,
                json!({"type": "response_item", "payload": {
                    "type": "function_call",
                    "call_id": "reused",
                    "name": "shell",
                    "arguments": "{\"cmd\":[\"first\"]}"
                }}),
            ),
            ev(1, assistant("first answer")),
            ev(
                2,
                json!({"type": "response_item", "payload": {
                    "type": "function_call_output",
                    "call_id": "reused",
                    "output": "later reply result"
                }}),
            ),
            ev(3, assistant("second answer")),
        ];
        let mut opts = default_options();
        opts.include_tools = true;
        opts.selected_indices = Some(vec![1, 3]);

        let markdown = render_markdown(&events, &header(), &opts).markdown;

        assert!(markdown.contains("<summary>🔧 shell: first</summary>"));
        assert!(markdown.contains("<summary>🔧 孤立工具结果</summary>"));
        assert_eq!(markdown.matches("later reply result").count(), 1);
    }

    #[test]
    fn standalone_tool_results_are_exported() {
        let events = vec![
            ev(
                0,
                json!({"type": "response_item", "payload": {
                    "type": "function_call_output",
                    "call_id": "missing_call",
                    "output": "orphan output"
                }}),
            ),
            ev(1, assistant("done")),
        ];
        let mut opts = default_options();
        opts.include_tools = true;

        let markdown = render_markdown(&events, &header(), &opts).markdown;

        assert!(markdown.contains("孤立工具结果"));
        assert!(markdown.contains("orphan output"));
    }

    #[test]
    fn only_complete_unified_diffs_use_the_diff_fence() {
        let unified = render_text_block("--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new");
        let bare_hunk = render_text_block("@@ -1 +1 @@\n-old\n+new");

        assert!(unified.starts_with("```diff\n"));
        assert!(bare_hunk.starts_with("```text\n"));
    }

    #[test]
    fn base64_at_the_start_of_tool_output_is_omitted_before_truncation() {
        let encoded = format!("data:image/png;base64,{}", "A".repeat(10_000));

        let (text, truncated) = limited_output_text(&Value::String(encoded));

        assert_eq!(text, "（图片数据已省略）");
        assert!(!truncated);

        let image_block = json!([{"type": "image", "data": "A".repeat(10_000)}]);
        let (text, truncated) = limited_output_text(&image_block);
        assert_eq!(text, "（图片数据已省略）");
        assert!(!truncated);
    }

    #[test]
    fn file_change_content_omits_base64_before_truncation() {
        let encoded = format!("data:image/png;base64,{}", "A".repeat(10_000));
        let change = patch_file_change("image.txt", &json!({"type": "add", "content": encoded}))
            .expect("add change should be parsed");

        assert_eq!(change.content, "（图片数据已省略）");
        assert!(!change.truncated);
        assert!(!render_file_change(&change, None, false, "").contains(&"A".repeat(1_000)));
    }

    #[test]
    fn exec_tool_output_blocks_are_preserved_after_the_output_marker() {
        let blocks = json!([
            {"type": "input_text", "text": "Script completed\nWall time 0.6 seconds\nOutput:\n"},
            {"type": "input_text", "text": "\r\nId ProcessName\r\n62872 v2rayN\r\n"}
        ]);
        assert_eq!(
            limited_output_text(&Value::String(blocks.to_string())),
            ("Id ProcessName\r\n62872 v2rayN".into(), false)
        );
        assert_eq!(
            limited_output_text(&blocks),
            ("Id ProcessName\r\n62872 v2rayN".into(), false)
        );

        let stats_only = json!([{"type": "input_text", "text": "Script completed\nWall time 0.6 seconds\nOutput:\n"}]);
        assert_eq!(
            limited_output_text(&Value::String(stats_only.to_string())),
            (
                "Script completed\nWall time 0.6 seconds\nOutput:".into(),
                false
            )
        );

        let plain = Value::String("plain first line\nsecond".into());
        assert_eq!(
            limited_output_text(&plain),
            ("plain first line\nsecond".into(), false)
        );
        assert_eq!(limited_output_text(&Value::Null), (String::new(), false));
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
