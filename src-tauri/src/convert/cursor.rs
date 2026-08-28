//! Cursor 会话到 Claude / Codex 两种写入形态的适配。
//!
//! Cursor 的会话不是行式文件，先由 [`crate::cursor_sessions`] 展开成 `PreviewEvent`，
//! 这里再归一成一份中立的会话表示，最后按目标各自包装：
//!
//! - 目标 Codex：包装成 [`ParsedClaudeSession`]，由 `native_codex_tool_call` 把
//!   Claude 工具名映射成 Codex 的 `shell_command` / `apply_patch` 等；
//! - 目标 Claude：包装成 [`ParsedCodexRollout`]，`native_tool_call` 认得 `Bash`
//!   `Read` `Edit` 这些名字（大小写不敏感），会原样保留。
//!
//! 也就是说 Cursor 侧只需要维护**一张**到 Claude 工具名的映射表，两个方向共用。
//!
//! 与既有的两个转换方向一致：thinking 一律丢弃并计入 `dropped_reasoning`，
//! 不重放工具，只搬运已经完成的调用与其输出。

use serde_json::{json, Value};

use super::*;

/// Cursor 会话的中立表示。
pub(super) struct ParsedCursorSession {
    pub source_id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub git_branch: Option<String>,
    pub messages: Vec<ConvMessage>,
    pub items: Vec<CursorItem>,
    pub stats: ExtractStats,
}

pub(super) enum CursorItem {
    Message(ConvMessage),
    Tool(CursorTool),
}

pub(super) struct CursorTool {
    /// 已经映射成 Claude 工具名。
    name: String,
    input: Value,
    /// 调用与输出在 Cursor 里同属一个气泡，所以这里一定是配对好的。
    output: String,
    is_error: bool,
    timestamp: Option<String>,
}

/// 读取一个 Cursor 会话并归一化。
pub(super) fn parse(
    cursor_dir: &Path,
    agent_dir: &Path,
    locator: &str,
) -> AppResult<ParsedCursorSession> {
    let summary = crate::cursor_sessions::list_sessions(cursor_dir, agent_dir)?
        .into_iter()
        .find(|session| session.rollout_path == locator)
        .ok_or_else(|| AppError::NotFound("Cursor 会话不存在或已被删除".into()))?;

    let mut out = ParsedCursorSession {
        source_id: summary.id.clone(),
        cwd: Some(summary.cwd.clone()).filter(|cwd| !cwd.trim().is_empty()),
        title: Some(summary.title.clone()).filter(|title| !title.trim().is_empty()),
        model: crate::cursor_sessions::preview_meta(locator)
            .ok()
            .and_then(|meta| meta.model_provider),
        git_branch: summary.git_branch.clone(),
        messages: Vec::new(),
        items: Vec::new(),
        stats: ExtractStats::default(),
    };

    absorb(
        &mut out,
        crate::cursor_sessions::load_preview_events_from_locator(locator)?,
    );
    Ok(out)
}

/// 把展开好的事件序列归一成中立形态。
fn absorb(out: &mut ParsedCursorSession, events: Vec<crate::models::PreviewEvent>) {
    // 工具调用先攒着：Cursor 的输出跟调用相邻，但中间可能夹着别的事件。
    let mut pending: Option<(String, String, Value, Option<String>)> = None;
    for event in events {
        let timestamp = Some(event.timestamp.clone()).filter(|value| !value.is_empty());
        match event.role.as_str() {
            "user" | "assistant" => {
                flush_unpaired(out, &mut pending);
                let text = event_text(&event.raw);
                if text.trim().is_empty() {
                    continue;
                }
                let role = if event.role == "user" {
                    Role::User
                } else {
                    Role::Assistant
                };
                let message = ConvMessage {
                    role,
                    text,
                    timestamp,
                    phase: None,
                    images: Vec::new(),
                };
                out.messages.push(message.clone());
                out.items.push(CursorItem::Message(message));
            }
            // 与 Claude→Codex、Codex→Claude 两个方向保持一致：推理内容不迁移。
            "reasoning" => {
                flush_unpaired(out, &mut pending);
                out.stats.dropped_reasoning += 1;
            }
            "tool_call" => {
                flush_unpaired(out, &mut pending);
                let Some(block) = content_block(&event.raw) else {
                    continue;
                };
                let raw_name = block.get("name").and_then(Value::as_str).unwrap_or("");
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                pending = Some((
                    id,
                    claude_tool_name(raw_name),
                    block.get("input").cloned().unwrap_or_else(|| json!({})),
                    timestamp,
                ));
            }
            "tool_result" => {
                let Some(block) = content_block(&event.raw) else {
                    continue;
                };
                let Some((id, name, input, call_ts)) = pending.take() else {
                    // 没有配对调用的输出无法还原成工具对，只能丢弃。
                    out.stats.tool_notes += 1;
                    continue;
                };
                let result_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !id.is_empty() && !result_id.is_empty() && id != result_id {
                    // 顺序被打乱时宁可丢掉这一对，也不要把输出接到别的调用上。
                    out.stats.tool_notes += 1;
                    continue;
                }
                out.stats.tool_notes += 1;
                out.items.push(CursorItem::Tool(CursorTool {
                    name,
                    input,
                    output: block
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    is_error: block.get("is_error").and_then(Value::as_bool) == Some(true),
                    timestamp: call_ts.or(timestamp),
                }));
            }
            _ => {}
        }
    }
    flush_unpaired(out, &mut pending);

    // 首条用户消息之前的内容不构成一轮对话，与另外两个方向的处理一致。
    if let Some(first) = out.messages.iter().position(|m| m.role == Role::User) {
        out.messages.drain(..first);
    }
    if let Some(first) = out
        .items
        .iter()
        .position(|item| matches!(item, CursorItem::Message(message) if message.role == Role::User))
    {
        out.items.drain(..first);
    }
}

