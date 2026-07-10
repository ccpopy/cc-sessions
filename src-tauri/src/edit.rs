//! 会话消息级编辑：改写文本、删除事件。
//!
//! 安全模型：
//! - 原地编辑（会话 id 与文件路径不变，`resume` 继续续聊同一会话）。
//! - 每次写入前，若无法证明与上一次编辑连续（文件哈希对不上或从未编辑过），
//!   自动把当前文件完整快照到编辑目录。
//! - 每个操作向 journal.jsonl 追加一条记录，保存被修改/删除行的原始字节，
//!   支持精确撤销（撤销的撤销即重做）。
//! - 快照与 journal 存放于 `<backup_dir>/session-edits/<provider>-<session_id>/`。
//!
//! 完整性规则：
//! - Codex：`response_item` 与 `event_msg` 镜像行同步改/删；`function_call`
//!   与 `function_call_output` 按 call_id 成对删除；reasoning（含
//!   encrypted_content，不可编辑）随其所属回复联动删除，避免续聊时
//!   OpenAI 回放报 "reasoning without required following item"。
//! - Claude：删除行后把幸存行的 parentUuid / leafUuid / logicalParentUuid
//!   重连到最近幸存祖先；`tool_use` 与 `tool_result` 成对删除；thinking
//!   带签名，只随整条消息删除，不允许改写。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::atomic_file;
use crate::error::{AppError, AppResult};
use crate::models::{
    DeletePlan, DeletePlanLine, EditApplyReport, EditHistory, EditHistoryEntry, EditSnapshotInfo,
};
use crate::paths;

const REASON_SELECTED: &str = "selected";
const REASON_TOOL_PAIR: &str = "tool_pair";
const REASON_MIRROR: &str = "mirror";
const REASON_REASONING: &str = "reasoning_attached";

// ========================= journal 内部结构 =========================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LineChange {
    /// 操作前状态（before-state）的物理行号。
    line_no: usize,
    /// None 表示该行是新插入的（仅撤销记录会出现）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before: Option<String>,
    /// None 表示该行被删除。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalEntry {
    op_id: String,
    ts: String,
    /// edit_text / delete_events / undo / restore_snapshot
    kind: String,
    provider: String,
    session_id: String,
    rollout_path: String,
    description: String,
    /// 撤销链上最初那条记录的描述（用于生成 撤销：/重做： 文案）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_description: Option<String>,
    /// 本次操作前新建的原始快照文件名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_snapshot: Option<String>,
    before_hash: String,
    after_hash: String,
    #[serde(default)]
    changes: Vec<LineChange>,
}

// ========================= 文件读写 =========================

struct LoadedFile {
    /// 按 '\n' 切分的行（不含换行符本身；CRLF 文件的 '\r' 保留在行尾）
    lines: Vec<String>,
    /// 每行 trim 后的 JSON 解析结果；空行/坏行为 None
    parsed: Vec<Option<Value>>,
    trailing_newline: bool,
    hash: String,
}

fn sha_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn load_file(path: &Path) -> AppResult<LoadedFile> {
    let raw = fs::read(path)?;
    let hash = sha_hex(&raw);
    let text = String::from_utf8(raw)
        .map_err(|_| AppError::Other("会话文件不是有效的 UTF-8，拒绝编辑".into()))?;
    let trailing_newline = text.ends_with('\n');
    let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
    if trailing_newline {
        lines.pop();
    }
    let parsed = lines
        .iter()
        .map(|l| {
            let t = l.trim();
            if t.is_empty() {
                None
            } else {
                serde_json::from_str::<Value>(t).ok()
            }
        })
        .collect();
    Ok(LoadedFile {
        lines,
        parsed,
        trailing_newline,
        hash,
    })
}

fn write_lines(
    path: &Path,
    lines: &[String],
    trailing_newline: bool,
    expected_hash: &str,
) -> AppResult<String> {
    let current = fs::read(path)?;
    if sha_hex(&current) != expected_hash {
        return Err(AppError::Other(format!(
            "会话文件在编辑期间发生变化，已拒绝覆盖: {}",
            path.to_string_lossy()
        )));
    }
    let expected = atomic_file::fingerprint(path)?;
    let mut body = lines.join("\n");
    if trailing_newline && !lines.is_empty() {
        body.push('\n');
    }
    atomic_file::replace_with_writer_if_unchanged(path, &expected, |file| {
        file.write_all(body.as_bytes())?;
        Ok(())
    })?;
    Ok(sha_hex(body.as_bytes()))
}

// ========================= 编辑目录 / journal / 快照 =========================

fn edit_dir(backup_dir: &str, provider: &str, session_id: &str) -> PathBuf {
    PathBuf::from(paths::strip_verbatim(backup_dir))
        .join("session-edits")
        .join(format!("{}-{}", provider, paths::sanitize_slug(session_id)))
}

fn journal_path(dir: &Path) -> PathBuf {
    dir.join("journal.jsonl")
}

fn read_journal(dir: &Path) -> AppResult<Vec<JournalEntry>> {
    let content = match fs::read_to_string(journal_path(dir)) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut entries = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        entries.push(serde_json::from_str::<JournalEntry>(line).map_err(|error| {
            AppError::Other(format!("编辑 journal 第 {} 行损坏: {error}", line_no + 1))
        })?);
    }
    Ok(entries)
}

fn append_journal(dir: &Path, entry: &JournalEntry) -> AppResult<()> {
    fs::create_dir_all(dir)?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path(dir))?;
    writeln!(f, "{}", serde_json::to_string(entry)?)?;
    f.sync_all()?;
    Ok(())
}

