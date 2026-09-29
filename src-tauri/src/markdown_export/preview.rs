//! Small export previews and message summaries; full text is generated only on explicit export/copy.
use super::*;
use serde::Serialize;

const PAGE_EVENTS: usize = 80;
const PAGE_SOURCE_BYTES: usize = 1024 * 1024;
const PREVIEW_BYTES: usize = 64 * 1024;
const PREVIEW_MESSAGES: usize = 20;

#[derive(Serialize)]
pub struct MarkdownMessageSummary {
    index: usize,
    role: &'static str,
    timestamp: String,
    text: String,
}

#[derive(Serialize)]
pub struct MarkdownPreviewPage {
    pub markdown: String,
    pub messages: Vec<MarkdownMessageSummary>,
    pub next_offset: usize,
    pub has_more: bool,
    pub truncated: bool,
}

pub fn preview_session_markdown(
    provider: Option<String>,
    rollout_path: String,
    header: MarkdownExportHeader,
    options: MarkdownExportOptions,
    offset: usize,
) -> AppResult<MarkdownPreviewPage> {
    let provider = provider.as_deref().unwrap_or("codex");
    let _measurement = crate::operation_metrics::Measurement::for_session(
        "markdown_preview",
        provider,
        &rollout_path,
    );
    let filtered = options.time_from.is_some()
        || options.time_to.is_some()
        || options.selected_indices.is_some();
    let paginated =
        provider == "codex" && crate::logical_history::is_paginated(Path::new(&rollout_path))?;
    let (mut events, has_more, next_offset) = if filtered || paginated {
        // The display budget applies after selection. A date range or a selected
        // message can be anywhere in the history, including inherited rollouts.
        let events = if paginated {
            crate::logical_history::read(Path::new(&rollout_path), None)?.events()
        } else {
            preview_session_range(Some(provider.into()), rollout_path.clone(), 0, usize::MAX)?
        };
        let segments: Vec<_> = events.iter().map(segment).collect();
        let (included, _) = plan_inclusion(&events, &segments, &options);
        let mut events: Vec<_> = events
            .into_iter()
            .zip(segments)
            .zip(included)
            .filter_map(|((event, segment), keep)| {
                (keep && visible_segment(&segment, &options)).then_some(event)
            })
            .skip(offset)
            .take(PAGE_EVENTS + 1)
            .collect();
        let more = events.len() > PAGE_EVENTS;
        events.truncate(PAGE_EVENTS);
        let next = offset.saturating_add(events.len());
        (events, more, next)
    } else if matches!(provider, "codex" | "claude") {
        jsonl_page(provider, &rollout_path, offset, &options)?
    } else {
        let mut events =
            preview_session_range(Some(provider.into()), rollout_path, offset, PAGE_EVENTS + 1)?;
        let more = events.len() > PAGE_EVENTS;
        events.truncate(PAGE_EVENTS);
        let next = offset.saturating_add(events.len());
        (events, more, next)
    };
    let messages = events
        .iter()
        .filter_map(|event| {
            let Segment::Message { role, text, .. } = segment(event) else {
                return None;
            };
            Some(MarkdownMessageSummary {
                index: event.index,
                role,
                timestamp: event.timestamp.clone(),
                text: text.chars().take(160).collect(),
            })
        })
        .collect::<Vec<_>>();
    // Bound both the number of rendered messages and the returned UTF-8 bytes.
    let mut message_count = 0;
    let end = events.iter().position(|event| {
        if matches!(segment(event), Segment::Message { .. }) {
            message_count += 1;
        }
        message_count > PREVIEW_MESSAGES
    });
    if let Some(end) = end {
        events.truncate(end);
    }
    let mut markdown = render_markdown(&events, &header, &options).markdown;
    let truncated = has_more || end.is_some() || markdown.len() > PREVIEW_BYTES;
    if markdown.len() > PREVIEW_BYTES {
        let mut end = PREVIEW_BYTES;
        while !markdown.is_char_boundary(end) {
            end -= 1;
        }
        markdown.truncate(end);
    }
    let page = MarkdownPreviewPage {
        markdown,
        messages,
        next_offset,
        has_more,
        truncated,
    };
    _measurement.response(&page);
    Ok(page)
}

fn visible_segment(segment: &Segment, options: &MarkdownExportOptions) -> bool {
    match segment {
        Segment::Message { .. } => true,
        Segment::Reasoning(_) => options.include_reasoning,
        Segment::ToolCalls(_) | Segment::ToolResults(_) | Segment::PatchApplied(_) => {
            options.include_tools
        }
        Segment::Skip => false,
    }
}

