//! Claude Code JSONL 与 Codex rollout 互转。
//!
//! 仅迁移可见对话，不迁移推理状态。无法配对的工具事件转为文本注记。
//! 原生模式只写入已完成且配对的工具事件，不会重新执行工具。
//! 生成记录不写转换来源，解析时仍会过滤旧版来源标记。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::error::{AppError, AppResult};
use crate::models::ConvertReport;
use crate::{atomic_file, family, fs_ops, paths, repair, state_db};

/// 降级工具注记不包含转换来源。
const TOOL_CALL_TAG: &str = "tool_call";
const TOOL_RESULT_TAG: &str = "tool_result";
const NOTE_MAX_LEN: usize = 2_000;
const TOOL_RESULT_MAX_LEN: usize = 4_000;
const NATIVE_TOOL_RESULT_MAX_LEN: usize = 30_000;
const LEGACY_IMPORTED_MARKER: &str = "<EXTERNAL SESSION IMPORTED>";
/// Codex 要求 `SessionMeta.cli_version` 为字符串。没有样本时写入占位版本。
const FALLBACK_CODEX_CLI_VERSION: &str = "0.0.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
struct ConvMessage {
    role: Role,
    text: String,
    /// RFC3339 时间戳；缺失时由写入方用上一条消息或转换时刻兜底。
    timestamp: Option<String>,
    /// Codex assistant 消息阶段，用于区分过程回复和最终答复。
    phase: Option<String>,
    /// Codex 用户消息内可直接迁移到 Claude content block 的内嵌图片。
    images: Vec<ImportedImage>,
}

#[derive(Debug, Clone)]
struct ImportedImage {
    media_type: String,
    data: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeImportMode {
    Simple,
    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexImportMode {
    Simple,
    Native,
}

impl CodexImportMode {
    fn parse(value: Option<&str>) -> AppResult<Self> {
        match value.unwrap_or("simple") {
            "simple" => Ok(Self::Simple),
            "native" => Ok(Self::Native),
            other => Err(AppError::Other(format!(
                "不支持的 Claude → Codex 转换模式: {other}"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Native => "native",
        }
    }
}

impl ClaudeImportMode {
    fn parse(value: Option<&str>) -> AppResult<Self> {
        match value.unwrap_or("simple") {
            "simple" => Ok(Self::Simple),
            "native" => Ok(Self::Native),
            other => Err(AppError::Other(format!(
                "不支持的 Codex → Claude 转换模式: {other}"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Native => "native",
        }
    }
}

#[derive(Debug, Clone)]
struct CodexIdentity {
    originator: String,
    cli_version: Option<String>,
    source: String,
}

impl Default for CodexIdentity {
    fn default() -> Self {
        Self {
            originator: "Codex Desktop".into(),
            cli_version: None,
            source: "vscode".into(),
        }
    }
}

#[derive(Debug, Clone)]
struct ClaudeIdentity {
    model: String,
    version: Option<String>,
}

impl Default for ClaudeIdentity {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-5".into(),
            version: None,
        }
    }
}

#[derive(Debug, Default)]
struct ExtractStats {
    dropped_reasoning: u32,
    tool_notes: u32,
}

pub fn convert_session_with_lock(
    codex_dir: String,
    claude_dir: String,
    source_provider: String,
    rollout_path: String,
    conversion_mode: Option<String>,
    lock: &family::FamilyLock,
) -> AppResult<ConvertReport> {
    family::with_lock(lock, |_g| match source_provider.as_str() {
        "claude" => convert_claude_to_codex(
            &codex_dir,
            &rollout_path,
            CodexImportMode::parse(conversion_mode.as_deref())?,
        ),
        "codex" => convert_codex_to_claude(
            &codex_dir,
            &claude_dir,
            &rollout_path,
            ClaudeImportMode::parse(conversion_mode.as_deref())?,
        ),
        other => Err(AppError::Other(format!(
            "不支持的转换来源 provider: {other}"
        ))),
    })
}

// ---------------------------------------------------------------------------
// Claude → Codex
// ---------------------------------------------------------------------------

fn convert_claude_to_codex(
    codex_dir: &str,
    source_path: &str,
    mode: CodexImportMode,
) -> AppResult<ConvertReport> {
    let codex = PathBuf::from(codex_dir);
    let source = PathBuf::from(source_path);
    let parsed = parse_claude_session(&source)?;
    let Some(cwd) = parsed.cwd.clone() else {
        return Err(AppError::Other(
            "源会话缺少 cwd，无法确定 Codex 项目目录".into(),
        ));
    };
    if parsed.messages.iter().all(|m| m.role != Role::User) {
        return Err(AppError::Other(
            "源会话没有可迁移的用户消息（thinking、工具和元数据不计入）".into(),
        ));
    }

    let new_id = repair::new_session_id();
    let now = chrono::Utc::now();
    let new_abs = repair::build_clone_path(&codex, &new_id, &now);
    repair::validate_rollout_filename(&new_abs)?;
    let provider = repair::effective_current_provider(&codex)?;
    let identity = detect_codex_identity(&codex);
    let built = build_codex_lines(&new_id, &cwd, &provider, &parsed, &identity, &now, mode);
    let imported_messages = parsed.messages.len() as u32;

    if let Some(parent) = new_abs.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_file::create_with_writer_if_absent(&new_abs, |out| {
        for line in &built.lines {
            writeln!(out, "{line}")?;
        }
        Ok(())
    })?;

    // 依次更新 threads、session_index 和 workspace roots。
    // 没有 state 库时只写 rollout，CLI 仍可恢复。
    let mut warnings: Vec<String> = Vec::new();
    if mode == CodexImportMode::Native {
        warnings.push(
            "原生Codex（实验）依赖当前 Codex 会话格式。如果会话无法恢复，请改用简洁续聊。".into(),
        );
        if built.degraded_tool_events > 0 {
            warnings.push(format!(
                "{} 条工具事件无法配对，已转为文本注记",
                built.degraded_tool_events
            ));
        }
    }
    if paths::state_db_path(&codex).is_file() {
        let state = state_db::open(&codex)?;
        if let Err(error) = repair::upsert_thread_from_rollout(&codex, &state, &new_abs, false)
            .and_then(|ok| {
                if ok {
                    Ok(())
                } else {
                    Err(AppError::Other("rollout 缺少有效 session_meta.id".into()))
                }
            })
        {
            cleanup_failed_import(&new_abs);
            return Err(AppError::Other(format!("同步 threads 失败: {error}")));
        }
        if let Some(title) = parsed.title.as_deref() {
            let _ = state.execute(
                "UPDATE threads SET title = ?1 WHERE id = ?2",
                rusqlite::params![title, new_id],
            );
        }
    } else {
        warnings.push("Codex App 会话列表未同步：未找到 state_5.sqlite".into());
    }
    if let Err(error) = repair::append_index_line(
        &codex,
        &new_id,
        &truncate_chars(first_user_preview(&parsed.messages), 200),
        &new_abs,
    ) {
        warnings.push(format!("写入 session_index 失败: {error}"));
    }
    if let Err(error) = repair::ensure_workspace_root_registered(&codex, &cwd) {
        warnings.push(format!("注册 workspace root 失败: {error}"));
    }

    Ok(ConvertReport {
        source_id: parsed.source_id.unwrap_or_else(|| {
            source
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        }),
        source_provider: "claude".into(),
        target_provider: "codex".into(),
        conversion_mode: Some(mode.as_str().into()),
        new_id: new_id.clone(),
        new_path: new_abs.to_string_lossy().into_owned(),
        resume_command: format!("codex resume {new_id}"),
        imported_messages,
        dropped_reasoning: parsed.stats.dropped_reasoning,
        tool_notes: parsed.stats.tool_notes,
        warnings,
    })
}

fn cleanup_failed_import(path: &Path) {
    let _ = fs::remove_file(path);
}

fn first_user_preview(messages: &[ConvMessage]) -> &str {
    messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| m.text.as_str())
        .unwrap_or("")
}

#[derive(Debug, Default)]
struct ParsedClaudeSession {
    source_id: Option<String>,
    cwd: Option<String>,
    title: Option<String>,
    messages: Vec<ConvMessage>,
    events: Vec<ClaudeEvent>,
    stats: ExtractStats,
}

#[derive(Debug, Clone)]
enum ClaudeEvent {
    Message(ConvMessage),
    ToolCall(ClaudeToolEvent),
    ToolResult(ClaudeToolEvent),
}

#[derive(Debug, Clone)]
struct ClaudeToolEvent {
    tool_use_id: Option<String>,
    payload: Value,
    timestamp: Option<String>,
}

#[derive(Debug, Clone)]
enum ClaudeContentBlock {
    Text(String),
    Image(ImportedImage),
    ToolCall(Value),
    ToolResult(Value),
}

fn parse_claude_session(path: &Path) -> AppResult<ParsedClaudeSession> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut out = ParsedClaudeSession::default();
    let mut custom_title: Option<String> = None;
    let mut ai_title: Option<String> = None;

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if out.source_id.is_none() {
            out.source_id = record
                .get("sessionId")
                .and_then(Value::as_str)
                .map(String::from);
        }
        if out.cwd.is_none() {
            out.cwd = record
                .get("cwd")
                .and_then(Value::as_str)
                .map(String::from)
                .filter(|s| !s.trim().is_empty());
        }
        match record.get("type").and_then(Value::as_str) {
            Some("custom-title") => {
                assign_title(&mut custom_title, &record, &["customTitle", "title"]);
                continue;
            }
            Some("ai-title") => {
                assign_title(&mut ai_title, &record, &["aiTitle", "title"]);
                continue;
            }
            Some("user") | Some("assistant") => {}
            _ => continue,
        }
        // 跳过元记录和子代理侧链。
        if record.get("isMeta").and_then(Value::as_bool) == Some(true)
            || record.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let is_assistant = record.get("type").and_then(Value::as_str) == Some("assistant");
        let timestamp = record
            .get("timestamp")
            .and_then(Value::as_str)
            .map(String::from);
        let Some(content) = record.get("message").and_then(|m| m.get("content")) else {
            continue;
        };
        let Some(extracted) = extract_claude_content(content, &mut out.stats) else {
            continue;
        };
        // 仅含 tool_result 的 user 记录是工具返回，归到 assistant 侧。
        let role = if is_assistant || extracted.only_tool_result {
            Role::Assistant
        } else {
            Role::User
        };
        let text = if role == Role::User {
            unwrap_user_query(extracted.text)
        } else {
            extracted.text
        };
        out.messages.push(ConvMessage {
            role,
            text,
            timestamp: timestamp.clone(),
            phase: None,
            images: if role == Role::User {
                extracted.images.clone()
            } else {
                Vec::new()
            },
        });
        append_native_claude_events(
            &mut out.events,
            is_assistant,
            extracted.blocks,
            timestamp.as_deref(),
        );
    }
    // 丢弃首条用户消息前的 assistant 记录。
    if let Some(first_user) = out.messages.iter().position(|m| m.role == Role::User) {
        out.messages.drain(..first_user);
    }
    if let Some(first_user) = out.events.iter().position(
        |event| matches!(event, ClaudeEvent::Message(message) if message.role == Role::User),
    ) {
        out.events.drain(..first_user);
    }
    classify_claude_assistant_phases(&mut out.events);
    out.title = custom_title.or(ai_title);
    Ok(out)
}

fn append_native_claude_events(
    events: &mut Vec<ClaudeEvent>,
    is_assistant: bool,
    blocks: Vec<ClaudeContentBlock>,
    timestamp: Option<&str>,
) {
    let role = if is_assistant {
        Role::Assistant
    } else {
        Role::User
    };
    let timestamp = timestamp.map(String::from);
    let mut text_parts = Vec::new();
    let mut images = Vec::new();

    for block in blocks {
        match block {
            ClaudeContentBlock::Text(text) => text_parts.push(text),
            ClaudeContentBlock::Image(image) => images.push(image),
            ClaudeContentBlock::ToolCall(payload) => {
                flush_claude_message_event(
                    events,
                    role,
                    &mut text_parts,
                    &mut images,
                    timestamp.as_deref(),
                );
                events.push(ClaudeEvent::ToolCall(ClaudeToolEvent {
                    tool_use_id: payload.get("id").and_then(Value::as_str).map(String::from),
                    payload,
                    timestamp: timestamp.clone(),
                }));
            }
            ClaudeContentBlock::ToolResult(payload) => {
                flush_claude_message_event(
                    events,
                    role,
                    &mut text_parts,
                    &mut images,
                    timestamp.as_deref(),
                );
                events.push(ClaudeEvent::ToolResult(ClaudeToolEvent {
                    tool_use_id: payload
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .map(String::from),
                    payload,
                    timestamp: timestamp.clone(),
                }));
            }
        }
    }
    flush_claude_message_event(
        events,
        role,
        &mut text_parts,
        &mut images,
        timestamp.as_deref(),
    );
}

fn flush_claude_message_event(
    events: &mut Vec<ClaudeEvent>,
    role: Role,
    text_parts: &mut Vec<String>,
    images: &mut Vec<ImportedImage>,
    timestamp: Option<&str>,
) {
    let mut text = std::mem::take(text_parts)
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if role == Role::User {
        text = unwrap_user_query(text);
    }
    let images = std::mem::take(images);
    if text.trim().is_empty() && images.is_empty() {
        return;
    }
    events.push(ClaudeEvent::Message(ConvMessage {
        role,
        text,
        timestamp: timestamp.map(String::from),
        phase: None,
        images,
    }));
}

fn classify_claude_assistant_phases(events: &mut [ClaudeEvent]) {
    let mut turn_start = None;
    for index in 0..=events.len() {
        let starts_user_turn = index < events.len()
            && matches!(
                &events[index],
                ClaudeEvent::Message(message) if message.role == Role::User
            );
        if !starts_user_turn && index != events.len() {
            continue;
        }
        if let Some(start) = turn_start {
            classify_claude_turn_phases(&mut events[start..index]);
        }
        turn_start = starts_user_turn.then_some(index);
    }
}

fn classify_claude_turn_phases(events: &mut [ClaudeEvent]) {
    let mut assistant_indices = Vec::new();
    for (index, event) in events.iter_mut().enumerate() {
        if let ClaudeEvent::Message(message) = event {
            if message.role == Role::Assistant {
                message.phase = Some("commentary".into());
                assistant_indices.push(index);
            }
        }
    }
    let last_is_assistant_message = matches!(
        events.last(),
        Some(ClaudeEvent::Message(message)) if message.role == Role::Assistant
    );
    if last_is_assistant_message {
        if let Some(index) = assistant_indices.last().copied() {
            if let ClaudeEvent::Message(message) = &mut events[index] {
                message.phase = Some("final_answer".into());
            }
        }
    }
}

fn assign_title(slot: &mut Option<String>, record: &Value, fields: &[&str]) {
    if slot.is_some() {
        return;
    }
    for field in fields {
        if let Some(title) = record
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            *slot = Some(title.to_string());
            return;
        }
    }
}

struct ExtractedClaudeContent {
    text: String,
    only_tool_result: bool,
    images: Vec<ImportedImage>,
    blocks: Vec<ClaudeContentBlock>,
}

fn extract_claude_content(
    content: &Value,
    stats: &mut ExtractStats,
) -> Option<ExtractedClaudeContent> {
    let blocks: Vec<Value> = match content {
        Value::String(text) => vec![json!({"type": "text", "text": text})],
        Value::Array(items) => items.clone(),
        _ => return None,
    };
    let mut parts: Vec<String> = Vec::new();
    let mut images = Vec::new();
    let mut native_blocks = Vec::new();
    let mut saw_visible_block = false;
    let mut only_tool_result = true;
    for block in &blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        parts.push(text.to_string());
                        native_blocks.push(ClaudeContentBlock::Text(text.to_string()));
                        saw_visible_block = true;
                        only_tool_result = false;
                    }
                }
            }
            Some("image") => {
                if let Some(image) = parse_claude_image(block) {
                    images.push(image.clone());
                    native_blocks.push(ClaudeContentBlock::Image(image));
                } else {
                    let note = "[unsupported content block: image]".to_string();
                    parts.push(note.clone());
                    native_blocks.push(ClaudeContentBlock::Text(note));
                }
                saw_visible_block = true;
                only_tool_result = false;
            }
            Some("tool_use") => {
                parts.push(tool_call_note(block));
                native_blocks.push(ClaudeContentBlock::ToolCall(block.clone()));
                stats.tool_notes += 1;
                saw_visible_block = true;
                only_tool_result = false;
            }
            Some("tool_result") => {
                parts.push(tool_result_note(block));
                native_blocks.push(ClaudeContentBlock::ToolResult(block.clone()));
                stats.tool_notes += 1;
                saw_visible_block = true;
            }
            Some("thinking") | Some("redacted_thinking") => {
                stats.dropped_reasoning += 1;
            }
            Some(other) => {
                let note = format!("[unsupported content block: {other}]");
                parts.push(note.clone());
                native_blocks.push(ClaudeContentBlock::Text(note));
                saw_visible_block = true;
                only_tool_result = false;
            }
            None => {}
        }
    }
    let text = parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if !saw_visible_block {
        None
    } else {
        Some(ExtractedClaudeContent {
            text,
            only_tool_result,
            images,
            blocks: native_blocks,
        })
    }
}

