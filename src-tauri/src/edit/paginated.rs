//! In-place edits of canonical paginated history and its thread-scoped projection.
use super::*;
pub(super) mod content_mapping;
pub(super) mod projection;
mod tool_message;
pub(super) use projection::{HistoryChange, HistoryImage};

pub(super) fn from_lines(lines: &[String], trailing_newline: bool) -> LoadedFile {
    LoadedFile {
        lines: lines.to_vec(),
        parsed: lines.iter().map(|s| serde_json::from_str(s).ok()).collect(),
        trailing_newline,
        hash: transaction::lines_hash(lines, trailing_newline),
    }
}

pub(super) struct WriterGuard {
    _rollout: fs::File,
    _coordination: fs::File,
}

pub(super) fn writer_guard(path: &Path) -> AppResult<WriterGuard> {
    // Native 0.155 alpha uses this coordination lock before opening/removing a
    // per-thread writer lock. Hold coordination through publication (upstream
    // WriterLockCoordinator::try_acquire_for_publication), including on Unix.
    let loaded = load_file(path)?;
    let id = loaded
        .parsed
        .iter()
        .flatten()
        .find(|v| codex_outer(v) == "session_meta")
        .and_then(|v| v["payload"]["id"].as_str())
        .ok_or_else(|| unsupported("缺少写入锁身份"))?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(unsupported("写入锁身份不合法"));
    }
    let directory = safety::codex_root(path)
        .ok_or_else(|| unsupported("缺少数据根"))?
        .join("thread-writer-locks");
    fs::create_dir_all(&directory)?;
    let coordination = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(".coordination.lock"))?;
    coordination.try_lock().map_err(|e| {
        AppError::Other(format!(
            "[EDIT_BUSY] 原生写入协调锁正被使用：{e}；停止会话写入后重试"
        ))
    })?;
    match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.join(format!("{id}.lock")))
    {
        Ok(file) => file.try_lock().map_err(|e| {
            AppError::Other(format!(
                "[EDIT_BUSY] 会话仍由原生进程持有：{e}；卸载或关闭该会话后重试"
            ))
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1 | 4); // permit readers/atomic replacement, deny an open append writer
    }
    let rollout = options.open(path).map_err(|e| {
        AppError::Other(format!(
            "[EDIT_BUSY] 无法排除会话写入方：{e}；请关闭该会话的原生写入进程后重试"
        ))
    })?;
    Ok(WriterGuard {
        _rollout: rollout,
        _coordination: coordination,
    })
}

pub(super) fn is_paginated(loaded: &LoadedFile) -> bool {
    loaded
        .parsed
        .iter()
        .flatten()
        .any(|v| v["payload"]["history_mode"] == "paginated" || codex_ptype(v) == "item_completed")
}

