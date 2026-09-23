//! Native user-input expansion, verified against pinned native versions (fixtures/README.md).
//! (0e2f848bf4a4e8d41a02d848a851ba126c09d185): protocol/models.rs
//! ResponseInputItem::from_user_input and core/event_mapping.rs::parse_user_message.
//! Align by identity and ordered block types, never by searching/normalizing text.
use super::*;
use crate::models::{
    ContentBlockPair, ContentMappingDetail, ContentMappingDifference, ContentWrapper,
    MessageEditOperations, MessageOperationCapability,
};

pub(in crate::edit) fn issue(detail: &ContentMappingDetail) -> Option<String> {
    let label = match detail.status.as_str() {
        "matched" => return None,
        "inconsistent" => "[EDIT_INCONSISTENT] 已确认映射中的正文不一致",
        _ => "[EDIT_MAPPING_UNSUPPORTED] 内容映射尚未支持",
    };
    Some(format!(
        "{label}：回合 {} / 消息 {}；{}。仅限制受影响消息的写入，请查看内容块映射详情并核对原生历史；未自动修改数据。",
        detail.turn_id, detail.item_id, detail.reason.as_deref().unwrap_or("")
    ))
}

pub(super) fn require_edit(details: &[ContentMappingDetail]) -> AppResult<()> {
    for detail in details {
        if !detail.operations.edit_text.supported {
            return Err(AppError::Other(issue(detail).unwrap_or_else(|| {
                format!(
                    "[EDIT_UNSUPPORTED] {}",
                    detail
                        .operations
                        .edit_text
                        .reason
                        .as_deref()
                        .unwrap_or("该消息暂不支持文本改写")
                )
            })));
        }
    }
    Ok(())
}

pub(super) fn item_mappings(loaded: &LoadedFile, item: &Item) -> Vec<ContentMappingDetail> {
    let mut details = if item.contexts.is_empty() {
        vec![map_content(loaded, item, None)]
    } else {
        item.contexts
            .iter()
            .map(|&i| map_content(loaded, item, loaded.parsed[i].as_ref()))
            .collect()
    };
    for detail in &mut details {
        let failure = blocked(
            detail
                .reason_code
                .as_deref()
                .unwrap_or("CONTENT_MAPPING_UNSUPPORTED"),
            detail.reason.as_deref().unwrap_or("内容映射尚未支持"),
        );
        let inconsistent = detail.status == "inconsistent";
        detail.operations.edit_text = if detail.status != "matched" {
            failure.clone()
        } else if detail.canonical_text_blocks == 0 {
            blocked("NO_TEXT_BLOCK", "该消息没有可改写的文本块")
        } else if detail.source == "tool.request_user_input_async"
            && !tool_message::can_rewrite(
                loaded.parsed[*item.records.last().unwrap()]
                    .as_ref()
                    .unwrap(),
            )
        {
            blocked(
                "ASYNC_TEXT_LAYOUT_UNMAPPED",
                "问题中的分隔换行无法唯一还原，删除消息及整轮删除仍可用",
            )
        } else {
            allowed()
        };
        let known_context = !item.contexts.is_empty()
            && item.contexts.iter().all(|&i| {
                let v = loaded.parsed[i].as_ref().unwrap();
                (codex_ptype(v) == "message"
                    && (codex_msg_role(v) == "assistant"
                        || (codex_msg_role(v) == "user"
                            && v["payload"]["internal_chat_message_metadata_passthrough"]
                                ["content_item_kinds"]
                                .as_array()
                                .is_some_and(|kinds| {
                                    Some(kinds.len())
                                        == v["payload"]["content"].as_array().map(Vec::len)
                                        && kinds.iter().all(|kind| {
                                            matches!(
                                                kind.as_str(),
                                                Some("user.text" | "user.image" | "user.audio")
                                            )
                                        })
                                }))))
                    || (tool_message::is_call(v, &item.key.id) && detail.status == "matched")
            });
        detail.operations.delete_message = if inconsistent || !known_context {
            failure.clone()
        } else {
            allowed()
        };
        detail.operations.delete_turn = if inconsistent { failure } else { allowed() };
    }
    details
}

fn allowed() -> MessageOperationCapability {
    MessageOperationCapability {
        supported: true,
        reason_code: None,
        reason: None,
    }
}

fn blocked(code: &str, reason: &str) -> MessageOperationCapability {
    MessageOperationCapability {
        supported: false,
        reason_code: Some(code.into()),
        reason: Some(reason.into()),
    }
}

pub(super) fn require_delete(details: &[ContentMappingDetail], whole_turn: bool) -> AppResult<()> {
    for detail in details {
        let operation = if whole_turn {
            &detail.operations.delete_turn
        } else {
            &detail.operations.delete_message
        };
        if !operation.supported {
            return Err(AppError::Other(issue(detail).unwrap_or_else(|| {
                format!(
                    "[EDIT_UNSUPPORTED] {}",
                    operation
                        .reason
                        .as_deref()
                        .unwrap_or("当前删除范围尚未支持")
                )
            })));
        }
    }
    Ok(())
}