fn parse_claude_image(block: &Value) -> Option<ImportedImage> {
    let source = block.get("source")?;
    if source.get("type").and_then(Value::as_str) != Some("base64") {
        return None;
    }
    let media_type = source.get("media_type").and_then(Value::as_str)?;
    let data = source.get("data").and_then(Value::as_str)?;
    if !media_type.starts_with("image/") || data.trim().is_empty() {
        return None;
    }
    Some(ImportedImage {
        media_type: media_type.to_string(),
        data: data.to_string(),
    })
}

fn unwrap_user_query(text: String) -> String {
    let trimmed = text.trim();
    trimmed
        .strip_prefix("<user_query>")
        .and_then(|inner| inner.strip_suffix("</user_query>"))
        .map(str::trim)
        .filter(|inner| !inner.is_empty())
        .map(String::from)
        .unwrap_or(text)
}

fn tool_call_note(block: &Value) -> String {
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let mut lines = vec![format!("[{TOOL_CALL_TAG}: {name}]")];
    if let Some(input) = block.get("input").and_then(Value::as_object) {
        if let Some(description) = input.get("description").and_then(Value::as_str) {
            lines.push(format!("description: {description}"));
        }
        if let Some(command) = input.get("command").and_then(Value::as_str) {
            lines.push(format!("command: {command}"));
        }
        if let Some(file) = input
            .get("file_path")
            .or_else(|| input.get("file"))
            .and_then(Value::as_str)
        {
            lines.push(format!("file: {file}"));
        }
        if lines.len() == 1 {
            lines.push(format!(
                "input: {}",
                truncate_chars(&Value::Object(input.clone()).to_string(), NOTE_MAX_LEN)
            ));
        }
    } else if let Some(input) = block.get("input") {
        lines.push(format!(
            "input: {}",
            truncate_chars(&input.to_string(), NOTE_MAX_LEN)
        ));
    }
    lines.push(format!("[/{TOOL_CALL_TAG}]"));
    lines.join("\n")
}

fn tool_result_note(block: &Value) -> String {
    let label = if block.get("is_error").and_then(Value::as_bool) == Some(true) {
        format!("[{TOOL_RESULT_TAG}: error]")
    } else {
        format!("[{TOOL_RESULT_TAG}]")
    };
    let text = match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .map(String::from)
                    .or_else(|| item.get("text").and_then(Value::as_str).map(String::from))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    if text.trim().is_empty() {
        format!("{label}\n[/{TOOL_RESULT_TAG}]")
    } else {
        format!(
            "{label}\n{}\n[/{TOOL_RESULT_TAG}]",
            truncate_chars(&text, TOOL_RESULT_MAX_LEN)
        )
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[derive(Debug)]
struct BuiltCodexSession {
    lines: Vec<String>,
    degraded_tool_events: usize,
}

/// 按 Codex v1 wire 格式生成 rollout 行。
fn build_codex_lines(
    new_id: &str,
    cwd: &str,
    provider: &str,
    parsed: &ParsedClaudeSession,
    identity: &CodexIdentity,
    now: &chrono::DateTime<chrono::Utc>,
    mode: CodexImportMode,
) -> BuiltCodexSession {
    let mut builder = CodexRolloutBuilder::new(new_id, cwd, provider, identity, now);
    let degraded_tool_events = match mode {
        CodexImportMode::Simple => {
            for message in &parsed.messages {
                builder.push_message(message);
            }
            0
        }
        CodexImportMode::Native => build_native_codex_events(&mut builder, cwd, &parsed.events),
    };
    let completed_at = match mode {
        CodexImportMode::Simple => parsed.messages.last().and_then(message_timestamp_seconds),
        CodexImportMode::Native => parsed
            .events
            .iter()
            .rev()
            .find_map(claude_event_timestamp_seconds),
    };
    builder.finish(completed_at);
    BuiltCodexSession {
        lines: builder.lines,
        degraded_tool_events,
    }
}

fn build_native_codex_events(
    builder: &mut CodexRolloutBuilder,
    cwd: &str,
    events: &[ClaudeEvent],
) -> usize {
    let paired_ids = paired_claude_tool_ids(events);
    let mut emitted_ids = HashSet::new();
    let mut degraded = 0usize;

    for event in events {
        match event {
            ClaudeEvent::Message(message) => builder.push_message(message),
            ClaudeEvent::ToolCall(call) => {
                let Some(source_id) = call.tool_use_id.as_deref() else {
                    degraded += 1;
                    builder
                        .push_tool_note(tool_call_note(&call.payload), call.timestamp.as_deref());
                    continue;
                };
                if !paired_ids.contains(source_id) {
                    degraded += 1;
                    builder
                        .push_tool_note(tool_call_note(&call.payload), call.timestamp.as_deref());
                    continue;
                }
                let call_id = codex_call_id(builder.new_id(), source_id);
                if builder.push_tool_call(&call_id, &call.payload, cwd) {
                    emitted_ids.insert(source_id.to_string());
                } else {
                    degraded += 1;
                    builder
                        .push_tool_note(tool_call_note(&call.payload), call.timestamp.as_deref());
                }
            }
            ClaudeEvent::ToolResult(result) => {
                let Some(source_id) = result.tool_use_id.as_deref() else {
                    degraded += 1;
                    builder.push_tool_note(
                        tool_result_note(&result.payload),
                        result.timestamp.as_deref(),
                    );
                    continue;
                };
                if !emitted_ids.contains(source_id) {
                    degraded += 1;
                    builder.push_tool_note(
                        tool_result_note(&result.payload),
                        result.timestamp.as_deref(),
                    );
                    continue;
                }
                let call_id = codex_call_id(builder.new_id(), source_id);
                if !builder.push_tool_result(&call_id, &result.payload) {
                    degraded += 1;
                    builder.push_tool_note(
                        tool_result_note(&result.payload),
                        result.timestamp.as_deref(),
                    );
                }
            }
        }
    }
    degraded
}

fn paired_claude_tool_ids(events: &[ClaudeEvent]) -> HashSet<String> {
    let mut active = HashSet::new();
    let mut paired = HashSet::new();
    for event in events {
        match event {
            ClaudeEvent::Message(message) if message.role == Role::User => active.clear(),
            ClaudeEvent::ToolCall(call) => {
                if let Some(id) = call.tool_use_id.as_deref() {
                    active.insert(id.to_string());
                }
            }
            ClaudeEvent::ToolResult(result) => {
                if let Some(id) = result.tool_use_id.as_deref() {
                    if active.contains(id) {
                        paired.insert(id.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    paired
}

struct CodexRolloutBuilder {
    new_id: String,
    import_ts: String,
    lines: Vec<String>,
    turn: Option<(String, Option<i64>)>,
    response_item_bytes: i64,
    last_model_visible_tokens: i64,
    item_sequence: u64,
}

impl CodexRolloutBuilder {
    fn new(
        new_id: &str,
        cwd: &str,
        provider: &str,
        identity: &CodexIdentity,
        now: &chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let import_ts = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let session_meta = json!({
            "session_id": new_id,
            "id": new_id,
            "timestamp": import_ts,
            "cwd": cwd,
            "originator": identity.originator,
            "source": identity.source,
            "model_provider": provider,
            "cli_version": identity
                .cli_version
                .as_deref()
                .unwrap_or(FALLBACK_CODEX_CLI_VERSION),
        });
        let mut builder = Self {
            new_id: new_id.to_string(),
            import_ts,
            lines: Vec::new(),
            turn: None,
            response_item_bytes: 0,
            last_model_visible_tokens: 0,
            item_sequence: 0,
        };
        builder.push_record("session_meta", session_meta);
        builder
    }

    fn new_id(&self) -> &str {
        &self.new_id
    }

    fn push_message(&mut self, message: &ConvMessage) {
        match message.role {
            Role::User => self.start_user_turn(message),
            Role::Assistant => self.push_assistant_message(message),
        }
    }

    fn start_user_turn(&mut self, message: &ConvMessage) {
        if let Some((turn_id, started_at)) = self.turn.take() {
            self.push_record(
                "event_msg",
                turn_complete_payload(&turn_id, started_at, None),
            );
        }
        let started_at = message_timestamp_seconds(message);
        let turn_id = repair::new_session_id();
        self.push_record(
            "event_msg",
            json!({
                "type": "task_started",
                "turn_id": turn_id,
                "started_at": started_at,
                "model_context_window": null,
            }),
        );
        self.push_record(
            "event_msg",
            json!({"type": "user_message", "message": message.text}),
        );
        self.response_item_bytes = self
            .response_item_bytes
            .saturating_add(message.text.len() as i64);
        self.push_record(
            "response_item",
            json!({
                "type": "message",
                "role": "user",
                "content": codex_user_content(message),
            }),
        );
        self.turn = Some((turn_id, started_at));
    }

    fn push_assistant_message(&mut self, message: &ConvMessage) {
        if self.turn.is_none() || message.text.trim().is_empty() {
            return;
        }
        self.response_item_bytes = self
            .response_item_bytes
            .saturating_add(message.text.len() as i64);
        self.last_model_visible_tokens = self.response_item_bytes / 4;

        let mut event = json!({"type": "agent_message", "message": message.text});
        let mut response = json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": message.text}],
        });
        if let Some(phase) = message.phase.as_deref() {
            event["phase"] = json!(phase);
            response["phase"] = json!(phase);
        }
        self.push_record("event_msg", event);
        self.push_record("response_item", response);
    }

    fn push_tool_note(&mut self, text: String, timestamp: Option<&str>) {
        self.push_assistant_message(&ConvMessage {
            role: Role::Assistant,
            text,
            timestamp: timestamp.map(String::from),
            phase: Some("commentary".into()),
            images: Vec::new(),
        });
    }

    fn push_tool_call(&mut self, call_id: &str, payload: &Value, cwd: &str) -> bool {
        let Some((name, arguments)) = native_codex_tool_call(payload, cwd) else {
            return false;
        };
        let Some(turn_id) = self.turn.as_ref().map(|turn| turn.0.clone()) else {
            return false;
        };
        self.response_item_bytes = self
            .response_item_bytes
            .saturating_add(arguments.len() as i64);
        let id = self.next_item_id("fc_");
        self.push_record(
            "response_item",
            json!({
                "type": "function_call",
                "id": id,
                "name": name,
                "arguments": arguments,
                "call_id": call_id,
                "internal_chat_message_metadata_passthrough": {"turn_id": turn_id},
            }),
        );
        true
    }

    fn push_tool_result(&mut self, call_id: &str, payload: &Value) -> bool {
        let Some(turn_id) = self.turn.as_ref().map(|turn| turn.0.clone()) else {
            return false;
        };
        let output = native_codex_tool_result(payload);
        self.response_item_bytes = self
            .response_item_bytes
            .saturating_add(codex_tool_output_text_len(&output));
        let id = self.next_item_id("fco_");
        self.push_record(
            "response_item",
            json!({
                "type": "function_call_output",
                "id": id,
                "call_id": call_id,
                "output": output,
                "internal_chat_message_metadata_passthrough": {"turn_id": turn_id},
            }),
        );
        true
    }

    fn finish(&mut self, completed_at: Option<i64>) {
        let Some((turn_id, started_at)) = self.turn.take() else {
            return;
        };
        let usage = json!({
            "input_tokens": 0,
            "cached_input_tokens": 0,
            "output_tokens": 0,
            "reasoning_output_tokens": 0,
            "total_tokens": self.last_model_visible_tokens,
        });
        self.push_record(
            "event_msg",
            json!({
                "type": "token_count",
                "info": {
                    "total_token_usage": usage,
                    "last_token_usage": usage,
                    "model_context_window": null,
                },
                "rate_limits": null,
            }),
        );
        self.push_record(
            "event_msg",
            turn_complete_payload(&turn_id, started_at, completed_at),
        );
    }

    fn push_record(&mut self, kind: &str, payload: Value) {
        self.lines.push(
            json!({"timestamp": self.import_ts, "type": kind, "payload": payload}).to_string(),
        );
    }

    fn next_item_id(&mut self, prefix: &str) -> String {
        self.item_sequence = self.item_sequence.saturating_add(1);
        codex_native_id(
            prefix,
            &format!("{}:{prefix}:{}", self.new_id, self.item_sequence),
        )
    }
}

fn message_timestamp_seconds(message: &ConvMessage) -> Option<i64> {
    message
        .timestamp
        .as_deref()
        .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok())
        .map(|timestamp| timestamp.timestamp())
}

fn claude_event_timestamp_seconds(event: &ClaudeEvent) -> Option<i64> {
    let timestamp = match event {
        ClaudeEvent::Message(message) => message.timestamp.as_deref(),
        ClaudeEvent::ToolCall(tool) | ClaudeEvent::ToolResult(tool) => tool.timestamp.as_deref(),
    }?;
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|timestamp| timestamp.timestamp())
}

fn codex_user_content(message: &ConvMessage) -> Vec<Value> {
    let mut content = Vec::new();
    if !message.text.trim().is_empty() {
        content.push(json!({"type": "input_text", "text": message.text}));
    }
    content.extend(message.images.iter().map(|image| {
        json!({
            "type": "input_image",
            "image_url": format!("data:{};base64,{}", image.media_type, image.data),
            "detail": "original",
        })
    }));
    content
}

fn native_codex_tool_call(payload: &Value, cwd: &str) -> Option<(String, String)> {
    let name = payload.get("name").and_then(Value::as_str)?.trim();
    if name.is_empty() {
        return None;
    }
    let input = payload.get("input").cloned().unwrap_or_else(|| json!({}));
    if name.eq_ignore_ascii_case("bash") {
        let input = input.as_object()?;
        let command = input
            .get("command")
            .or_else(|| input.get("cmd"))
            .and_then(Value::as_str)?;
        let workdir = input
            .get("workdir")
            .or_else(|| input.get("cwd"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(cwd);
        let mut arguments = serde_json::Map::new();
        arguments.insert("command".into(), json!(command));
        arguments.insert("workdir".into(), json!(workdir));
        if let Some(timeout) = input.get("timeout_ms").or_else(|| input.get("timeout")) {
            arguments.insert("timeout_ms".into(), timeout.clone());
        }
        return Some(("shell_command".into(), Value::Object(arguments).to_string()));
    }
    Some((name.to_string(), input.to_string()))
}

fn native_codex_tool_result(payload: &Value) -> Value {
    let (mut text, images) = claude_tool_result_parts(payload);
    if payload.get("is_error").and_then(Value::as_bool) == Some(true) {
        if text.trim().is_empty() {
            text = "Error".into();
        } else if !text.trim_start().starts_with("Error") {
            text = format!("Error: {text}");
        }
    }
    text = truncate_chars(&text, NATIVE_TOOL_RESULT_MAX_LEN);
    if images.is_empty() {
        return Value::String(text);
    }
    let mut output = Vec::new();
    if !text.trim().is_empty() {
        output.push(json!({"type": "input_text", "text": text}));
    }
    output.extend(images.into_iter().map(|image| {
        json!({
            "type": "input_image",
            "image_url": format!("data:{};base64,{}", image.media_type, image.data),
            "detail": "original",
        })
    }));
    Value::Array(output)
}

fn claude_tool_result_parts(payload: &Value) -> (String, Vec<ImportedImage>) {
    fn visit(value: &Value, text: &mut Vec<String>, images: &mut Vec<ImportedImage>) {
        match value {
            Value::String(value) => text.push(value.clone()),
            Value::Array(items) => {
                for item in items {
                    visit(item, text, images);
                }
            }
            Value::Object(object) => match object.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(value) = object.get("text").and_then(Value::as_str) {
                        text.push(value.to_string());
                    }
                }
                Some("image") => {
                    if let Some(image) = parse_claude_image(value) {
                        images.push(image);
                    }
                }
                _ => {
                    if let Some(value) = object.get("text").and_then(Value::as_str) {
                        text.push(value.to_string());
                    }
                }
            },
            _ => {}
        }
    }

    let mut text = Vec::new();
    let mut images = Vec::new();
    if let Some(content) = payload.get("content") {
        visit(content, &mut text, &mut images);
    }
    (text.join("\n"), images)
}

fn codex_tool_output_text_len(output: &Value) -> i64 {
    match output {
        Value::String(text) => text.len() as i64,
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .map(|text| text.len() as i64)
            .sum(),
        _ => 0,
    }
}

fn codex_native_id(prefix: &str, seed: &str) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let digest = Sha256::digest(seed.as_bytes());
    let suffix = digest
        .iter()
        .take(24)
        .map(|byte| ALPHABET[(*byte as usize) % ALPHABET.len()] as char)
        .collect::<String>();
    format!("{prefix}{suffix}")
}

fn codex_call_id(new_id: &str, source_id: &str) -> String {
    codex_native_id("call_", &format!("{new_id}:call:{source_id}"))
}

fn turn_complete_payload(
    turn_id: &str,
    started_at: Option<i64>,
    completed_at: Option<i64>,
) -> Value {
    json!({
        "type": "task_complete",
        "turn_id": turn_id,
        "last_agent_message": null,
        "started_at": started_at,
        "completed_at": completed_at,
    })
}

// ---------------------------------------------------------------------------
// Codex → Claude
// ---------------------------------------------------------------------------

fn convert_codex_to_claude(
    codex_dir: &str,
    claude_dir: &str,
    rollout_path: &str,
    mode: ClaudeImportMode,
) -> AppResult<ConvertReport> {
    let _ = codex_dir;
    let claude = PathBuf::from(claude_dir);
    let source = PathBuf::from(rollout_path);
    let parsed = parse_codex_rollout(&source)?;
    let Some(cwd) = parsed.cwd.clone() else {
        return Err(AppError::Other(
            "源 rollout 缺少 cwd（session_meta/turn_context 均未提供）".into(),
        ));
    };
    if parsed.messages.iter().all(|m| m.role != Role::User) {
        return Err(AppError::Other(
            "源会话没有可迁移的用户消息（推理、工具和内部上下文不计入）".into(),
        ));
    }

    let projects = paths::claude_projects_dir(&claude);
    let project_dir = projects.join(encode_claude_project_dir(&cwd));
    fs::create_dir_all(&project_dir)?;
    let new_id = repair::new_session_id();
    let new_abs = project_dir.join(format!("{new_id}.jsonl"));
    let identity = detect_claude_identity(&projects);
    let built = build_claude_lines(&new_id, &cwd, &parsed, &identity, mode);
    let imported_messages = built.lines.len() as u32;

    atomic_file::create_with_writer_if_absent(&new_abs, |out| {
        for line in &built.lines {
            writeln!(out, "{line}")?;
        }
        Ok(())
    })?;

    let mut warnings = Vec::new();
    if mode == ClaudeImportMode::Native {
        warnings.push(
            "原生Claude（实验）依赖当前 Claude 会话格式。如果会话无法恢复，请改用简洁续聊。".into(),
        );
        if built.degraded_tool_events > 0 {
            warnings.push(format!(
                "{} 条工具事件无法配对，已转为文本注记",
                built.degraded_tool_events
            ));
        }
    }

    Ok(ConvertReport {
        source_id: parsed.source_id.unwrap_or_default(),
        source_provider: "codex".into(),
        target_provider: "claude".into(),
        conversion_mode: Some(mode.as_str().into()),
        new_id: new_id.clone(),
        new_path: new_abs.to_string_lossy().into_owned(),
        resume_command: fs_ops::claude_resume_command(&new_id, Some(&cwd)),
        imported_messages,
        dropped_reasoning: parsed.stats.dropped_reasoning,
        tool_notes: parsed.stats.tool_notes,
        warnings,
    })
}

/// Claude Code 的项目目录编码：非 ASCII 字母数字一律替换为 `-`。
fn encode_claude_project_dir(cwd: &str) -> String {
    paths::strip_verbatim(cwd)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn newest_jsonl_paths(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut paths = WalkDir::new(root)
        .min_depth(1)
        .max_depth(max_depth)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.into_path()))
        })
        .collect::<Vec<_>>();
    paths.sort_by(|left, right| right.0.cmp(&left.0));
    paths.into_iter().map(|(_, path)| path).collect()
}