fn unsupported(message: impl Into<String>) -> AppError {
    AppError::Other(format!("[EDIT_UNSUPPORTED] {}", message.into()))
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ItemKey {
    turn: String,
    id: String,
}

struct Item {
    key: ItemKey,
    kind: String,
    records: Vec<usize>,
    contexts: Vec<usize>,
}
struct History {
    items: Vec<Item>,
    turns: Vec<Option<String>>,
}

fn model(loaded: &LoadedFile) -> AppResult<History> {
    let thread = loaded
        .parsed
        .iter()
        .flatten()
        .find(|v| codex_outer(v) == "session_meta")
        .and_then(|v| v["payload"]["id"].as_str())
        .ok_or_else(|| unsupported("缺少会话身份"))?;
    let mut items: Vec<Item> = Vec::new();
    let mut active: Option<String> = None;
    let mut turns = Vec::new();
    let mut previous_ordinal = None;
    for (i, v) in loaded.parsed.iter().enumerate() {
        let Some(v) = v else {
            return Err(unsupported(format!("第 {} 行无法解析", i + 1)));
        };
        let ordinal = v["ordinal"]
            .as_u64()
            .ok_or_else(|| unsupported("分页记录缺少 ordinal；请先用匹配版本读取并核对该会话"))?;
        if previous_ordinal.is_some_and(|prev| prev >= ordinal) {
            return Err(unsupported("分页 ordinal 重复或倒序"));
        }
        previous_ordinal = Some(ordinal);
        let p = &v["payload"];
        if codex_ptype(v) == "task_started" {
            active = p["turn_id"].as_str().map(str::to_owned);
        }
        let turn = p["turn_id"]
            .as_str()
            .or_else(|| p["internal_chat_message_metadata_passthrough"]["turn_id"].as_str())
            .map(str::to_owned)
            .or_else(|| active.clone());
        turns.push(turn.clone());
        if codex_ptype(v) == "item_completed" {
            if p["thread_id"].as_str() != Some(thread) {
                return Err(unsupported("正式消息 thread_id 与会话身份不一致"));
            }
            let key = ItemKey {
                turn: p["turn_id"]
                    .as_str()
                    .ok_or_else(|| unsupported("正式消息缺少 turn_id"))?
                    .into(),
                id: p["item"]["id"]
                    .as_str()
                    .ok_or_else(|| unsupported("正式消息缺少 item ID"))?
                    .into(),
            };
            let kind = p["item"]["type"]
                .as_str()
                .ok_or_else(|| unsupported("正式消息缺少类型"))?
                .to_owned();
            if let Some(item) = items.iter_mut().find(|it| it.key == key) {
                if item.kind != kind {
                    return Err(unsupported("同一 item 的快照类型发生变化"));
                }
                item.records.push(i);
            } else {
                items.push(Item {
                    key,
                    kind,
                    records: vec![i],
                    contexts: Vec::new(),
                });
            }
        }
        if matches!(codex_ptype(v), "task_complete" | "turn_aborted") {
            active = None;
        }
    }
    // User response IDs differ from canonical IDs. Match the ordered, explicit user-input
    // records within a turn, never their text or an unbounded nearest-line heuristic.
    let user_order: Vec<(ItemKey, usize)> = items
        .iter()
        .filter(|it| it.kind == "UserMessage")
        .map(|it| (it.key.clone(), it.records[0]))
        .collect();
    for item in &mut items {
        if item.kind == "UserMessage" {
            let first = item.records[0];
            let previous = user_order
                .iter()
                .filter(|(key, i)| key.turn == item.key.turn && *i < first)
                .map(|(_, i)| *i + 1)
                .max()
                .unwrap_or(0);
            let candidates: Vec<usize> = (previous..first)
                .filter(|&i| {
                    let v = loaded.parsed[i].as_ref().unwrap();
                    codex_outer(v) == "response_item"
                        && codex_msg_role(v) == "user"
                        && turns[i].as_deref() == Some(&item.key.turn)
                        && v["payload"]["internal_chat_message_metadata_passthrough"]
                            ["content_item_kinds"]
                            .as_array()
                            .is_some_and(|k| {
                                k.iter()
                                    .any(|s| s.as_str().is_some_and(|s| s.starts_with("user.")))
                            })
                })
                .collect();
            if candidates.len() == 1 {
                item.contexts = candidates;
            }
        } else {
            item.contexts = (0..loaded.parsed.len())
                .filter(|&i| {
                    let v = loaded.parsed[i].as_ref().unwrap();
                    codex_outer(v) == "response_item"
                        && turns[i].as_deref() == Some(&item.key.turn)
                        && v["payload"]["id"].as_str() == Some(&item.key.id)
                })
                .collect();
            if item.kind == "AgentMessage" && item.contexts.is_empty() {
                item.contexts = loaded
                    .parsed
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| {
                        (turns[i].as_deref() == Some(&item.key.turn)
                            && tool_message::is_call(v.as_ref().unwrap(), &item.key.id))
                        .then_some(i)
                    })
                    .collect();
            }
        }
    }
    Ok(History { items, turns })
}

pub(super) fn inspect(path: &Path, loaded: &LoadedFile) -> AppResult<()> {
    model(loaded)?;
    projection::read(path, loaded)?;
    Ok(())
}