// Explicit versions observed in read-only samples, with matching upstream image
// expansion/label predicates. 0.144 predates audio; later verified versions share
// the audio wrapper rule. Unknown versions still require separate evidence.
fn verified_media_version(version: &str, media: &str) -> bool {
    match version {
        "0.144.0-alpha.4" => media == "image",
        "0.146.0-alpha.3.1" | "0.147.0-alpha.1.2" | "0.147.0-alpha.6.6" | "0.148.0-alpha.15"
        | "0.150.0-alpha.8" | "0.150.0-alpha.12.2" | "0.151.0-alpha.7.2" | "0.152.1"
        | "0.153.0-alpha.5" | "0.153.0" | "0.153.3" | "0.153.4" | "0.154.0-alpha.6.2"
        | "0.155.0-alpha.9" | "0.155.0-alpha.9.2" | "0.155.0-alpha.16" => true,
        _ => false,
    }
}

fn block_types(content: &[Value]) -> Vec<String> {
    content
        .iter()
        .map(|b| b["type"].as_str().unwrap_or("unknown").into())
        .collect()
}

fn is_text(block: &Value) -> bool {
    matches!(
        block["type"].as_str(),
        Some("text" | "Text" | "input_text" | "output_text")
    )
}

fn unsupported_at(
    detail: &mut ContentMappingDetail,
    reason: &str,
    canonical: Option<usize>,
    context: Option<usize>,
) {
    detail.reason = Some(reason.into());
    detail.reason_code = Some(
        match reason {
            "缺少可唯一关联的上下文或内容块数组" => "MISSING_CONTEXT",
            "该客户端版本的媒体包装尚未验证" => "MEDIA_VERSION_NOT_VERIFIED",
            "用户上下文内容块来源缺失或数量不同" => "SOURCE_METADATA_MISSING",
            "内容块类型与来源标签无法确认" => "SOURCE_METADATA_UNMAPPED",
            _ => "CONTENT_SHAPE_UNMAPPED",
        }
        .into(),
    );
    detail.first_difference = Some(ContentMappingDifference {
        canonical_index: canonical,
        context_index: context,
        utf8_byte_offset: None,
    });
}

// Exact native tag predicates, only evaluated at a media slot with an adjacent
// matching media block and closing tag. A canonical text slot is ALWAYS text,
// including literal tags next to media. No trim, tag stripping or fuzzy matching.
fn wrapper_kind(content: &[Value], index: usize, media: &str) -> Option<&'static str> {
    let text = content.get(index)?["text"].as_str()?;
    let (tag, prefix, input, local_kind, inline_kind, close) = match media {
        "image" => (
            "<image>",
            "<image name=",
            "input_image",
            "local_image_open",
            "image_open",
            "</image>",
        ),
        "audio" => (
            "<audio>",
            "<audio name=",
            "input_audio",
            "local_audio_open",
            "audio_open",
            "</audio>",
        ),
        _ => return None,
    };
    if content[index]["type"] != "input_text"
        || content.get(index + 1)?["type"] != input
        || content.get(index + 2)?["type"] != "input_text"
        || content[index + 2]["text"] != close
    {
        return None;
    }
    if text == tag {
        Some(inline_kind)
    } else if text.starts_with(prefix) && text.ends_with('>') {
        Some(local_kind)
    } else {
        None
    }
}

