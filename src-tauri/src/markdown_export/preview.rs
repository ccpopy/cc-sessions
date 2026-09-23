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
    let (mut events, has_more) = if matches!(provider, "codex" | "claude") {
        jsonl_page(provider, &rollout_path, offset)?
    } else {
        let mut events =
            preview_session_range(Some(provider.into()), rollout_path, offset, PAGE_EVENTS + 1)?;
        let more = events.len() > PAGE_EVENTS;
        events.truncate(PAGE_EVENTS);
        (events, more)
    };
    let next_offset = offset.saturating_add(events.len());
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

fn jsonl_page(provider: &str, path: &str, offset: usize) -> AppResult<(Vec<PreviewEvent>, bool)> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut events = Vec::new();
    let mut line = Vec::new();
    let mut event_offset = 0;
    let mut line_index = 0;
    let mut page_bytes = 0;
    let mut canonical = false;
    loop {
        if events.len() >= PAGE_EVENTS || page_bytes >= PAGE_SOURCE_BYTES {
            return Ok((events, !reader.fill_buf()?.is_empty()));
        }
        line.clear();
        // A single native record may exceed the page budget; never allocate without a limit.
        const MAX_EVENT_BYTES: u64 = 64 * 1024 * 1024;
        let count = reader
            .by_ref()
            .take(MAX_EVENT_BYTES + 1)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            return Ok((events, false));
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
            if event_offset <= offset {
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