pub(super) fn diagnostics(
    loaded: &LoadedFile,
) -> AppResult<Vec<crate::models::ContentMappingDetail>> {
    let h = model(loaded)?;
    let mut diagnostics = Vec::new();
    let dependency = loaded.parsed.iter().enumerate().rev().find_map(|(i, v)| {
        let v = v.as_ref()?;
        (matches!(codex_outer(v), "compacted" | "retained_context")
            || matches!(codex_ptype(v), "context_compacted" | "thread_rolled_back"))
        .then_some(i)
    });
    let ended: BTreeSet<_> = loaded
        .parsed
        .iter()
        .flatten()
        .filter(|v| matches!(codex_ptype(v), "task_complete" | "turn_aborted"))
        .filter_map(|v| v["payload"]["turn_id"].as_str())
        .collect();
    let mut turn_starts = BTreeMap::new();
    for (i, turn) in h.turns.iter().enumerate() {
        if let Some(turn) = turn {
            turn_starts.entry(turn.as_str()).or_insert(i);
        }
    }
    for item in h
        .items
        .iter()
        .filter(|item| matches!(item.kind.as_str(), "UserMessage" | "AgentMessage"))
    {
        let mut details = content_mapping::item_mappings(loaded, item);
        let first = item
            .records
            .iter()
            .chain(&item.contexts)
            .min()
            .copied()
            .unwrap();
        for detail in &mut details {
            let restrict =
                |cap: &mut crate::models::MessageOperationCapability, code: &str, reason: &str| {
                    if cap.supported {
                        *cap = crate::models::MessageOperationCapability {
                            supported: false,
                            reason_code: Some(code.into()),
                            reason: Some(reason.into()),
                        };
                    }
                };
            if !ended.contains(item.key.turn.as_str()) {
                for op in [
                    &mut detail.operations.edit_text,
                    &mut detail.operations.delete_message,
                    &mut detail.operations.delete_turn,
                ] {
                    restrict(op, "TURN_NOT_ENDED", "该回合尚未结束；停止写入并刷新后重试");
                }
            }
            if dependency.is_some_and(|i| i >= first) {
                for op in [
                    &mut detail.operations.edit_text,
                    &mut detail.operations.delete_message,
                ] {
                    restrict(op,"CONTEXT_DEPENDENCY","该消息被后续压缩或保留上下文引用；需先重建相关摘要，摘要之后的独立消息仍可操作");
                }
            }
            if dependency.is_some_and(|i| i >= turn_starts[item.key.turn.as_str()]) {
                restrict(
                    &mut detail.operations.delete_turn,
                    "CONTEXT_DEPENDENCY",
                    "该回合涉及压缩或保留上下文；不能连带删除仍被引用的历史",
                );
            }
            if let Some(other) = h.items.iter().find(|other| {
                other.key.turn == item.key.turn
                    && !matches!(
                        other.kind.as_str(),
                        "UserMessage"
                            | "AgentMessage"
                            | "Reasoning"
                            | "CommandExecution"
                            | "McpToolCall"
                            | "FileChange"
                    )
            }) {
                restrict(
                    &mut detail.operations.delete_turn,
                    "TURN_ITEM_UNMAPPED",
                    &format!(
                        "该回合包含尚未适配的 {}；单条消息按自身关联处理",
                        other.kind
                    ),
                );
            }
        }
        diagnostics.extend(details);
    }
    Ok(diagnostics)
}

fn message(loaded: &LoadedFile, item: &Item, reason: &str) -> crate::models::DeletePlanMessage {
    let i = *item.records.last().unwrap();
    let raw = loaded.parsed[i].as_ref().unwrap();
    let (role, _, summary) = codex_line_brief(raw);
    crate::models::DeletePlanMessage {
        target: Some(crate::models::PaginatedItemTarget {
            thread_id: raw["payload"]["thread_id"].as_str().unwrap().into(),
            turn_id: item.key.turn.clone(),
            item_id: item.key.id.clone(),
        }),
        line_no: i,
        role,
        summary,
        reason: reason.into(),
    }
}

