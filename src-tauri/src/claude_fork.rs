//! Offline Claude forks, following the Agent SDK's session_mutations.py transform.
//! See docs/claude-session-copy.md for the pinned upstream source and scope.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use chrono::Utc;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::family::{self, FamilyLock};
use crate::models::{DuplicateSessionReport, ForkSessionReport, PreviewEvent};
use crate::{atomic_file, claude_sessions, paths};

pub fn duplicate_session_with_lock(
    claude_dir: String,
    session_id: String,
    rollout_path: String,
    lock: &FamilyLock,
) -> AppResult<DuplicateSessionReport> {
    family::with_lock(lock, |_| {
        let (output, destination) = copy_session(&claude_dir, &session_id, &rollout_path, None)?;
        Ok(DuplicateSessionReport {
            source_id: session_id,
            new_id: output.new_id,
            new_rollout_path: paths::strip_verbatim(&destination.to_string_lossy()),
            total_lines: output.entries.len() as u64,
            desktop_restart_required: false,
        })
    })
}

pub fn fork_session_at_event_with_lock(
    claude_dir: String,
    session_id: String,
    rollout_path: String,
    event_index: usize,
    message_uuid: String,
    lock: &FamilyLock,
) -> AppResult<ForkSessionReport> {
    family::with_lock(lock, |_| {
        let (output, destination) = copy_session(
            &claude_dir,
            &session_id,
            &rollout_path,
            Some((event_index, &message_uuid)),
        )?;
        let cut = output.cut.expect("validated cutoff is present");
        Ok(ForkSessionReport {
            desktop_restart_required: false,
            source_id: session_id,
            new_id: output.new_id,
            new_rollout_path: paths::strip_verbatim(&destination.to_string_lossy()),
            event_index: cut.index,
            included_lines: output.entries.len() as u64,
            cut_role: cut.role,
            cut_kind: cut.kind,
            cut_summary: cut.text_summary,
        })
    })
}

#[derive(Debug)]
struct ForkOutput {
    new_id: String,
    entries: Vec<Value>,
    cut: Option<PreviewEvent>,
}

fn copy_session(
    claude_dir: &str,
    session_id: &str,
    rollout_path: &str,
    cutoff: Option<(usize, &str)>,
) -> AppResult<(ForkOutput, PathBuf)> {
    let root = PathBuf::from(paths::strip_verbatim(claude_dir));
    let source: PathBuf = PathBuf::from(paths::strip_verbatim(rollout_path))
        .components()
        .collect();
    claude_sessions::validate_main_transcript(&root, &source, session_id)?;
    let bytes = fs::read(&source)?;
    let content = std::str::from_utf8(&bytes)
        .map_err(|_| AppError::Other("Claude 会话不是有效的 UTF-8，无法复制".into()))?;
    let output = build_fork(content, session_id, cutoff)?;
    let destination = source.with_file_name(format!("{}.jsonl", output.new_id));
    let expected = atomic_file::fingerprint_bytes(&bytes);
    atomic_file::create_with_writer_if_absent(&destination, |file| {
        // Match the SDK's 0o600 creation mode before writing conversation data.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        for entry in &output.entries {
            serde_json::to_writer(&mut *file, entry)?;
            file.write_all(b"\n")?;
        }
        // Claude can still be writing. Publish only a complete, unchanged snapshot;
        // a partial final JSON line is rejected by build_fork before reaching here.
        claude_sessions::validate_main_transcript(&root, &source, session_id)?;
        if atomic_file::fingerprint(&source)? != expected {
            return Err(AppError::AtomicWriteConflict(
                "Claude 会话在复制期间已变化，请刷新后重试".into(),
            ));
        }
        Ok(())
    })?;
    Ok((output, destination))
}

