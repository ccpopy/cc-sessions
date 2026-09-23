//! Fail closed for history formats whose native representations we cannot coordinate.
use super::*;
use crate::models::EditCapability;

pub(super) fn revision(path: &Path, file_hash: &str) -> AppResult<String> {
    Ok(sha_hex(
        format!("{}\0{file_hash}", path.canonicalize()?.display()).as_bytes(),
    ))
}

pub(super) fn check_revision(
    path: &Path,
    loaded: &LoadedFile,
    expected: Option<&str>,
) -> AppResult<()> {
    if expected != Some(revision(path, &loaded.hash)?.as_str()) {
        return Err(AppError::Other(
            "[EDIT_CONFLICT] 未执行：预览之后会话发生更新或缺少预览版本，请刷新并重新选择。".into(),
        ));
    }
    Ok(())
}

pub(super) fn inspect(
    provider: &str,
    path: &Path,
    loaded: &LoadedFile,
) -> AppResult<EditCapability> {
    let mut result = EditCapability {
        revision: revision(path, &loaded.hash)?,
        file_sha256: loaded.hash.clone(),
        thread_id: None,
        format: "legacy".into(),
        blocked_reasons: Vec::new(),
        diagnostics: Vec::new(),
    };
    if loaded
        .lines
        .iter()
        .zip(&loaded.parsed)
        .any(|(line, value)| !line.trim().is_empty() && value.is_none())
    {
        result
            .blocked_reasons
            .push("存在无法解析的记录，消息写入已禁用".into());
    }
    if provider != "codex" {
        return Ok(result);
    }
    let meta = loaded
        .parsed
        .iter()
        .flatten()
        .find(|v| codex_outer(v) == "session_meta")
        .and_then(|v| v.get("payload"));
    result.thread_id = meta
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if result.thread_id.is_none() {
        result
            .blocked_reasons
            .push("缺少 session_meta 会话身份".into());
    }
    let history_mode = meta
        .and_then(|v| v.get("history_mode"))
        .and_then(Value::as_str);
    if history_mode == Some("paginated")
        || loaded
            .parsed
            .iter()
            .flatten()
            .any(|v| codex_outer(v) == "event_msg" && codex_ptype(v) == "item_completed")
    {
        result.format = "paginated".into();
        result.diagnostics = paginated::diagnostics(loaded).unwrap_or_default();
        if let Err(error) = paginated::inspect(path, loaded) {
            result.blocked_reasons.push(error.to_string());
        }
        return Ok(result);
    } else if history_mode.is_some_and(|mode| mode != "legacy") {
        result.format = "unknown".into();
        result
            .blocked_reasons
            .push("未识别的历史模式，消息写入已禁用".into());
    }
    if meta
        .and_then(|v| v.get("history_base"))
        .is_some_and(|v| !v.is_null())
    {
        result
            .blocked_reasons
            .push("会话引用共享历史，无法安全原地修改".into());
    }
    let mut active_turn = false;
    for value in loaded.parsed.iter().flatten() {
        match (codex_outer(value), codex_ptype(value)) {
            ("compacted", _) => result
                .blocked_reasons
                .push("会话包含压缩摘要，无法安全重算保留上下文".into()),
            ("session_meta" | "turn_context", _) => {}
            ("response_item", kind)
                if matches!(kind, "message" | "reasoning")
                    || CODEX_CALL_TYPES.contains(&kind)
                    || CODEX_CALL_OUTPUT_TYPES.contains(&kind) => {}
            ("event_msg", "task_started") => active_turn = true,
            ("event_msg", "task_complete" | "turn_aborted") => active_turn = false,
            ("event_msg", kind)
                if matches!(
                    kind,
                    "user_message"
                        | "agent_message"
                        | "agent_reasoning"
                        | "token_count"
                        | "exec_command_begin"
                        | "exec_command_end"
                        | "exec_command_output_delta"
                        | "mcp_tool_call_begin"
                        | "mcp_tool_call_end"
                        | "patch_apply_begin"
                        | "patch_apply_end"
                        | "web_search_begin"
                        | "web_search_end"
                        | "warning"
                        | "error"
                        | "turn_diff"
                        | "context_compacted"
                        | "thread_rolled_back"
                ) =>
            {
                if matches!(kind, "context_compacted" | "thread_rolled_back") {
                    result
                        .blocked_reasons
                        .push("会话包含上下文压缩或回退记录，无法安全原地修改".into());
                }
            }
            (outer, kind) if result.format == "legacy" => result
                .blocked_reasons
                .push(format!("未支持的记录 {outer}/{kind}，消息写入已禁用")),
            _ => {}
        }
    }
    if active_turn {
        result
            .blocked_reasons
            .push("会话存在尚未结束的回合，请先停止生成；安静的文件不代表写入方已退出".into());
    }
    // Existing projected history and shared descendants cannot be repaired by rewriting JSONL.
    if result.blocked_reasons.is_empty() {
        if let Some(root) = codex_root(path) {
            if let Err(error) = inspect_dependencies(
                &root,
                result.thread_id.as_deref().unwrap(),
                &mut result.blocked_reasons,
            ) {
                result
                    .blocked_reasons
                    .push(format!("无法确认历史依赖，消息写入已禁用：{error}"));
            }
        }
    }
    result.blocked_reasons.sort();
    result.blocked_reasons.dedup();
    Ok(result)
}

pub(super) fn codex_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| {
            matches!(
                p.file_name().and_then(|s| s.to_str()),
                Some("sessions" | "archived_sessions")
            )
        })
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

fn inspect_dependencies(root: &Path, id: &str, blocked: &mut Vec<String>) -> AppResult<()> {
    let history = root.join("thread_history_1.sqlite");
    if history.try_exists()? {
        let db = rusqlite::Connection::open_with_flags(
            history,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        for table in [
            "thread_items",
            "thread_turns",
            "thread_realtime_items",
            "thread_history_projection_state",
        ] {
            let found: bool = db.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE thread_id=?1)"),
                [id],
                |r| r.get(0),
            )?;
            if found {
                blocked.push("此会话已有原生历史投影，当前消息写入不支持安全同步该投影".into());
                return Ok(());
            }
        }
    }
    for directory in [root.join("sessions"), root.join("archived_sessions")] {
        if !directory.try_exists()? {
            continue;
        }
        for entry in walkdir::WalkDir::new(directory).follow_links(false) {
            let entry = entry.map_err(|e| AppError::Other(format!("无法检查共享历史依赖：{e}")))?;
            if entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let meta = crate::family::read_session_meta(entry.path())?;
            if meta
                .get("payload")
                .and_then(|p| p.get("history_base"))
                .and_then(|h| h.get("thread_id"))
                .and_then(Value::as_str)
                == Some(id)
            {
                blocked.push("其他会话仍引用此会话的共享历史，消息写入已禁用".into());
                return Ok(());
            }
        }
    }
    Ok(())
}

pub(super) fn ensure_writable(
    provider: &str,
    path: &Path,
    loaded: &LoadedFile,
    id: &str,
) -> AppResult<()> {
    let capability = inspect(provider, path, loaded)?;
    if provider == "codex" && capability.thread_id.as_deref() != Some(id) {
        return Err(AppError::Other(
            "[EDIT_IDENTITY] 会话 ID 与日志元数据不一致，未执行修改".into(),
        ));
    }
    if !capability.blocked_reasons.is_empty() {
        return Err(AppError::Other(format!(
            "[EDIT_UNSUPPORTED] 未执行：{}",
            capability.blocked_reasons.join("；")
        )));
    }
    Ok(())
}