pub(super) fn logical_messages(
    loaded: &LoadedFile,
    plan: &BTreeMap<usize, String>,
) -> AppResult<Vec<crate::models::DeletePlanMessage>> {
    Ok(model(loaded)?
        .items
        .iter()
        .filter_map(|it| {
            let reason = it
                .records
                .iter()
                .filter_map(|i| plan.get(i))
                .find(|r| r.as_str() == REASON_SELECTED)
                .or_else(|| it.records.iter().find_map(|i| plan.get(i)))?;
            Some(message(loaded, it, reason))
        })
        .collect())
}

pub(super) fn required_turns(
    loaded: &LoadedFile,
    selected: &[usize],
) -> AppResult<Vec<crate::models::DeleteTurnSelection>> {
    let h = model(loaded)?;
    let selected_items: BTreeSet<_> = h
        .items
        .iter()
        .filter(|it| it.records.iter().any(|i| selected.contains(i)))
        .map(|it| &it.key)
        .collect();
    let turns: BTreeSet<_> = h
        .items
        .iter()
        .filter(|it| {
            selected_items.contains(&it.key)
                && if matches!(it.kind.as_str(), "UserMessage" | "AgentMessage") {
                    let details = content_mapping::item_mappings(loaded, it);
                    details
                        .iter()
                        .any(|d| !d.operations.delete_message.supported)
                        && details.iter().all(|d| d.operations.delete_turn.supported)
                } else {
                    it.kind != "Reasoning"
                }
                && h.items.iter().any(|other| {
                    other.key.turn == it.key.turn && !selected_items.contains(&other.key)
                })
        })
        .map(|it| it.key.turn.as_str())
        .collect();
    Ok(turns
        .into_iter()
        .map(|turn| crate::models::DeleteTurnSelection {
            turn_id: turn.into(),
            messages: h
                .items
                .iter()
                .filter(|it| it.key.turn == turn)
                .map(|it| message(loaded, it, REASON_SELECTED))
                .collect(),
        })
        .collect())
}

fn check_scope(path: &Path, loaded: &LoadedFile, selected: &BTreeSet<usize>) -> AppResult<()> {
    let history = model(loaded)?;
    let touched: BTreeSet<&str> = selected
        .iter()
        .filter_map(|&i| history.turns.get(i)?.as_deref())
        .collect();
    for turn in &touched {
        let ended = loaded.parsed.iter().flatten().any(|v| {
            v["payload"]["turn_id"] == *turn
                && matches!(codex_ptype(v), "task_complete" | "turn_aborted")
        });
        if !ended {
            return Err(unsupported(format!(
                "回合 {turn} 尚未结束；停止该回合并刷新后重试"
            )));
        }
        for (i, v) in loaded.parsed.iter().enumerate() {
            if history.turns[i].as_deref() != Some(*turn) {
                continue;
            }
            let v = v.as_ref().unwrap();
            let known = match codex_outer(v) {
                "session_meta" | "turn_context" | "token_usage_record" | "world_state"
                | "compacted" | "retained_context" => true,
                "response_item" => {
                    matches!(codex_ptype(v), "message" | "reasoning")
                        || CODEX_CALL_TYPES.contains(&codex_ptype(v))
                        || CODEX_CALL_OUTPUT_TYPES.contains(&codex_ptype(v))
                }
                "event_msg" => matches!(
                    codex_ptype(v),
                    "task_started"
                        | "task_complete"
                        | "turn_aborted"
                        | "item_completed"
                        | "token_count"
                        | "thread_settings_applied"
                        | "error"
                        | "warning"
                ),
                _ => false,
            };
            if !known {
                return Err(unsupported(format!(
                    "受影响回合 {turn} 的第 {} 行包含尚未映射的 {}/{}；需增加该记录的关联规则",
                    i + 1,
                    codex_outer(v),
                    codex_ptype(v)
                )));
            }
            if codex_ptype(v) == "item_completed"
                && selected.contains(&i)
                && !matches!(
                    v["payload"]["item"]["type"].as_str(),
                    Some(
                        "UserMessage"
                            | "AgentMessage"
                            | "Reasoning"
                            | "CommandExecution"
                            | "McpToolCall"
                            | "FileChange"
                    )
                )
            {
                return Err(unsupported(format!(
                    "受影响回合 {turn} 包含尚未适配的 item 类型 {}；需核对其上下文及关联状态",
                    v["payload"]["item"]["type"]
                )));
            }
        }
    }
    let min = selected
        .iter()
        .next()
        .copied()
        .unwrap_or(loaded.lines.len());
    for (i, v) in loaded.parsed.iter().enumerate().skip(min) {
        let Some(v) = v else { continue };
        if matches!(codex_outer(v), "compacted" | "retained_context")
            || matches!(codex_ptype(v), "context_compacted" | "thread_rolled_back")
        {
            return Err(unsupported(format!(
                "所选内容被第 {} 行压缩/保留上下文引用；需先重建该摘要，摘要之后的独立消息仍可编辑",
                i + 1
            )));
        }
    }
    if let Some(root) = safety::codex_root(path) {
        let id = loaded
            .parsed
            .iter()
            .flatten()
            .find(|v| codex_outer(v) == "session_meta")
            .unwrap()["payload"]["id"]
            .as_str()
            .unwrap();
        let ordinal = selected
            .iter()
            .filter_map(|&i| loaded.parsed[i].as_ref()?["ordinal"].as_u64())
            .min()
            .unwrap_or(u64::MAX);
        for dir in [root.join("sessions"), root.join("archived_sessions")] {
            if !dir.exists() {
                continue;
            }
            for entry in walkdir::WalkDir::new(dir).follow_links(false) {
                let entry = entry.map_err(|e| unsupported(format!("无法检查共享历史：{e}")))?;
                if entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                let meta = crate::family::read_session_meta(entry.path())?;
                let base = &meta["payload"]["history_base"];
                if base["thread_id"] == id
                    && base["end_ordinal_exclusive"]
                        .as_u64()
                        .is_none_or(|end| ordinal < end)
                {
                    return Err(unsupported(format!("所选 ordinal {ordinal} 被会话 {} 继承；需先解除该共享引用，继承范围之后仍可编辑",meta["payload"]["id"])));
                }
            }
        }
    }
    Ok(())
}