fn generated_origin_label(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("cc-sessions")
        || value.contains("codex-import")
        || value.contains("external-import")
        || value.contains("external_import")
        || value.contains("external_agent")
        || value.contains("imported")
}

/// 从目标 Codex 最近的 session_meta 读取客户端身份；没有样本时使用默认值。
fn detect_codex_identity(codex: &Path) -> CodexIdentity {
    'files: for path in newest_jsonl_paths(&paths::sessions_dir(codex), 5) {
        let Ok(file) = fs::File::open(path) else {
            continue;
        };
        for line in BufReader::new(file).lines().take(20).flatten() {
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if record.get("type").and_then(Value::as_str) != Some("session_meta") {
                continue;
            }
            let Some(payload) = record.get("payload") else {
                continue;
            };
            let Some(originator) = payload.get("originator").and_then(Value::as_str) else {
                continue 'files;
            };
            if generated_origin_label(originator) {
                continue 'files;
            }
            return CodexIdentity {
                originator: originator.to_string(),
                cli_version: payload
                    .get("cli_version")
                    .and_then(Value::as_str)
                    .map(String::from),
                source: payload
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("vscode")
                    .to_string(),
            };
        }
    }
    CodexIdentity::default()
}

/// 从目标 Claude 会话读取 CLI 版本和 `claude-*` 模型，忽略源会话中的其他模型名。
fn detect_claude_identity(projects: &Path) -> ClaudeIdentity {
    let mut detected_version = None;
    for path in newest_jsonl_paths(projects, 2) {
        let Ok(file) = fs::File::open(path) else {
            continue;
        };
        let mut file_version = None;
        for line in BufReader::new(file).lines().take(200).flatten() {
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if file_version.is_none() {
                file_version = record
                    .get("version")
                    .and_then(Value::as_str)
                    .map(String::from);
            }
            if record.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(message) = record.get("message") else {
                continue;
            };
            let Some(model) = message.get("model").and_then(Value::as_str) else {
                continue;
            };
            let message_id = message.get("id").and_then(Value::as_str).unwrap_or("");
            if model.starts_with("claude-") && !generated_origin_label(message_id) {
                return ClaudeIdentity {
                    model: model.to_string(),
                    version: file_version.or(detected_version),
                };
            }
        }
        if detected_version.is_none() {
            detected_version = file_version;
        }
    }
    ClaudeIdentity {
        version: detected_version,
        ..ClaudeIdentity::default()
    }
}

#[derive(Debug, Default)]
struct ParsedCodexRollout {
    source_id: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    model: Option<String>,
    messages: Vec<ConvMessage>,
    events: Vec<CodexEvent>,
    stats: ExtractStats,
}

#[derive(Debug, Clone)]
enum CodexEvent {
    Message(ConvMessage),
    ToolCall(CodexToolEvent),
    ToolResult(CodexToolEvent),
    ToolNote(ConvMessage),
}

#[derive(Debug, Clone)]
struct CodexToolEvent {
    call_id: Option<String>,
    payload: Value,
    timestamp: Option<String>,
}

fn parse_codex_rollout(path: &Path) -> AppResult<ParsedCodexRollout> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut out = ParsedCodexRollout::default();
    // 优先读取 response_item；没有 message 时回退到旧版 event_msg。
    let mut fallback_messages: Vec<ConvMessage> = Vec::new();
    let mut fallback_events: Vec<CodexEvent> = Vec::new();
    let mut has_response_messages = false;

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let timestamp = record
            .get("timestamp")
            .and_then(Value::as_str)
            .map(String::from);
        let Some(payload) = record.get("payload") else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                out.source_id = out
                    .source_id
                    .or_else(|| payload.get("id").and_then(Value::as_str).map(String::from));
                out.cwd = out
                    .cwd
                    .or_else(|| payload.get("cwd").and_then(Value::as_str).map(String::from));
                out.git_branch = out.git_branch.or_else(|| {
                    payload
                        .get("git")
                        .and_then(|git| git.get("branch"))
                        .and_then(Value::as_str)
                        .map(String::from)
                });
            }
            Some("turn_context") => {
                out.cwd = out
                    .cwd
                    .or_else(|| payload.get("cwd").and_then(Value::as_str).map(String::from));
                out.model = out.model.or_else(|| {
                    payload
                        .get("model")
                        .and_then(Value::as_str)
                        .map(String::from)
                });
            }
            Some("response_item") => match payload.get("type").and_then(Value::as_str) {
                Some("message") => {
                    let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                    let text = flatten_codex_content(payload.get("content"));
                    let images = if role == "user" {
                        extract_codex_images(payload.get("content"))
                    } else {
                        Vec::new()
                    };
                    if text.trim().is_empty() && images.is_empty() {
                        continue;
                    }
                    match role {
                        "user" => {
                            if is_internal_codex_context(&text) {
                                continue;
                            }
                            has_response_messages = true;
                            let message = ConvMessage {
                                role: Role::User,
                                text: strip_codex_request_wrapper(&text),
                                timestamp: timestamp.clone(),
                                phase: None,
                                images,
                            };
                            out.messages.push(message.clone());
                            out.events.push(CodexEvent::Message(message));
                        }
                        "assistant" => {
                            has_response_messages = true;
                            let message = ConvMessage {
                                role: Role::Assistant,
                                text,
                                timestamp: timestamp.clone(),
                                phase: payload
                                    .get("phase")
                                    .and_then(Value::as_str)
                                    .map(String::from),
                                images: Vec::new(),
                            };
                            out.messages.push(message.clone());
                            out.events.push(CodexEvent::Message(message));
                        }
                        _ => {}
                    }
                }
                Some("reasoning") => {
                    out.stats.dropped_reasoning += 1;
                }
                Some("function_call")
                | Some("custom_tool_call")
                | Some("web_search_call")
                | Some("tool_search_call") => {
                    out.stats.tool_notes += 1;
                    let note = ConvMessage {
                        role: Role::Assistant,
                        text: codex_tool_call_note(payload),
                        timestamp: timestamp.clone(),
                        phase: Some("commentary".into()),
                        images: Vec::new(),
                    };
                    out.messages.push(note);
                    out.events.push(CodexEvent::ToolCall(CodexToolEvent {
                        call_id: payload
                            .get("call_id")
                            .and_then(Value::as_str)
                            .map(String::from),
                        payload: payload.clone(),
                        timestamp,
                    }));
                }
                Some("function_call_output")
                | Some("custom_tool_call_output")
                | Some("tool_search_output") => {
                    out.stats.tool_notes += 1;
                    let note = ConvMessage {
                        role: Role::Assistant,
                        text: codex_tool_result_note(payload),
                        timestamp: timestamp.clone(),
                        phase: Some("commentary".into()),
                        images: Vec::new(),
                    };
                    out.messages.push(note);
                    out.events.push(CodexEvent::ToolResult(CodexToolEvent {
                        call_id: payload
                            .get("call_id")
                            .and_then(Value::as_str)
                            .map(String::from),
                        payload: payload.clone(),
                        timestamp,
                    }));
                }
                Some("image_generation_call") => {
                    out.stats.tool_notes += 1;
                    let note = ConvMessage {
                        role: Role::Assistant,
                        text: codex_image_generation_note(payload),
                        timestamp,
                        phase: Some("commentary".into()),
                        images: Vec::new(),
                    };
                    out.messages.push(note.clone());
                    out.events.push(CodexEvent::ToolNote(note));
                }
                _ => {}
            },
            Some("event_msg") => match payload.get("type").and_then(Value::as_str) {
                Some("user_message") => {
                    if let Some(text) = payload.get("message").and_then(Value::as_str) {
                        if !text.trim().is_empty() && !is_internal_codex_context(text) {
                            let message = ConvMessage {
                                role: Role::User,
                                text: strip_codex_request_wrapper(text),
                                timestamp: timestamp.clone(),
                                phase: None,
                                images: Vec::new(),
                            };
                            fallback_messages.push(message.clone());
                            fallback_events.push(CodexEvent::Message(message));
                        }
                    }
                }
                Some("agent_message") => {
                    if let Some(text) = payload.get("message").and_then(Value::as_str) {
                        if !text.trim().is_empty() && text != LEGACY_IMPORTED_MARKER {
                            let message = ConvMessage {
                                role: Role::Assistant,
                                text: text.to_string(),
                                timestamp: timestamp.clone(),
                                phase: payload
                                    .get("phase")
                                    .and_then(Value::as_str)
                                    .map(String::from),
                                images: Vec::new(),
                            };
                            fallback_messages.push(message.clone());
                            fallback_events.push(CodexEvent::Message(message));
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    if !has_response_messages {
        out.messages = fallback_messages;
        out.events = fallback_events;
    }
    Ok(out)
}

fn codex_tool_call_note(payload: &Value) -> String {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| match payload.get("type").and_then(Value::as_str) {
            Some("web_search_call") => Some("web_search"),
            Some("tool_search_call") => Some("tool_search"),
            Some("image_generation_call") => Some("image_generation"),
            _ => None,
        })
        .unwrap_or("unknown");
    let input = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .or_else(|| payload.get("action"))
        .or_else(|| payload.get("revised_prompt"))
        .map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let mut lines = vec![format!("[{TOOL_CALL_TAG}: {name}]")];
    if !input.trim().is_empty() {
        lines.push(format!("input: {}", truncate_chars(&input, NOTE_MAX_LEN)));
    }
    lines.push(format!("[/{TOOL_CALL_TAG}]"));
    lines.join("\n")
}

fn codex_tool_result_note(payload: &Value) -> String {
    let label = if payload.get("is_error").and_then(Value::as_bool) == Some(true) {
        format!("[{TOOL_RESULT_TAG}: error]")
    } else {
        format!("[{TOOL_RESULT_TAG}]")
    };
    let text = codex_tool_output_text(payload.get("output").or_else(|| payload.get("tools")));
    if text.trim().is_empty() {
        format!("{label}\n[/{TOOL_RESULT_TAG}]")
    } else {
        format!(
            "{label}\n{}\n[/{TOOL_RESULT_TAG}]",
            truncate_chars(&text, TOOL_RESULT_MAX_LEN)
        )
    }
}

fn codex_image_generation_note(payload: &Value) -> String {
    let call_note = codex_tool_call_note(payload);
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    format!(
        "{call_note}\n\n[{TOOL_RESULT_TAG}]\nimage output omitted (status: {status})\n[/{TOOL_RESULT_TAG}]"
    )
}

fn codex_tool_output_text(output: Option<&Value>) -> String {
    fn extract(value: &Value) -> Option<String> {
        match value {
            Value::Null => None,
            Value::String(text) => (!text.trim().is_empty()).then(|| text.clone()),
            Value::Array(items) => {
                let text = items
                    .iter()
                    .filter_map(extract)
                    .collect::<Vec<_>>()
                    .join("\n");
                (!text.trim().is_empty()).then_some(text)
            }
            Value::Object(obj) => {
                if matches!(
                    obj.get("type").and_then(Value::as_str),
                    Some("image" | "image_url" | "input_image" | "computer_screenshot")
                ) {
                    return None;
                }
                obj.get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                    .map(String::from)
                    .or_else(|| obj.get("content").and_then(extract))
                    .or_else(|| obj.get("output").and_then(extract))
                    .or_else(|| Some(Value::Object(obj.clone()).to_string()))
            }
            other => Some(other.to_string()),
        }
    }

    output.and_then(extract).unwrap_or_default()
}

fn extract_codex_images(content: Option<&Value>) -> Vec<ImportedImage> {
    let Some(Value::Array(items)) = content else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("input_image"))
        .filter_map(|item| item.get("image_url").and_then(Value::as_str))
        .filter_map(parse_data_image_uri)
        .collect()
}

fn parse_data_image_uri(uri: &str) -> Option<ImportedImage> {
    let body = uri.strip_prefix("data:")?;
    let (metadata, data) = body.split_once(',')?;
    let media_type = metadata.strip_suffix(";base64")?;
    if !media_type.starts_with("image/") || data.trim().is_empty() {
        return None;
    }
    Some(ImportedImage {
        media_type: media_type.to_string(),
        data: data.to_string(),
    })
}

#[derive(Debug)]
struct NativeToolResultData {
    content: Value,
    text: String,
    images: Vec<ImportedImage>,
}

fn native_tool_result_data(payload: &Value) -> NativeToolResultData {
    let output = native_display_output_value(payload);
    let images = extract_codex_tool_images(&output);
    let text = truncate_chars(
        &codex_tool_output_text(Some(&output)),
        NATIVE_TOOL_RESULT_MAX_LEN,
    );
    let content = if images.is_empty() {
        json!(text.clone())
    } else {
        let mut blocks = Vec::new();
        if !text.trim().is_empty() {
            blocks.push(json!({"type": "text", "text": text.clone()}));
        }
        blocks.extend(images.iter().map(|image| {
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.media_type.clone(),
                    "data": image.data.clone(),
                }
            })
        }));
        Value::Array(blocks)
    };
    NativeToolResultData {
        content,
        text,
        images,
    }
}

