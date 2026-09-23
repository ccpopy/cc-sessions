//! Native alpha.16 rollout_file_name / thread_rollout_resolver use the physical
//! rollout ID for history tables, and the stable logical ID for Core metadata.
use super::*;
use crate::models::{EditProjectionStatus, ProjectionItemEvidence};

pub(in crate::edit) fn rollout_identity(
    path: &Path,
    loaded: &LoadedFile,
) -> AppResult<ProjectionIdentity> {
    let thread = identity(loaded)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let parsed = name
        .strip_prefix("rollout-")
        .and_then(|s| s.strip_suffix(".jsonl"))
        .filter(|s| s.as_bytes().get(19) == Some(&b'-'))
        .and_then(|s| {
            chrono::NaiveDateTime::parse_from_str(s.get(..19)?, "%Y-%m-%dT%H-%M-%S").ok()?;
            let ids = s.get(20..)?;
            let (logical, physical) = ids.split_once('_').unwrap_or((ids, ids));
            (uuid_shape(logical) && uuid_shape(physical) && logical == thread).then_some(physical)
        });
    // Unit fixtures deliberately use short IDs. Production accepts only the
    // canonical native filename, including agreement with session_meta.id.
    #[cfg(test)]
    let parsed =
        parsed.or_else(|| (thread == "thread-1" && name == "rollout-test.jsonl").then_some(thread));
    let Some(rollout) = parsed else {
        return Err(failure(base_status(path, loaded), "identity_pending", "ROLLOUT_IDENTITY_UNVERIFIED", "身份待核对：日志文件名与会话元数据无法按原生规则对应，请刷新会话列表并核对当前日志路径"));
    };
    Ok(ProjectionIdentity {
        thread_id: thread.into(),
        rollout_id: rollout.into(),
        rollout_path: path.canonicalize()?.to_string_lossy().into(),
        selected_rollout_path: None,
    })
}