pub(super) fn delete_plan(
    path: &Path,
    loaded: &LoadedFile,
    selected: &[usize],
) -> AppResult<(BTreeMap<usize, String>, Vec<String>)> {
    let h = model(loaded)?;
    let mut plan = BTreeMap::new();
    let mut selected_items = BTreeSet::new();
    for &i in selected {
        let item = h
            .items
            .iter()
            .find(|it| it.records.contains(&i))
            .ok_or_else(|| {
                unsupported(format!(
                    "第 {} 行不是正式 item；请在对话视图中选择消息",
                    i + 1
                ))
            })?;
        selected_items.insert(item.key.clone());
        plan.insert(i, REASON_SELECTED.into());
    }
    let full_turns: BTreeSet<_> = selected_items
        .iter()
        .filter(|key| {
            h.items
                .iter()
                .filter(|it| it.key.turn == key.turn)
                .all(|it| selected_items.contains(&it.key))
        })
        .map(|key| key.turn.as_str())
        .collect();
    for item in &h.items {
        if !selected_items.contains(&item.key) {
            continue;
        }
        if matches!(item.kind.as_str(), "UserMessage" | "AgentMessage") {
            content_mapping::require_delete(
                &content_mapping::item_mappings(loaded, item),
                full_turns.contains(item.key.turn.as_str()),
            )?;
        }
        for &i in &item.records {
            plan.entry(i).or_insert_with(|| "item_snapshot".into());
        }
        if full_turns.contains(item.key.turn.as_str()) {
            for (i, turn) in h.turns.iter().enumerate() {
                if turn.as_deref() == Some(&item.key.turn)
                    && codex_outer(loaded.parsed[i].as_ref().unwrap()) == "response_item"
                {
                    plan.entry(i).or_insert_with(|| REASON_TOOL_PAIR.into());
                }
            }
        } else if item.kind == "UserMessage" {
            if item.contexts.len() != 1
                || h.items
                    .iter()
                    .filter(|other| other.kind == "UserMessage" && other.contexts == item.contexts)
                    .count()
                    != 1
            {
                return Err(unsupported(format!(
                    "回合 {} 的用户上下文映射缺失或不唯一；先核对该回合上下文",
                    item.key.turn
                )));
            }
        } else if item.kind != "AgentMessage" && item.kind != "Reasoning" {
            // Tool adapters have no stable call ID for arbitrary nested executions. Only a
            // fully selected turn permits deleting their entire context/call/result chain.
            if h.items
                .iter()
                .any(|it| it.key.turn == item.key.turn && !selected_items.contains(&it.key))
            {
                return Err(unsupported(format!(
                    "工具 item {} 的调用链涉及整个回合 {}；请选择该回合的全部正式记录",
                    item.key.id, item.key.turn
                )));
            }
            for (i, turn) in h.turns.iter().enumerate() {
                if turn.as_deref() == Some(&item.key.turn)
                    && codex_outer(loaded.parsed[i].as_ref().unwrap()) == "response_item"
                {
                    plan.entry(i).or_insert_with(|| REASON_TOOL_PAIR.into());
                }
            }
        }
        for &i in &item.contexts {
            plan.entry(i).or_insert_with(|| REASON_MIRROR.into());
            if tool_message::is_call(loaded.parsed[i].as_ref().unwrap(), &item.key.id) {
                for (j, row) in loaded.parsed.iter().enumerate() {
                    let row = row.as_ref().unwrap();
                    if h.turns[j].as_deref() == Some(&item.key.turn)
                        && codex_outer(row) == "response_item"
                        && codex_ptype(row) == "function_call_output"
                        && row["payload"]["call_id"] == item.key.id
                    {
                        plan.entry(j).or_insert_with(|| REASON_TOOL_PAIR.into());
                    }
                }
            }
        }
    }
    // Delete attached reasoning using its exact response item ID and canonical snapshots.
    let contexts: Vec<usize> = plan
        .keys()
        .copied()
        .filter(|&i| codex_outer(loaded.parsed[i].as_ref().unwrap()) == "response_item")
        .collect();
    for i in contexts {
        let v = loaded.parsed[i].as_ref().unwrap();
        if codex_msg_role(v) != "assistant" && !CODEX_CALL_TYPES.contains(&codex_ptype(v)) {
            continue;
        }
        if let Some(j) = (0..i)
            .rev()
            .find(|&j| codex_outer(loaded.parsed[j].as_ref().unwrap()) == "response_item")
        {
            if codex_ptype(loaded.parsed[j].as_ref().unwrap()) == "reasoning"
                && h.turns[j] == h.turns[i]
            {
                plan.entry(j).or_insert_with(|| REASON_REASONING.into());
                for it in h.items.iter().filter(|it| it.contexts.contains(&j)) {
                    for &k in &it.records {
                        plan.entry(k).or_insert_with(|| REASON_REASONING.into());
                    }
                }
            }
        }
    }
    check_scope(path, loaded, &plan.keys().copied().collect())?;
    Ok((plan, vec![]))
}