fn native_tool_result_representation(
    tool_name: &str,
    tool_input: &Value,
    call: &CodexToolEvent,
    result: &CodexToolEvent,
    native_result: &NativeToolResultData,
    is_error: bool,
) -> (Value, Option<Value>) {
    if tool_name == "AskUserQuestion" {
        if let Some((content, renderer)) = native_ask_user_question_result(call, result) {
            return (json!(content), Some(renderer));
        }
    }
    let renderer = match tool_name {
        "Bash" => Some(native_bash_tool_use_result(&native_result.text, is_error)),
        "TaskOutput" => Some(native_task_output_tool_use_result(
            tool_input,
            &native_result.text,
            is_error,
        )),
        "Read" => native_result
            .images
            .first()
            .map(native_image_tool_use_result),
        _ => None,
    };
    (native_result.content.clone(), renderer)
}

fn native_bash_tool_use_result(output: &str, is_error: bool) -> Value {
    if is_error {
        json!(format!("Error: {output}"))
    } else {
        json!({
            "stdout": output,
            "stderr": "",
            "interrupted": false,
            "isImage": false,
            "noOutputExpected": false,
        })
    }
}

fn native_task_output_tool_use_result(input: &Value, output: &str, is_error: bool) -> Value {
    let task_id = input
        .get("task_id")
        .and_then(Value::as_str)
        .unwrap_or("task");
    json!({
        "retrieval_status": "success",
        "task": {
            "task_id": task_id,
            "task_type": "local_bash",
            "status": if is_error { "failed" } else { "completed" },
            "description": "Command output",
            "output": output,
            "exitCode": if is_error { 1 } else { 0 },
            "error": if is_error { Some(output) } else { None },
        }
    })
}

fn raw_codex_tool_output(payload: &Value) -> Value {
    payload
        .get("output")
        .or_else(|| payload.get("tools"))
        .cloned()
        .unwrap_or(Value::Null)
}

fn parsed_codex_tool_output(payload: &Value) -> Option<Value> {
    match raw_codex_tool_output(payload) {
        Value::Null => None,
        Value::String(text) => serde_json::from_str(&text).ok(),
        other => Some(other),
    }
}

fn native_display_output_value(payload: &Value) -> Value {
    let raw = raw_codex_tool_output(payload);
    let Value::String(text) = &raw else {
        return raw;
    };
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(text) else {
        return raw;
    };
    if object.contains_key("metadata") {
        if let Some(output) = object.get("output") {
            return output.clone();
        }
    }
    raw
}

fn extract_codex_tool_images(output: &Value) -> Vec<ImportedImage> {
    fn visit(value: &Value, images: &mut Vec<ImportedImage>) {
        match value {
            Value::Array(items) => {
                for item in items {
                    visit(item, images);
                }
            }
            Value::Object(object) => {
                if let Some(image) = object
                    .get("image_url")
                    .and_then(Value::as_str)
                    .and_then(parse_data_image_uri)
                {
                    images.push(image);
                    return;
                }
                if let Some(source) = object.get("source").and_then(Value::as_object) {
                    if source.get("type").and_then(Value::as_str) == Some("base64") {
                        if let (Some(media_type), Some(data)) = (
                            source.get("media_type").and_then(Value::as_str),
                            source.get("data").and_then(Value::as_str),
                        ) {
                            images.push(ImportedImage {
                                media_type: media_type.to_string(),
                                data: data.to_string(),
                            });
                            return;
                        }
                    }
                }
                if let Some(content) = object.get("content") {
                    visit(content, images);
                }
                if let Some(output) = object.get("output") {
                    visit(output, images);
                }
            }
            _ => {}
        }
    }

    let mut images = Vec::new();
    visit(output, &mut images);
    images
}

fn native_image_tool_use_result(image: &ImportedImage) -> Value {
    let mut file = serde_json::Map::new();
    file.insert("base64".into(), json!(image.data.clone()));
    file.insert("type".into(), json!(image.media_type.clone()));
    if let Ok(bytes) = BASE64_STANDARD.decode(image.data.as_bytes()) {
        file.insert("originalSize".into(), json!(bytes.len()));
        if let Some((width, height)) = image_dimensions(&bytes) {
            let (display_width, display_height) = display_image_dimensions(width, height);
            file.insert(
                "dimensions".into(),
                json!({
                    "originalWidth": width,
                    "originalHeight": height,
                    "displayWidth": display_width,
                    "displayHeight": display_height,
                }),
            );
        }
    }
    json!({"type": "image", "file": Value::Object(file)})
}

fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() >= 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return (width > 0 && height > 0).then_some((width, height));
    }
    if bytes.len() >= 10 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        let width = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;
        let height = u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32;
        return (width > 0 && height > 0).then_some((width, height));
    }
    if bytes.len() >= 26 && bytes.starts_with(b"BM") {
        let width = u32::from_le_bytes(bytes[18..22].try_into().ok()?);
        let height = i32::from_le_bytes(bytes[22..26].try_into().ok()?).unsigned_abs();
        return (width > 0 && height > 0).then_some((width, height));
    }
    if bytes.len() >= 4 && bytes.starts_with(&[0xff, 0xd8]) {
        let mut offset = 2usize;
        while offset + 9 < bytes.len() {
            if bytes[offset] != 0xff {
                offset += 1;
                continue;
            }
            let marker = bytes[offset + 1];
            if matches!(
                marker,
                0xc0 | 0xc1
                    | 0xc2
                    | 0xc3
                    | 0xc5
                    | 0xc6
                    | 0xc7
                    | 0xc9
                    | 0xca
                    | 0xcb
                    | 0xcd
                    | 0xce
                    | 0xcf
            ) {
                let height = u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]]) as u32;
                let width = u16::from_be_bytes([bytes[offset + 7], bytes[offset + 8]]) as u32;
                return (width > 0 && height > 0).then_some((width, height));
            }
            let segment_len = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
            if segment_len < 2 {
                break;
            }
            offset = offset.saturating_add(segment_len + 2);
        }
    }
    None
}

fn display_image_dimensions(width: u32, height: u32) -> (u32, u32) {
    const MAX_DISPLAY_EDGE: f64 = 2_000.0;
    let scale = (MAX_DISPLAY_EDGE / width as f64)
        .min(MAX_DISPLAY_EDGE / height as f64)
        .min(1.0);
    (
        (width as f64 * scale).round().max(1.0) as u32,
        (height as f64 * scale).round().max(1.0) as u32,
    )
}

fn native_ask_user_question_result(
    call: &CodexToolEvent,
    result: &CodexToolEvent,
) -> Option<(String, Value)> {
    let original_input = codex_native_input(&call.payload);
    let normalized_input = normalize_ask_user_question_input(original_input.clone());
    let answers_root = parsed_codex_tool_output(&result.payload)?;
    let raw_answers = answers_root.get("answers")?.as_object()?;
    let questions = original_input.get("questions")?.as_array()?;
    let mut answers = serde_json::Map::new();
    for question in questions {
        let question = question.as_object()?;
        let prompt = question.get("question")?.as_str()?;
        let key = question.get("id").and_then(Value::as_str).unwrap_or(prompt);
        let Some(answer) = raw_answers
            .get(key)
            .or_else(|| raw_answers.get(prompt))
            .and_then(native_answer_text)
        else {
            continue;
        };
        answers.insert(prompt.to_string(), json!(answer));
    }
    if answers.is_empty() {
        return None;
    }
    let summary = answers
        .iter()
        .filter_map(|(question, answer)| {
            answer
                .as_str()
                .map(|answer| format!("\"{question}\"=\"{answer}\""))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let content = format!(
        "Your questions have been answered: {summary}. You can now continue with these answers in mind."
    );
    let renderer = json!({
        "questions": normalized_input
            .get("questions")
            .cloned()
            .unwrap_or_else(|| json!([])),
        "answers": Value::Object(answers),
    });
    Some((content, renderer))
}

fn native_answer_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let values = items
                .iter()
                .filter_map(native_answer_text)
                .collect::<Vec<_>>();
            (!values.is_empty()).then(|| values.join(", "))
        }
        Value::Object(object) => object
            .get("answers")
            .or_else(|| object.get("answer"))
            .and_then(native_answer_text),
        other => Some(other.to_string()),
    }
}

fn flatten_codex_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                match item.get("type").and_then(Value::as_str) {
                    // 加密内容读不出明文，直接跳过。
                    Some("encrypted_content") => None,
                    _ => item
                        .get("text")
                        .and_then(Value::as_str)
                        .map(String::from)
                        .or_else(|| item.as_str().map(String::from)),
                }
            })
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// Codex 会把 AGENTS.md、环境上下文等内部信息包装成 user 消息，不属于对话。
fn is_internal_codex_context(text: &str) -> bool {
    let trimmed = text.trim();
    let first_line = trimmed
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('#')
        .trim_start();
    (first_line.starts_with("AGENTS.md instructions") && trimmed.contains("<INSTRUCTIONS>"))
        || (first_line == "<environment_context>" && trimmed.contains("</environment_context>"))
        || (first_line == "<recommended_plugins>" && trimmed.contains("</recommended_plugins>"))
        || (first_line == "<user_instructions>" && trimmed.contains("</user_instructions>"))
}

/// 带附件的 Codex 用户消息把真实请求包在 `## My request for Codex:` 之后。
fn strip_codex_request_wrapper(text: &str) -> String {
    const MARKER: &str = "## My request for Codex:";
    match text.find(MARKER) {
        Some(idx) => text[idx + MARKER.len()..]
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                !trimmed.starts_with("<image") && !trimmed.starts_with("</image")
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
        None => text.to_string(),
    }
}

#[derive(Debug, Default)]
struct ClaudeBuild {
    lines: Vec<String>,
    degraded_tool_events: u32,
}

#[derive(Debug, Default)]
struct PendingAssistant {
    blocks: Vec<Value>,
    timestamp: Option<String>,
    phase: Option<String>,
}

impl PendingAssistant {
    fn push_message(&mut self, message: &ConvMessage) {
        if message.text.trim().is_empty() {
            return;
        }
        self.set_timestamp(message.timestamp.as_deref());
        self.update_phase(message.phase.as_deref());
        self.insert_text_before_tools(message.text.clone());
    }

    fn push_note(&mut self, text: String, timestamp: Option<&str>) {
        if text.trim().is_empty() {
            return;
        }
        self.set_timestamp(timestamp);
        if self.phase.as_deref() != Some("final_answer") {
            self.phase = Some("commentary".into());
        }
        self.insert_text_before_tools(text);
    }

    fn push_tool_use(&mut self, block: Value, timestamp: Option<&str>) {
        self.set_timestamp(timestamp);
        if self.phase.is_none() {
            self.phase = Some("commentary".into());
        }
        self.blocks.push(block);
    }

    fn has_tool_use(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
    }

    fn set_timestamp(&mut self, timestamp: Option<&str>) {
        if self.timestamp.is_none() {
            self.timestamp = timestamp.map(String::from);
        }
    }

    fn update_phase(&mut self, phase: Option<&str>) {
        match phase {
            Some("final_answer") => self.phase = Some("final_answer".into()),
            Some("commentary") if self.phase.is_none() => self.phase = Some("commentary".into()),
            None if self.phase.as_deref() != Some("final_answer") => self.phase = None,
            _ => {}
        }
    }

    fn insert_text_before_tools(&mut self, text: String) {
        let index = self
            .blocks
            .iter()
            .position(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
            .unwrap_or(self.blocks.len());
        self.blocks
            .insert(index, json!({"type": "text", "text": text}));
    }
}

struct ClaudeRecordBuilder<'a> {
    new_id: &'a str,
    cwd: &'a str,
    git_branch: &'a str,
    model: &'a str,
    version: Option<&'a str>,
    last_timestamp: String,
    parent_uuid: Option<String>,
    assistant_seq: usize,
    tool_parents: HashMap<String, String>,
    lines: Vec<String>,
}

impl<'a> ClaudeRecordBuilder<'a> {
    fn new(
        new_id: &'a str,
        cwd: &'a str,
        parsed: &'a ParsedCodexRollout,
        identity: &'a ClaudeIdentity,
    ) -> Self {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let last_timestamp = parsed
            .messages
            .first()
            .and_then(|message| message.timestamp.clone())
            .unwrap_or(now);
        Self {
            new_id,
            cwd,
            git_branch: parsed.git_branch.as_deref().unwrap_or(""),
            model: identity.model.as_str(),
            version: identity.version.as_deref(),
            last_timestamp,
            parent_uuid: None,
            assistant_seq: 0,
            tool_parents: HashMap::new(),
            lines: Vec::new(),
        }
    }

    fn emit_user_message(&mut self, message: &ConvMessage) {
        let content = claude_user_content(message);
        if content.is_empty() {
            return;
        }
        let timestamp = self.resolve_timestamp(message.timestamp.as_deref());
        let uuid = repair::new_session_id();
        let mut record = self.base_record(self.parent_uuid.as_deref(), &uuid, &timestamp);
        record["type"] = json!("user");
        record["message"] = json!({"role": "user", "content": content});
        self.parent_uuid = Some(uuid);
        self.lines.push(record.to_string());
    }

    fn emit_assistant(&mut self, pending: &mut PendingAssistant) -> Option<String> {
        if pending.blocks.is_empty() {
            return None;
        }
        let has_tool_use = pending.has_tool_use();
        let timestamp = self.resolve_timestamp(pending.timestamp.as_deref());
        let uuid = repair::new_session_id();
        let mut record = self.base_record(self.parent_uuid.as_deref(), &uuid, &timestamp);
        self.assistant_seq += 1;
        let blocks = std::mem::take(&mut pending.blocks);
        for block in &blocks {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    self.tool_parents.insert(id.to_string(), uuid.clone());
                }
            }
        }
        record["type"] = json!("assistant");
        record["message"] = json!({
            "id": claude_native_id("msg_", &format!("{}:{}", self.new_id, self.assistant_seq)),
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": blocks,
            "stop_reason": if has_tool_use { "tool_use" } else { "end_turn" },
            "stop_sequence": null,
            "usage": {"input_tokens": 0, "output_tokens": 0},
        });
        if let Some(phase) = pending.phase.take() {
            record["message"]["phase"] = json!(phase);
        }
        pending.timestamp = None;
        self.parent_uuid = Some(uuid.clone());
        self.lines.push(record.to_string());
        Some(uuid)
    }

    fn emit_tool_result(
        &mut self,
        tool_id: &str,
        call: &CodexToolEvent,
        result: &CodexToolEvent,
    ) -> bool {
        let Some(tool_parent) = self.tool_parents.get(tool_id).cloned() else {
            return false;
        };
        let (tool_name, tool_input) = native_tool_call(&call.payload);
        let native_result = native_tool_result_data(&result.payload);
        let is_error = codex_tool_event_is_error(&result.payload);
        let (content, renderer) = native_tool_result_representation(
            &tool_name,
            &tool_input,
            call,
            result,
            &native_result,
            is_error,
        );
        let timestamp = self.resolve_timestamp(result.timestamp.as_deref());
        let uuid = repair::new_session_id();
        let mut record = self.base_record(Some(&tool_parent), &uuid, &timestamp);
        record["type"] = json!("user");
        record["promptId"] = json!(repair::new_session_id());
        record["sourceToolAssistantUUID"] = json!(tool_parent);
        record["message"] = json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": tool_id,
                "content": content,
                "is_error": is_error,
            }],
        });
        if let Some(renderer) = renderer {
            record["toolUseResult"] = renderer;
        }
        self.parent_uuid = Some(uuid);
        self.lines.push(record.to_string());
        true
    }

    fn base_record(&self, parent_uuid: Option<&str>, uuid: &str, timestamp: &str) -> Value {
        let mut record = json!({
            "parentUuid": parent_uuid,
            "isSidechain": false,
            "userType": "external",
            "cwd": self.cwd,
            "sessionId": self.new_id,
            "gitBranch": self.git_branch,
            "uuid": uuid,
            "timestamp": timestamp,
        });
        if let Some(version) = self.version {
            record["version"] = json!(version);
        }
        record
    }

    fn resolve_timestamp(&mut self, timestamp: Option<&str>) -> String {
        if let Some(timestamp) = timestamp {
            self.last_timestamp = timestamp.to_string();
        }
        self.last_timestamp.clone()
    }
}