fn build_fork(
    content: &str,
    session_id: &str,
    cutoff: Option<(usize, &str)>,
) -> AppResult<ForkOutput> {
    let mut transcript = Vec::new();
    let mut replacements = Vec::new();
    let mut custom_title = None;
    let mut ai_title = None;
    let mut first_prompt = None;
    let mut recorded_session_id = None;
    let mut cut = None;

    // Keep physical line indexes: preview_range skips blank lines for pagination,
    // but PreviewEvent.index still refers to the original JSONL line.
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: Value = serde_json::from_str(line).map_err(|error| {
            AppError::Other(format!(
                "Claude 会话第 {} 行 JSON 无效，未创建副本: {error}",
                index + 1
            ))
        })?;
        if recorded_session_id.is_none() {
            recorded_session_id = entry
                .get("sessionId")
                .and_then(Value::as_str)
                .map(String::from);
        }
        for (key, target) in [
            ("customTitle", &mut custom_title),
            ("aiTitle", &mut ai_title),
        ] {
            if let Some(text) = entry
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                *target = Some(text.to_string());
            }
        }
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        let sidechain = entry.get("isSidechain").and_then(Value::as_bool) == Some(true);
        if first_prompt.is_none()
            && kind == "user"
            && !sidechain
            && entry.get("isMeta").and_then(Value::as_bool) != Some(true)
        {
            let text = entry
                .pointer("/message/content")
                .map(claude_sessions::extract_text)
                .unwrap_or_default();
            if !text.trim().is_empty() && !claude_sessions::is_generated_user_prompt(&text) {
                first_prompt = Some(text.trim().chars().take(80).collect::<String>());
            }
        }
        if let Some((cut_index, uuid)) = cutoff {
            if index == cut_index {
                if !can_fork_at(&entry) || entry.get("uuid").and_then(Value::as_str) != Some(uuid) {
                    return Err(AppError::Other(
                        "所选 Claude 消息已变化或不支持复制，请刷新预览后重试".into(),
                    ));
                }
                cut = claude_sessions::classify_preview(index, entry.clone());
            }
        }
        if kind == "content-replacement"
            && entry.get("sessionId").and_then(Value::as_str) == Some(session_id)
        {
            if let Some(items) = entry.get("replacements").and_then(Value::as_array) {
                replacements.extend(items.iter().cloned());
            }
        }
        if !sidechain
            && matches!(
                kind,
                "user" | "assistant" | "attachment" | "system" | "progress"
            )
            && entry
                .get("uuid")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            && cutoff.is_none_or(|(cut_index, _)| index <= cut_index)
        {
            transcript.push(entry);
        }
    }
    if recorded_session_id
        .as_deref()
        .is_some_and(|id| id != session_id)
    {
        return Err(AppError::Other(
            "Claude 会话 ID 与所选文件内容不匹配，未创建副本".into(),
        ));
    }
    if cutoff.is_some() && cut.is_none() {
        return Err(AppError::Other(
            "所选 Claude 消息不存在，请刷新预览后重试".into(),
        ));
    }
    let writable_count = transcript
        .iter()
        .filter(|e| e["type"] != "progress")
        .count();
    if writable_count == 0 {
        return Err(AppError::Other("Claude 会话没有可复制的主会话消息".into()));
    }

    let mut uuid_mapping = HashMap::new();
    let mut by_uuid = HashMap::new();
    for entry in &transcript {
        let uuid = entry["uuid"].as_str().expect("transcript UUID checked");
        uuid_mapping.insert(uuid, new_uuid()?);
        by_uuid.insert(uuid, entry);
    }
    let new_id = new_uuid()?;
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut entries = Vec::with_capacity(writable_count + 2);
    for original in transcript.iter().filter(|e| e["type"] != "progress") {
        let uuid = original["uuid"].as_str().expect("transcript UUID checked");
        let mut parent_id = original.get("parentUuid").and_then(Value::as_str);
        let mut parent_uuid = None;
        let mut visited = HashSet::new();
        while let Some(id) = parent_id {
            if !visited.insert(id) {
                return Err(AppError::Other(
                    "Claude 消息的 progress 父链存在循环，未创建副本".into(),
                ));
            }
            let Some(parent) = by_uuid.get(id) else {
                break;
            };
            if parent["type"] != "progress" {
                parent_uuid = uuid_mapping.get(id);
                break;
            }
            parent_id = parent.get("parentUuid").and_then(Value::as_str);
        }
        let logical_parent = original
            .get("logicalParentUuid")
            .and_then(Value::as_str)
            .and_then(|id| uuid_mapping.get(id));
        let mut forked = original.clone();
        forked["uuid"] = json!(uuid_mapping[uuid]);
        forked["parentUuid"] = json!(parent_uuid);
        forked["logicalParentUuid"] = json!(logical_parent);
        forked["sessionId"] = json!(new_id);
        forked["isSidechain"] = json!(false);
        forked["forkedFrom"] = json!({ "sessionId": session_id, "messageUuid": uuid });
        if entries.len() + 1 == writable_count || original.get("timestamp").is_none() {
            forked["timestamp"] = json!(now);
        }
        for key in ["teamName", "agentName", "slug", "sourceToolAssistantUUID"] {
            forked
                .as_object_mut()
                .expect("transcript object")
                .remove(key);
        }
        // Never recursively remap payload IDs: tool_use/tool_result pairs,
        // signed thinking, image data and message.id must remain byte-equivalent values.
        entries.push(forked);
    }
    if !replacements.is_empty() {
        entries.push(json!({
            "type": "content-replacement", "sessionId": new_id,
            "replacements": replacements, "uuid": new_uuid()?, "timestamp": now,
        }));
    }
    let title = custom_title
        .or(ai_title)
        .or(first_prompt)
        .unwrap_or_else(|| "Forked session".into());
    entries.push(json!({
        "type": "custom-title", "sessionId": new_id,
        "customTitle": format!("{title} (fork)"), "uuid": new_uuid()?, "timestamp": now,
    }));
    Ok(ForkOutput {
        new_id,
        entries,
        cut,
    })
}