fn map_content(loaded: &LoadedFile, item: &Item, context: Option<&Value>) -> ContentMappingDetail {
    let canonical = loaded.parsed[*item.records.last().unwrap()]
        .as_ref()
        .unwrap();
    let formal = canonical["payload"]["item"]["content"].as_array();
    let context_blocks = context.and_then(|v| v["payload"]["content"].as_array());
    let kinds = context.and_then(|v| {
        v["payload"]["internal_chat_message_metadata_passthrough"]["content_item_kinds"].as_array()
    });
    let version = loaded
        .parsed
        .iter()
        .flatten()
        .find(|v| codex_outer(v) == "session_meta")
        .and_then(|v| v["payload"]["cli_version"].as_str())
        .unwrap_or("unknown");
    let mut detail = ContentMappingDetail {
        thread_id: canonical["payload"]["thread_id"]
            .as_str()
            .unwrap_or_default()
            .into(),
        turn_id: item.key.turn.clone(),
        item_id: item.key.id.clone(),
        context_ordinal: context.and_then(|v| v["ordinal"].as_u64()),
        cli_version: version.into(),
        item_type: item.kind.clone(),
        source: "response_item.message".into(),
        mapping_basis:
            "正式 item 身份 + 块类型与顺序；媒体包装依据已核实版本的精确标签和相邻媒体块".into(),
        status: "unsupported".into(),
        reason_code: None,
        reason: None,
        operations: MessageEditOperations {
            edit_text: allowed(),
            delete_message: allowed(),
            delete_turn: allowed(),
        },
        canonical_block_types: formal.map(|c| block_types(c)).unwrap_or_default(),
        context_block_types: context_blocks.map(|c| block_types(c)).unwrap_or_default(),
        source_kinds: kinds
            .map(|ks| {
                ks.iter()
                    .map(|k| k.as_str().unwrap_or("unknown").into())
                    .collect()
            })
            .unwrap_or_default(),
        canonical_text_blocks: formal
            .map(|c| c.iter().filter(|b| is_text(b)).count())
            .unwrap_or(0),
        context_text_blocks: context_blocks
            .map(|c| c.iter().filter(|b| is_text(b)).count())
            .unwrap_or(0),
        context_body_text_blocks: None,
        block_pairs: Vec::new(),
        wrappers: Vec::new(),
        first_difference: None,
    };
    if context.is_some_and(|v| tool_message::is_call(v, &item.key.id)) {
        return tool_message::map_content(canonical, context.unwrap(), detail);
    }
    let (Some(formal), Some(blocks)) = (formal, context_blocks) else {
        unsupported_at(
            &mut detail,
            "缺少可唯一关联的上下文或内容块数组",
            None,
            None,
        );
        return detail;
    };
    let user = item.kind == "UserMessage";
    if user && kinds.map(Vec::len) != Some(blocks.len()) {
        unsupported_at(
            &mut detail,
            "用户上下文内容块来源缺失或数量不同",
            None,
            None,
        );
        return detail;
    }
    // Source metadata constrains structure but user.text alone is NOT proof of body text.
    for (i, block) in blocks.iter().enumerate() {
        let expected = match block["type"].as_str() {
            Some("input_text") => "user.text",
            Some("input_image") => "user.image",
            Some("input_audio") => "user.audio",
            Some("output_text") if !user => "",
            _ => {
                unsupported_at(&mut detail, "上下文含尚未支持的内容块类型", None, Some(i));
                return detail;
            }
        };
        if (user && kinds.unwrap()[i] != expected) || (is_text(block) && !block["text"].is_string())
        {
            unsupported_at(&mut detail, "内容块类型与来源标签无法确认", None, Some(i));
            return detail;
        }
    }
    let mut cursor = 0;
    for (i, block) in formal.iter().enumerate() {
        let media = match (user, block["type"].as_str()) {
            (true, Some("image" | "local_image")) => Some("image"),
            (true, Some("audio" | "local_audio")) => Some("audio"),
            (true, Some("text")) | (false, Some("Text")) if block["text"].is_string() => None,
            _ => {
                unsupported_at(
                    &mut detail,
                    "正式消息含尚未支持的内容块类型",
                    Some(i),
                    Some(cursor),
                );
                return detail;
            }
        };
        let expected = match media {
            Some("image") => "input_image",
            Some(_) => "input_audio",
            None if user => "input_text",
            None => blocks
                .get(cursor)
                .and_then(|b| b["type"].as_str())
                .filter(|t| matches!(*t, "input_text" | "output_text"))
                .unwrap_or("output_text"),
        };
        let wrapped = media.and_then(|media| wrapper_kind(blocks, cursor, media));
        if let Some(kind) = wrapped {
            if !verified_media_version(version, media.unwrap()) {
                unsupported_at(
                    &mut detail,
                    "该客户端版本的媒体包装尚未验证",
                    Some(i),
                    Some(cursor),
                );
                return detail;
            }
            detail.wrappers.push(ContentWrapper {
                context_index: cursor,
                kind: kind.into(),
            });
            detail.wrappers.push(ContentWrapper {
                context_index: cursor + 2,
                kind: format!("{}_close", media.unwrap()),
            });
            cursor += 1;
        }
        if blocks.get(cursor).is_none_or(|b| b["type"] != expected) {
            unsupported_at(
                &mut detail,
                "移除已确认包装后，块类型或数量无法按顺序对应",
                Some(i),
                Some(cursor),
            );
            return detail;
        }
        detail.block_pairs.push(ContentBlockPair {
            canonical_index: i,
            context_index: cursor,
        });
        cursor += if wrapped.is_some() { 2 } else { 1 };
    }
    if cursor != blocks.len() {
        unsupported_at(
            &mut detail,
            "上下文存在未能关联的剩余内容块",
            None,
            Some(cursor),
        );
        return detail;
    }
    detail.context_body_text_blocks = Some(
        detail
            .block_pairs
            .iter()
            .filter(|p| is_text(&blocks[p.context_index]))
            .count(),
    );
    // Only compare text AFTER the entire ordered mapping has been confirmed.
    for pair in &detail.block_pairs {
        if !is_text(&formal[pair.canonical_index]) {
            continue;
        }
        let a = formal[pair.canonical_index]["text"].as_str().unwrap();
        let b = blocks[pair.context_index]["text"].as_str().unwrap();
        if a != b {
            detail.status = "inconsistent".into();
            detail.reason_code = Some("BODY_MISMATCH".into());
            detail.reason = Some("正文逐字节比较不同（未做标签删除、空白归一化或模糊匹配）".into());
            detail.first_difference = Some(ContentMappingDifference {
                canonical_index: Some(pair.canonical_index),
                context_index: Some(pair.context_index),
                utf8_byte_offset: Some(
                    a.bytes()
                        .zip(b.bytes())
                        .position(|(a, b)| a != b)
                        .unwrap_or(a.len().min(b.len())),
                ),
            });
            return detail;
        }
    }
    detail.status = "matched".into();
    detail
}