/// 与上一次编辑不连续（外部改动过 / 从未编辑）时，快照当前文件。
fn ensure_snapshot(
    dir: &Path,
    rollout: &Path,
    current_hash: &str,
    journal: &[JournalEntry],
) -> AppResult<Option<String>> {
    if let Some(last) = journal.last() {
        if last.after_hash == current_hash {
            return Ok(None);
        }
    }
    fs::create_dir_all(dir)?;
    let name = format!("original-{}.jsonl", chrono::Utc::now().timestamp_millis());
    fs::copy(rollout, dir.join(&name))?;
    Ok(Some(name))
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn new_op_id(journal_len: usize) -> String {
    format!(
        "op-{}-{}",
        chrono::Utc::now().timestamp_millis(),
        journal_len + 1
    )
}

// ========================= changes 的正放 / 逆放 =========================
//
// 约束：一条记录的 changes 只包含
//   - 纯编辑（before/after 均为 Some），或
//   - 编辑 + 删除（删除 before=Some, after=None），
// 且所有 line_no 都以操作前状态（before-state）表示。

fn forward_apply(lines: &mut Vec<String>, changes: &[LineChange]) -> AppResult<()> {
    for c in changes {
        if let (Some(_), Some(after)) = (&c.before, &c.after) {
            let slot = lines
                .get_mut(c.line_no)
                .ok_or_else(|| AppError::Other(format!("行号 {} 超出范围", c.line_no)))?;
            *slot = after.clone();
        }
    }
    let mut to_delete: Vec<usize> = changes
        .iter()
        .filter(|c| c.after.is_none())
        .map(|c| c.line_no)
        .collect();
    to_delete.sort_unstable();
    to_delete.dedup();
    for &i in to_delete.iter().rev() {
        if i >= lines.len() {
            return Err(AppError::Other(format!("行号 {} 超出范围", i)));
        }
        lines.remove(i);
    }
    Ok(())
}

fn reverse_apply(lines: &mut Vec<String>, changes: &[LineChange]) -> AppResult<()> {
    // 先按原行号升序把删除的行插回，恢复 before-state 的形状
    let mut deleted: Vec<(usize, String)> = changes
        .iter()
        .filter(|c| c.after.is_none())
        .filter_map(|c| c.before.clone().map(|b| (c.line_no, b)))
        .collect();
    deleted.sort_by_key(|(i, _)| *i);
    for (i, raw) in deleted {
        if i > lines.len() {
            return Err(AppError::Other(format!("行号 {} 超出范围，无法恢复", i)));
        }
        lines.insert(i, raw);
    }
    for c in changes {
        if let (Some(before), Some(_)) = (&c.before, &c.after) {
            let slot = lines
                .get_mut(c.line_no)
                .ok_or_else(|| AppError::Other(format!("行号 {} 超出范围", c.line_no)))?;
            *slot = before.clone();
        }
    }
    Ok(())
}

// ========================= Codex 行分类 =========================

fn codex_outer(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or("")
}

fn codex_ptype(v: &Value) -> &str {
    v.get("payload")
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn codex_msg_role(v: &Value) -> &str {
    v.get("payload")
        .and_then(|p| p.get("role"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn codex_call_id(v: &Value) -> Option<&str> {
    v.get("payload")
        .and_then(|p| p.get("call_id"))
        .and_then(Value::as_str)
}

/// response_item message 的纯文本（text 项按换行拼接）
fn codex_flat_text(v: &Value) -> String {
    let content = v.get("payload").and_then(|p| p.get("content"));
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|it| it.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn codex_event_message_text(v: &Value) -> Option<&str> {
    v.get("payload")
        .and_then(|p| p.get("message"))
        .and_then(Value::as_str)
}

const CODEX_CALL_TYPES: &[&str] = &[
    "function_call",
    "custom_tool_call",
    "local_shell_call",
    "web_search_call",
];
const CODEX_CALL_OUTPUT_TYPES: &[&str] = &["function_call_output", "custom_tool_call_output"];

/// 是否允许删除该行；不允许时返回原因。
fn codex_delete_blocked(v: &Value) -> Option<String> {
    match codex_outer(v) {
        "session_meta" => Some("session_meta 是会话元数据，不能删除".into()),
        "turn_context" => Some("turn_context 保存回合运行配置，不能删除".into()),
        "compacted" => Some("compacted 是压缩摘要锚点，删除会破坏续聊".into()),
        "event_msg" => None,
        "response_item" => {
            let pt = codex_ptype(v);
            if pt == "message"
                || pt == "reasoning"
                || CODEX_CALL_TYPES.contains(&pt)
                || CODEX_CALL_OUTPUT_TYPES.contains(&pt)
            {
                None
            } else {
                Some(format!("response_item/{pt} 类型未知，暂不允许删除"))
            }
        }
        other => Some(format!("{other} 类型未知，暂不允许删除")),
    }
}

fn codex_line_brief(v: &Value) -> (String, String, String) {
    let outer = codex_outer(v);
    let pt = codex_ptype(v);
    let (role, text) = match (outer, pt) {
        ("response_item", "message") => (codex_msg_role(v).to_string(), codex_flat_text(v)),
        ("response_item", "reasoning") => ("reasoning".into(), codex_flat_text(v)),
        ("response_item", _) if CODEX_CALL_TYPES.contains(&pt) => (
            "tool_call".into(),
            v.get("payload")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(pt)
                .to_string(),
        ),
        ("response_item", _) if CODEX_CALL_OUTPUT_TYPES.contains(&pt) => {
            ("tool_result".into(), "工具返回".into())
        }
        ("event_msg", "user_message") => (
            "user".into(),
            codex_event_message_text(v).unwrap_or("").to_string(),
        ),
        ("event_msg", "agent_message") => (
            "assistant".into(),
            codex_event_message_text(v).unwrap_or("").to_string(),
        ),
        _ => ("other".into(), String::new()),
    };
    (role, format!("{outer}/{pt}"), summarize(&text))
}

fn summarize(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    let mut out: String = flat.chars().take(80).collect();
    if flat.chars().count() > 80 {
        out.push('…');
    }
    out
}

/// 在 parsed 里找与 line_no 最近的镜像行（Codex 消息双写）。
fn find_codex_mirror(parsed: &[Option<Value>], line_no: usize, old_text: &str) -> Option<usize> {
    let src = parsed.get(line_no)?.as_ref()?;
    let (want_outer, want_ptype, want_role) = match (codex_outer(src), codex_ptype(src)) {
        ("response_item", "message") => match codex_msg_role(src) {
            "user" => ("event_msg", "user_message", ""),
            "assistant" => ("event_msg", "agent_message", ""),
            _ => return None,
        },
        ("event_msg", "user_message") => ("response_item", "message", "user"),
        ("event_msg", "agent_message") => ("response_item", "message", "assistant"),
        _ => return None,
    };
    let matches_target = |v: &Value| -> bool {
        if codex_outer(v) != want_outer || codex_ptype(v) != want_ptype {
            return false;
        }
        if want_outer == "event_msg" {
            codex_event_message_text(v) == Some(old_text)
        } else {
            codex_msg_role(v) == want_role && codex_flat_text(v) == old_text
        }
    };
    // 从近到远向两侧扫描，优先取距离最近者
    for dist in 1..parsed.len() {
        let before = line_no.checked_sub(dist);
        if let Some(i) = before {
            if let Some(Some(v)) = parsed.get(i) {
                if matches_target(v) {
                    return Some(i);
                }
            }
        }
        let after = line_no + dist;
        if after < parsed.len() {
            if let Some(Some(v)) = parsed.get(after) {
                if matches_target(v) {
                    return Some(after);
                }
            }
        }
        if before.is_none() && after >= parsed.len() {
            break;
        }
    }
    None
}

/// Codex 删除集合扩展：镜像行、call_id 配对、附着 reasoning。
fn codex_expand_delete(
    parsed: &[Option<Value>],
    selected: &[usize],
) -> AppResult<(BTreeMap<usize, String>, Vec<String>)> {
    let mut plan: BTreeMap<usize, String> = BTreeMap::new();
    let mut blocked: Vec<String> = Vec::new();

    for &i in selected {
        let Some(Some(v)) = parsed.get(i) else {
            blocked.push(format!("第 {i} 行不存在或不是有效 JSON"));
            continue;
        };
        match codex_delete_blocked(v) {
            Some(reason) => blocked.push(format!("第 {i} 行：{reason}")),
            None => {
                plan.insert(i, REASON_SELECTED.into());
            }
        }
    }
    if !blocked.is_empty() {
        return Ok((plan, blocked));
    }

    // 1) call_id 配对：选中调用或返回时，扫清同 call_id 的所有行（含 event_msg 执行流水）
    let mut call_ids: BTreeSet<String> = BTreeSet::new();
    for (&i, _) in plan.clone().iter() {
        let v = parsed[i].as_ref().unwrap();
        let pt = codex_ptype(v);
        if CODEX_CALL_TYPES.contains(&pt) || CODEX_CALL_OUTPUT_TYPES.contains(&pt) {
            if let Some(id) = codex_call_id(v) {
                call_ids.insert(id.to_string());
            }
        }
    }
    if !call_ids.is_empty() {
        for (i, v) in parsed.iter().enumerate() {
            let Some(v) = v else { continue };
            if plan.contains_key(&i) {
                continue;
            }
            if let Some(id) = codex_call_id(v) {
                if call_ids.contains(id) && codex_delete_blocked(v).is_none() {
                    plan.insert(i, REASON_TOOL_PAIR.into());
                }
            }
        }
    }

    // 2) 消息镜像行
    for (&i, _) in plan.clone().iter() {
        let v = parsed[i].as_ref().unwrap();
        let is_msg = matches!(
            (codex_outer(v), codex_ptype(v)),
            ("response_item", "message")
                | ("event_msg", "user_message")
                | ("event_msg", "agent_message")
        );
        if !is_msg {
            continue;
        }
        let old_text = match codex_outer(v) {
            "event_msg" => codex_event_message_text(v).unwrap_or("").to_string(),
            _ => codex_flat_text(v),
        };
        if old_text.is_empty() {
            continue;
        }
        if let Some(m) = find_codex_mirror(parsed, i, &old_text) {
            plan.entry(m).or_insert_with(|| REASON_MIRROR.into());
        }
    }

    // 3a) 被删回复/调用的直接前驱 reasoning 一并删除（OpenAI 回放要求配对）
    let is_reasoning_follower = |v: &Value| -> bool {
        let pt = codex_ptype(v);
        codex_outer(v) == "response_item"
            && ((pt == "message" && codex_msg_role(v) == "assistant")
                || CODEX_CALL_TYPES.contains(&pt))
    };
    for (&i, _) in plan.clone().iter() {
        let v = parsed[i].as_ref().unwrap();
        if !is_reasoning_follower(v) {
            continue;
        }
        let mut j = i;
        while j > 0 {
            j -= 1;
            let Some(Some(prev)) = parsed.get(j) else {
                continue;
            };
            if codex_outer(prev) != "response_item" {
                continue;
            }
            if plan.contains_key(&j) {
                continue; // 已删的 response_item，继续向前找
            }
            if codex_ptype(prev) == "reasoning" {
                plan.insert(j, REASON_REASONING.into());
            }
            break;
        }
    }

    // 3b) 不动点兜底：reasoning 原本的后继被删且新的后继不合法时，联动删除
    loop {
        let mut grew = false;
        for (i, v) in parsed.iter().enumerate() {
            let Some(v) = v else { continue };
            if plan.contains_key(&i) {
                continue;
            }
            if codex_outer(v) != "response_item" || codex_ptype(v) != "reasoning" {
                continue;
            }
            // 原始后继必须在删除集合内才触发（不去"修复"文件既有结构）
            let orig_next = ((i + 1)..parsed.len()).find(|&j| {
                parsed[j]
                    .as_ref()
                    .is_some_and(|x| codex_outer(x) == "response_item")
            });
            let Some(orig_next) = orig_next else { continue };
            if !plan.contains_key(&orig_next) {
                continue;
            }
            let surviving_next = ((i + 1)..parsed.len()).find(|&j| {
                !plan.contains_key(&j)
                    && parsed[j]
                        .as_ref()
                        .is_some_and(|x| codex_outer(x) == "response_item")
            });
            let orphaned = match surviving_next {
                None => true,
                Some(j) => {
                    let nv = parsed[j].as_ref().unwrap();
                    let pt = codex_ptype(nv);
                    pt == "reasoning" || (pt == "message" && codex_msg_role(nv) == "user")
                }
            };
            if orphaned {
                plan.insert(i, REASON_REASONING.into());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    Ok((plan, blocked))
}

// ========================= Claude 行分类 =========================

fn claude_type(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or("")
}

fn claude_uuid(v: &Value) -> Option<&str> {
    v.get("uuid").and_then(Value::as_str)
}

fn claude_is_message_line(v: &Value) -> bool {
    matches!(claude_type(v), "user" | "assistant") && v.get("message").is_some()
}

fn claude_content<'a>(v: &'a Value) -> Option<&'a Value> {
    v.get("message").and_then(|m| m.get("content"))
}

fn claude_block_ids(v: &Value, block_type: &str, id_field: &str) -> Vec<String> {
    let Some(Value::Array(items)) = claude_content(v) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|it| it.get("type").and_then(Value::as_str) == Some(block_type))
        .filter_map(|it| it.get(id_field).and_then(Value::as_str))
        .map(String::from)
        .collect()
}

fn claude_delete_blocked(v: &Value) -> Option<String> {
    if claude_is_message_line(v) {
        if claude_uuid(v).is_none() {
            return Some("该行缺少 uuid，无法安全重连链路".into());
        }
        return None;
    }
    Some(format!(
        "{} 类型行暂不允许删除（仅支持 user/assistant 消息）",
        claude_type(v)
    ))
}

fn claude_line_brief(v: &Value) -> (String, String, String) {
    let t = claude_type(v).to_string();
    let role = v
        .get("message")
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        .unwrap_or(&t)
        .to_string();
    let text = match claude_content(v) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|it| {
                it.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| it.get("thinking").and_then(Value::as_str))
                    .or_else(|| it.get("type").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    (role, t, summarize(&text))
}

/// Claude 删除集合扩展：tool_use / tool_result 双向不动点。
fn claude_expand_delete(
    parsed: &[Option<Value>],
    selected: &[usize],
) -> AppResult<(BTreeMap<usize, String>, Vec<String>)> {
    let mut plan: BTreeMap<usize, String> = BTreeMap::new();
    let mut blocked: Vec<String> = Vec::new();

    for &i in selected {
        let Some(Some(v)) = parsed.get(i) else {
            blocked.push(format!("第 {i} 行不存在或不是有效 JSON"));
            continue;
        };
        match claude_delete_blocked(v) {
            Some(reason) => blocked.push(format!("第 {i} 行：{reason}")),
            None => {
                plan.insert(i, REASON_SELECTED.into());
            }
        }
    }
    if !blocked.is_empty() {
        return Ok((plan, blocked));
    }

    loop {
        let mut grew = false;
        // 已删集合内的 tool_use / tool_result id 全集
        let mut use_ids: BTreeSet<String> = BTreeSet::new();
        let mut result_ids: BTreeSet<String> = BTreeSet::new();
        for (&i, _) in plan.iter() {
            let v = parsed[i].as_ref().unwrap();
            use_ids.extend(claude_block_ids(v, "tool_use", "id"));
            result_ids.extend(claude_block_ids(v, "tool_result", "tool_use_id"));
        }
        for (i, v) in parsed.iter().enumerate() {
            let Some(v) = v else { continue };
            if plan.contains_key(&i) || !claude_is_message_line(v) {
                continue;
            }
            let refs_deleted_use = claude_block_ids(v, "tool_result", "tool_use_id")
                .iter()
                .any(|id| use_ids.contains(id));
            let owns_deleted_result = claude_block_ids(v, "tool_use", "id")
                .iter()
                .any(|id| result_ids.contains(id));
            if refs_deleted_use || owns_deleted_result {
                plan.insert(i, REASON_TOOL_PAIR.into());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    Ok((plan, blocked))
}

/// 计算 Claude 删除后的链路重连编辑（parentUuid / leafUuid / logicalParentUuid）。
fn claude_relink_changes(
    lines: &[String],
    parsed: &[Option<Value>],
    doomed: &BTreeMap<usize, String>,
) -> AppResult<Vec<LineChange>> {
    // 被删 uuid → 其 parentUuid
    let mut parent_of: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (&i, _) in doomed.iter() {
        let v = parsed[i].as_ref().unwrap();
        if let Some(u) = claude_uuid(v) {
            let p = v
                .get("parentUuid")
                .and_then(Value::as_str)
                .map(String::from);
            parent_of.insert(u.to_string(), p);
        }
    }
    let resolve = |start: &str| -> Option<String> {
        let mut cur = start.to_string();
        let mut hops = 0;
        while let Some(p) = parent_of.get(&cur) {
            hops += 1;
            if hops > parent_of.len() + 1 {
                return None; // 环，断链
            }
            match p {
                Some(next) => cur = next.clone(),
                None => return None,
            }
        }
        Some(cur)
    };

    let mut changes = Vec::new();
    for (i, v) in parsed.iter().enumerate() {
        let Some(v) = v else { continue };
        if doomed.contains_key(&i) {
            continue;
        }
        let mut new_v = v.clone();
        let mut touched = false;
        for field in ["parentUuid", "leafUuid", "logicalParentUuid"] {
            let Some(cur) = new_v.get(field).and_then(Value::as_str) else {
                continue;
            };
            if !parent_of.contains_key(cur) {
                continue;
            }
            let replacement = resolve(cur);
            new_v[field] = match replacement {
                Some(u) => Value::String(u),
                None => Value::Null,
            };
            touched = true;
        }
        if touched {
            changes.push(LineChange {
                line_no: i,
                before: Some(lines[i].clone()),
                after: Some(serde_json::to_string(&new_v)?),
            });
        }
    }
    Ok(changes)
}

// ========================= 文本改写 =========================

/// 把 content 数组里第一个带 text 的项改为 new_text，移除其余 text 项，保留非文本项。
/// 返回旧的拼接文本；没有可改写文本时返回 None。
fn replace_text_items(content: &mut Value, new_text: &str, text_types: &[&str]) -> Option<String> {
    match content {
        Value::String(s) => {
            let old = s.clone();
            *content = Value::String(new_text.to_string());
            Some(old)
        }
        Value::Array(items) => {
            let is_text_item = |it: &Value| -> bool {
                let ty = it.get("type").and_then(Value::as_str).unwrap_or("");
                (text_types.is_empty() || text_types.contains(&ty))
                    && it.get("text").and_then(Value::as_str).is_some()
            };
            let old_parts: Vec<String> = items
                .iter()
                .filter(|it| is_text_item(it))
                .filter_map(|it| it.get("text").and_then(Value::as_str).map(String::from))
                .collect();
            if old_parts.is_empty() {
                return None;
            }
            let mut replaced = false;
            items.retain_mut(|it| {
                if !is_text_item(it) {
                    return true;
                }
                if replaced {
                    return false; // 多余的 text 项移除
                }
                it["text"] = Value::String(new_text.to_string());
                replaced = true;
                true
            });
            Some(old_parts.join("\n"))
        }
        _ => None,
    }
}

/// 计算 Codex 单行文本改写（含镜像行同步），返回 changes。
fn codex_edit_changes(
    lines: &[String],
    parsed: &[Option<Value>],
    line_no: usize,
    new_text: &str,
) -> AppResult<Vec<LineChange>> {
    let v = parsed
        .get(line_no)
        .and_then(|x| x.as_ref())
        .ok_or_else(|| AppError::Other(format!("第 {line_no} 行不存在或不是有效 JSON")))?;

    let (old_text, new_line): (String, Value) = match (codex_outer(v), codex_ptype(v)) {
        ("response_item", "message") => {
            let mut nv = v.clone();
            let content = nv
                .get_mut("payload")
                .and_then(|p| p.get_mut("content"))
                .ok_or_else(|| AppError::Other("该消息缺少 content".into()))?;
            let old = replace_text_items(content, new_text, &["input_text", "output_text"])
                .ok_or_else(|| AppError::Other("该消息没有可改写的文本内容".into()))?;
            (old, nv)
        }
        ("event_msg", "user_message") | ("event_msg", "agent_message") => {
            let mut nv = v.clone();
            let old = codex_event_message_text(v)
                .ok_or_else(|| AppError::Other("该事件缺少 message 文本".into()))?
                .to_string();
            nv["payload"]["message"] = Value::String(new_text.to_string());
            (old, nv)
        }
        ("response_item", "reasoning") => {
            return Err(AppError::Other(
                "推理内容由模型加密/签名，不能改写；如不需要可删除该事件".into(),
            ))
        }
        (o, p) => {
            return Err(AppError::Other(format!(
                "{o}/{p} 不是可改写的消息（仅支持用户/助手消息文本）"
            )))
        }
    };

    let mut changes = vec![LineChange {
        line_no,
        before: Some(lines[line_no].clone()),
        after: Some(serde_json::to_string(&new_line)?),
    }];

    // 镜像行同步
    if let Some(m) = find_codex_mirror(parsed, line_no, &old_text) {
        let mv = parsed[m].as_ref().unwrap();
        let mut nv = mv.clone();
        let synced = match codex_outer(mv) {
            "event_msg" => {
                nv["payload"]["message"] = Value::String(new_text.to_string());
                true
            }
            _ => {
                let content = nv.get_mut("payload").and_then(|p| p.get_mut("content"));
                match content {
                    Some(c) => {
                        replace_text_items(c, new_text, &["input_text", "output_text"]).is_some()
                    }
                    None => false,
                }
            }
        };
        if synced {
            changes.push(LineChange {
                line_no: m,
                before: Some(lines[m].clone()),
                after: Some(serde_json::to_string(&nv)?),
            });
        }
    }
    Ok(changes)
}

/// 计算 Claude 单行文本改写，返回 changes。thinking / tool 块保持原样。
fn claude_edit_changes(
    lines: &[String],
    parsed: &[Option<Value>],
    line_no: usize,
    new_text: &str,
) -> AppResult<Vec<LineChange>> {
    let v = parsed
        .get(line_no)
        .and_then(|x| x.as_ref())
        .ok_or_else(|| AppError::Other(format!("第 {line_no} 行不存在或不是有效 JSON")))?;
    if !claude_is_message_line(v) {
        return Err(AppError::Other(
            "仅支持改写 user / assistant 消息文本".into(),
        ));
    }
    let mut nv = v.clone();
    let content = nv
        .get_mut("message")
        .and_then(|m| m.get_mut("content"))
        .ok_or_else(|| AppError::Other("该消息缺少 content".into()))?;
    replace_text_items(content, new_text, &["text"]).ok_or_else(|| {
        AppError::Other(
            "该消息没有可改写的文本（thinking 带签名、工具块结构化，均不可改写，只能删除整条）"
                .into(),
        )
    })?;
    Ok(vec![LineChange {
        line_no,
        before: Some(lines[line_no].clone()),
        after: Some(serde_json::to_string(&nv)?),
    }])
}

// ========================= 对外操作 =========================

fn provider_normalized(provider: &str) -> AppResult<&str> {
    match provider {
        "codex" | "claude" => Ok(provider),
        other => Err(AppError::Other(format!("不支持的 provider: {other}"))),
    }
}

pub fn plan_delete(
    provider: &str,
    rollout_path: &str,
    line_nos: &[usize],
) -> AppResult<DeletePlan> {
    let provider = provider_normalized(provider)?;
    let path = paths::strip_verbatim(rollout_path);
    let loaded = load_file(Path::new(&path))?;
    let (plan, blocked) = match provider {
        "codex" => codex_expand_delete(&loaded.parsed, line_nos)?,
        _ => claude_expand_delete(&loaded.parsed, line_nos)?,
    };
    let lines = plan
        .iter()
        .map(|(&i, reason)| {
            let v = loaded.parsed[i].as_ref().unwrap();
            let (role, kind, summary) = if provider == "codex" {
                codex_line_brief(v)
            } else {
                claude_line_brief(v)
            };
            DeletePlanLine {
                line_no: i,
                role,
                kind,
                summary,
                reason: reason.clone(),
            }
        })
        .collect();
    Ok(DeletePlan {
        rollout_path: path,
        lines,
        blocked,
    })
}

struct OpContext {
    dir: PathBuf,
    journal: Vec<JournalEntry>,
    loaded: LoadedFile,
    path: PathBuf,
}

fn open_op_context(
    provider: &str,
    rollout_path: &str,
    session_id: &str,
    backup_dir: &str,
) -> AppResult<OpContext> {
    let path = PathBuf::from(paths::strip_verbatim(rollout_path));
    if !path.is_file() {
        return Err(AppError::NotFound(path.to_string_lossy().into_owned()));
    }
    let dir = edit_dir(backup_dir, provider, session_id);
    let journal = read_journal(&dir)?;
    let loaded = load_file(&path)?;
    Ok(OpContext {
        dir,
        journal,
        loaded,
        path,
    })
}

fn commit_op(
    ctx: &mut OpContext,
    provider: &str,
    session_id: &str,
    kind: &str,
    description: String,
    base_description: Option<String>,
    changes: Vec<LineChange>,
    new_lines: Vec<String>,
) -> AppResult<EditApplyReport> {
    let snapshot = ensure_snapshot(&ctx.dir, &ctx.path, &ctx.loaded.hash, &ctx.journal)?;
    let after_hash = write_lines(
        &ctx.path,
        &new_lines,
        ctx.loaded.trailing_newline,
        &ctx.loaded.hash,
    )?;
    let changed = changes
        .iter()
        .filter(|c| c.before.is_some() && c.after.is_some())
        .count() as u32;
    let deleted = changes.iter().filter(|c| c.after.is_none()).count() as u32;
    let restored = changes.iter().filter(|c| c.before.is_none()).count() as u32;
    let entry = JournalEntry {
        op_id: new_op_id(ctx.journal.len()),
        ts: now_rfc3339(),
        kind: kind.into(),
        provider: provider.into(),
        session_id: session_id.into(),
        rollout_path: ctx.path.to_string_lossy().into_owned(),
        description,
        base_description,
        base_snapshot: snapshot.clone(),
        before_hash: ctx.loaded.hash.clone(),
        after_hash,
        changes,
    };
    append_journal(&ctx.dir, &entry)?;
    Ok(EditApplyReport {
        op_id: entry.op_id,
        kind: kind.into(),
        snapshot_created: snapshot,
        changed_lines: changed,
        deleted_lines: deleted,
        restored_lines: restored,
    })
}

pub fn apply_edit_text(
    provider: &str,
    rollout_path: &str,
    session_id: &str,
    backup_dir: &str,
    line_no: usize,
    new_text: &str,
) -> AppResult<EditApplyReport> {
    let provider = provider_normalized(provider)?;
    let mut ctx = open_op_context(provider, rollout_path, session_id, backup_dir)?;
    let changes = match provider {
        "codex" => codex_edit_changes(&ctx.loaded.lines, &ctx.loaded.parsed, line_no, new_text)?,
        _ => claude_edit_changes(&ctx.loaded.lines, &ctx.loaded.parsed, line_no, new_text)?,
    };
    let mut new_lines = ctx.loaded.lines.clone();
    forward_apply(&mut new_lines, &changes)?;
    let mirror_note = if changes.len() > 1 {
        format!("（含镜像行 {} 处）", changes.len() - 1)
    } else {
        String::new()
    };
    let description = format!("改写第 {line_no} 行文本{mirror_note}");
    commit_op(
        &mut ctx,
        provider,
        session_id,
        "edit_text",
        description.clone(),
        Some(description),
        changes,
        new_lines,
    )
}

pub fn apply_delete(
    provider: &str,
    rollout_path: &str,
    session_id: &str,
    backup_dir: &str,
    line_nos: &[usize],
) -> AppResult<EditApplyReport> {
    let provider = provider_normalized(provider)?;
    if line_nos.is_empty() {
        return Err(AppError::Other("未选择要删除的事件".into()));
    }
    let mut ctx = open_op_context(provider, rollout_path, session_id, backup_dir)?;
    let (plan, blocked) = match provider {
        "codex" => codex_expand_delete(&ctx.loaded.parsed, line_nos)?,
        _ => claude_expand_delete(&ctx.loaded.parsed, line_nos)?,
    };
    if !blocked.is_empty() {
        return Err(AppError::Other(format!(
            "存在不可删除的行：{}",
            blocked.join("；")
        )));
    }
    if plan.is_empty() {
        return Err(AppError::Other("没有可删除的行".into()));
    }

    let mut changes: Vec<LineChange> = if provider == "claude" {
        claude_relink_changes(&ctx.loaded.lines, &ctx.loaded.parsed, &plan)?
    } else {
        Vec::new()
    };
    for (&i, _) in plan.iter() {
        changes.push(LineChange {
            line_no: i,
            before: Some(ctx.loaded.lines[i].clone()),
            after: None,
        });
    }

    let mut new_lines = ctx.loaded.lines.clone();
    forward_apply(&mut new_lines, &changes)?;
    let cascaded = plan
        .values()
        .filter(|r| r.as_str() != REASON_SELECTED)
        .count();
    let description = format!(
        "删除 {} 个事件（选中 {}，级联 {}）",
        plan.len(),
        plan.len() - cascaded,
        cascaded
    );
    commit_op(
        &mut ctx,
        provider,
        session_id,
        "delete_events",
        description.clone(),
        Some(description),
        changes,
        new_lines,
    )
}

pub fn undo_last(
    provider: &str,
    rollout_path: &str,
    session_id: &str,
    backup_dir: &str,
) -> AppResult<EditApplyReport> {
    let provider = provider_normalized(provider)?;
    let mut ctx = open_op_context(provider, rollout_path, session_id, backup_dir)?;
    let last = ctx
        .journal
        .last()
        .cloned()
        .ok_or_else(|| AppError::Other("该会话没有编辑记录".into()))?;
    if last.after_hash != ctx.loaded.hash {
        return Err(AppError::Other(
            "会话文件在本工具之外被修改过，无法直接撤销；可从原始快照还原".into(),
        ));
    }
    if last.changes.is_empty() {
        return Err(AppError::Other(
            "上一步是快照还原，没有可逆放的行级变更；请从快照列表继续还原".into(),
        ));
    }

    let mut new_lines = ctx.loaded.lines.clone();
    let redo = last.kind == "undo";
    if redo {
        forward_apply(&mut new_lines, &last.changes)?;
    } else {
        reverse_apply(&mut new_lines, &last.changes)?;
    }
    let expected = last.before_hash.clone();
    let base = last
        .base_description
        .clone()
        .unwrap_or_else(|| last.description.clone());
    let description = if redo {
        format!("重做：{base}")
    } else {
        format!("撤销：{base}")
    };
    let report = commit_op(
        &mut ctx,
        provider,
        session_id,
        "undo",
        description,
        Some(base),
        last.changes.clone(),
        new_lines,
    )?;
    // 校验逆放结果与记录一致（不一致仅提示，不回滚——journal 里已有完整字节可再撤销）
    let now = load_file(&ctx.path)?;
    if now.hash != expected {
        return Err(AppError::Other(
            "撤销已执行，但结果哈希与记录不一致，请从原始快照核对".into(),
        ));
    }
    // commit_op 按原操作语义统计；撤销一次删除实际是恢复行
    let mut report = report;
    if !redo {
        report.restored_lines = report.deleted_lines;
        report.deleted_lines = 0;
    }
    Ok(report)
}

pub fn restore_snapshot(
    provider: &str,
    rollout_path: &str,
    session_id: &str,
    backup_dir: &str,
    snapshot_name: &str,
) -> AppResult<EditApplyReport> {
    let provider = provider_normalized(provider)?;
    if snapshot_name.contains('/') || snapshot_name.contains('\\') || snapshot_name.contains("..") {
        return Err(AppError::Other("快照名称不合法".into()));
    }
    let mut ctx = open_op_context(provider, rollout_path, session_id, backup_dir)?;
    let snap_path = ctx.dir.join(snapshot_name);
    if !snap_path.is_file() {
        return Err(AppError::NotFound(snap_path.to_string_lossy().into_owned()));
    }
    // 还原前先把当前状态存为新快照，保证任何状态都可回退
    let pre_name = format!(
        "pre-restore-{}.jsonl",
        chrono::Utc::now().timestamp_millis()
    );
    fs::create_dir_all(&ctx.dir)?;
    fs::copy(&ctx.path, ctx.dir.join(&pre_name))?;

    let snap_loaded = load_file(&snap_path)?;
    let after_hash = write_lines(
        &ctx.path,
        &snap_loaded.lines,
        snap_loaded.trailing_newline,
        &ctx.loaded.hash,
    )?;
    let description = format!("还原快照 {snapshot_name}（还原前状态已保存为 {pre_name}）");
    let entry = JournalEntry {
        op_id: new_op_id(ctx.journal.len()),
        ts: now_rfc3339(),
        kind: "restore_snapshot".into(),
        provider: provider.into(),
        session_id: session_id.into(),
        rollout_path: ctx.path.to_string_lossy().into_owned(),
        description: description.clone(),
        base_description: None,
        base_snapshot: Some(snapshot_name.to_string()),
        before_hash: ctx.loaded.hash.clone(),
        after_hash,
        changes: Vec::new(),
    };
    append_journal(&ctx.dir, &entry)?;
    ctx.journal.push(entry.clone());
    Ok(EditApplyReport {
        op_id: entry.op_id,
        kind: "restore_snapshot".into(),
        snapshot_created: Some(pre_name),
        changed_lines: 0,
        deleted_lines: 0,
        restored_lines: snap_loaded.lines.len() as u32,
    })
}

pub fn history(
    provider: &str,
    rollout_path: &str,
    session_id: &str,
    backup_dir: &str,
) -> AppResult<EditHistory> {
    let provider = provider_normalized(provider)?;
    let path = PathBuf::from(paths::strip_verbatim(rollout_path));
    let dir = edit_dir(backup_dir, provider, session_id);
    let journal = read_journal(&dir)?;

    let mut snapshots: Vec<EditSnapshotInfo> = Vec::new();
    if dir.is_dir() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".jsonl") || name == "journal.jsonl" {
                continue;
            }
            let meta = entry.metadata()?;
            let created_at = chrono::DateTime::<chrono::Utc>::from(meta.modified()?).to_rfc3339();
            snapshots.push(EditSnapshotInfo {
                name,
                created_at,
                bytes: meta.len(),
            });
        }
    }
    snapshots.sort_by(|a, b| b.name.cmp(&a.name));

    let current_hash = if path.is_file() {
        Some(load_file(&path)?.hash)
    } else {
        None
    };
    let (undo_available, undo_blocked_reason) = match (journal.last(), current_hash.as_deref()) {
        (None, _) => (false, None),
        (Some(_), None) => (false, Some("会话文件不存在".to_string())),
        (Some(last), Some(hash)) => {
            if last.after_hash != hash {
                (
                    false,
                    Some("会话文件在本工具之外被修改过，只能从快照还原".to_string()),
                )
            } else if last.changes.is_empty() {
                (
                    false,
                    Some("上一步是快照还原，请继续使用快照回退".to_string()),
                )
            } else {
                (true, None)
            }
        }
    };

    let entries = journal
        .iter()
        .rev()
        .map(|e| EditHistoryEntry {
            op_id: e.op_id.clone(),
            ts: e.ts.clone(),
            kind: e.kind.clone(),
            description: e.description.clone(),
            changes: e.changes.len() as u32,
        })
        .collect();

    Ok(EditHistory {
        entries,
        snapshots,
        undo_available,
        undo_blocked_reason,
    })
}

// ========================= 命令包装（带锁） =========================

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn plan_session_event_deletion(
    provider: String,
    rollout_path: String,
    line_nos: Vec<usize>,
) -> AppResult<DeletePlan> {
    plan_delete(&provider, &rollout_path, &line_nos)
}

pub fn edit_session_event_text_with_lock(
    provider: String,
    rollout_path: String,
    session_id: String,
    backup_dir: String,
    line_no: usize,
    new_text: String,
    lock: &crate::family::FamilyLock,
) -> AppResult<EditApplyReport> {
    crate::family::with_lock(lock, |_g| {
        apply_edit_text(
            &provider,
            &rollout_path,
            &session_id,
            &backup_dir,
            line_no,
            &new_text,
        )
    })
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn edit_session_event_text(
    provider: String,
    rollout_path: String,
    session_id: String,
    backup_dir: String,
    line_no: usize,
    new_text: String,
    lock: tauri::State<'_, crate::family::FamilyLock>,
) -> AppResult<EditApplyReport> {
    edit_session_event_text_with_lock(
        provider,
        rollout_path,
        session_id,
        backup_dir,
        line_no,
        new_text,
        lock.inner(),
    )
}

pub fn delete_session_events_with_lock(
    provider: String,
    rollout_path: String,
    session_id: String,
    backup_dir: String,
    line_nos: Vec<usize>,
    lock: &crate::family::FamilyLock,
) -> AppResult<EditApplyReport> {
    crate::family::with_lock(lock, |_g| {
        apply_delete(
            &provider,
            &rollout_path,
            &session_id,
            &backup_dir,
            &line_nos,
        )
    })
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn delete_session_events(
    provider: String,
    rollout_path: String,
    session_id: String,
    backup_dir: String,
    line_nos: Vec<usize>,
    lock: tauri::State<'_, crate::family::FamilyLock>,
) -> AppResult<EditApplyReport> {
    delete_session_events_with_lock(
        provider,
        rollout_path,
        session_id,
        backup_dir,
        line_nos,
        lock.inner(),
    )
}

pub fn undo_last_session_edit_with_lock(
    provider: String,
    rollout_path: String,
    session_id: String,
    backup_dir: String,
    lock: &crate::family::FamilyLock,
) -> AppResult<EditApplyReport> {
    crate::family::with_lock(lock, |_g| {
        undo_last(&provider, &rollout_path, &session_id, &backup_dir)
    })
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn undo_last_session_edit(
    provider: String,
    rollout_path: String,
    session_id: String,
    backup_dir: String,
    lock: tauri::State<'_, crate::family::FamilyLock>,
) -> AppResult<EditApplyReport> {
    undo_last_session_edit_with_lock(provider, rollout_path, session_id, backup_dir, lock.inner())
}

pub fn restore_session_edit_snapshot_with_lock(
    provider: String,
    rollout_path: String,
    session_id: String,
    backup_dir: String,
    snapshot_name: String,
    lock: &crate::family::FamilyLock,
) -> AppResult<EditApplyReport> {
    crate::family::with_lock(lock, |_g| {
        restore_snapshot(
            &provider,
            &rollout_path,
            &session_id,
            &backup_dir,
            &snapshot_name,
        )
    })
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn restore_session_edit_snapshot(
    provider: String,
    rollout_path: String,
    session_id: String,
    backup_dir: String,
    snapshot_name: String,
    lock: tauri::State<'_, crate::family::FamilyLock>,
) -> AppResult<EditApplyReport> {
    restore_session_edit_snapshot_with_lock(
        provider,
        rollout_path,
        session_id,
        backup_dir,
        snapshot_name,
        lock.inner(),
    )
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn session_edit_history(
    provider: String,
    rollout_path: String,
    session_id: String,
    backup_dir: String,
) -> AppResult<EditHistory> {
    history(&provider, &rollout_path, &session_id, &backup_dir)
}

// ========================= 测试 =========================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("cc-edit-{name}-{}-{nanos}", std::process::id()))
    }

    fn write_jsonl(path: &Path, values: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut body = values
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        body.push('\n');
        fs::write(path, body).unwrap();
    }

    fn codex_fixture() -> Vec<Value> {
        vec![
            json!({"timestamp":"t0","type":"session_meta","payload":{"id":"sess-1","cwd":"/w"}}),
            json!({"timestamp":"t1","type":"event_msg","payload":{"type":"user_message","message":"hello world"}}),
            json!({"timestamp":"t1","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello world"}]}}),
            json!({"timestamp":"t1","type":"turn_context","payload":{"cwd":"/w"}}),
            json!({"timestamp":"t2","type":"response_item","payload":{"type":"reasoning","summary":[],"encrypted_content":"AAA"}}),
            json!({"timestamp":"t2","type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"c1","arguments":"{}"}}),
            json!({"timestamp":"t2","type":"event_msg","payload":{"type":"exec_command_begin","call_id":"c1","command":["ls"]}}),
            json!({"timestamp":"t2","type":"event_msg","payload":{"type":"exec_command_end","call_id":"c1","exit_code":0}}),
            json!({"timestamp":"t2","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}),
            json!({"timestamp":"t3","type":"response_item","payload":{"type":"reasoning","summary":[],"encrypted_content":"BBB"}}),
            json!({"timestamp":"t3","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"the answer"}]}}),
            json!({"timestamp":"t3","type":"event_msg","payload":{"type":"agent_message","message":"the answer"}}),
            json!({"timestamp":"t3","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":10}}}}),
        ]
    }

    fn claude_fixture() -> Vec<Value> {
        vec![
            json!({"type":"summary","summary":"标题","leafUuid":"u4"}),
            json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","message":{"role":"user","content":"hi"}}),
            json!({"type":"assistant","uuid":"u2","parentUuid":"u1","sessionId":"s","message":{"role":"assistant","content":[
                {"type":"thinking","thinking":"let me look","signature":"SIG"},
                {"type":"text","text":"I'll check"},
                {"type":"tool_use","id":"tu1","name":"Read","input":{}}
            ]}}),
            json!({"type":"user","uuid":"u3","parentUuid":"u2","sessionId":"s","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu1","content":"file data"}
            ]}}),
            json!({"type":"assistant","uuid":"u4","parentUuid":"u3","sessionId":"s","message":{"role":"assistant","content":[
                {"type":"text","text":"done"}
            ]}}),
        ]
    }

    fn read_bytes(p: &Path) -> Vec<u8> {
        fs::read(p).unwrap()
    }

    #[test]
    fn codex_edit_text_syncs_mirror_and_undo_restores_bytes() {
        let root = temp_dir("codex-edit");
        let rollout = root.join("rollout.jsonl");
        let backup = root.join("backup");
        write_jsonl(&rollout, &codex_fixture());
        let original = read_bytes(&rollout);

        let report = apply_edit_text(
            "codex",
            rollout.to_str().unwrap(),
            "sess-1",
            backup.to_str().unwrap(),
            10,
            "edited answer",
        )
        .unwrap();
        assert_eq!(report.changed_lines, 2, "应同步镜像行");
        assert!(report.snapshot_created.is_some(), "首次编辑应建快照");

        let loaded = load_file(&rollout).unwrap();
        let v10 = loaded.parsed[10].as_ref().unwrap();
        assert_eq!(codex_flat_text(v10), "edited answer");
        let v11 = loaded.parsed[11].as_ref().unwrap();
        assert_eq!(codex_event_message_text(v11), Some("edited answer"));
        // 未动行保持原字节
        let before = String::from_utf8(original.clone()).unwrap();
        assert_eq!(
            before.split('\n').nth(2).unwrap(),
            loaded.lines[2],
            "未编辑的行不应被改写"
        );

        undo_last(
            "codex",
            rollout.to_str().unwrap(),
            "sess-1",
            backup.to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(read_bytes(&rollout), original, "撤销后应与原文件逐字节一致");

        // 再撤销一次 = 重做
        undo_last(
            "codex",
            rollout.to_str().unwrap(),
            "sess-1",
            backup.to_str().unwrap(),
        )
        .unwrap();
        let redone = load_file(&rollout).unwrap();
        assert_eq!(
            codex_flat_text(redone.parsed[10].as_ref().unwrap()),
            "edited answer"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn codex_delete_tool_output_cascades_call_events_and_reasoning() {
        let root = temp_dir("codex-del-tool");
        let rollout = root.join("rollout.jsonl");
        let backup = root.join("backup");
        write_jsonl(&rollout, &codex_fixture());
        let original = read_bytes(&rollout);

        let plan = plan_delete("codex", rollout.to_str().unwrap(), &[8]).unwrap();
        let planned: Vec<usize> = plan.lines.iter().map(|l| l.line_no).collect();
        assert_eq!(planned, vec![4, 5, 6, 7, 8], "应级联 call/事件/前置推理");
        assert!(plan.blocked.is_empty());

        let report = apply_delete(
            "codex",
            rollout.to_str().unwrap(),
            "sess-1",
            backup.to_str().unwrap(),
            &[8],
        )
        .unwrap();
        assert_eq!(report.deleted_lines, 5);

        let loaded = load_file(&rollout).unwrap();
        assert_eq!(loaded.lines.len(), 8);
        // 后续 assistant 回复与其推理保留
        assert!(loaded
            .parsed
            .iter()
            .flatten()
            .any(|v| codex_ptype(v) == "reasoning"));

        undo_last(
            "codex",
            rollout.to_str().unwrap(),
            "sess-1",
            backup.to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(read_bytes(&rollout), original);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn codex_delete_assistant_message_takes_mirror_and_reasoning() {
        let root = temp_dir("codex-del-msg");
        let rollout = root.join("rollout.jsonl");
        write_jsonl(&rollout, &codex_fixture());

        let plan = plan_delete("codex", rollout.to_str().unwrap(), &[10]).unwrap();
        let planned: Vec<usize> = plan.lines.iter().map(|l| l.line_no).collect();
        assert_eq!(planned, vec![9, 10, 11], "镜像 event_msg 与前置推理应联动");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn codex_meta_lines_are_blocked() {
        let root = temp_dir("codex-blocked");
        let rollout = root.join("rollout.jsonl");
        write_jsonl(&rollout, &codex_fixture());
        let plan = plan_delete("codex", rollout.to_str().unwrap(), &[0]).unwrap();
        assert!(!plan.blocked.is_empty());
        let plan2 = plan_delete("codex", rollout.to_str().unwrap(), &[3]).unwrap();
        assert!(!plan2.blocked.is_empty(), "turn_context 应被拒绝");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn codex_reasoning_cannot_be_edited() {
        let root = temp_dir("codex-edit-reasoning");
        let rollout = root.join("rollout.jsonl");
        let backup = root.join("backup");
        write_jsonl(&rollout, &codex_fixture());
        let err = apply_edit_text(
            "codex",
            rollout.to_str().unwrap(),
            "sess-1",
            backup.to_str().unwrap(),
            4,
            "new",
        )
        .unwrap_err();
        assert!(err.to_string().contains("不能改写"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn claude_edit_text_preserves_thinking_and_tools() {
        let root = temp_dir("claude-edit");
        let rollout = root.join("s.jsonl");
        let backup = root.join("backup");
        write_jsonl(&rollout, &claude_fixture());

        apply_edit_text(
            "claude",
            rollout.to_str().unwrap(),
            "s",
            backup.to_str().unwrap(),
            2,
            "I'll verify instead",
        )
        .unwrap();

        let loaded = load_file(&rollout).unwrap();
        let content = claude_content(loaded.parsed[2].as_ref().unwrap()).unwrap();
        let items = content.as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["thinking"], "let me look");
        assert_eq!(items[0]["signature"], "SIG", "thinking 签名必须原样保留");
        assert_eq!(items[1]["text"], "I'll verify instead");
        assert_eq!(items[2]["type"], "tool_use");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn claude_tool_result_only_line_rejects_edit() {
        let root = temp_dir("claude-edit-toolresult");
        let rollout = root.join("s.jsonl");
        let backup = root.join("backup");
        write_jsonl(&rollout, &claude_fixture());
        let err = apply_edit_text(
            "claude",
            rollout.to_str().unwrap(),
            "s",
            backup.to_str().unwrap(),
            3,
            "new",
        )
        .unwrap_err();
        assert!(err.to_string().contains("没有可改写的文本"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn claude_delete_cascades_tool_pair_and_relinks_parent() {
        let root = temp_dir("claude-del");
        let rollout = root.join("s.jsonl");
        let backup = root.join("backup");
        write_jsonl(&rollout, &claude_fixture());
        let original = read_bytes(&rollout);

        let plan = plan_delete("claude", rollout.to_str().unwrap(), &[3]).unwrap();
        let planned: Vec<usize> = plan.lines.iter().map(|l| l.line_no).collect();
        assert_eq!(planned, vec![2, 3], "tool_result 应连带其 tool_use 消息");

        apply_delete(
            "claude",
            rollout.to_str().unwrap(),
            "s",
            backup.to_str().unwrap(),
            &[3],
        )
        .unwrap();

        let loaded = load_file(&rollout).unwrap();
        assert_eq!(loaded.lines.len(), 3);
        let last = loaded.parsed[2].as_ref().unwrap();
        assert_eq!(claude_uuid(last), Some("u4"));
        assert_eq!(
            last.get("parentUuid").and_then(Value::as_str),
            Some("u1"),
            "u4 的 parent 应重连到幸存祖先 u1"
        );

        undo_last(
            "claude",
            rollout.to_str().unwrap(),
            "s",
            backup.to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(read_bytes(&rollout), original);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn claude_delete_leaf_relinks_summary_leaf_uuid() {
        let root = temp_dir("claude-del-leaf");
        let rollout = root.join("s.jsonl");
        let backup = root.join("backup");
        write_jsonl(&rollout, &claude_fixture());

        apply_delete(
            "claude",
            rollout.to_str().unwrap(),
            "s",
            backup.to_str().unwrap(),
            &[4],
        )
        .unwrap();

        let loaded = load_file(&rollout).unwrap();
        let summary = loaded.parsed[0].as_ref().unwrap();
        assert_eq!(
            summary.get("leafUuid").and_then(Value::as_str),
            Some("u3"),
            "summary.leafUuid 应重连到 u4 的父节点 u3"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn snapshot_restore_recovers_original_after_external_change() {
        let root = temp_dir("snapshot-restore");
        let rollout = root.join("s.jsonl");
        let backup = root.join("backup");
        write_jsonl(&rollout, &claude_fixture());
        let original = read_bytes(&rollout);

        let report = apply_edit_text(
            "claude",
            rollout.to_str().unwrap(),
            "s",
            backup.to_str().unwrap(),
            1,
            "hello again",
        )
        .unwrap();
        let snap = report.snapshot_created.expect("首次编辑必建快照");

        // 模拟外部改动（CLI 追加了一行）
        {
            let mut f = fs::OpenOptions::new().append(true).open(&rollout).unwrap();
            writeln!(f, "{}", json!({"type":"user","uuid":"u9","parentUuid":"u4","message":{"role":"user","content":"ext"}})).unwrap();
        }

        let h = history(
            "claude",
            rollout.to_str().unwrap(),
            "s",
            backup.to_str().unwrap(),
        )
        .unwrap();
        assert!(!h.undo_available, "外部改动后不允许直接撤销");
        assert!(h.undo_blocked_reason.is_some());
        assert!(h.snapshots.iter().any(|s| s.name == snap));

        restore_snapshot(
            "claude",
            rollout.to_str().unwrap(),
            "s",
            backup.to_str().unwrap(),
            &snap,
        )
        .unwrap();
        assert_eq!(read_bytes(&rollout), original, "还原快照应恢复原始字节");

        let h2 = history(
            "claude",
            rollout.to_str().unwrap(),
            "s",
            backup.to_str().unwrap(),
        )
        .unwrap();
        assert!(h2
            .snapshots
            .iter()
            .any(|s| s.name.starts_with("pre-restore-")));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn consecutive_edits_reuse_snapshot() {
        let root = temp_dir("snapshot-reuse");
        let rollout = root.join("s.jsonl");
        let backup = root.join("backup");
        write_jsonl(&rollout, &claude_fixture());

        let r1 = apply_edit_text(
            "claude",
            rollout.to_str().unwrap(),
            "s",
            backup.to_str().unwrap(),
            1,
            "first",
        )
        .unwrap();
        assert!(r1.snapshot_created.is_some());
        let r2 = apply_edit_text(
            "claude",
            rollout.to_str().unwrap(),
            "s",
            backup.to_str().unwrap(),
            1,
            "second",
        )
        .unwrap();
        assert!(r2.snapshot_created.is_none(), "连续编辑不应重复建快照");
        fs::remove_dir_all(&root).ok();
    }
}