/// 调用没等到输出（会话被中断）时只保留调用本身，输出留空。
fn flush_unpaired(
    out: &mut ParsedCursorSession,
    pending: &mut Option<(String, String, Value, Option<String>)>,
) {
    let Some((_, name, input, timestamp)) = pending.take() else {
        return;
    };
    out.stats.tool_notes += 1;
    out.items.push(CursorItem::Tool(CursorTool {
        name,
        input,
        output: String::new(),
        is_error: false,
        timestamp,
    }));
}

fn content_block(raw: &Value) -> Option<&Value> {
    raw.get("message")?.get("content")?.as_array()?.first()
}

fn event_text(raw: &Value) -> String {
    let Some(blocks) = raw.get("message").and_then(|m| m.get("content")) else {
        return String::new();
    };
    blocks
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Cursor 工具名 → Claude 工具名。
///
/// 两个目标方向共用这一张表：写 Codex 时由 `native_codex_tool_call` 继续映射成
/// Codex 名字，写 Claude 时 `native_tool_call` 会原样保留这些名字。
/// 表里没有的（含 `mcp_*` 这类 MCP 工具）原样透传，由下游做字符清洗。
fn claude_tool_name(raw: &str) -> String {
    let name = match raw {
        // Ⓐ IDE Composer 的工具集
        "run_terminal_command_v2" | "run_terminal_cmd" => "Bash",
        "edit_file_v2" | "StrReplace" => "Edit",
        "read_file_v2" => "Read",
        "ripgrep_raw_search" => "Grep",
        "glob_file_search" => "Glob",
        "todo_write" => "TodoWrite",
        "web_search" => "WebSearch",
        "ask_question" => "AskUserQuestion",
        "task_v2" => "Task",
        "await" => "TaskOutput",
        // Ⓒ cursor-agent 已经在用 Claude 的名字，只有少数几个不同
        "Shell" => "Bash",
        "Delete" => "Bash",
        other => other,
    };
    if name.trim().is_empty() {
        "unknown".to_string()
    } else {
        name.to_string()
    }
}

// ---------------------------------------------------------------------------
// 目标形态包装
// ---------------------------------------------------------------------------

/// 工具调用 id 的种子。带上会话 id 与序号，保证同一会话内互不相同、跨会话也不撞。
fn tool_seed(parsed: &ParsedCursorSession, index: usize, tool: &CursorTool) -> String {
    format!("{}:{index}:{}", parsed.source_id, tool.name)
}

/// 包装成 Claude 事件流，供 `write_codex_session` 使用。
pub(super) fn as_claude(parsed: &ParsedCursorSession) -> ParsedClaudeSession {
    let mut events = Vec::new();
    // 种子里带上序号：同一个会话里重复执行同一条命令是常态，只用名字和输出做种
    // 会让两次调用拿到同一个 id，配对时就会张冠李戴。
    for (index, item) in parsed.items.iter().enumerate() {
        match item {
            CursorItem::Message(message) => events.push(ClaudeEvent::Message(message.clone())),
            CursorItem::Tool(tool) => {
                let id = claude_native_id("toolu_", &tool_seed(parsed, index, tool));
                events.push(ClaudeEvent::ToolCall(ClaudeToolEvent {
                    tool_use_id: Some(id.clone()),
                    payload: json!({
                        "type": "tool_use",
                        "id": id,
                        "name": tool.name,
                        "input": tool.input,
                    }),
                    timestamp: tool.timestamp.clone(),
                }));
                events.push(ClaudeEvent::ToolResult(ClaudeToolEvent {
                    tool_use_id: Some(id.clone()),
                    payload: json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": truncate_chars(&tool.output, NATIVE_TOOL_RESULT_MAX_LEN),
                        "is_error": tool.is_error,
                    }),
                    timestamp: tool.timestamp.clone(),
                }));
            }
        }
    }
    classify_claude_assistant_phases(&mut events);
    ParsedClaudeSession {
        source_id: Some(parsed.source_id.clone()),
        cwd: parsed.cwd.clone(),
        title: parsed.title.clone(),
        messages: parsed.messages.clone(),
        events,
        stats: ExtractStats {
            dropped_reasoning: parsed.stats.dropped_reasoning,
            tool_notes: parsed.stats.tool_notes,
        },
    }
}