fn build_claude_lines(
    new_id: &str,
    cwd: &str,
    parsed: &ParsedCodexRollout,
    identity: &ClaudeIdentity,
    mode: ClaudeImportMode,
) -> ClaudeBuild {
    match mode {
        ClaudeImportMode::Simple => build_simple_claude_lines(new_id, cwd, parsed, identity),
        ClaudeImportMode::Native => build_native_claude_lines(new_id, cwd, parsed, identity),
    }
}

fn build_simple_claude_lines(
    new_id: &str,
    cwd: &str,
    parsed: &ParsedCodexRollout,
    identity: &ClaudeIdentity,
) -> ClaudeBuild {
    let mut builder = ClaudeRecordBuilder::new(new_id, cwd, parsed, identity);
    let mut pending_final: Option<&ConvMessage> = None;
    let start = parsed
        .messages
        .iter()
        .position(|message| message.role == Role::User)
        .unwrap_or(0);
    for message in &parsed.messages[start..] {
        match message.role {
            Role::User => {
                if let Some(final_message) = pending_final.take() {
                    let mut pending = PendingAssistant::default();
                    pending.push_message(final_message);
                    builder.emit_assistant(&mut pending);
                }
                builder.emit_user_message(message);
            }
            Role::Assistant => match message.phase.as_deref() {
                Some("commentary") => {}
                Some("final_answer") | None => pending_final = Some(message),
                Some(_) => {}
            },
        }
    }
    if let Some(final_message) = pending_final {
        let mut pending = PendingAssistant::default();
        pending.push_message(final_message);
        builder.emit_assistant(&mut pending);
    }
    ClaudeBuild {
        lines: builder.lines,
        degraded_tool_events: 0,
    }
}

fn build_native_claude_lines(
    new_id: &str,
    cwd: &str,
    parsed: &ParsedCodexRollout,
    identity: &ClaudeIdentity,
) -> ClaudeBuild {
    let paired_ids = paired_codex_call_ids(&parsed.events);
    let calls: HashMap<&str, &CodexToolEvent> = parsed
        .events
        .iter()
        .filter_map(|event| match event {
            CodexEvent::ToolCall(call) => call.call_id.as_deref().map(|id| (id, call)),
            _ => None,
        })
        .collect();
    let mut builder = ClaudeRecordBuilder::new(new_id, cwd, parsed, identity);
    let mut pending = PendingAssistant::default();
    let mut degraded_tool_events = 0u32;
    let start = parsed
        .events
        .iter()
        .position(|event| matches!(event, CodexEvent::Message(m) if m.role == Role::User))
        .unwrap_or(0);

    for event in &parsed.events[start..] {
        match event {
            CodexEvent::Message(message) if message.role == Role::User => {
                builder.emit_assistant(&mut pending);
                builder.emit_user_message(message);
            }
            CodexEvent::Message(message) => {
                if message.phase.as_deref() == Some("final_answer") && !pending.blocks.is_empty() {
                    builder.emit_assistant(&mut pending);
                }
                pending.push_message(message);
            }
            CodexEvent::ToolCall(call) => {
                let Some(call_id) = call.call_id.as_deref() else {
                    degraded_tool_events += 1;
                    pending.push_note(
                        codex_tool_call_note(&call.payload),
                        call.timestamp.as_deref(),
                    );
                    continue;
                };
                if paired_ids.contains(call_id) {
                    let (name, input) = native_tool_call(&call.payload);
                    pending.push_tool_use(
                        json!({
                            "type": "tool_use",
                            "id": claude_tool_use_id(call_id),
                            "name": name,
                            "input": input,
                        }),
                        call.timestamp.as_deref(),
                    );
                } else {
                    degraded_tool_events += 1;
                    pending.push_note(
                        codex_tool_call_note(&call.payload),
                        call.timestamp.as_deref(),
                    );
                }
            }
            CodexEvent::ToolResult(result) => {
                let Some(call_id) = result.call_id.as_deref() else {
                    degraded_tool_events += 1;
                    pending.push_note(
                        codex_tool_result_note(&result.payload),
                        result.timestamp.as_deref(),
                    );
                    continue;
                };
                if paired_ids.contains(call_id) {
                    builder.emit_assistant(&mut pending);
                    let tool_id = claude_tool_use_id(call_id);
                    if calls
                        .get(call_id)
                        .is_none_or(|call| !builder.emit_tool_result(&tool_id, call, result))
                    {
                        degraded_tool_events += 1;
                        pending.push_note(
                            codex_tool_result_note(&result.payload),
                            result.timestamp.as_deref(),
                        );
                    }
                } else {
                    degraded_tool_events += 1;
                    pending.push_note(
                        codex_tool_result_note(&result.payload),
                        result.timestamp.as_deref(),
                    );
                }
            }
            CodexEvent::ToolNote(note) => {
                degraded_tool_events += 1;
                pending.push_note(note.text.clone(), note.timestamp.as_deref());
            }
        }
    }
    builder.emit_assistant(&mut pending);
    ClaudeBuild {
        lines: builder.lines,
        degraded_tool_events,
    }
}