pub(super) fn edit_changes(
    path: &Path,
    loaded: &LoadedFile,
    index: usize,
    text: &str,
    edits: Option<&[crate::models::TextBlockEdit]>,
) -> AppResult<Vec<LineChange>> {
    let h = model(loaded)?;
    let item = h
        .items
        .iter()
        .find(|it| it.records.contains(&index))
        .ok_or_else(|| unsupported("请选择正式消息"))?;
    if !matches!(item.kind.as_str(), "UserMessage" | "AgentMessage") {
        return Err(unsupported("工具和推理内容不支持文本改写"));
    }
    let mappings = content_mapping::item_mappings(loaded, item);
    content_mapping::require_edit(&mappings)?;
    if item.contexts.is_empty()
        || (item.kind == "UserMessage"
            && (item.contexts.len() != 1
                || h.items
                    .iter()
                    .filter(|it| it.kind == "UserMessage" && it.contexts == item.contexts)
                    .count()
                    != 1))
    {
        return Err(unsupported("消息上下文缺失或不唯一，需先核对该回合"));
    }
    let indices: BTreeSet<usize> = item.records.iter().chain(&item.contexts).copied().collect();
    check_scope(path, loaded, &indices)?;
    let canonical = &loaded.parsed[*item.records.last().unwrap()]
        .as_ref()
        .unwrap()["payload"]["item"]["content"];
    let positions = text_positions(canonical)?;
    let single;
    let edits = if let Some(edits) = edits {
        edits
    } else {
        if positions.len() != 1 {
            return Err(unsupported(
                "多文本块消息请逐块改写，不能将全部文本合并到首块",
            ));
        }
        single = vec![crate::models::TextBlockEdit {
            content_index: positions[0],
            text: text.into(),
        }];
        &single
    };
    let mut seen = BTreeSet::new();
    if edits.is_empty()
        || edits
            .iter()
            .any(|e| !positions.contains(&e.content_index) || !seen.insert(e.content_index))
    {
        return Err(unsupported("改写包含重复或非文本内容块"));
    }
    let mut updated = canonical.clone();
    for edit in edits {
        replace_block(&mut updated[edit.content_index], &edit.text);
    }
    let final_text = positions
        .iter()
        .filter_map(|&i| updated[i]["text"].as_str())
        .collect::<String>();
    let mut changes = Vec::new();
    let tool_source = mappings
        .iter()
        .all(|m| m.source == "tool.request_user_input_async");
    for i in indices {
        let mut raw = loaded.parsed[i].clone().unwrap();
        if tool_source {
            tool_message::rewrite(&mut raw, &final_text)?;
            if raw != *loaded.parsed[i].as_ref().unwrap() {
                changes.push(LineChange {
                    line_no: i,
                    before: Some(loaded.lines[i].clone()),
                    after: Some(serde_json::to_string(&raw)?),
                });
            }
            continue;
        }
        let formal = codex_ptype(&raw) == "item_completed";
        let mapping = if formal {
            let content = &raw["payload"]["item"]["content"];
            if content.as_array().map(Vec::len) != canonical.as_array().map(Vec::len)
                || text_positions(content)? != positions
            {
                return Err(unsupported(
                    "同一消息的快照内容块结构不同，无法逐块关联；请先核对快照",
                ));
            }
            positions.clone()
        } else {
            let mapping = mappings
                .iter()
                .find(|m| m.context_ordinal == raw["ordinal"].as_u64())
                .unwrap();
            positions
                .iter()
                .map(|&position| {
                    mapping
                        .block_pairs
                        .iter()
                        .find(|pair| pair.canonical_index == position)
                        .unwrap()
                        .context_index
                })
                .collect()
        };
        let content = if codex_ptype(&raw) == "item_completed" {
            &mut raw["payload"]["item"]["content"]
        } else {
            &mut raw["payload"]["content"]
        };
        for edit in edits {
            let ordinal = positions
                .iter()
                .position(|&p| p == edit.content_index)
                .unwrap();
            replace_block(&mut content[mapping[ordinal]], &edit.text);
        }
        if raw == *loaded.parsed[i].as_ref().unwrap() {
            continue;
        }
        changes.push(LineChange {
            line_no: i,
            before: Some(loaded.lines[i].clone()),
            after: Some(serde_json::to_string(&raw)?),
        });
    }
    // Native turn completion carries a copy of the last assistant text.
    if final_agent(&h, loaded, &item.key.turn, &BTreeMap::new(), false)
        .is_some_and(|last| last.key == item.key)
    {
        for (i, v) in loaded.parsed.iter().enumerate() {
            let Some(v) = v else { continue };
            if codex_ptype(v) == "task_complete" && v["payload"]["turn_id"] == item.key.turn {
                let mut raw = v.clone();
                raw["payload"]["last_agent_message"] = Value::String(final_text.clone());
                changes.push(LineChange {
                    line_no: i,
                    before: Some(loaded.lines[i].clone()),
                    after: Some(serde_json::to_string(&raw)?),
                });
            }
        }
    }
    Ok(changes)
}