fn jsonl_page(
    provider: &str,
    path: &str,
    offset: usize,
    options: &MarkdownExportOptions,
) -> AppResult<(Vec<PreviewEvent>, bool, usize)> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut events = Vec::new();
    let mut line = Vec::new();
    let mut event_offset = 0;
    let mut line_index = 0;
    let mut page_bytes = 0;
    let mut canonical = false;
    loop {
        if events.len() >= PAGE_EVENTS || page_bytes >= PAGE_SOURCE_BYTES {
            return Ok((events, !reader.fill_buf()?.is_empty(), event_offset));
        }
        line.clear();
        // A single native record may exceed the page budget; never allocate without a limit.
        const MAX_EVENT_BYTES: u64 = 64 * 1024 * 1024;
        let count = reader
            .by_ref()
            .take(MAX_EVENT_BYTES + 1)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            return Ok((events, false, event_offset));
        }
        crate::operation_metrics::record(|c| {
            c.read_bytes += count as u64;
            c.parsed_lines += 1;
        });
        if count as u64 > MAX_EVENT_BYTES {
            return Err(AppError::Other(
                "单条记录超过 64 MiB，无法生成受限预览".into(),
            ));
        }
        let index = line_index;
        line_index += 1;
        let Ok(raw) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if raw["type"] == "session_meta" {
            canonical = raw["payload"]["history_mode"] == "paginated";
        }
        let event = if provider == "claude" {
            crate::claude_sessions::classify_preview(index, raw)
        } else {
            Some(crate::rollout::classify_history(index, raw, canonical))
        };
        if let Some(event) = event {
            event_offset += 1;
            if event_offset <= offset || !visible_segment(&segment(&event), options) {
                continue;
            }
            page_bytes += count;
            events.push(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtered_preview_does_not_spend_message_page_on_hidden_process_records() -> AppResult<()> {
        let path = super::super::tests::temp_file("filtered-process-preview");
        let mut file = fs::File::create(&path)?;
        for _ in 0..200 {
            writeln!(
                file,
                "{}",
                serde_json::json!({"type":"event_msg", "payload":{"type":"token_count"}})
            )?;
        }
        for i in 0..100 {
            let mut raw = super::super::tests::user(&format!("LATEST-AFTER-PROCESS-{i}"));
            raw["timestamp"] = serde_json::json!("2026-09-28T10:00:00Z");
            writeln!(file, "{raw}")?;
        }
        drop(file);
        for time_from in [None, Some(1)] {
            let mut options = super::super::tests::default_options();
            options.time_from = time_from;
            let page = preview_session_markdown(
                Some("codex".into()),
                path.to_string_lossy().into_owned(),
                super::super::tests::header(),
                options.clone(),
                0,
            )?;
            assert!(page.markdown.contains("LATEST-AFTER-PROCESS"));
            assert_eq!(page.messages.len(), 80);
            assert!(page.has_more);
            let next = preview_session_markdown(
                Some("codex".into()),
                path.to_string_lossy().into_owned(),
                super::super::tests::header(),
                options,
                page.next_offset,
            )?;
            assert_eq!(next.messages.len(), 20);
            assert_eq!(next.messages[0].index, 280);
            assert!(!next.has_more);
        }
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn filtered_preview_finds_recent_messages_beyond_the_first_page() -> AppResult<()> {
        let path = super::super::tests::temp_file("filtered-preview");
        let mut file = fs::File::create(&path)?;
        for i in 0..450 {
            let mut raw = super::super::tests::user(&format!("message-{i}"));
            raw["timestamp"] = serde_json::json!(if i < 200 {
                "2026-09-01T10:00:00Z"
            } else {
                "2026-09-28T10:00:00Z"
            });
            writeln!(file, "{raw}")?;
        }
        drop(file);
        let mut options = super::super::tests::default_options();
        options.time_from = Some(
            chrono::DateTime::parse_from_rfc3339("2026-09-28T00:00:00Z")
                .unwrap()
                .timestamp(),
        );
        let page = preview_session_markdown(
            Some("codex".into()),
            path.to_string_lossy().into_owned(),
            super::super::tests::header(),
            options.clone(),
            0,
        )?;
        assert_eq!(page.messages.first().map(|m| m.index), Some(200));
        assert!(page.markdown.contains("message-200"));
        assert!(!page.markdown.contains("message-0\n"));
        assert!(page.markdown.len() <= PREVIEW_BYTES);
        assert!(page.has_more);
        let next = preview_session_markdown(
            Some("codex".into()),
            path.to_string_lossy().into_owned(),
            super::super::tests::header(),
            options.clone(),
            page.next_offset,
        )?;
        assert_eq!(next.messages.first().map(|m| m.index), Some(280));
        let full = export_session_markdown(
            Some("codex".into()),
            path.to_string_lossy().into_owned(),
            None,
            super::super::tests::header(),
            options.clone(),
        )?;
        assert_eq!(full.message_count, 250);
        assert!(full.markdown.contains("message-449"));
        options.selected_indices = Some(vec![249]);
        let selected = preview_session_markdown(
            Some("codex".into()),
            path.to_string_lossy().into_owned(),
            super::super::tests::header(),
            options,
            0,
        )?;
        assert!(selected.markdown.contains("message-249"));
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn performance_preview_is_bounded_and_summaries_have_no_payload() -> AppResult<()> {
        for provider in ["codex", "claude"] {
            let path = super::super::tests::temp_file("bounded-preview");
            let mut file = fs::File::create(&path)?;
            for _ in 0..500 {
                let raw = if provider == "codex" {
                    super::super::tests::user(&"界".repeat(4000))
                } else {
                    serde_json::json!({"type":"user","message":{"role":"user","content":"界".repeat(4000)}})
                };
                writeln!(file, "{raw}")?;
            }
            drop(file);
            let (page, counters) = crate::operation_metrics::measured(|| {
                preview_session_markdown(
                    Some(provider.into()),
                    path.to_string_lossy().into_owned(),
                    super::super::tests::header(),
                    super::super::tests::default_options(),
                    0,
                )
            });
            let page = page?;
            assert!(page.has_more && page.truncated);
            assert!(page.markdown.len() <= PREVIEW_BYTES);
            assert!(page.messages.len() <= PAGE_EVENTS);
            assert!(page.messages.iter().all(|m| m.text.chars().count() <= 160));
            assert!(counters.read_bytes < 2 * PAGE_SOURCE_BYTES as u64);
            assert!(serde_json::to_string(&page)?.len() < 128 * 1024);
            let next = preview_session_markdown(
                Some(provider.into()),
                path.to_string_lossy().into_owned(),
                super::super::tests::header(),
                super::super::tests::default_options(),
                page.next_offset,
            )?;
            assert_eq!(next.messages[0].index, page.next_offset);
            fs::remove_file(path)?;
        }
        Ok(())
    }
}