fn uuid_shape(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

fn base_status(path: &Path, loaded: &LoadedFile) -> EditProjectionStatus {
    EditProjectionStatus {
        state: "ready".into(),
        reason_code: "PROJECTION_READY".into(),
        message: "日志与当前原生投影已同步".into(),
        thread_id: identity(loaded).unwrap_or("").into(),
        rollout_id: String::new(),
        rollout_path: path.to_string_lossy().into(),
        next_rollout_byte_offset: None,
        next_rollout_ordinal: None,
        file_bytes: loaded.lines.iter().map(|s| s.len() as u64).sum::<u64>()
            + loaded.lines.len().saturating_sub(1) as u64
            + u64::from(loaded.trailing_newline),
        item: None,
    }
}

fn failure(mut status: EditProjectionStatus, state: &str, code: &str, message: &str) -> AppError {
    status.state = state.into();
    status.reason_code = code.into();
    status.message = message.into();
    AppError::EditProjection(Box::new(status))
}

pub(in crate::edit) fn observe(
    path: &Path,
    loaded: &LoadedFile,
) -> AppResult<(HistoryImage, EditProjectionStatus)> {
    let identity = rollout_identity(path, loaded)?;
    let mut status = base_status(path, loaded);
    status.rollout_id = identity.rollout_id.clone();
    let result = observe_snapshot(path, loaded, &identity, &mut status);
    // The file and SQLite cannot share a transaction. Reject any intervening
    // append/replacement before interpreting a missing row or a stale checkpoint.
    if atomic_file::fingerprint(path)?.sha256_hex() != loaded.hash {
        return Err(failure(
            status,
            "updating",
            "ROLLOUT_CHANGING",
            "正在更新：日志在读取期间变化，已暂停写入判断；请稍后刷新",
        ));
    }
    result.map(|image| (image, status))
}

fn observe_snapshot(
    path: &Path,
    loaded: &LoadedFile,
    identity: &ProjectionIdentity,
    status: &mut EditProjectionStatus,
) -> AppResult<HistoryImage> {
    let conn = open(&super::path(path)?, false).map_err(|error| {
        // Schema restrictions remain explicit; database I/O failure does not
        // establish that native projection itself failed or history is corrupt.
        if matches!(error, AppError::Sqlite(_)) {
            failure(status.clone(), "failed", "PROJECTION_READ_FAILED", "更新失败：本次无法读取历史数据库；解除数据库占用后刷新，尚不能据此判断原生投影失败")
        } else { error }
    })?;
    let image = capture_for(&conn, &identity.rollout_id, Some(identity)).map_err(|error| {
        if error.to_string().contains("PROJECTION_CHANGING") {
            failure(
                status.clone(),
                "updating",
                "PROJECTION_CHANGING",
                "正在更新：数据库在读取期间变化，有限重试后仍未稳定，请稍后刷新",
            )
        } else {
            failure(
                status.clone(),
                "failed",
                "PROJECTION_READ_FAILED",
                "更新失败：无法取得一致的历史数据库快照；请解除占用并核对数据库可读性后刷新",
            )
        }
    })?;
    if let Some(selected) = &image.identity.as_ref().unwrap().selected_rollout_path {
        if Path::new(&paths::strip_verbatim(selected))
            .canonicalize()
            .ok()
            .as_ref()
            != Some(&path.canonicalize()?)
        {
            return Err(failure(status.clone(), "identity_pending", "SELECTED_ROLLOUT_CHANGED", "身份待核对：Core 当前选中的日志路径与此预览不同；请刷新会话列表并打开当前日志，未读取或覆盖旧投影"));
        }
    } else if attached(&conn)? {
        return Err(failure(
            status.clone(),
            "identity_pending",
            "SELECTED_THREAD_MISSING",
            "身份待核对：Core 中没有该会话的当前日志记录，请核对数据根与会话身份",
        ));
    }
    let checkpoint = image.rows["thread_history_projection_state"].first();
    status.next_rollout_byte_offset =
        checkpoint.and_then(|r| r["next_rollout_byte_offset"].as_u64());
    status.next_rollout_ordinal = checkpoint.and_then(|r| r["next_rollout_ordinal"].as_u64());
    let mut offset = 0;
    for (i, line) in loaded.lines.iter().enumerate() {
        let end = offset
            + line.len() as u64
            + u64::from(i + 1 < loaded.lines.len() || loaded.trailing_newline);
        if let Some(v) = &loaded.parsed[i] {
            if codex_ptype(v) == "item_completed"
                && !image.rows["thread_items"].iter().any(|r| {
                    r["turn_id"] == v["payload"]["turn_id"]
                        && r["item_id"] == v["payload"]["item"]["id"]
                })
            {
                let ordinal = v["ordinal"].as_u64().unwrap_or(u64::MAX);
                status.item = Some(ProjectionItemEvidence {
                    turn_id: v["payload"]["turn_id"].as_str().unwrap_or("").into(),
                    item_id: v["payload"]["item"]["id"].as_str().unwrap_or("").into(),
                    item_type: v["payload"]["item"]["type"].as_str().unwrap_or("").into(),
                    ordinal,
                    start_byte: offset,
                    end_byte: end,
                    covered: status.next_rollout_byte_offset.is_some_and(|b| b >= end)
                        && status.next_rollout_ordinal.is_some_and(|o| o > ordinal),
                });
                break;
            }
        }
        offset = end;
    }
    let Some(bytes) = status.next_rollout_byte_offset else {
        return Err(failure(
            status.clone(),
            "updating",
            "PROJECTION_NOT_STARTED",
            "正在更新：当前 rollout 尚无投影检查点；等待原生历史同步后刷新，不能据此认定记录缺失",
        ));
    };
    if bytes > status.file_bytes {
        return Err(failure(
            status.clone(),
            "identity_pending",
            "CHECKPOINT_OUTSIDE_ROLLOUT",
            "身份待核对：投影检查点超出当前日志长度，请核对 rollout 路径及原生读取结果",
        ));
    }
    if bytes < status.file_bytes || !loaded.trailing_newline {
        return Err(failure(status.clone(), "updating", "PROJECTION_BEHIND", "正在更新：日志已落盘，投影检查点尚未追上；停止生成并完成原生历史读取后刷新。尚无证据认定原生投影持续失败"));
    }
    model(loaded)?;
    if let Some(item) = &status.item {
        let meta = loaded
            .parsed
            .iter()
            .flatten()
            .find(|v| codex_outer(v) == "session_meta")
            .unwrap();
        if meta["payload"]["subagent_history_start_ordinal"]
            .as_u64()
            .is_some_and(|start| item.ordinal < start)
        {
            return Err(failure(
                status.clone(),
                "identity_pending",
                "NATIVE_FILTERED_RANGE",
                "该项处于原生明确过滤的继承子代理历史范围；需核对历史来源，不视为记录损坏",
            ));
        }
        let record = loaded
            .parsed
            .iter()
            .flatten()
            .find(|v| v["ordinal"] == item.ordinal)
            .unwrap();
        if record["payload"]["started_at_ms"].is_null()
            && chrono::DateTime::parse_from_rfc3339(record["timestamp"].as_str().unwrap_or(""))
                .is_err()
        {
            return Err(failure(status.clone(), "identity_pending", "NATIVE_TIMESTAMP_FILTERED", "该项缺少原生投影需要的有效时间戳；原生读取会跳过此记录，需核对日志来源，未自动重建投影"));
        }
        if !matches!(
            item.item_type.as_str(),
            "UserMessage"
                | "AgentMessage"
                | "Reasoning"
                | "CommandExecution"
                | "McpToolCall"
                | "FileChange"
        ) {
            return Err(failure(status.clone(), "identity_pending", "NATIVE_ITEM_PARSE_UNVERIFIED", "该 item 类型的原生解析/过滤规则尚未核实；请在隔离副本用匹配版本读取，不能据此认定历史损坏"));
        }
        if !item.covered {
            return Err(failure(
                status.clone(),
                "identity_pending",
                "CHECKPOINT_ORDINAL_MISMATCH",
                "身份待核对：字节检查点与 ordinal 覆盖范围不对应，请核对原生解析结果",
            ));
        }
        return Err(failure(status.clone(), "inconsistent", "PROJECTED_ITEM_MISSING", "已确认异常：同一 rollout 的检查点已覆盖该正式 item，但投影记录缺失。请在隔离副本核对匹配版本的解析/过滤结果；未自动重建数据库"));
    }
    if project(loaded, &image)? != image {
        return Err(failure(status.clone(), "inconsistent", "PROJECTION_CONTENT_MISMATCH", "已确认异常：稳定读取下，当前 rollout 与其投影的消息、回合或检查点不一致；写入已保护，请在隔离副本核对原生读取结果"));
    }
    Ok(image)
}