pub(super) fn validate_change(
    path: &Path,
    before: &LoadedFile,
    after: &LoadedFile,
) -> AppResult<()> {
    let changed = before
        .parsed
        .iter()
        .enumerate()
        .filter_map(|(i, v)| {
            let v = v.as_ref()?;
            let ordinal = &v["ordinal"];
            (!after
                .parsed
                .iter()
                .flatten()
                .any(|a| &a["ordinal"] == ordinal && a == v))
            .then_some(i)
        })
        .collect();
    check_scope(path, before, &changed)?;
    let inserted = after
        .parsed
        .iter()
        .enumerate()
        .filter_map(|(i, v)| {
            let v = v.as_ref()?;
            (!before
                .parsed
                .iter()
                .flatten()
                .any(|a| a["ordinal"] == v["ordinal"] && a == v))
            .then_some(i)
        })
        .collect();
    check_scope(path, after, &inserted)
}

// Preserve block positions, image content and per-block metadata. Text offsets no
// longer describe rewritten text, so reset only their documented text_elements.
fn replace_block(block: &mut Value, text: &str) {
    if block["text"].as_str() == Some(text) {
        return;
    }
    block["text"] = Value::String(text.into());
    if block.get("text_elements").is_some() {
        block["text_elements"] = serde_json::json!([]);
    }
}

