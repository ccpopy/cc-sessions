//! Native user-input expansion, verified against rust-v0.155.0-alpha.16
//! (0e2f848bf4a4e8d41a02d848a851ba126c09d185): protocol/models.rs
//! ResponseInputItem::from_user_input and core/event_mapping.rs::parse_user_message.
//! Align by identity and ordered block types, never by searching/normalizing text.
use super::*;
use crate::models::{
    ContentBlockPair, ContentMappingDetail, ContentMappingDifference, ContentWrapper,
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

pub(super) fn require_supported(details: &[ContentMappingDetail]) -> AppResult<()> {
    if let Some(message) = details.iter().find_map(issue) {
        return Err(AppError::Other(message));
    }
    Ok(())
}

pub(super) fn item_mappings(loaded: &LoadedFile, item: &Item) -> Vec<ContentMappingDetail> {
    if item.contexts.is_empty() {
        vec![map_content(loaded, item, None)]
    } else {
        item.contexts
            .iter()
            .map(|&i| map_content(loaded, item, loaded.parsed[i].as_ref()))
            .collect()
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
        mapping_basis: "正式 item 身份 + 块类型与顺序；媒体包装依据原生 alpha.16 标签和相邻媒体块"
            .into(),
        status: "unsupported".into(),
        reason: None,
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
            if version != "0.155.0-alpha.16" {
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