fn paired_codex_call_ids(events: &[CodexEvent]) -> HashSet<String> {
    let mut active = HashSet::new();
    let mut paired = HashSet::new();
    for event in events {
        match event {
            CodexEvent::Message(message) if message.role == Role::User => active.clear(),
            CodexEvent::ToolCall(call) => {
                if let Some(call_id) = call.call_id.as_deref() {
                    active.insert(call_id.to_string());
                }
            }
            CodexEvent::ToolResult(result) => {
                if let Some(call_id) = result.call_id.as_deref() {
                    if active.contains(call_id) {
                        paired.insert(call_id.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    paired
}

fn claude_user_content(message: &ConvMessage) -> Vec<Value> {
    let mut content = Vec::new();
    if !message.text.trim().is_empty() {
        content.push(json!({"type": "text", "text": message.text}));
    }
    content.extend(message.images.iter().map(|image| {
        json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": image.media_type,
                "data": image.data,
            }
        })
    }));
    content
}

fn claude_native_id(prefix: &str, seed: &str) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let digest = Sha256::digest(seed.as_bytes());
    let suffix = digest
        .iter()
        .take(24)
        .map(|byte| ALPHABET[(*byte as usize) % ALPHABET.len()] as char)
        .collect::<String>();
    format!("{prefix}01{suffix}")
}

fn claude_tool_use_id(call_id: &str) -> String {
    claude_native_id("toolu_", call_id)
}

fn native_tool_call(payload: &Value) -> (String, Value) {
    let raw_name = payload
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| match payload.get("type").and_then(Value::as_str) {
            Some("web_search_call") => Some("WebSearch"),
            Some("tool_search_call") => Some("ToolSearch"),
            _ => None,
        })
        .unwrap_or("unknown");
    if raw_name.eq_ignore_ascii_case("exec") {
        if let Some(source) = payload.get("input").and_then(Value::as_str) {
            let commands = extract_exec_commands(source);
            if !commands.is_empty() {
                return (
                    "Bash".into(),
                    json!({
                        "command": commands.join("\n\n# --- next command ---\n"),
                    }),
                );
            }
        }
    }

    let raw_name_lower = raw_name.to_ascii_lowercase();
    if raw_name_lower == "apply_patch" {
        let patch = payload
            .get("input")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let separator = if patch.ends_with('\n') { "" } else { "\n" };
        return (
            "Bash".into(),
            json!({
                "command": format!("apply_patch <<'PATCH'\n{patch}{separator}PATCH"),
                "description": "Apply patch",
            }),
        );
    }

    let input = codex_native_input(payload);
    match raw_name_lower.as_str() {
        "bash" | "shell" | "shell_command" | "exec_command" => {
            ("Bash".into(), normalize_bash_input(input))
        }
        "view_image" => ("Read".into(), normalize_read_image_input(input)),
        "read" | "read_file" => ("Read".into(), input),
        "write" | "write_file" => ("Write".into(), input),
        "edit" => ("Edit".into(), input),
        "grep" => ("Grep".into(), input),
        "glob" => ("Glob".into(), input),
        "websearch" | "web_search" => ("WebSearch".into(), input),
        "webfetch" | "web_fetch" => ("WebFetch".into(), input),
        "toolsearch" | "tool_search" => ("ToolSearch".into(), input),
        "wait" => ("TaskOutput".into(), normalize_task_output_input(input)),
        "write_stdin" if write_stdin_is_poll(&input) => {
            ("TaskOutput".into(), normalize_task_output_input(input))
        }
        "write_stdin" => ("WriteStdin".into(), input),
        "request_user_input" => (
            "AskUserQuestion".into(),
            normalize_ask_user_question_input(input),
        ),
        "spawn_agent" => ("Agent".into(), normalize_agent_input(input)),
        _ => (sanitize_tool_name(raw_name), input),
    }
}

fn codex_native_input(payload: &Value) -> Value {
    let input = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .or_else(|| payload.get("action"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let parsed = match input {
        Value::String(text) => serde_json::from_str::<Value>(&text)
            .unwrap_or_else(|_| json!({"input": truncate_chars(&text, NOTE_MAX_LEN)})),
        other => other,
    };
    match parsed {
        Value::Object(_) => parsed,
        other => json!({"value": other}),
    }
}

fn normalize_bash_input(input: Value) -> Value {
    let Value::Object(mut object) = input else {
        return json!({"command": input.to_string()});
    };
    let command = object
        .remove("command")
        .or_else(|| object.remove("cmd"))
        .map(|value| match value {
            Value::String(text) => text,
            Value::Array(parts) => parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "[command unavailable]".into());
    let workdir = object
        .remove("workdir")
        .and_then(|value| value.as_str().map(String::from));
    let description = object
        .remove("description")
        .or_else(|| object.remove("justification"))
        .and_then(|value| value.as_str().map(String::from));
    let description = match (description, workdir) {
        (Some(mut description), Some(workdir)) => {
            description.push_str(&format!(" (workdir: {workdir})"));
            Some(description)
        }
        (Some(description), None) => Some(description),
        (None, Some(workdir)) => Some(format!("Run command in {workdir}")),
        (None, None) => None,
    };

    let mut normalized = serde_json::Map::new();
    normalized.insert("command".into(), json!(command));
    if let Some(description) = description {
        normalized.insert("description".into(), json!(description));
    }
    if let Some(timeout) = object
        .remove("timeout")
        .or_else(|| object.remove("timeout_ms"))
        .and_then(|value| value.as_u64())
    {
        normalized.insert("timeout".into(), json!(timeout));
    }
    if let Some(run_in_background) = object
        .remove("run_in_background")
        .and_then(|value| value.as_bool())
    {
        normalized.insert("run_in_background".into(), json!(run_in_background));
    }
    let dangerous = object
        .remove("dangerouslyDisableSandbox")
        .and_then(|value| value.as_bool())
        .or_else(|| {
            object
                .remove("sandbox_permissions")
                .and_then(|value| value.as_str().map(|value| value == "require_escalated"))
        });
    if let Some(dangerous) = dangerous {
        normalized.insert("dangerouslyDisableSandbox".into(), json!(dangerous));
    }
    Value::Object(normalized)
}

fn normalize_read_image_input(input: Value) -> Value {
    let Value::Object(mut object) = input else {
        return json!({"file_path": input.to_string()});
    };
    let file_path = object
        .remove("file_path")
        .or_else(|| object.remove("path"))
        .map(|value| match value {
            Value::String(text) => text,
            other => other.to_string(),
        })
        .unwrap_or_default();
    json!({"file_path": file_path})
}

fn normalize_ask_user_question_input(input: Value) -> Value {
    let questions = input
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| {
            questions
                .iter()
                .filter_map(|question| {
                    let question = question.as_object()?;
                    let prompt = question.get("question")?.as_str()?;
                    let header = question
                        .get("header")
                        .and_then(Value::as_str)
                        .unwrap_or("Question");
                    let multi_select = question
                        .get("multiSelect")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let options = question
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|options| {
                            options
                                .iter()
                                .filter_map(|option| {
                                    let option = option.as_object()?;
                                    Some(json!({
                                        "label": option.get("label")?.as_str()?,
                                        "description": option
                                            .get("description")
                                            .and_then(Value::as_str)
                                            .unwrap_or_default(),
                                    }))
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    Some(json!({
                        "question": prompt,
                        "header": header,
                        "multiSelect": multi_select,
                        "options": options,
                    }))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({"questions": questions})
}

fn write_stdin_is_poll(input: &Value) -> bool {
    input
        .get("chars")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
}

fn normalize_agent_input(input: Value) -> Value {
    let Value::Object(mut object) = input else {
        return json!({
            "description": "Background task",
            "prompt": input.to_string(),
            "subagent_type": "general-purpose",
        });
    };
    let description = object
        .remove("description")
        .or_else(|| object.remove("task_name"))
        .and_then(|value| value.as_str().map(String::from))
        .unwrap_or_else(|| "Background task".into());
    let prompt = object
        .remove("prompt")
        .or_else(|| object.remove("message"))
        .and_then(|value| value.as_str().map(String::from))
        .unwrap_or_else(|| "Continue the assigned task".into());
    let subagent_type = object
        .remove("subagent_type")
        .and_then(|value| value.as_str().map(String::from))
        .unwrap_or_else(|| "general-purpose".into());
    let mut normalized = json!({
        "description": description,
        "prompt": prompt,
        "subagent_type": subagent_type,
    });
    if let Some(run_in_background) = object
        .remove("run_in_background")
        .and_then(|value| value.as_bool())
    {
        normalized["run_in_background"] = json!(run_in_background);
    }
    normalized
}

fn normalize_task_output_input(input: Value) -> Value {
    let Value::Object(mut object) = input else {
        return json!({"task_id": input.to_string(), "block": true, "timeout": 300000});
    };
    let task_id = object
        .remove("task_id")
        .or_else(|| object.remove("cell_id"))
        .or_else(|| object.remove("session_id"))
        .map(|value| match value {
            Value::String(text) => text,
            other => other.to_string(),
        })
        .unwrap_or_else(|| "task".into());
    let timeout = object
        .remove("timeout")
        .or_else(|| object.remove("yield_time_ms"))
        .and_then(|value| value.as_u64())
        .unwrap_or(300_000)
        .min(300_000);
    json!({"task_id": task_id, "block": true, "timeout": timeout})
}

fn sanitize_tool_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        .take(48)
        .collect();
    if safe.is_empty() {
        "unknown".into()
    } else {
        safe
    }
}

fn extract_exec_commands(source: &str) -> Vec<String> {
    const MARKER: &str = "tools.exec_command";
    let mut commands = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative) = source[cursor..].find(MARKER) {
        let start = cursor + relative + MARKER.len();
        let end = source[start..]
            .find(MARKER)
            .map(|next| start + next)
            .unwrap_or(source.len());
        if let Some(command) = extract_json_string_field(&source[start..end], "cmd") {
            commands.push(command);
        }
        cursor = end;
        if cursor >= source.len() {
            break;
        }
    }
    if commands.is_empty() {
        extract_json_string_fields(source, "cmd")
    } else {
        commands
    }
}

fn extract_json_string_field(source: &str, field: &str) -> Option<String> {
    extract_json_string_fields(source, field).into_iter().next()
}

fn extract_json_string_fields(source: &str, field: &str) -> Vec<String> {
    let markers = [format!("{field}:"), format!("\"{field}\":")];
    let mut values = Vec::new();
    let mut cursor = 0usize;
    while cursor < source.len() {
        let next = markers
            .iter()
            .filter_map(|marker| source[cursor..].find(marker).map(|index| (index, marker)))
            .min_by_key(|(index, _)| *index);
        let Some((relative, marker)) = next else {
            break;
        };
        let start = cursor + relative + marker.len();
        let value = source[start..].trim_start();
        let mut stream = serde_json::Deserializer::from_str(value).into_iter::<String>();
        if let Some(Ok(text)) = stream.next() {
            values.push(text);
        }
        cursor = start;
    }
    values
}

fn codex_tool_event_is_error(payload: &Value) -> bool {
    payload.get("is_error").and_then(Value::as_bool) == Some(true)
        || matches!(
            payload.get("status").and_then(Value::as_str),
            Some("error" | "failed")
        )
        || codex_tool_exit_code(payload).is_some_and(|code| code != 0)
}

fn codex_tool_exit_code(payload: &Value) -> Option<i64> {
    let output = raw_codex_tool_output(payload);
    exit_code_from_value(&output).or_else(|| match output {
        Value::String(text) => parse_exit_code_text(&text),
        _ => None,
    })
}

fn exit_code_from_value(value: &Value) -> Option<i64> {
    match value {
        Value::Object(object) => object
            .get("exit_code")
            .or_else(|| object.get("exitCode"))
            .and_then(Value::as_i64)
            .or_else(|| object.get("metadata").and_then(exit_code_from_value)),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .as_ref()
            .and_then(exit_code_from_value),
        _ => None,
    }
}

fn parse_exit_code_text(text: &str) -> Option<i64> {
    const PREFIXES: [&str; 6] = [
        "Exit code:",
        "Exit code ",
        "Process exited with code:",
        "Process exited with code ",
        "Script exited with code:",
        "Script exited with code ",
    ];
    text.lines().take(8).find_map(|line| {
        let line = line.trim();
        PREFIXES.iter().find_map(|prefix| {
            line.strip_prefix(prefix)
                .and_then(|rest| rest.trim().split_whitespace().next())
                .and_then(|code| code.parse::<i64>().ok())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cc-sessions-convert-{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_lines(path: &Path, lines: &[Value]) {
        let mut out = fs::File::create(path).unwrap();
        for line in lines {
            writeln!(out, "{}", serde_json::to_string(line).unwrap()).unwrap();
        }
    }

    fn claude_record(kind: &str, text: &str, cwd: &str) -> Value {
        json!({
            "type": kind,
            "isSidechain": false,
            "cwd": cwd,
            "sessionId": "src-session",
            "timestamp": "2026-07-20T10:00:00.000Z",
            "message": {
                "role": kind,
                "content": [{"type": "text", "text": text}],
            },
        })
    }

    #[test]
    fn claude_to_codex_produces_official_rollout_shape() {
        let root = temp_dir("cla2codex");
        let codex = root.join("codex");
        fs::create_dir_all(&codex).unwrap();
        let source = root.join("session.jsonl");
        write_lines(
            &source,
            &[
                claude_record("user", "第一个问题", "F:\\demo\\project"),
                json!({
                    "type": "assistant",
                    "cwd": "F:\\demo\\project",
                    "timestamp": "2026-07-20T10:00:05.000Z",
                    "message": {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "内部推理", "signature": "sig"},
                        {"type": "text", "text": "第一个回答"},
                        {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}},
                    ]},
                }),
                json!({
                    "type": "user",
                    "cwd": "F:\\demo\\project",
                    "timestamp": "2026-07-20T10:00:06.000Z",
                    "message": {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "t1", "content": "file-a\nfile-b"},
                    ]},
                }),
            ],
        );

        let report = convert_claude_to_codex(
            codex.to_string_lossy().as_ref(),
            source.to_string_lossy().as_ref(),
            CodexImportMode::Simple,
        )
        .unwrap();

        assert_eq!(report.target_provider, "codex");
        assert_eq!(report.conversion_mode.as_deref(), Some("simple"));
        assert_eq!(report.dropped_reasoning, 1);
        assert_eq!(report.tool_notes, 2);
        let content = fs::read_to_string(&report.new_path).unwrap();
        let lines: Vec<Value> = content
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["type"], "session_meta");
        assert_eq!(lines[0]["payload"]["id"], report.new_id);
        assert_eq!(lines[0]["payload"]["cwd"], "F:\\demo\\project");
        assert_eq!(lines[0]["payload"]["originator"], "Codex Desktop");
        assert_eq!(lines[0]["payload"]["source"], "vscode");
        assert_eq!(lines[0]["payload"]["cli_version"], "0.0.0");
        assert_eq!(lines[1]["payload"]["type"], "task_started");
        assert_eq!(lines[2]["payload"]["type"], "user_message");
        assert_eq!(lines[2]["payload"]["message"], "第一个问题");
        assert_eq!(lines[3]["payload"]["type"], "message");
        assert_eq!(lines[3]["payload"]["content"][0]["type"], "input_text");
        // 思考被丢弃、工具转注记；tool_result-only 的 user 记录归为 assistant。
        assert!(!content.contains("内部推理"));
        assert!(content.contains(TOOL_CALL_TAG));
        assert!(!content.contains(LEGACY_IMPORTED_MARKER));
        let last: &Value = lines.last().unwrap();
        assert_eq!(last["payload"]["type"], "task_complete");
    }

    #[test]
    fn claude_to_codex_native_mode_writes_tool_events_and_images() {
        let root = temp_dir("cla2codex-native");
        let codex = root.join("codex");
        fs::create_dir_all(&codex).unwrap();
        let source = root.join("session.jsonl");
        write_lines(
            &source,
            &[
                json!({
                    "type": "user",
                    "cwd": "F:\\demo\\project",
                    "sessionId": "src-native",
                    "timestamp": "2026-07-20T10:00:00.000Z",
                    "message": {"role": "user", "content": [
                        {"type": "text", "text": "检查并修复"},
                        {"type": "image", "source": {
                            "type": "base64", "media_type": "image/png", "data": "QUJD"
                        }}
                    ]},
                }),
                json!({
                    "type": "assistant",
                    "cwd": "F:\\demo\\project",
                    "timestamp": "2026-07-20T10:00:01.000Z",
                    "message": {"role": "assistant", "content": [
                        {"type": "text", "text": "我先检查文件。"},
                        {"type": "tool_use", "id": "toolu_bash", "name": "Bash",
                            "input": {"command": "pwd", "description": "Inspect cwd"}},
                        {"type": "tool_use", "id": "toolu_edit", "name": "Edit",
                            "input": {"file_path": "F:\\demo\\project\\a.txt", "old_string": "a", "new_string": "b"}}
                    ]},
                }),
                json!({
                    "type": "user",
                    "cwd": "F:\\demo\\project",
                    "timestamp": "2026-07-20T10:00:02.000Z",
                    "message": {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_bash", "content": "F:\\demo\\project"},
                        {"type": "tool_result", "tool_use_id": "toolu_edit", "is_error": true,
                            "content": [
                                {"type": "text", "text": "replace failed"},
                                {"type": "image", "source": {
                                    "type": "base64", "media_type": "image/png", "data": "REVG"
                                }}
                            ]}
                    ]},
                }),
                json!({
                    "type": "assistant",
                    "cwd": "F:\\demo\\project",
                    "timestamp": "2026-07-20T10:00:03.000Z",
                    "message": {"role": "assistant", "content": [
                        {"type": "text", "text": "检查完成。"}
                    ]},
                }),
            ],
        );

        let report = convert_claude_to_codex(
            codex.to_string_lossy().as_ref(),
            source.to_string_lossy().as_ref(),
            CodexImportMode::Native,
        )
        .unwrap();
        assert_eq!(report.conversion_mode.as_deref(), Some("native"));
        assert_eq!(report.tool_notes, 4);
        let content = fs::read_to_string(&report.new_path).unwrap();
        let records = content
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let response_items = records
            .iter()
            .filter(|record| record["type"] == "response_item")
            .map(|record| &record["payload"])
            .collect::<Vec<_>>();

        assert!(!content.contains(&format!("[{TOOL_CALL_TAG}:")));
        let user = response_items
            .iter()
            .find(|payload| payload["type"] == "message" && payload["role"] == "user")
            .unwrap();
        assert!(user["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| block["type"] == "input_image"
                && block["image_url"] == "data:image/png;base64,QUJD"));

        let calls = response_items
            .iter()
            .filter(|payload| payload["type"] == "function_call")
            .copied()
            .collect::<Vec<_>>();
        let outputs = response_items
            .iter()
            .filter(|payload| payload["type"] == "function_call_output")
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2);
        assert_eq!(outputs.len(), 2);
        assert!(calls.iter().all(|call| call["call_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("call_"))));
        let bash = calls
            .iter()
            .find(|call| call["name"] == "shell_command")
            .unwrap();
        let bash_args: Value = serde_json::from_str(bash["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(bash_args["command"], "pwd");
        assert_eq!(bash_args["workdir"], "F:\\demo\\project");
        let edit = calls.iter().find(|call| call["name"] == "Edit").unwrap();
        let edit_args: Value = serde_json::from_str(edit["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(edit_args["old_string"], "a");
        assert!(outputs
            .iter()
            .any(|output| output["output"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|block| {
                    block["type"] == "input_image"
                        && block["image_url"] == "data:image/png;base64,REVG"
                }))));

        let assistant_messages = response_items
            .iter()
            .filter(|payload| payload["type"] == "message" && payload["role"] == "assistant")
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(assistant_messages[0]["phase"], "commentary");
        assert_eq!(assistant_messages.last().unwrap()["phase"], "final_answer");
    }

    #[test]
    fn claude_to_codex_native_mode_degrades_unpaired_tool_events() {
        let root = temp_dir("cla2codex-native-unpaired");
        let codex = root.join("codex");
        fs::create_dir_all(&codex).unwrap();
        let source = root.join("session.jsonl");
        write_lines(
            &source,
            &[
                claude_record("user", "继续", "F:\\demo\\project"),
                json!({
                    "type": "assistant",
                    "cwd": "F:\\demo\\project",
                    "timestamp": "2026-07-20T10:00:01.000Z",
                    "message": {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "toolu_orphan", "name": "Bash",
                            "input": {"command": "echo orphan"}}
                    ]},
                }),
            ],
        );

        let report = convert_claude_to_codex(
            codex.to_string_lossy().as_ref(),
            source.to_string_lossy().as_ref(),
            CodexImportMode::Native,
        )
        .unwrap();
        let content = fs::read_to_string(&report.new_path).unwrap();
        assert!(content.contains("[tool_call: Bash]"));
        assert!(!content.contains("\"type\":\"function_call\""));
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("1 条工具事件无法配对")));
    }

    #[test]
    fn claude_to_codex_requires_cwd() {
        let root = temp_dir("cla2codex-nocwd");
        let codex = root.join("codex");
        fs::create_dir_all(&codex).unwrap();
        let source = root.join("session.jsonl");
        write_lines(
            &source,
            &[json!({
                "type": "user",
                "message": {"role": "user", "content": "hello"},
            })],
        );
        let error = convert_claude_to_codex(
            codex.to_string_lossy().as_ref(),
            source.to_string_lossy().as_ref(),
            CodexImportMode::Simple,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cwd"));
    }

    #[test]
    fn claude_to_codex_uses_import_time_for_rollout_record_timestamps() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-23T12:34:56.789Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let parsed = ParsedClaudeSession {
            cwd: Some("F:\\demo\\project".into()),
            messages: vec![
                ConvMessage {
                    role: Role::User,
                    text: "历史问题".into(),
                    timestamp: Some("2020-01-02T03:04:05.000Z".into()),
                    phase: None,
                    images: Vec::new(),
                },
                ConvMessage {
                    role: Role::Assistant,
                    text: "历史回答".into(),
                    timestamp: Some("2020-01-02T03:04:06.000Z".into()),
                    phase: None,
                    images: Vec::new(),
                },
            ],
            ..Default::default()
        };

        let lines = build_codex_lines(
            "new-session",
            "F:\\demo\\project",
            "openai",
            &parsed,
            &CodexIdentity::default(),
            &now,
            CodexImportMode::Simple,
        )
        .lines;
        let records: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let import_timestamp = "2026-07-23T12:34:56.789Z";
        assert!(records
            .iter()
            .all(|record| record["timestamp"] == import_timestamp));

        let started = records
            .iter()
            .find(|record| record["payload"]["type"] == "task_started")
            .unwrap();
        assert_eq!(started["payload"]["started_at"], 1_577_934_245i64);
        let completed = records
            .iter()
            .find(|record| record["payload"]["type"] == "task_complete")
            .unwrap();
        assert_eq!(completed["payload"]["completed_at"], 1_577_934_246i64);
    }

    #[test]
    fn codex_import_mode_defaults_to_simple_and_accepts_native() {
        assert_eq!(
            CodexImportMode::parse(None).unwrap(),
            CodexImportMode::Simple
        );
        assert_eq!(
            CodexImportMode::parse(Some("native")).unwrap(),
            CodexImportMode::Native
        );
        assert!(CodexImportMode::parse(Some("lossless")).is_err());
    }

    #[test]
    fn codex_to_claude_builds_parent_uuid_chain() {
        let root = temp_dir("codex2cla");
        let claude = root.join("claude");
        fs::create_dir_all(paths::claude_projects_dir(&claude)).unwrap();
        let source = root.join("rollout.jsonl");
        write_lines(
            &source,
            &[
                json!({
                    "timestamp": "2026-07-20T10:00:00.000Z",
                    "type": "session_meta",
                    "payload": {"id": "src-codex-id", "cwd": "F:\\demo\\project", "git": {"branch": "main"}},
                }),
                json!({
                    "timestamp": "2026-07-20T10:00:01.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "<environment_context>\n  <cwd>F:\\demo\\project</cwd>\n</environment_context>"}]},
                }),
                json!({
                    "timestamp": "2026-07-20T10:00:02.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "帮我看看"}]},
                }),
                json!({
                    "timestamp": "2026-07-20T10:00:03.000Z",
                    "type": "response_item",
                    "payload": {"type": "reasoning", "content": [{"type": "encrypted_content", "encrypted_content": "xxx"}]},
                }),
                json!({
                    "timestamp": "2026-07-20T10:00:04.000Z",
                    "type": "response_item",
                    "payload": {"type": "function_call", "name": "shell", "arguments": "{\"cmd\":[\"ls\"]}"},
                }),
                json!({
                    "timestamp": "2026-07-20T10:00:04.100Z",
                    "type": "response_item",
                    "payload": {"type": "function_call_output", "output": [
                        {"type": "text", "text": "function result"},
                        {"type": "image_url", "image_url": "data:function-image"},
                    ]},
                }),
                json!({
                    "timestamp": "2026-07-20T10:00:04.200Z",
                    "type": "response_item",
                    "payload": {"type": "custom_tool_call", "name": "commentary", "input": "{\"path\":\"README.md\"}"},
                }),
                json!({
                    "timestamp": "2026-07-20T10:00:04.300Z",
                    "type": "response_item",
                    "payload": {"type": "custom_tool_call_output", "output": [
                        {"type": "text", "text": "custom result"},
                        {"type": "image_url", "image_url": "data:custom-image"},
                    ]},
                }),
                json!({
                    "timestamp": "2026-07-20T10:00:04.400Z",
                    "type": "response_item",
                    "payload": {"type": "web_search_call", "status": "completed", "action": {
                        "type": "search", "query": "Codex", "queries": ["Codex"]
                    }},
                }),
                json!({
                    "timestamp": "2026-07-20T10:00:04.500Z",
                    "type": "response_item",
                    "payload": {"type": "tool_search_call", "status": "completed", "arguments": {
                        "query": "calendar", "limit": 5
                    }},
                }),
                json!({
                    "timestamp": "2026-07-20T10:00:04.600Z",
                    "type": "response_item",
                    "payload": {"type": "tool_search_output", "status": "completed", "tools": [
                        {"type": "mcp", "name": "calendar.search", "description": "搜索日历", "tools": []}
                    ]},
                }),
                json!({
                    "timestamp": "2026-07-20T10:00:04.700Z",
                    "type": "response_item",
                    "payload": {"type": "image_generation_call", "status": "completed",
                        "revised_prompt": "生成一张测试图片",
                        "result": "data:image/png;base64,SECRET_IMAGE_BYTES"},
                }),
                json!({
                    "timestamp": "2026-07-20T10:00:05.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "看完了"}]},
                }),
            ],
        );

        let report = convert_codex_to_claude(
            "",
            claude.to_string_lossy().as_ref(),
            source.to_string_lossy().as_ref(),
            ClaudeImportMode::Native,
        )
        .unwrap();

        assert_eq!(report.source_id, "src-codex-id");
        assert_eq!(report.dropped_reasoning, 1);
        assert_eq!(report.tool_notes, 8);
        assert!(report.new_path.contains("F--demo-project"));
        let content = fs::read_to_string(&report.new_path).unwrap();
        let lines: Vec<Value> = content
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // 内部上下文不迁移
        assert!(!content.contains("environment_context"));
        assert!(!content.contains(LEGACY_IMPORTED_MARKER));
        assert!(content.contains("function result"));
        assert!(content.contains("custom result"));
        assert!(content.contains("tool_call: commentary"));
        assert!(content.contains("tool_call: web_search"));
        assert!(content.contains("tool_call: tool_search"));
        assert!(content.contains("calendar.search"));
        assert!(content.contains("image output omitted"));
        assert!(!content.contains("data:function-image"));
        assert!(!content.contains("data:custom-image"));
        assert!(!content.contains("SECRET_IMAGE_BYTES"));
        // parentUuid 链条连续、sessionId 一致
        assert!(lines[0]["parentUuid"].is_null());
        for pair in lines.windows(2) {
            assert_eq!(pair[1]["parentUuid"], pair[0]["uuid"]);
        }
        for line in &lines {
            assert_eq!(line["sessionId"], report.new_id);
            assert_eq!(line["gitBranch"], "main");
        }
        assert_eq!(lines[0]["type"], "user");
        assert_eq!(lines[0]["message"]["content"][0]["text"], "帮我看看");
    }

    #[test]
    fn codex_to_claude_keeps_final_answer_visible_and_resume_command_project_scoped() {
        let root = temp_dir("codex2cla-visible-final");
        let claude = root.join("claude");
        fs::create_dir_all(paths::claude_projects_dir(&claude)).unwrap();
        let source = root.join("rollout.jsonl");
        write_lines(
            &source,
            &[
                json!({
                    "timestamp": "2026-07-23T07:00:00.000Z",
                    "type": "session_meta",
                    "payload": {"id": "src-visible-final", "cwd": "F:\\demo\\project"},
                }),
                json!({
                    "timestamp": "2026-07-23T07:00:01.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "请排查问题"}]},
                }),
                json!({
                    "timestamp": "2026-07-23T07:00:02.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "assistant", "phase": "commentary",
                        "content": [{"type": "output_text", "text": "正在检查"}]},
                }),
                json!({
                    "timestamp": "2026-07-23T07:00:03.000Z",
                    "type": "response_item",
                    "payload": {"type": "function_call", "name": "exec", "arguments": "{\"cmd\":\"echo ok\"}"},
                }),
                json!({
                    "timestamp": "2026-07-23T07:00:04.000Z",
                    "type": "response_item",
                    "payload": {"type": "function_call_output", "output": "ok"},
                }),
                json!({
                    "timestamp": "2026-07-23T07:00:05.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "assistant", "phase": "final_answer",
                        "content": [{"type": "output_text", "text": "这是最终结论"}]},
                }),
            ],
        );

        let report = convert_codex_to_claude(
            "",
            claude.to_string_lossy().as_ref(),
            source.to_string_lossy().as_ref(),
            ClaudeImportMode::Simple,
        )
        .unwrap();
        let timeline = crate::rollout::preview_session_user_prompts(
            Some("claude".into()),
            report.new_path.clone(),
        )
        .unwrap();

        assert_eq!(timeline.prompts.len(), 1);
        assert_eq!(timeline.prompts[0].text, "请排查问题");
        assert_eq!(
            timeline.prompts[0]
                .response
                .as_ref()
                .map(|reply| reply.text.as_str()),
            Some("这是最终结论")
        );
        assert!(!fs::read_to_string(&report.new_path)
            .unwrap()
            .contains(LEGACY_IMPORTED_MARKER));
        assert!(report.resume_command.contains("F:\\demo\\project"));
        assert!(report
            .resume_command
            .ends_with(&format!("claude --resume {}", report.new_id)));
    }

    #[test]
    fn codex_to_claude_simple_mode_keeps_only_users_and_turn_finals() {
        let root = temp_dir("codex2cla-simple");
        let claude = root.join("claude");
        fs::create_dir_all(paths::claude_projects_dir(&claude)).unwrap();
        let source = root.join("rollout.jsonl");
        write_lines(
            &source,
            &[
                json!({
                    "timestamp": "2026-07-23T08:00:00.000Z",
                    "type": "session_meta",
                    "payload": {"id": "src-simple", "cwd": "F:\\demo\\project"},
                }),
                json!({
                    "timestamp": "2026-07-23T08:00:01.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "user", "content": [
                        {"type": "input_text", "text": "第一问"},
                        {"type": "input_image", "image_url": "data:image/png;base64,QUJD", "detail": "auto"}
                    ]},
                }),
                json!({
                    "timestamp": "2026-07-23T08:00:02.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "assistant", "phase": "commentary",
                        "content": [{"type": "output_text", "text": "处理中"}]},
                }),
                json!({
                    "timestamp": "2026-07-23T08:00:03.000Z",
                    "type": "response_item",
                    "payload": {"type": "function_call", "call_id": "call_simple", "name": "Bash",
                        "arguments": "{\"command\":\"echo hidden\"}"},
                }),
                json!({
                    "timestamp": "2026-07-23T08:00:04.000Z",
                    "type": "response_item",
                    "payload": {"type": "function_call_output", "call_id": "call_simple", "output": "hidden"},
                }),
                json!({
                    "timestamp": "2026-07-23T08:00:05.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "assistant", "phase": "final_answer",
                        "content": [{"type": "output_text", "text": "第一答"}]},
                }),
                json!({
                    "timestamp": "2026-07-23T08:01:00.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "第二问"}]},
                }),
                json!({
                    "timestamp": "2026-07-23T08:01:01.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "旧格式过程"}]},
                }),
                json!({
                    "timestamp": "2026-07-23T08:01:02.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "旧格式最终答复"}]},
                }),
            ],
        );

        let report = convert_codex_to_claude(
            "",
            claude.to_string_lossy().as_ref(),
            source.to_string_lossy().as_ref(),
            ClaudeImportMode::Simple,
        )
        .unwrap();
        let content = fs::read_to_string(&report.new_path).unwrap();
        let lines: Vec<Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["type"], "user");
        assert_eq!(lines[0]["message"]["content"][0]["text"], "第一问");
        assert_eq!(lines[0]["message"]["content"][1]["type"], "image");
        assert_eq!(
            lines[0]["message"]["content"][1]["source"]["media_type"],
            "image/png"
        );
        assert_eq!(lines[1]["message"]["content"][0]["text"], "第一答");
        assert_eq!(lines[2]["message"]["content"][0]["text"], "第二问");
        assert_eq!(lines[3]["message"]["content"][0]["text"], "旧格式最终答复");
        assert!(!content.contains("处理中"));
        assert!(!content.contains("echo hidden"));
        assert!(!content.contains("旧格式过程"));
        assert!(!content.contains("tool_use"));
        assert!(!content.contains("tool_result"));
    }

    #[test]
    fn codex_to_claude_native_mode_emits_complete_tool_pairs_and_degrades_orphans() {
        let root = temp_dir("codex2cla-native");
        let claude = root.join("claude");
        fs::create_dir_all(paths::claude_projects_dir(&claude)).unwrap();
        let source = root.join("rollout.jsonl");
        write_lines(
            &source,
            &[
                json!({
                    "timestamp": "2026-07-23T09:00:00.000Z",
                    "type": "session_meta",
                    "payload": {"id": "src-native", "cwd": "F:\\demo\\project"},
                }),
                json!({
                    "timestamp": "2026-07-23T09:00:01.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "执行检查"}]},
                }),
                json!({
                    "timestamp": "2026-07-23T09:00:02.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "assistant", "phase": "commentary",
                        "content": [{"type": "output_text", "text": "开始检查"}]},
                }),
                json!({
                    "timestamp": "2026-07-23T09:00:03.000Z",
                    "type": "response_item",
                    "payload": {"type": "function_call", "call_id": "call_one", "name": "Bash",
                        "arguments": "{\"command\":\"echo one\"}"},
                }),
                json!({
                    "timestamp": "2026-07-23T09:00:03.100Z",
                    "type": "response_item",
                    "payload": {"type": "function_call", "call_id": "call_two", "name": "Bash",
                        "arguments": "{\"command\":\"echo two\"}"},
                }),
                json!({
                    "timestamp": "2026-07-23T09:00:03.200Z",
                    "type": "response_item",
                    "payload": {"type": "function_call", "call_id": "call_orphan", "name": "Bash",
                        "arguments": "{\"command\":\"echo orphan\"}"},
                }),
                json!({
                    "timestamp": "2026-07-23T09:00:04.000Z",
                    "type": "response_item",
                    "payload": {"type": "function_call_output", "call_id": "call_one", "output": "one"},
                }),
                json!({
                    "timestamp": "2026-07-23T09:00:04.100Z",
                    "type": "response_item",
                    "payload": {"type": "function_call_output", "call_id": "call_two", "output": "two"},
                }),
                json!({
                    "timestamp": "2026-07-23T09:00:04.200Z",
                    "type": "response_item",
                    "payload": {"type": "function_call_output", "call_id": "call_result_only", "output": "orphan result"},
                }),
                json!({
                    "timestamp": "2026-07-23T09:00:05.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "assistant", "phase": "final_answer",
                        "content": [{"type": "output_text", "text": "检查完成"}]},
                }),
            ],
        );

        let report = convert_codex_to_claude(
            "",
            claude.to_string_lossy().as_ref(),
            source.to_string_lossy().as_ref(),
            ClaudeImportMode::Native,
        )
        .unwrap();
        let content = fs::read_to_string(&report.new_path).unwrap();
        let lines: Vec<Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let tool_record = lines
            .iter()
            .find(|line| {
                line["message"]["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_use"))
            })
            .unwrap();
        let tool_blocks: Vec<&Value> = tool_record["message"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|block| block["type"] == "tool_use")
            .collect();
        assert_eq!(tool_blocks.len(), 2);
        assert_eq!(tool_record["message"]["stop_reason"], "tool_use");
        assert!(tool_record["message"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| block["text"] == "开始检查"));

        let tool_ids: std::collections::HashSet<String> = tool_blocks
            .iter()
            .map(|block| block["id"].as_str().unwrap().to_string())
            .collect();
        let result_records: Vec<&Value> = lines
            .iter()
            .filter(|line| {
                line["message"]["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().all(|block| block["type"] == "tool_result"))
            })
            .collect();
        let result_ids: std::collections::HashSet<String> = result_records
            .iter()
            .map(|line| {
                line["message"]["content"][0]["tool_use_id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(result_ids, tool_ids);
        assert_eq!(result_records.len(), 2);
        for result in &result_records {
            assert_eq!(result["type"], "user");
            assert_eq!(result["parentUuid"], tool_record["uuid"]);
            assert_eq!(result["sourceToolAssistantUUID"], tool_record["uuid"]);
            assert!(result["toolUseResult"]["stdout"].is_string());
        }
        assert!(content.contains("tool_call: Bash"));
        assert!(content.contains("tool_result"));
        assert!(content.contains("orphan result"));
        assert!(!tool_blocks
            .iter()
            .any(|block| block["input"]["command"] == "echo orphan"));

        let final_index = lines
            .iter()
            .rposition(|line| line["type"] == "assistant")
            .unwrap();
        let final_record = &lines[final_index];
        assert_eq!(final_record["message"]["stop_reason"], "end_turn");
        assert_eq!(final_record["message"]["content"][0]["text"], "检查完成");
        assert_eq!(final_record["parentUuid"], lines[final_index - 1]["uuid"]);
        assert_eq!(lines[final_index - 1]["message"]["phase"], "commentary");
    }

    #[test]
    fn native_tool_call_maps_codex_exec_wrapper_to_bash_commands() {
        let payload = json!({
            "type": "custom_tool_call",
            "name": "exec",
            "input": concat!(
                "const first = await tools.exec_command({cmd:\"rg -n \\\"hello\\\" src\",workdir:\"F:\\\\demo\"});\n",
                "const second = await tools.exec_command({\"cmd\":\"cargo test\"});\n",
                "text(first.output); text(second.output);"
            ),
        });

        let (name, input) = native_tool_call(&payload);
        assert_eq!(name, "Bash");
        assert_eq!(
            input["command"],
            "rg -n \"hello\" src\n\n# --- next command ---\ncargo test"
        );
    }

    #[test]
    fn native_tool_call_maps_verified_codex_tools_to_claude_shapes() {
        let shell = json!({
            "type": "function_call",
            "name": "shell_command",
            "arguments": "{\"command\":\"Get-Content README.md\",\"workdir\":\"F:\\\\demo\",\"timeout_ms\":10000}",
        });
        let (shell_name, shell_input) = native_tool_call(&shell);
        assert_eq!(shell_name, "Bash");
        assert_eq!(shell_input["command"], "Get-Content README.md");
        assert_eq!(shell_input["timeout"], 10_000);
        assert!(shell_input.get("workdir").is_none());
        assert!(shell_input.get("timeout_ms").is_none());
        assert!(shell_input["description"]
            .as_str()
            .unwrap()
            .contains("F:\\demo"));

        let patch_text =
            "*** Begin Patch\n*** Update File: demo.txt\n@@\n-old\n+new\n*** End Patch\n";
        let patch = json!({
            "type": "custom_tool_call",
            "name": "apply_patch",
            "input": patch_text,
        });
        let (patch_name, patch_input) = native_tool_call(&patch);
        assert_eq!(patch_name, "Bash");
        assert!(patch_input["command"]
            .as_str()
            .unwrap()
            .contains(patch_text));
        assert_eq!(patch_input["description"], "Apply patch");

        let image = json!({
            "type": "function_call",
            "name": "view_image",
            "arguments": "{\"path\":\"F:\\\\demo\\\\screen.png\",\"detail\":\"original\"}",
        });
        let (image_name, image_input) = native_tool_call(&image);
        assert_eq!(image_name, "Read");
        assert_eq!(image_input, json!({"file_path": "F:\\demo\\screen.png"}));

        let question = json!({
            "type": "function_call",
            "name": "request_user_input",
            "arguments": serde_json::to_string(&json!({
                "questions": [{
                    "id": "choice",
                    "header": "Choice",
                    "question": "Pick one?",
                    "options": [{"label": "A", "description": "First"}]
                }]
            })).unwrap(),
        });
        let (question_name, question_input) = native_tool_call(&question);
        assert_eq!(question_name, "AskUserQuestion");
        assert_eq!(question_input["questions"][0]["multiSelect"], false);
        assert!(question_input["questions"][0].get("id").is_none());

        let poll = json!({
            "type": "function_call",
            "name": "write_stdin",
            "arguments": "{\"session_id\":84672,\"chars\":\"\",\"yield_time_ms\":1000}",
        });
        let (poll_name, poll_input) = native_tool_call(&poll);
        assert_eq!(poll_name, "TaskOutput");
        assert_eq!(poll_input["task_id"], "84672");
        assert_eq!(poll_input["block"], true);
        assert_eq!(poll_input["timeout"], 1_000);

        let interactive = json!({
            "type": "function_call",
            "name": "write_stdin",
            "arguments": "{\"session_id\":84672,\"chars\":\"y\\n\"}",
        });
        let (interactive_name, interactive_input) = native_tool_call(&interactive);
        assert_eq!(interactive_name, "WriteStdin");
        assert_eq!(interactive_input["chars"], "y\n");

        let agent = json!({
            "type": "function_call",
            "name": "spawn_agent",
            "arguments": serde_json::to_string(&json!({
                "task_name": "audit_tools",
                "message": "Audit the tool formats",
                "subagent_type": "research",
                "run_in_background": true
            })).unwrap(),
        });
        let (agent_name, agent_input) = native_tool_call(&agent);
        assert_eq!(agent_name, "Agent");
        assert_eq!(agent_input["description"], "audit_tools");
        assert_eq!(agent_input["prompt"], "Audit the tool formats");
        assert_eq!(agent_input["subagent_type"], "research");
        assert_eq!(agent_input["run_in_background"], true);
    }

    #[test]
    fn native_tool_results_unwrap_custom_outputs_and_detect_exit_failures() {
        let wrapped = json!({
            "output": json!({
                "output": "Success. Updated demo.txt",
                "metadata": {"exit_code": 0, "duration_seconds": 0.1}
            })
            .to_string()
        });
        let native = native_tool_result_data(&wrapped);
        assert_eq!(native.content, "Success. Updated demo.txt");
        assert!(!codex_tool_event_is_error(&wrapped));

        let wrapped_error = json!({
            "output": json!({
                "output": "Patch failed",
                "metadata": {"exit_code": 1}
            })
            .to_string()
        });
        assert!(codex_tool_event_is_error(&wrapped_error));
        assert!(codex_tool_event_is_error(&json!({
            "output": "Exit code: 2\nWall time: 0.1 seconds\nOutput:\ncommand failed"
        })));
    }

    #[test]
    fn native_mode_preserves_image_question_and_large_tool_results() {
        let root = temp_dir("codex2cla-rich-native-results");
        let claude = root.join("claude");
        fs::create_dir_all(paths::claude_projects_dir(&claude)).unwrap();
        let source = root.join("rollout.jsonl");
        let image_data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Y9Z7JkAAAAASUVORK5CYII=";
        let large_output = format!("{}TAIL", "x".repeat(12_000));
        write_lines(
            &source,
            &[
                json!({
                    "timestamp": "2026-07-23T11:00:00.000Z",
                    "type": "session_meta",
                    "payload": {"id": "src-rich-native", "cwd": "F:\\demo\\project"},
                }),
                json!({
                    "timestamp": "2026-07-23T11:00:01.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "检查富工具结果"}]},
                }),
                json!({
                    "timestamp": "2026-07-23T11:00:02.000Z",
                    "type": "response_item",
                    "payload": {"type": "function_call", "call_id": "call_image", "name": "view_image",
                        "arguments": "{\"path\":\"F:\\\\demo\\\\screen.png\",\"detail\":\"original\"}"},
                }),
                json!({
                    "timestamp": "2026-07-23T11:00:02.100Z",
                    "type": "response_item",
                    "payload": {"type": "function_call", "call_id": "call_question", "name": "request_user_input",
                        "arguments": serde_json::to_string(&json!({
                            "questions": [{
                                "id": "choice",
                                "header": "Choice",
                                "question": "Pick one?",
                                "options": [{"label": "A", "description": "First"}]
                            }]
                        })).unwrap()},
                }),
                json!({
                    "timestamp": "2026-07-23T11:00:02.200Z",
                    "type": "response_item",
                    "payload": {"type": "function_call", "call_id": "call_large", "name": "shell_command",
                        "arguments": "{\"command\":\"emit-large-output\"}"},
                }),
                json!({
                    "timestamp": "2026-07-23T11:00:03.000Z",
                    "type": "response_item",
                    "payload": {"type": "function_call_output", "call_id": "call_image", "output": [{
                        "type": "input_image",
                        "image_url": format!("data:image/png;base64,{image_data}"),
                        "detail": "original"
                    }]},
                }),
                json!({
                    "timestamp": "2026-07-23T11:00:03.100Z",
                    "type": "response_item",
                    "payload": {"type": "function_call_output", "call_id": "call_question",
                        "output": json!({"answers": {"choice": {"answers": ["A"]}}}).to_string()},
                }),
                json!({
                    "timestamp": "2026-07-23T11:00:03.200Z",
                    "type": "response_item",
                    "payload": {"type": "function_call_output", "call_id": "call_large",
                        "output": large_output},
                }),
                json!({
                    "timestamp": "2026-07-23T11:00:04.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "assistant", "phase": "final_answer",
                        "content": [{"type": "output_text", "text": "完成"}]},
                }),
            ],
        );

        let report = convert_codex_to_claude(
            "",
            claude.to_string_lossy().as_ref(),
            source.to_string_lossy().as_ref(),
            ClaudeImportMode::Native,
        )
        .unwrap();
        let records: Vec<Value> = fs::read_to_string(report.new_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let tool_record = records
            .iter()
            .find(|record| {
                record["message"]["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_use"))
            })
            .unwrap();
        let tool_id = |name: &str| {
            tool_record["message"]["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|block| block["name"] == name)
                .unwrap()["id"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let result_for = |id: &str| {
            records
                .iter()
                .find(|record| record["message"]["content"][0]["tool_use_id"] == id)
                .unwrap()
        };

        let image_result = result_for(&tool_id("Read"));
        assert_eq!(
            image_result["message"]["content"][0]["content"][0]["type"],
            "image"
        );
        assert_eq!(
            image_result["message"]["content"][0]["content"][0]["source"]["data"],
            image_data
        );
        assert_eq!(image_result["toolUseResult"]["type"], "image");
        assert_eq!(image_result["toolUseResult"]["file"]["type"], "image/png");
        assert_eq!(
            image_result["toolUseResult"]["file"]["dimensions"]["originalWidth"],
            1
        );

        let question_result = result_for(&tool_id("AskUserQuestion"));
        assert_eq!(
            question_result["toolUseResult"]["answers"]["Pick one?"],
            "A"
        );
        assert_eq!(
            question_result["toolUseResult"]["questions"][0]["multiSelect"],
            false
        );
        assert!(question_result["message"]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Pick one?"));

        let large_result = result_for(&tool_id("Bash"));
        let preserved = large_result["message"]["content"][0]["content"]
            .as_str()
            .unwrap();
        assert_eq!(preserved.chars().count(), 12_004);
        assert!(preserved.ends_with("TAIL"));
        assert_eq!(large_result["toolUseResult"]["stdout"], preserved);
    }

    #[test]
    fn native_mode_uses_claude_error_renderer_for_failed_commands() {
        let root = temp_dir("codex2cla-native-command-error");
        let claude = root.join("claude");
        fs::create_dir_all(paths::claude_projects_dir(&claude)).unwrap();
        let source = root.join("rollout.jsonl");
        write_lines(
            &source,
            &[
                json!({
                    "timestamp": "2026-07-23T12:00:00.000Z",
                    "type": "session_meta",
                    "payload": {"id": "src-command-error", "cwd": "F:\\demo\\project"},
                }),
                json!({
                    "timestamp": "2026-07-23T12:00:01.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "执行失败命令"}]},
                }),
                json!({
                    "timestamp": "2026-07-23T12:00:02.000Z",
                    "type": "response_item",
                    "payload": {"type": "function_call", "call_id": "call_error", "name": "shell_command",
                        "arguments": "{\"command\":\"exit 2\"}"},
                }),
                json!({
                    "timestamp": "2026-07-23T12:00:03.000Z",
                    "type": "response_item",
                    "payload": {"type": "function_call_output", "call_id": "call_error",
                        "output": "Exit code: 2\nWall time: 0.1 seconds\nOutput:\ncommand failed"},
                }),
            ],
        );

        let report = convert_codex_to_claude(
            "",
            claude.to_string_lossy().as_ref(),
            source.to_string_lossy().as_ref(),
            ClaudeImportMode::Native,
        )
        .unwrap();
        let result: Value = fs::read_to_string(report.new_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|record| record["message"]["content"][0]["type"] == "tool_result")
            .unwrap();
        assert_eq!(result["message"]["content"][0]["is_error"], true);
        assert!(result["toolUseResult"]
            .as_str()
            .unwrap()
            .starts_with("Error: Exit code: 2"));
    }

    #[test]
    fn native_tool_call_maps_parallel_command_arrays_and_waits() {
        let parallel = json!({
            "type": "custom_tool_call",
            "name": "exec",
            "input": concat!(
                "const checks=[{name:\"one\",cmd:\"echo one\"},{name:\"two\",cmd:\"echo two\"}];",
                "const rs=await Promise.all(checks.map(x=>tools.exec_command({cmd:x.cmd})));"
            ),
        });
        let (parallel_name, parallel_input) = native_tool_call(&parallel);
        assert_eq!(parallel_name, "Bash");
        assert_eq!(
            parallel_input["command"],
            "echo one\n\n# --- next command ---\necho two"
        );

        let wait = json!({
            "type": "function_call",
            "name": "wait",
            "arguments": "{\"cell_id\":\"16\",\"yield_time_ms\":30000,\"max_tokens\":40000}",
        });
        let (wait_name, wait_input) = native_tool_call(&wait);
        assert_eq!(wait_name, "TaskOutput");
        assert_eq!(wait_input["task_id"], "16");
        assert_eq!(wait_input["block"], true);
        assert_eq!(wait_input["timeout"], 30000);
    }

    #[test]
    fn native_mode_writes_task_output_renderer_payload() {
        let root = temp_dir("codex2cla-task-output");
        let claude = root.join("claude");
        fs::create_dir_all(paths::claude_projects_dir(&claude)).unwrap();
        let source = root.join("rollout.jsonl");
        write_lines(
            &source,
            &[
                json!({
                    "timestamp": "2026-07-23T10:00:00.000Z",
                    "type": "session_meta",
                    "payload": {"id": "src-task-output", "cwd": "F:\\demo\\project"},
                }),
                json!({
                    "timestamp": "2026-07-23T10:00:01.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "等待结果"}]},
                }),
                json!({
                    "timestamp": "2026-07-23T10:00:02.000Z",
                    "type": "response_item",
                    "payload": {"type": "function_call", "call_id": "call_wait", "name": "wait",
                        "arguments": "{\"cell_id\":\"16\",\"yield_time_ms\":30000}"},
                }),
                json!({
                    "timestamp": "2026-07-23T10:00:03.000Z",
                    "type": "response_item",
                    "payload": {"type": "function_call_output", "call_id": "call_wait",
                        "output": "Script completed\nOutput:\nok"},
                }),
                json!({
                    "timestamp": "2026-07-23T10:00:04.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "assistant", "phase": "final_answer",
                        "content": [{"type": "output_text", "text": "完成"}]},
                }),
            ],
        );

        let report = convert_codex_to_claude(
            "",
            claude.to_string_lossy().as_ref(),
            source.to_string_lossy().as_ref(),
            ClaudeImportMode::Native,
        )
        .unwrap();
        let records: Vec<Value> = fs::read_to_string(report.new_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let call = records
            .iter()
            .find(|record| record["message"]["content"][0]["type"] == "tool_use")
            .unwrap();
        assert_eq!(call["message"]["content"][0]["name"], "TaskOutput");
        assert_eq!(call["message"]["content"][0]["input"]["task_id"], "16");
        let result = records
            .iter()
            .find(|record| record["message"]["content"][0]["type"] == "tool_result")
            .unwrap();
        assert_eq!(result["toolUseResult"]["retrieval_status"], "success");
        assert_eq!(result["toolUseResult"]["task"]["task_type"], "local_bash");
        assert_eq!(result["toolUseResult"]["task"]["task_id"], "16");
        assert_eq!(result["toolUseResult"]["task"]["status"], "completed");
        assert!(result["toolUseResult"]["task"]["output"]
            .as_str()
            .unwrap()
            .contains("Script completed"));
    }

    #[test]
    fn claude_import_mode_defaults_to_simple_and_rejects_unknown_values() {
        assert_eq!(
            ClaudeImportMode::parse(None).unwrap(),
            ClaudeImportMode::Simple
        );
        assert_eq!(
            ClaudeImportMode::parse(Some("native")).unwrap(),
            ClaudeImportMode::Native
        );
        assert!(ClaudeImportMode::parse(Some("lossless")).is_err());
    }

    #[test]
    fn encode_claude_project_dir_matches_official_encoding() {
        assert_eq!(
            encode_claude_project_dir("F:\\project\\sessions-management\\codex-session-manager"),
            "F--project-sessions-management-codex-session-manager"
        );
        assert_eq!(
            encode_claude_project_dir("/home/user/项目"),
            "-home-user---"
        );
    }

    #[test]
    fn target_identity_detection_skips_legacy_conversion_metadata() {
        let root = temp_dir("target-identity");

        let codex = root.join("codex");
        let codex_day = paths::sessions_dir(&codex)
            .join("2026")
            .join("07")
            .join("23");
        fs::create_dir_all(&codex_day).unwrap();
        write_lines(
            &codex_day.join("native.jsonl"),
            &[json!({
                "type": "session_meta",
                "payload": {
                    "id": "native",
                    "originator": "Codex Desktop",
                    "cli_version": "0.145.0-alpha.30",
                    "source": "vscode"
                }
            })],
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_lines(
            &codex_day.join("legacy.jsonl"),
            &[json!({
                "type": "session_meta",
                "payload": {
                    "id": "legacy",
                    "originator": "cc-sessions",
                    "cli_version": "0.4.3",
                    "source": "cli"
                }
            })],
        );
        let codex_identity = detect_codex_identity(&codex);
        assert_eq!(codex_identity.originator, "Codex Desktop");
        assert_eq!(
            codex_identity.cli_version.as_deref(),
            Some("0.145.0-alpha.30")
        );
        assert_eq!(codex_identity.source, "vscode");

        let claude_projects = root.join("claude").join("projects");
        let claude_project = claude_projects.join("F--demo-project");
        fs::create_dir_all(&claude_project).unwrap();
        write_lines(
            &claude_project.join("native.jsonl"),
            &[json!({
                "type": "assistant",
                "version": "2.1.217",
                "message": {
                    "id": "msg_01wegWJ4tMJJzSxGdGgIo5P9",
                    "model": "claude-fable-5"
                }
            })],
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_lines(
            &claude_project.join("legacy.jsonl"),
            &[json!({
                "type": "assistant",
                "version": "0.4.3",
                "message": {
                    "id": "msg_external_import_1",
                    "model": "claude-legacy-placeholder"
                }
            })],
        );
        let claude_identity = detect_claude_identity(&claude_projects);
        assert_eq!(claude_identity.model, "claude-fable-5");
        assert_eq!(claude_identity.version.as_deref(), Some("2.1.217"));
    }

    #[test]
    fn generated_transcripts_do_not_disclose_conversion_origin() {
        let user = ConvMessage {
            role: Role::User,
            text: "检查项目".into(),
            timestamp: Some("2026-07-23T09:00:01.000Z".into()),
            phase: None,
            images: Vec::new(),
        };
        let assistant = ConvMessage {
            role: Role::Assistant,
            text: "检查完成".into(),
            timestamp: Some("2026-07-23T09:00:04.000Z".into()),
            phase: Some("final_answer".into()),
            images: Vec::new(),
        };

        let claude_source = ParsedClaudeSession {
            cwd: Some("F:\\demo\\project".into()),
            messages: vec![user.clone(), assistant.clone()],
            ..Default::default()
        };
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-23T12:34:56.789Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let codex_content = build_codex_lines(
            "new-codex-session",
            "F:\\demo\\project",
            "openai",
            &claude_source,
            &CodexIdentity::default(),
            &now,
            CodexImportMode::Simple,
        )
        .lines
        .join("\n");

        let call = CodexToolEvent {
            call_id: Some("call_shell".into()),
            payload: json!({
                "type": "function_call",
                "call_id": "call_shell",
                "name": "shell_command",
                "arguments": "{\"command\":\"echo ok\"}",
            }),
            timestamp: Some("2026-07-23T09:00:02.000Z".into()),
        };
        let result = CodexToolEvent {
            call_id: Some("call_shell".into()),
            payload: json!({
                "type": "function_call_output",
                "call_id": "call_shell",
                "output": "ok",
            }),
            timestamp: Some("2026-07-23T09:00:03.000Z".into()),
        };
        let codex_source = ParsedCodexRollout {
            cwd: Some("F:\\demo\\project".into()),
            model: Some("gpt-5.6-sol".into()),
            messages: vec![user.clone(), assistant.clone()],
            events: vec![
                CodexEvent::Message(user),
                CodexEvent::ToolCall(call),
                CodexEvent::ToolResult(result),
                CodexEvent::Message(assistant),
            ],
            ..Default::default()
        };
        let claude_content = build_native_claude_lines(
            "new-claude-session",
            "F:\\demo\\project",
            &codex_source,
            &ClaudeIdentity {
                model: "claude-fable-5".into(),
                version: Some("2.1.217".into()),
            },
        )
        .lines
        .join("\n");

        for forbidden in [
            "cc-sessions",
            "Imported Codex",
            "imported Codex",
            "codex-import",
            "external-import",
            "external_agent_",
            "msg_external_",
            "toolu_external_",
            "<EXTERNAL SESSION IMPORTED>",
            "gpt-5.6-sol",
        ] {
            assert!(
                !codex_content.contains(forbidden),
                "Codex transcript leaked generated source marker: {forbidden}"
            );
            assert!(
                !claude_content.contains(forbidden),
                "Claude transcript leaked generated source marker: {forbidden}"
            );
        }
    }
}
