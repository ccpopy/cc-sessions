//! request_user_input_async emits an AgentMessage directly from its arguments.
//! Verified with native alpha.9.2/alpha.16 handlers and untouched fixture captures.
use super::*;
use crate::models::{ContentBlockPair, ContentMappingDetail, ContentMappingDifference};

pub(super) fn is_call(raw: &Value, item_id: &str) -> bool {
    let p = &raw["payload"];
    codex_outer(raw) == "response_item"
        && codex_ptype(raw) == "function_call"
        && p["call_id"].as_str() == Some(item_id)
        && p["name"] == "request_user_input_async"
        && (p["namespace"].is_null() || p["namespace"] == "functions")
}

fn arguments(raw: &Value) -> Option<Value> {
    let value: Value = serde_json::from_str(raw["payload"]["arguments"].as_str()?).ok()?;
    let object = value.as_object()?;
    (object.len() == 1 && object.contains_key("questions")).then_some(value)
}

fn normalized_questions(questions: &Value) -> Option<Value> {
    let questions = questions.as_array()?;
    if questions.is_empty() {
        return None;
    }
    let mut result = Vec::new();
    for question in questions {
        let object = question.as_object()?;
        if object.keys().any(|k| k != "title" && k != "options") {
            return None;
        }
        let title = question["title"].as_str()?;
        if title.trim().is_empty() {
            return None;
        }
        if !question["options"].is_null() {
            let options = question["options"].as_array()?;
            if options.is_empty()
                || options
                    .iter()
                    .any(|o| o.as_str().is_none_or(|s| s.trim().is_empty()))
            {
                return None;
            }
        }
        result.push(serde_json::json!({"title":title,"options":question["options"]}));
    }
    Some(Value::Array(result))
}

fn render(questions: &Value) -> Option<String> {
    let normalized = normalized_questions(questions)?;
    Some(
        normalized
            .as_array()?
            .iter()
            .map(|q| {
                let mut lines = vec![q["title"].as_str().unwrap().to_owned()];
                if let Some(options) = q["options"].as_array() {
                    lines.extend(options.iter().map(|v| format!("- {}", v.as_str().unwrap())));
                }
                lines.join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

pub(super) fn map_content(
    canonical: &Value,
    context: &Value,
    mut detail: ContentMappingDetail,
) -> ContentMappingDetail {
    detail.source = "tool.request_user_input_async".into();
    detail.source_kinds = vec![detail.source.clone()];
    detail.context_block_types = vec!["function_call.arguments.questions".into()];
    detail.mapping_basis = "同回合 call_id + request_user_input_async + questions 参数；按原生标题/选项规则渲染并逐字节核对".into();
    let item = &canonical["payload"]["item"];
    let args = arguments(context);
    let expected = args.as_ref().and_then(|a| render(&a["questions"]));
    if item["delivery"] != "async"
        || item["phase"] != "final_answer"
        || item["content"].as_array().map(Vec::len) != Some(1)
        || item["content"][0]["type"] != "Text"
        || !item["content"][0]["text"].is_string()
        || expected.is_none()
        || normalized_questions(&item["questions"]).is_none()
    {
        detail.reason_code = Some("ASYNC_QUESTION_SCHEMA_UNMAPPED".into());
        detail.reason = Some("异步提问的参数或正式消息结构尚未确认".into());
        return detail;
    }
    detail.context_text_blocks = 1;
    detail.context_body_text_blocks = Some(1);
    detail.block_pairs.push(ContentBlockPair {
        canonical_index: 0,
        context_index: 0,
    });
    let expected = expected.unwrap();
    let actual = item["content"][0]["text"].as_str().unwrap();
    if actual != expected
        || normalized_questions(&item["questions"])
            != normalized_questions(&args.unwrap()["questions"])
    {
        detail.status = "inconsistent".into();
        detail.reason_code = Some("ASYNC_QUESTION_BODY_MISMATCH".into());
        detail.reason = Some("异步提问正文或问题字段与同一调用的参数不一致".into());
        detail.first_difference = Some(ContentMappingDifference {
            canonical_index: Some(0),
            context_index: Some(0),
            utf8_byte_offset: (actual != expected).then(|| {
                actual
                    .bytes()
                    .zip(expected.bytes())
                    .position(|(a, b)| a != b)
                    .unwrap_or(actual.len().min(expected.len()))
            }),
        });
    } else {
        detail.status = "matched".into();
    }
    detail
}

// Preserve the existing question/option count. Only accept a reversible rendering;
// never guess ambiguous separators or flatten distinct questions into one title.
fn parse_rendered(template: &Value, text: &str) -> Option<Value> {
    let mut result = template.clone();
    let questions = result.as_array_mut()?;
    let parts: Vec<_> = if questions.len() == 1 {
        vec![text]
    } else {
        text.split("\n\n").collect()
    };
    if parts.len() != questions.len() {
        return None;
    }
    for (question, part) in questions.iter_mut().zip(parts) {
        let count = question["options"].as_array().map_or(0, Vec::len);
        let mut fields: Vec<_> = part.rsplitn(count + 1, "\n- ").collect();
        if fields.len() != count + 1 {
            return None;
        }
        fields.reverse();
        question["title"] = Value::String(fields[0].into());
        if count > 0 {
            question["options"] = serde_json::json!(fields[1..]);
        }
    }
    (render(&result).as_deref() == Some(text)).then_some(result)
}

pub(super) fn can_rewrite(canonical: &Value) -> bool {
    let q = &canonical["payload"]["item"]["questions"];
    render(q).and_then(|text| parse_rendered(q, &text)).as_ref() == Some(q)
}

pub(super) fn rewrite(raw: &mut Value, text: &str) -> AppResult<()> {
    let formal = codex_ptype(raw) == "item_completed";
    let mut args = if formal {
        Value::Null
    } else {
        arguments(raw).ok_or_else(|| unsupported("异步提问参数结构无法解析"))?
    };
    let original = if formal {
        &raw["payload"]["item"]["questions"]
    } else {
        &args["questions"]
    };
    let old_text = render(original).ok_or_else(|| unsupported("异步提问字段无效"))?;
    if parse_rendered(original, &old_text).as_ref() != Some(original) {
        return Err(unsupported(
            "问题中的换行分隔无法唯一还原；不能将问题或选项合并",
        ));
    }
    let updated = parse_rendered(original, text)
        .ok_or_else(|| unsupported("请保持问题和选项的数量及分隔格式，逐项修改文字"))?;
    if formal {
        raw["payload"]["item"]["questions"] = updated;
        raw["payload"]["item"]["content"][0]["text"] = Value::String(text.into());
    } else {
        args["questions"] = updated;
        raw["payload"]["arguments"] = Value::String(serde_json::to_string(&args)?);
    }
    Ok(())
}