/// Same user-visible boundaries as the frontend. The SDK permits both user and
/// assistant records, including tool messages; hidden metadata is not a UI target.
fn can_fork_at(entry: &Value) -> bool {
    matches!(
        entry.get("type").and_then(Value::as_str),
        Some("user" | "assistant")
    ) && matches!(
        entry.pointer("/message/role").and_then(Value::as_str),
        Some("user" | "assistant")
    ) && entry
        .get("uuid")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
        && entry.get("isSidechain").and_then(Value::as_bool) != Some(true)
        && entry.get("isMeta").and_then(Value::as_bool) != Some(true)
}

fn new_uuid() -> AppResult<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| AppError::Other(format!("生成 Claude 副本 UUID 失败: {e}")))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const SOURCE_ID: &str = "10000000-0000-4000-8000-000000000001";
    const FIRST: &str = "11111111-1111-4111-8111-111111111111";
    const ASSISTANT: &str = "33333333-3333-4333-8333-333333333333";
    const CUTOFF: &str = "99999999-9999-4999-8999-999999999999";
    const FIXTURE: &str = include_str!("../tests/fixtures/claude-fork.jsonl");

    fn source_entries() -> Vec<Value> {
        FIXTURE
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn forked_from<'a>(entries: &'a [Value], uuid: &str) -> &'a Value {
        entries
            .iter()
            .find(|e| e["forkedFrom"]["messageUuid"] == uuid)
            .unwrap()
    }

    fn is_uuid_v4(id: &str) -> bool {
        id.len() == 36
            && id.as_bytes()[14] == b'4'
            && matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b')
            && id.split('-').map(str::len).eq([8, 4, 4, 4, 12])
            && id.chars().all(|c| c == '-' || c.is_ascii_hexdigit())
    }

    #[test]
    fn full_copy_remaps_native_links_and_preserves_opaque_payloads() -> AppResult<()> {
        let result = build_fork(FIXTURE, SOURCE_ID, None)?;
        assert!(is_uuid_v4(&result.new_id));
        assert_ne!(result.new_id, SOURCE_ID);
        assert_eq!(result.entries.len(), 10);
        let originals = source_entries();
        let original_ids: HashSet<_> = originals
            .iter()
            .filter_map(|e| e["uuid"].as_str())
            .collect();
        let mut new_ids = HashSet::new();
        for entry in &result.entries {
            assert_eq!(entry["sessionId"], result.new_id);
            let uuid = entry["uuid"].as_str().unwrap();
            assert!(is_uuid_v4(uuid) && !original_ids.contains(uuid));
            assert!(new_ids.insert(uuid));
            if let Some(from) = entry["forkedFrom"]["messageUuid"].as_str() {
                let source = originals.iter().find(|e| e["uuid"] == from).unwrap();
                assert_eq!(entry["message"], source["message"]);
                assert_eq!(entry["attachment"], source["attachment"]);
                assert_eq!(entry["compactMetadata"], source["compactMetadata"]);
                assert_eq!(entry["forkedFrom"]["sessionId"], SOURCE_ID);
                assert_eq!(entry["isSidechain"], false);
                for key in ["teamName", "agentName", "slug", "sourceToolAssistantUUID"] {
                    assert!(entry.get(key).is_none());
                }
            }
        }
        // progress links are removed, but their ancestors remain connected.
        let first = forked_from(&result.entries, FIRST);
        let assistant = forked_from(&result.entries, ASSISTANT);
        let tool_result = forked_from(&result.entries, "55555555-5555-4555-8555-555555555555");
        assert!(first["parentUuid"].is_null());
        assert_eq!(assistant["parentUuid"], first["uuid"]);
        assert_eq!(tool_result["parentUuid"], assistant["uuid"]);
        assert_eq!(assistant["timestamp"], "2026-01-01T00:00:01.000Z");
        let compact = forked_from(&result.entries, "88888888-8888-4888-8888-888888888888");
        let attachment = forked_from(&result.entries, "77777777-7777-4777-8777-777777777777");
        assert_eq!(compact["logicalParentUuid"], attachment["uuid"]);
        assert!(
            forked_from(&result.entries, "66666666-6666-4666-8666-666666666666")["isMeta"]
                .as_bool()
                .unwrap()
        );
        for entry in &result.entries {
            for key in ["parentUuid", "logicalParentUuid"] {
                if let Some(parent) = entry[key].as_str() {
                    assert!(new_ids.contains(parent));
                }
            }
        }
        assert_eq!(
            result.entries[8]["replacements"],
            json!([{"synthetic":"opaque replacement payload"}])
        );
        assert_eq!(result.entries[9]["customTitle"], "Copy fixture (fork)");
        assert_ne!(
            forked_from(&result.entries, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")["timestamp"],
            "2026-01-01T00:00:04.000Z"
        );
        Ok(())
    }

    #[test]
    fn cutoff_is_inclusive_and_uses_physical_line_and_uuid() -> AppResult<()> {
        let padded = format!("\n{FIXTURE}");
        let result = build_fork(&padded, SOURCE_ID, Some((10, CUTOFF)))?;
        assert_eq!(result.cut.unwrap().index, 10);
        assert_eq!(result.entries.len(), 9);
        assert_eq!(result.entries[6]["forkedFrom"]["messageUuid"], CUTOFF);
        assert_ne!(result.entries[6]["timestamp"], "2026-01-01T00:00:03.000Z");
        assert!(!result
            .entries
            .iter()
            .any(|e| e["forkedFrom"]["messageUuid"] == "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"));
        assert!(build_fork(&padded, SOURCE_ID, Some((9, CUTOFF))).is_err());
        assert!(build_fork(&padded, SOURCE_ID, Some((100, CUTOFF))).is_err());
        let first_only = build_fork(FIXTURE, SOURCE_ID, Some((1, FIRST)))?;
        assert_eq!(first_only.entries[0]["forkedFrom"]["messageUuid"], FIRST);
        assert_eq!(first_only.entries.len(), 3); // first message, replacements, title
        Ok(())
    }

    #[test]
    fn tool_messages_are_valid_cutoffs_but_metadata_and_sidechains_are_not() -> AppResult<()> {
        for index in [3, 5, 9, 10] {
            let entries = source_entries();
            let uuid = entries[index]["uuid"].as_str().unwrap();
            build_fork(FIXTURE, SOURCE_ID, Some((index, uuid)))?;
        }
        for index in [0, 2, 6, 7, 8, 11, 12] {
            let entries = source_entries();
            let uuid = entries[index]["uuid"].as_str().unwrap_or("missing");
            assert!(build_fork(FIXTURE, SOURCE_ID, Some((index, uuid))).is_err());
        }
        Ok(())
    }

    #[test]
    fn dangling_parents_are_cleared_and_progress_cycles_are_rejected() -> AppResult<()> {
        let mut entries = source_entries();
        entries[1]["parentUuid"] = json!("missing");
        entries[1]["logicalParentUuid"] = json!("missing");
        let encode = |values: &[Value]| {
            values
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let result = build_fork(&encode(&entries), SOURCE_ID, None)?;
        assert!(result.entries[0]["parentUuid"].is_null());
        assert!(result.entries[0]["logicalParentUuid"].is_null());
        entries[2]["parentUuid"] = entries[2]["uuid"].clone();
        assert!(build_fork(&encode(&entries), SOURCE_ID, None)
            .unwrap_err()
            .to_string()
            .contains("循环"));
        Ok(())
    }

    #[test]
    fn rejects_empty_incomplete_or_mismatched_sources() {
        for content in [
            "",
            "\n",
            "{\"type\":",
            "{\"type\":\"file-history-snapshot\"}",
        ] {
            assert!(build_fork(content, SOURCE_ID, None).is_err());
        }
        assert!(build_fork(&format!("{FIXTURE}{{\"type\":"), SOURCE_ID, None).is_err());
        assert!(build_fork(FIXTURE, "wrong-id", None).is_err());
    }

    struct FixtureDir(PathBuf);
    impl FixtureDir {
        fn new() -> AppResult<Self> {
            let path =
                std::env::temp_dir().join(format!("cc-sessions-claude-fork-{}", new_uuid()?));
            fs::create_dir_all(path.join("projects/project"))?;
            Ok(Self(path))
        }
        fn source(&self) -> PathBuf {
            self.0
                .join("projects/project")
                .join(format!("{SOURCE_ID}.jsonl"))
        }
        fn copy(&self) -> AppResult<(ForkOutput, PathBuf)> {
            copy_session(
                &self.0.to_string_lossy(),
                SOURCE_ID,
                &self.source().to_string_lossy(),
                None,
            )
        }
    }
    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn publishes_independent_discoverable_copies_without_modifying_source_or_sidecars(
    ) -> AppResult<()> {
        let dir = FixtureDir::new()?;
        fs::write(dir.source(), FIXTURE)?;
        let sidecar = dir.source().with_extension("");
        fs::create_dir_all(sidecar.join("subagents"))?;
        fs::write(
            sidecar.join("subagents/agent-test.jsonl"),
            "unchanged sidecar",
        )?;
        let tasks = dir.0.join("tasks").join(SOURCE_ID);
        fs::create_dir_all(&tasks)?;
        fs::write(tasks.join("task.json"), "unchanged task")?;
        let report = duplicate_session_with_lock(
            dir.0.to_string_lossy().into(),
            SOURCE_ID.into(),
            dir.source().to_string_lossy().into(),
            &FamilyLock::default(),
        )?;
        let second = dir.copy()?;
        assert_ne!(report.new_id, second.0.new_id);
        assert_eq!(fs::read_to_string(dir.source())?, FIXTURE);
        assert_eq!(
            Path::new(&report.new_rollout_path).parent(),
            dir.source().parent()
        );
        assert!(!Path::new(&report.new_rollout_path)
            .with_extension("")
            .exists());
        assert!(!dir.0.join("tasks").join(&report.new_id).exists());
        assert_eq!(
            fs::read_to_string(tasks.join("task.json"))?,
            "unchanged task"
        );
        let summary = claude_sessions::resolve_session_summary(
            &dir.0,
            &report.new_id,
            Some(&report.new_rollout_path),
        )?;
        assert_eq!(summary.title, "Copy fixture (fork)");
        assert_eq!(summary.cwd, "/example/project");
        assert!(summary.resume_command.contains(&report.new_id));
        assert!(!summary.archived);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&report.new_rollout_path)?.permissions().mode() & 0o777,
                0o600
            );
        }
        Ok(())
    }

    #[test]
    fn failures_leave_no_copy_and_exact_project_path_selects_the_source() -> AppResult<()> {
        let dir = FixtureDir::new()?;
        fs::write(dir.source(), format!("{FIXTURE}{{\"type\":"))?;
        assert!(dir.copy().is_err());
        assert_eq!(fs::read_dir(dir.source().parent().unwrap())?.count(), 1);
        fs::write(dir.source(), FIXTURE)?;
        assert!(copy_session(
            &dir.0.to_string_lossy(),
            SOURCE_ID,
            &dir.source().to_string_lossy(),
            Some((9, FIRST))
        )
        .is_err());
        assert_eq!(fs::read_dir(dir.source().parent().unwrap())?.count(), 1);
        let other = dir.0.join("projects/other");
        fs::create_dir_all(&other)?;
        fs::write(
            other.join(format!("{SOURCE_ID}.jsonl")),
            FIXTURE.replace("Copy fixture", "Other project"),
        )?;
        let result = dir.copy()?;
        assert_eq!(
            result.0.entries.last().unwrap()["customTitle"],
            "Copy fixture (fork)"
        );
        assert_eq!(fs::read_dir(&other)?.count(), 1);
        Ok(())
    }

    #[test]
    fn rejects_paths_outside_projects_and_subagent_files() -> AppResult<()> {
        let dir = FixtureDir::new()?;
        let outside = dir.0.join(format!("{SOURCE_ID}.jsonl"));
        fs::write(&outside, FIXTURE)?;
        assert!(copy_session(
            &dir.0.to_string_lossy(),
            SOURCE_ID,
            &outside.to_string_lossy(),
            None
        )
        .is_err());
        let nested = dir.source().with_extension("").join("subagents");
        fs::create_dir_all(&nested)?;
        let agent = nested.join(format!("{SOURCE_ID}.jsonl"));
        fs::write(&agent, FIXTURE)?;
        assert!(copy_session(
            &dir.0.to_string_lossy(),
            SOURCE_ID,
            &agent.to_string_lossy(),
            None
        )
        .is_err());
        Ok(())
    }
}