/// 包装成 Codex 事件流，供 `write_claude_session` 使用。
///
/// 工具名保持 Claude 侧的取值：`native_tool_call` 对 `bash` / `read` / `edit` 等
/// 是大小写不敏感匹配，未收录的名字也会原样透传，因此不需要再绕一次 Codex 名字。
pub(super) fn as_codex(parsed: &ParsedCursorSession) -> ParsedCodexRollout {
    let mut events = Vec::new();
    let mut messages = Vec::new();
    for (index, item) in parsed.items.iter().enumerate() {
        match item {
            CursorItem::Message(message) => {
                messages.push(message.clone());
                events.push(CodexEvent::Message(message.clone()));
            }
            CursorItem::Tool(tool) => {
                let call_id = codex_native_id("call_", &tool_seed(parsed, index, tool));
                // 工具事件在简洁模式下不参与对话，与 Codex 侧一样标成 commentary。
                let note = ConvMessage {
                    role: Role::Assistant,
                    text: format!("[tool_call: {}]", tool.name),
                    timestamp: tool.timestamp.clone(),
                    phase: Some("commentary".into()),
                    images: Vec::new(),
                };
                messages.push(note);
                events.push(CodexEvent::ToolCall(CodexToolEvent {
                    call_id: Some(call_id.clone()),
                    payload: json!({
                        "type": "function_call",
                        "name": tool.name,
                        "arguments": tool.input.to_string(),
                        "call_id": call_id,
                    }),
                    timestamp: tool.timestamp.clone(),
                }));
                events.push(CodexEvent::ToolResult(CodexToolEvent {
                    call_id: Some(call_id.clone()),
                    payload: json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": truncate_chars(&tool.output, NATIVE_TOOL_RESULT_MAX_LEN),
                        "is_error": tool.is_error,
                    }),
                    timestamp: tool.timestamp.clone(),
                }));
            }
        }
    }
    classify_codex_turn_phases(&mut messages);
    ParsedCodexRollout {
        source_id: Some(parsed.source_id.clone()),
        cwd: parsed.cwd.clone(),
        git_branch: parsed.git_branch.clone(),
        model: parsed.model.clone(),
        title: parsed.title.clone(),
        messages,
        events,
        stats: ExtractStats {
            dropped_reasoning: parsed.stats.dropped_reasoning,
            tool_notes: parsed.stats.tool_notes,
        },
    }
}