fn text_positions(content: &Value) -> AppResult<Vec<usize>> {
    let blocks = content
        .as_array()
        .ok_or_else(|| unsupported("消息内容不是已验证的块数组"))?;
    Ok(blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| {
            matches!(
                b["type"].as_str(),
                Some("text" | "Text" | "input_text" | "output_text")
            ) && b["text"].is_string()
        })
        .map(|(i, _)| i)
        .collect())
}

pub(super) fn deletion_updates(
    loaded: &LoadedFile,
    plan: &BTreeMap<usize, String>,
) -> AppResult<Vec<LineChange>> {
    let h = model(loaded)?;
    let mut changes = Vec::new();
    let turns: BTreeSet<&str> = h
        .items
        .iter()
        .filter(|it| it.kind == "AgentMessage" && it.records.iter().any(|i| plan.contains_key(i)))
        .map(|it| it.key.turn.as_str())
        .collect();
    for (i, v) in loaded.parsed.iter().enumerate() {
        let v = v.as_ref().unwrap();
        if codex_ptype(v) != "task_complete"
            || !turns.contains(v["payload"]["turn_id"].as_str().unwrap_or(""))
        {
            continue;
        }
        let last = final_agent(
            &h,
            loaded,
            v["payload"]["turn_id"].as_str().unwrap(),
            plan,
            false,
        );
        let text = last.map(|it| {
            loaded.parsed[*it.records.last().unwrap()].as_ref().unwrap()["payload"]["item"]
                ["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        });
        let mut raw = v.clone();
        raw["payload"]["last_agent_message"] = text.map(Value::String).unwrap_or(Value::Null);
        if &raw != v {
            changes.push(LineChange {
                line_no: i,
                before: Some(loaded.lines[i].clone()),
                after: Some(serde_json::to_string(&raw)?),
            });
        }
    }
    Ok(changes)
}

fn final_agent<'a>(
    h: &'a History,
    loaded: &LoadedFile,
    turn: &str,
    excluded: &BTreeMap<usize, String>,
    include_async: bool,
) -> Option<&'a Item> {
    // Native thread_history selects final_answer items including async questions;
    // task_complete.last_agent_message comes only from model response messages
    // (core/session/turn.rs), not messages emitted directly by tool handlers.
    let find = |phase: &Value| {
        h.items.iter().rev().find(|it| {
            it.kind == "AgentMessage"
                && it.key.turn == turn
                && !it.records.iter().any(|i| excluded.contains_key(i))
                && (include_async
                    || loaded.parsed[*it.records.last().unwrap()].as_ref().unwrap()["payload"]
                        ["item"]["delivery"]
                        != "async")
                && &loaded.parsed[*it.records.last().unwrap()].as_ref().unwrap()["payload"]["item"]
                    ["phase"]
                    == phase
        })
    };
    find(&Value::String("final_answer".into())).or_else(|| find(&Value::Null))
}

#[cfg(test)]
mod tests;