/// 一轮里最后一条 assistant 是最终答复，其余是过程回复。
///
/// Claude 的简洁模式只保留用户提问和每轮的 `final_answer`，没有这一步的话
/// 整个会话会退化成只剩提问。
fn classify_codex_turn_phases(messages: &mut [ConvMessage]) {
    let mut last_assistant: Option<usize> = None;
    for index in 0..messages.len() {
        match messages[index].role {
            Role::User => {
                if let Some(previous) = last_assistant.take() {
                    messages[previous].phase = Some("final_answer".into());
                }
            }
            Role::Assistant => {
                if messages[index].phase.is_none() {
                    messages[index].phase = Some("commentary".into());
                    last_assistant = Some(index);
                }
            }
        }
    }
    if let Some(previous) = last_assistant {
        messages[previous].phase = Some("final_answer".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::PreviewEvent;

    fn event(index: usize, role: &str, message: Value) -> PreviewEvent {
        PreviewEvent {
            index,
            timestamp: format!("2026-08-01T00:00:{:02}.000Z", index),
            role: role.into(),
            kind: role.into(),
            text_summary: String::new(),
            raw: json!({ "message": message }),
        }
    }

    /// 事件序列 → 中立形态。直接复用 `cursor_sessions` 产出的事件形状。
    fn normalize(events: Vec<PreviewEvent>) -> ParsedCursorSession {
        let mut out = ParsedCursorSession {
            source_id: "cursor-src".into(),
            cwd: Some("/work/demo".into()),
            title: Some("演示会话".into()),
            model: Some("claude-opus-5".into()),
            git_branch: Some("main".into()),
            messages: Vec::new(),
            items: Vec::new(),
            stats: ExtractStats::default(),
        };
        absorb(&mut out, events);
        out
    }

    #[test]
    fn cursor_tool_names_map_to_claude_and_pass_unknown_ones_through() {
        assert_eq!(claude_tool_name("run_terminal_command_v2"), "Bash");
        assert_eq!(claude_tool_name("edit_file_v2"), "Edit");
        assert_eq!(claude_tool_name("read_file_v2"), "Read");
        assert_eq!(claude_tool_name("ripgrep_raw_search"), "Grep");
        assert_eq!(claude_tool_name("glob_file_search"), "Glob");
        assert_eq!(claude_tool_name("todo_write"), "TodoWrite");
        assert_eq!(claude_tool_name("ask_question"), "AskUserQuestion");
        assert_eq!(claude_tool_name("await"), "TaskOutput");
        // cursor-agent 侧已经在用 Claude 的名字。
        assert_eq!(claude_tool_name("Shell"), "Bash");
        assert_eq!(claude_tool_name("Read"), "Read");
        // MCP 工具原样透传。
        assert_eq!(
            claude_tool_name("mcp_cloudfirewall_get_alerts"),
            "mcp_cloudfirewall_get_alerts"
        );
        assert_eq!(claude_tool_name(""), "unknown");
    }

    #[test]
    fn tool_calls_and_their_output_are_normalized_into_one_item() {
        let parsed = normalize(vec![
            event(
                0,
                "user",
                json!({"role": "user", "content": [{"type": "text", "text": "看下目录"}]}),
            ),
            event(
                1,
                "reasoning",
                json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "先列目录"}]}),
            ),
            event(
                2,
                "assistant",
                json!({"role": "assistant", "content": [{"type": "text", "text": "好的"}]}),
            ),
            event(
                3,
                "tool_call",
                json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "run_terminal_command_v2", "input": {"command": "ls"}}
                ]}),
            ),
            event(
                4,
                "tool_result",
                json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "a.txt", "is_error": false}
                ]}),
            ),
        ]);

        // 推理不迁移，但要如实计数。
        assert_eq!(parsed.stats.dropped_reasoning, 1);
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.items.len(), 3);
        let CursorItem::Tool(tool) = &parsed.items[2] else {
            panic!("第三项应当是工具调用");
        };
        assert_eq!(tool.name, "Bash");
        assert_eq!(tool.output, "a.txt");
        assert!(!tool.is_error);
    }

    /// 会话被中断时只有调用没有输出，仍要保留调用本身。
    #[test]
    fn an_unfinished_tool_call_is_kept_without_output() {
        let parsed = normalize(vec![
            event(
                0,
                "user",
                json!({"role": "user", "content": [{"type": "text", "text": "跑一下"}]}),
            ),
            event(
                1,
                "tool_call",
                json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "read_file_v2", "input": {"target_file": "a.rs"}}
                ]}),
            ),
        ]);
        let CursorItem::Tool(tool) = parsed.items.last().unwrap() else {
            panic!("末项应当是工具调用");
        };
        assert_eq!(tool.name, "Read");
        assert!(tool.output.is_empty());
    }

    /// 输出的 id 对不上调用时宁可丢弃，也不能把结果接到别的调用上。
    #[test]
    fn a_mismatched_tool_result_is_dropped_instead_of_being_attached() {
        let parsed = normalize(vec![
            event(
                0,
                "user",
                json!({"role": "user", "content": [{"type": "text", "text": "跑一下"}]}),
            ),
            event(
                1,
                "tool_call",
                json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Grep", "input": {}}
                ]}),
            ),
            event(
                2,
                "tool_result",
                json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "OTHER", "content": "x"}
                ]}),
            ),
        ]);
        assert!(parsed
            .items
            .iter()
            .all(|item| !matches!(item, CursorItem::Tool(tool) if !tool.output.is_empty())));
    }

    #[test]
    fn the_claude_shape_emits_paired_tool_use_and_tool_result_blocks() {
        let parsed = normalize(vec![
            event(
                0,
                "user",
                json!({"role": "user", "content": [{"type": "text", "text": "看下目录"}]}),
            ),
            event(
                1,
                "tool_call",
                json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "run_terminal_command_v2", "input": {"command": "ls"}}
                ]}),
            ),
            event(
                2,
                "tool_result",
                json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "a.txt"}
                ]}),
            ),
        ]);
        let claude = as_claude(&parsed);
        let mut call_id = None;
        let mut result_id = None;
        for e in &claude.events {
            match e {
                ClaudeEvent::ToolCall(call) => {
                    assert_eq!(call.payload["name"], "Bash");
                    call_id = call.tool_use_id.clone();
                }
                ClaudeEvent::ToolResult(result) => result_id = result.tool_use_id.clone(),
                ClaudeEvent::Message(_) => {}
            }
        }
        assert!(call_id.is_some());
        assert_eq!(call_id, result_id, "工具调用与输出必须共用同一个 id");
    }

    #[test]
    fn the_codex_shape_emits_paired_function_call_items() {
        let parsed = normalize(vec![
            event(
                0,
                "user",
                json!({"role": "user", "content": [{"type": "text", "text": "看下目录"}]}),
            ),
            event(
                1,
                "tool_call",
                json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "run_terminal_command_v2", "input": {"command": "ls"}}
                ]}),
            ),
            event(
                2,
                "tool_result",
                json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "a.txt"}
                ]}),
            ),
        ]);
        let codex = as_codex(&parsed);
        assert_eq!(codex.git_branch.as_deref(), Some("main"));
        assert_eq!(codex.model.as_deref(), Some("claude-opus-5"));
        let calls = codex
            .events
            .iter()
            .filter_map(|e| match e {
                CodexEvent::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].payload["type"], "function_call");
        // 工具名保持 Claude 侧取值，native_tool_call 会原样识别。
        assert_eq!(calls[0].payload["name"], "Bash");
        let outputs = codex
            .events
            .iter()
            .filter_map(|e| match e {
                CodexEvent::ToolResult(result) => result.call_id.clone(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(outputs, vec![calls[0].call_id.clone().unwrap()]);
    }

    /// 同一条命令跑两次不能拿到同一个工具 id，否则配对会张冠李戴。
    #[test]
    fn repeated_identical_tool_calls_get_distinct_ids() {
        let repeat = |index: usize| {
            vec![
                event(
                    index,
                    "tool_call",
                    json!({"role": "assistant", "content": [
                        {"type": "tool_use", "id": format!("t{index}"), "name": "Shell", "input": {"command": "ls"}}
                    ]}),
                ),
                event(
                    index + 1,
                    "tool_result",
                    json!({"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": format!("t{index}"), "content": "same"}
                    ]}),
                ),
            ]
        };
        let mut events = vec![event(
            0,
            "user",
            json!({"role": "user", "content": [{"type": "text", "text": "跑两次"}]}),
        )];
        events.extend(repeat(1));
        events.extend(repeat(3));

        let claude = as_claude(&normalize(events));
        let ids = claude
            .events
            .iter()
            .filter_map(|e| match e {
                ClaudeEvent::ToolCall(call) => call.tool_use_id.clone(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn turn_phases_mark_the_last_assistant_of_each_turn_as_the_final_answer() {
        let mut messages = vec![
            ConvMessage {
                role: Role::User,
                text: "问题一".into(),
                timestamp: None,
                phase: None,
                images: Vec::new(),
            },
            ConvMessage {
                role: Role::Assistant,
                text: "过程".into(),
                timestamp: None,
                phase: None,
                images: Vec::new(),
            },
            ConvMessage {
                role: Role::Assistant,
                text: "答复一".into(),
                timestamp: None,
                phase: None,
                images: Vec::new(),
            },
            ConvMessage {
                role: Role::User,
                text: "问题二".into(),
                timestamp: None,
                phase: None,
                images: Vec::new(),
            },
            ConvMessage {
                role: Role::Assistant,
                text: "答复二".into(),
                timestamp: None,
                phase: None,
                images: Vec::new(),
            },
        ];
        classify_codex_turn_phases(&mut messages);
        let phases = messages
            .iter()
            .map(|m| m.phase.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(
            phases,
            vec![
                None,
                Some("commentary"),
                Some("final_answer"),
                None,
                Some("final_answer")
            ]
        );
    }
}
