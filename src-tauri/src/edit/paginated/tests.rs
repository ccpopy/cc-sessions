use super::super::*;
use serde_json::json;

mod projection_read_tests;

#[test]
#[ignore = "read-only audit of explicitly supplied rollout paths; structural output only"]
fn paginated_mapping_readonly_audit() {
    let inventory: Value = serde_json::from_slice(
        &fs::read(std::env::var("CC_MAPPING_AUDIT_INVENTORY").unwrap()).unwrap(),
    )
    .unwrap();
    let mut results = Vec::new();
    for entry in inventory.as_array().unwrap() {
        let loaded = load_file(Path::new(entry["path"].as_str().unwrap())).unwrap();
        match super::diagnostics(&loaded) {
            Ok(mappings) => for mapping in mappings { results.push(json!({"item_type":mapping.item_type,"mapping":mapping})); },
            Err(_) => results.push(json!({"thread_id":entry["thread_id"],"cli_version":entry["version"],"reason_code":"HISTORY_STRUCTURE_UNSUPPORTED"})),
        }
    }
    fs::write(
        std::env::var("CC_MAPPING_AUDIT_REPORT").unwrap(),
        serde_json::to_vec(&results).unwrap(),
    )
    .unwrap();
}

fn native_media_cases() -> Vec<Value> {
    serde_json::from_str::<Value>(include_str!("fixtures/native-media-alpha16.json")).unwrap()
        ["cases"]
        .as_array()
        .unwrap()
        .clone()
}

fn native_media_loaded(case: &Value) -> LoadedFile {
    let p = &case["canonical"]["payload"];
    let rows = [
        json!({"type":"session_meta","payload":{"id":p["thread_id"],"history_mode":"paginated","cli_version":case["cli_version"].as_str().unwrap_or("0.155.0-alpha.16")}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":p["turn_id"]}}),
        case["context"].clone(),
        case["canonical"].clone(),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":p["turn_id"]}}),
    ];
    let lines = rows
        .into_iter()
        .enumerate()
        .map(|(i, mut row)| {
            row["ordinal"] = json!(i);
            row.to_string()
        })
        .collect::<Vec<_>>();
    paginated::from_lines(&lines, true)
}

macro_rules! native_media_diagnostic_test {
    ($test:ident, $case:literal) => {
        #[test]
        fn $test() {
            let case = native_media_cases()
                .into_iter()
                .find(|v| v["name"] == $case)
                .unwrap();
            let diagnostics = super::diagnostics(&native_media_loaded(&case)).unwrap();
            assert!(
                diagnostics.iter().all(|d| d.status == "matched"),
                "untouched native {}: {diagnostics:?}",
                $case
            );
        }
    };
}
native_media_diagnostic_test!(native_media_unedited_pure_image, "pure-image");
native_media_diagnostic_test!(native_media_unedited_text_image, "text-image");
native_media_diagnostic_test!(native_media_unedited_multi_image, "multi-image");
native_media_diagnostic_test!(native_media_unedited_local_audio, "local-audio");
native_media_diagnostic_test!(native_media_unedited_mixed_blocks, "mixed-blocks");
native_media_diagnostic_test!(native_media_unedited_inline_image, "inline-image");
native_media_diagnostic_test!(native_media_unedited_inline_audio, "inline-audio");
native_media_diagnostic_test!(native_media_unedited_literal_tags, "literal-tags");

#[test]
fn compatibility_native_alpha9_media_is_editable() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/native-media-alpha9.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let mappings = super::diagnostics(&native_media_loaded(case)).unwrap();
        assert!(
            mappings.iter().all(|m| m.status == "matched"),
            "{}: {mappings:?}",
            case["name"]
        );
    }
}

fn native_async_cases() -> Vec<Value> {
    serde_json::from_str::<Value>(include_str!("fixtures/native-async-alpha9.json")).unwrap()
        ["cases"]
        .as_array()
        .unwrap()
        .clone()
}

fn native_async_loaded(case: &Value) -> LoadedFile {
    let captured = case["rows"].as_array().unwrap();
    let canonical = captured
        .iter()
        .find(|r| r["payload"]["type"] == "item_completed")
        .unwrap();
    let p = &canonical["payload"];
    let mut rows = vec![
        json!({"type":"session_meta","payload":{"id":p["thread_id"],"history_mode":"paginated","cli_version":"0.155.0-alpha.9.2"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":p["turn_id"]}}),
    ];
    rows.extend(captured.iter().cloned());
    rows.push(
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":p["turn_id"]}}),
    );
    let lines = rows
        .into_iter()
        .enumerate()
        .map(|(i, mut r)| {
            r["ordinal"] = json!(i);
            r.to_string()
        })
        .collect::<Vec<_>>();
    super::from_lines(&lines, true)
}

#[test]
fn compatibility_native_async_source_is_mapped() {
    for case in native_async_cases() {
        let mappings = super::diagnostics(&native_async_loaded(&case)).unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(
            mappings[0].status, "matched",
            "{}: {mappings:?}",
            case["name"]
        );
    }
}

#[test]
fn compatibility_async_edit_delete_and_undo_preserve_call_chain() {
    for case in ["freeform", "options", "multi-question"] {
        let f = Fixture::native_async(case);
        let before = fs::read(&f.path).unwrap();
        let before_items = f.items();
        let loaded = load_file(&f.path).unwrap();
        let canonical = loaded.parsed[11].as_ref().unwrap();
        let id = canonical["payload"]["item"]["id"].as_str().unwrap();
        let old = canonical["payload"]["item"]["content"][0]["text"]
            .as_str()
            .unwrap();
        let updated = old
            .replace("QUESTION", "EDITED QUESTION")
            .replace("CHOICE", "EDITED CHOICE")
            .replace("SECOND", "UPDATED SECOND");
        f.rewrite(11, &updated);
        let after = load_file(&f.path).unwrap();
        let mappings = super::diagnostics(&after).unwrap();
        assert!(mappings.iter().all(|m| m.status == "matched"));
        assert_eq!(
            after.parsed[12], loaded.parsed[12],
            "receipt is not rewritten or re-executed"
        );
        assert_eq!(
            after.parsed[13..],
            loaded.parsed[13..],
            "following formal answer and turn completion stay intact"
        );
        assert!(f.items().contains("EDITED"));
        let args: Value = serde_json::from_str(
            after.parsed[10].as_ref().unwrap()["payload"]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert!(args["questions"][0]["title"]
            .as_str()
            .unwrap()
            .contains("EDITED"));
        undo_last(
            "codex",
            f.path.to_str().unwrap(),
            "thread-1",
            f.backup.to_str().unwrap(),
            Some(&f.revision()),
        )
        .unwrap();
        assert_eq!(fs::read(&f.path).unwrap(), before);
        assert_eq!(f.items(), before_items);
        assert!(super::required_turns(&loaded, &[11]).unwrap().is_empty());
        f.delete(&[11]).unwrap();
        let deleted = load_file(&f.path).unwrap();
        assert!(!deleted.lines.iter().any(|line| line.contains(id)));
        assert!(f.items().contains("agent-1"));
        undo_last(
            "codex",
            f.path.to_str().unwrap(),
            "thread-1",
            f.backup.to_str().unwrap(),
            Some(&f.revision()),
        )
        .unwrap();
        assert_eq!(fs::read(&f.path).unwrap(), before);
        assert_eq!(f.items(), before_items);
    }
}

#[test]
fn compatibility_async_real_difference_rejects_all_affected_writes() {
    let f = Fixture::native_async("options");
    f.update(|rows, _| {
        rows[11]["payload"]["item"]["questions"][0]["title"] = json!("different question metadata")
    });
    let before = fs::read(&f.path).unwrap();
    let capability = inspect_edit_capability("codex", f.path.to_str().unwrap()).unwrap();
    let detail = capability
        .content_mappings
        .iter()
        .find(|m| m.source == "tool.request_user_input_async")
        .unwrap();
    assert_eq!(detail.status, "inconsistent");
    assert!(!detail.operations.edit_text.supported);
    assert!(!detail.operations.delete_message.supported);
    assert!(!detail.operations.delete_turn.supported);
    assert_eq!(capability.diagnostics.len(), 1);
    assert!(f
        .delete(&[11])
        .unwrap_err()
        .to_string()
        .contains("EDIT_INCONSISTENT"));
    assert!(f
        .delete(&[9, 11, 13])
        .unwrap_err()
        .to_string()
        .contains("EDIT_INCONSISTENT"));
    assert_eq!(fs::read(&f.path).unwrap(), before);
    f.rewrite(3, "unaffected");
}

#[test]
fn compatibility_async_duplicate_snapshots_follow_the_same_identity() {
    let f = Fixture::native_async("options");
    f.update(|rows, _| {
        let repeated = vec![rows[10].clone(), rows[11].clone()];
        rows.splice(12..12, repeated);
    });
    let before = fs::read(&f.path).unwrap();
    let items = f.items();
    let original = load_file(&f.path).unwrap();
    let id = original.parsed[11].as_ref().unwrap()["payload"]["item"]["id"]
        .as_str()
        .unwrap();
    f.rewrite(11, "UPDATED CHOICE\n- FIRST\n- SECOND");
    let after = load_file(&f.path).unwrap();
    for index in [11, 13] {
        assert_eq!(
            after.parsed[index].as_ref().unwrap()["payload"]["item"]["content"][0]["text"],
            "UPDATED CHOICE\n- FIRST\n- SECOND"
        );
    }
    assert!(super::diagnostics(&after)
        .unwrap()
        .iter()
        .all(|m| m.status == "matched"));
    undo_last(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read(&f.path).unwrap(), before);
    f.delete(&[13]).unwrap();
    assert!(!fs::read_to_string(&f.path).unwrap().contains(id));
    undo_last(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read(&f.path).unwrap(), before);
    assert_eq!(f.items(), items);
}

#[test]
fn compatibility_async_question_is_not_the_model_completion_text() {
    let f = Fixture::native_async("options");
    let before = fs::read(&f.path).unwrap();
    f.delete(&[13]).unwrap();
    let after = load_file(&f.path).unwrap();
    let complete = after
        .parsed
        .iter()
        .flatten()
        .find(|r| r["payload"]["type"] == "task_complete" && r["payload"]["turn_id"] == "turn-1")
        .unwrap();
    assert!(complete["payload"]["last_agent_message"].is_null());
    // Native history selects final_answer items (including async), whereas the
    // completion text comes only from the model's ordinary response messages.
    let image = super::projection::read(&f.path, &after).unwrap();
    assert_eq!(
        image.rows["thread_turns"]
            .iter()
            .find(|r| r["turn_id"] == "turn-1")
            .unwrap()["final_agent_item_id"],
        after.parsed[11].as_ref().unwrap()["payload"]["item"]["id"]
    );
    undo_last(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn compatibility_missing_context_allows_explicit_whole_turn_only() {
    let f = Fixture::new();
    f.update(|rows, _| {
        rows.remove(8);
    });
    let before = fs::read(&f.path).unwrap();
    let loaded = load_file(&f.path).unwrap();
    let required = super::required_turns(&loaded, &[8]).unwrap();
    assert_eq!(required.len(), 1);
    assert_eq!(required[0].messages.len(), 2);
    assert!(f.delete(&[8]).is_err());
    f.delete(&[8, 9]).unwrap();
    assert!(!f.items().contains("user-1"));
    undo_last(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn compatibility_tool_identity_and_compaction_scope_are_independent() {
    for mutation in ["tool", "namespace", "turn"] {
        let case = native_async_cases().remove(0);
        let loaded = native_async_loaded(&case);
        let mut rows = loaded
            .parsed
            .iter()
            .map(|r| r.clone().unwrap())
            .collect::<Vec<_>>();
        let call = rows
            .iter_mut()
            .find(|r| r["payload"]["type"] == "function_call")
            .unwrap();
        match mutation {
            "tool" => call["payload"]["name"] = json!("unrelated_tool"),
            "namespace" => call["payload"]["namespace"] = json!("unrelated_namespace"),
            _ => {
                call["payload"]["internal_chat_message_metadata_passthrough"]["turn_id"] =
                    json!("different-turn")
            }
        }
        let lines = rows.iter().map(Value::to_string).collect::<Vec<_>>();
        assert_eq!(
            super::diagnostics(&super::from_lines(&lines, true)).unwrap()[0].status,
            "unsupported"
        );
    }
    let f = Fixture::native_async("options");
    f.update(|rows,_| {
        rows.insert(10,json!({"type":"compacted","payload":{"message":"synthetic summary","replacement_history":[]}}));
    });
    let capability = inspect_edit_capability("codex", f.path.to_str().unwrap()).unwrap();
    let tool = capability
        .content_mappings
        .iter()
        .find(|m| m.source == "tool.request_user_input_async")
        .unwrap();
    assert_eq!(tool.status, "matched");
    assert!(tool.operations.edit_text.supported);
    assert!(tool.operations.delete_message.supported);
    assert!(!tool.operations.delete_turn.supported);
    let before = fs::read(&f.path).unwrap();
    f.rewrite(12, "UPDATED CHOICE\n- FIRST\n- SECOND");
    undo_last(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

struct Fixture {
    root: PathBuf,
    path: PathBuf,
    backup: PathBuf,
}
impl Fixture {
    fn native_async(name: &str) -> Self {
        let case = native_async_cases()
            .into_iter()
            .find(|c| c["name"] == name)
            .unwrap();
        let f = Self::new();
        f.update(|rows, seed| {
            rows[0]["payload"]["cli_version"] = json!("0.155.0-alpha.9.2");
            let mut captured = case["rows"].as_array().unwrap().clone();
            for row in &mut captured {
                if row["payload"]["internal_chat_message_metadata_passthrough"].get("turn_id").is_some() {
                    row["payload"]["internal_chat_message_metadata_passthrough"]["turn_id"] = json!("turn-1");
                }
            }
            let canonical = captured.iter_mut().find(|r| r["payload"]["type"] == "item_completed").unwrap();
            canonical["payload"]["thread_id"] = json!("thread-1");
            canonical["payload"]["turn_id"] = json!("turn-1");
            let item = canonical["payload"]["item"].clone();
            let mut projected = seed.rows["thread_items"].iter().find(|r| r["item_id"] == "agent-1").unwrap().clone();
            projected["item_id"] = item["id"].clone();
            projected["item_json"] = json!(json!({"type":"agentMessage","id":item["id"],"text":item["content"][0]["text"],"phase":item["phase"],"memoryCitation":null,"delivery":item["delivery"],"questions":item["questions"]}).to_string());
            seed.rows.get_mut("thread_items").unwrap().push(projected);
            rows.splice(10..10,captured);
        });
        f
    }

    fn native_media(name: &str) -> Self {
        let case = native_media_cases()
            .into_iter()
            .find(|v| v["name"] == name)
            .unwrap();
        let f = Self::new();
        f.update(|rows, seed| {
            rows[9]["payload"]["item"]["content"] =
                case["canonical"]["payload"]["item"]["content"].clone();
            rows[8]["payload"]["content"] = case["context"]["payload"]["content"].clone();
            rows[8]["payload"]["internal_chat_message_metadata_passthrough"]
                ["content_item_kinds"] = case["context"]["payload"]
                ["internal_chat_message_metadata_passthrough"]["content_item_kinds"]
                .clone();
            let row = seed
                .rows
                .get_mut("thread_items")
                .unwrap()
                .iter_mut()
                .find(|r| r["item_id"] == "user-1")
                .unwrap();
            let mut native: Value =
                serde_json::from_str(row["item_json"].as_str().unwrap()).unwrap();
            native["content"] = rows[9]["payload"]["item"]["content"].clone();
            for block in native["content"].as_array_mut().unwrap() {
                match block["type"].as_str() {
                    Some("local_image") => block["type"] = json!("localImage"),
                    Some("local_audio") => block["type"] = json!("localAudio"),
                    _ => {}
                }
            }
            row["item_json"] = json!(native.to_string());
        });
        f
    }

    fn update(&self, change: impl FnOnce(&mut Vec<Value>, &mut paginated::HistoryImage)) {
        let loaded = load_file(&self.path).unwrap();
        let mut seed = paginated::projection::read(&self.path, &loaded).unwrap();
        let mut rows: Vec<Value> = loaded.parsed.into_iter().map(Option::unwrap).collect();
        change(&mut rows, &mut seed);
        for (i, row) in rows.iter_mut().enumerate() {
            row["ordinal"] = json!(i);
        }
        let lines: Vec<String> = rows.iter().map(Value::to_string).collect();
        fs::write(&self.path, lines.join("\n") + "\n").unwrap();
        let image = paginated::projection::project(&load_file(&self.path).unwrap(), &seed).unwrap();
        let db =
            paginated::projection::open(&self.root.join("thread_history_1.sqlite"), true).unwrap();
        paginated::projection::replace(&db, "thread-1", &image).unwrap();
    }
    fn rewrite(&self, index: usize, text: &str) -> EditApplyReport {
        apply_edit_text(
            "codex",
            self.path.to_str().unwrap(),
            "thread-1",
            self.backup.to_str().unwrap(),
            index,
            text,
            Some(&self.revision()),
        )
        .unwrap()
    }
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "cc-paginated-edit-{}",
            crate::repair::new_session_id()
        ));
        let path = root.join("sessions/rollout-test.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let backup = root.join("backup");
        let mut rows = vec![
            json!({"type":"session_meta","payload":{"id":"thread-1","history_mode":"paginated","cli_version":"0.155.0-alpha.16"}}),
        ];
        for (n, text) in ["KEEP-A", "DELETE-B", "KEEP-C"].iter().enumerate() {
            let turn = format!("turn-{n}");
            rows.extend([
                json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn,"started_at":1790059469}}),
                json!({"type":"response_item","payload":{"type":"message","id":format!("context-{n}"),"role":"user","content":[{"type":"input_text","text":text}],"internal_chat_message_metadata_passthrough":{"turn_id":turn,"content_item_kinds":["user.text"]}}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"thread-1","turn_id":turn,"started_at_ms":1790059469000i64,"completed_at_ms":1790059469000i64,"item":{"type":"UserMessage","id":format!("user-{n}"),"client_id":null,"content":[{"type":"text","text":text}]}}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"thread-1","turn_id":turn,"started_at_ms":1790059470000i64,"completed_at_ms":1790059470000i64,"item":{"type":"AgentMessage","id":format!("agent-{n}"),"content":[{"type":"Text","text":format!("reply-{n}")}],"phase":"final_answer"}}}),
                json!({"type":"response_item","payload":{"type":"message","id":format!("agent-{n}"),"role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":format!("reply-{n}")}]}}),
                json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn,"last_agent_message":format!("reply-{n}"),"started_at":1790059469,"completed_at":1790059470,"duration_ms":1000}}),
            ]);
        }
        let db = rusqlite::Connection::open(root.join("thread_history_1.sqlite")).unwrap();
        db.execute_batch(include_str!("schema_v6.sql")).unwrap();
        let mut file = fs::File::create(&path).unwrap();
        let mut offset = 0;
        for (i, mut row) in rows.into_iter().enumerate() {
            row["ordinal"] = json!(i);
            row["timestamp"] = json!("2026-09-22T08:04:29Z");
            let line = serde_json::to_string(&row).unwrap() + "\n";
            let p = &row["payload"];
            let turn = p["turn_id"].as_str().unwrap_or("");
            match p["type"].as_str().unwrap_or("") {
                "task_started" => {
                    db.execute("INSERT INTO thread_turns(thread_id,turn_id,rollout_ordinal,status,started_at,rollout_byte_offset) VALUES('thread-1',?1,?2,'inProgress',1790059469,?3)",rusqlite::params![turn,i,offset]).unwrap();
                }
                "task_complete" => {
                    db.execute("UPDATE thread_turns SET status='completed',completed_at=1790059470,duration_ms=1000,rollout_end_ordinal=?2,rollout_end_byte_offset=?3 WHERE turn_id=?1",rusqlite::params![turn,i,offset+line.len()]).unwrap();
                }
                "item_completed" => {
                    let item = &p["item"];
                    let id = item["id"].as_str().unwrap();
                    let projected = if item["type"] == "UserMessage" {
                        json!({"type":"userMessage","id":id,"clientId":null,"content":item["content"]})
                    } else {
                        json!({"type":"agentMessage","id":id,"text":item["content"][0]["text"],"phase":"final_answer","memoryCitation":null,"delivery":null,"questions":null})
                    };
                    db.execute("INSERT INTO thread_items(thread_id,turn_id,item_id,rollout_ordinal,created_at_ms,item_json,item_type,updated_at_ordinal) VALUES('thread-1',?1,?2,?3,1790059469000,?4,?5,?3)",rusqlite::params![turn,id,i,projected.to_string(),projected["type"].as_str().unwrap()]).unwrap();
                    let column = if item["type"] == "UserMessage" {
                        "first_user_item_id"
                    } else {
                        "final_agent_item_id"
                    };
                    db.execute(
                        &format!("UPDATE thread_turns SET {column}=?2 WHERE turn_id=?1"),
                        rusqlite::params![turn, id],
                    )
                    .unwrap();
                }
                _ => {}
            }
            file.write_all(line.as_bytes()).unwrap();
            offset += line.len();
        }
        db.execute(
            "INSERT INTO thread_history_projection_state VALUES('thread-1',?1,19)",
            [offset],
        )
        .unwrap();
        Self { root, path, backup }
    }
    fn revision(&self) -> String {
        inspect_edit_capability("codex", self.path.to_str().unwrap())
            .unwrap()
            .revision
    }
    fn delete(&self, indices: &[usize]) -> AppResult<EditApplyReport> {
        apply_delete(
            "codex",
            self.path.to_str().unwrap(),
            "thread-1",
            self.backup.to_str().unwrap(),
            indices,
            Some(&self.revision()),
        )
    }
    fn items(&self) -> String {
        let db = rusqlite::Connection::open(self.root.join("thread_history_1.sqlite")).unwrap();
        let mut q=db.prepare("SELECT item_json FROM thread_items WHERE thread_id='thread-1' ORDER BY rollout_ordinal").unwrap();
        q.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n")
    }
}

#[test]
fn native_media_edit_delete_and_undo_preserve_all_other_blocks() {
    for case in native_media_cases() {
        let f = Fixture::native_media(case["name"].as_str().unwrap());
        let original = fs::read(&f.path).unwrap();
        let original_items = f.items();
        let loaded = load_file(&f.path).unwrap();
        let mapping = super::diagnostics(&loaded)
            .unwrap()
            .into_iter()
            .find(|d| d.item_id == "user-1")
            .unwrap();
        assert_eq!(mapping.status, "matched");
        let undo = || {
            undo_last(
                "codex",
                f.path.to_str().unwrap(),
                "thread-1",
                f.backup.to_str().unwrap(),
                Some(&f.revision()),
            )
            .unwrap()
        };
        for pair in &mapping.block_pairs {
            let canonical = &loaded.parsed[9].as_ref().unwrap()["payload"]["item"]["content"];
            if canonical[pair.canonical_index]["type"] != "text" {
                continue;
            }
            apply_edit_text_blocks(
                "codex",
                f.path.to_str().unwrap(),
                "thread-1",
                f.backup.to_str().unwrap(),
                9,
                "",
                Some(&f.revision()),
                Some(&[crate::models::TextBlockEdit {
                    content_index: pair.canonical_index,
                    text: "EDITED-BLOCK".into(),
                }]),
            )
            .unwrap();
            let after = load_file(&f.path).unwrap();
            for (row, pointer, edited_index) in [
                (9, "/payload/item/content", pair.canonical_index),
                (8, "/payload/content", pair.context_index),
            ] {
                let before = loaded.parsed[row]
                    .as_ref()
                    .unwrap()
                    .pointer(pointer)
                    .unwrap()
                    .as_array()
                    .unwrap();
                let content = after.parsed[row]
                    .as_ref()
                    .unwrap()
                    .pointer(pointer)
                    .unwrap()
                    .as_array()
                    .unwrap();
                assert_eq!(before.len(), content.len());
                for i in 0..before.len() {
                    if i == edited_index {
                        assert_eq!(content[i]["text"], "EDITED-BLOCK");
                    } else {
                        assert_eq!(
                            before[i], content[i],
                            "{}: untouched block {i}",
                            case["name"]
                        );
                    }
                }
            }
            assert!(inspect_edit_capability("codex", f.path.to_str().unwrap())
                .unwrap()
                .diagnostics
                .is_empty());
            undo();
            assert_eq!(fs::read(&f.path).unwrap(), original);
            assert_eq!(f.items(), original_items);
        }
        f.delete(&[9]).unwrap(); // Pure image messages must also remain deletable.
        let after = load_file(&f.path).unwrap();
        assert!(!super::model(&after)
            .unwrap()
            .items
            .iter()
            .any(|i| i.key.id == "user-1"));
        assert!(!after
            .parsed
            .iter()
            .flatten()
            .any(|r| r["payload"]["id"] == "context-1"));
        assert!(f.items().contains("KEEP-A") && f.items().contains("KEEP-C"));
        undo();
        assert_eq!(fs::read(&f.path).unwrap(), original);
        assert_eq!(f.items(), original_items);
    }
}

#[test]
fn native_media_unsupported_mapping_is_scoped_and_not_reported_as_damage() {
    let f = Fixture::native_media("text-image");
    f.update(|rows, _| rows[8]["payload"]["content"][1]["text"] = json!("<future_image>"));
    let before = fs::read(&f.path).unwrap();
    let capability = inspect_edit_capability("codex", f.path.to_str().unwrap()).unwrap();
    assert!(capability.blocked_reasons.is_empty());
    assert!(capability.diagnostics.is_empty());
    let detail = capability
        .content_mappings
        .iter()
        .find(|m| m.item_id == "user-1")
        .unwrap();
    assert_eq!(detail.status, "unsupported");
    assert!(!detail.operations.edit_text.supported);
    assert!(detail.operations.delete_message.supported);
    assert!(apply_edit_text(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        9,
        "changed",
        Some(&f.revision())
    )
    .unwrap_err()
    .to_string()
    .contains("EDIT_MAPPING_UNSUPPORTED"));
    assert_eq!(fs::read(&f.path).unwrap(), before);
    assert!(history(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap()
    )
    .unwrap()
    .snapshots
    .is_empty());
    f.delete(&[9]).unwrap();
    assert!(!f.items().contains("user-1"));
    f.rewrite(3, "unaffected edit");
}

#[test]
fn native_media_real_body_difference_blocks_writes_and_reports_only_positions() {
    let f = Fixture::native_media("mixed-blocks");
    f.update(|rows, _| rows[8]["payload"]["content"][5]["text"] = json!("MIDDLe"));
    let before = fs::read(&f.path).unwrap();
    let capability = inspect_edit_capability("codex", f.path.to_str().unwrap()).unwrap();
    let detail = capability
        .content_mappings
        .iter()
        .find(|d| d.item_id == "user-1")
        .unwrap();
    assert_eq!(detail.status, "inconsistent");
    let diff = detail.first_difference.as_ref().unwrap();
    assert_eq!(
        (
            diff.canonical_index,
            diff.context_index,
            diff.utf8_byte_offset
        ),
        (Some(3), Some(5), Some(5))
    );
    let serialized = serde_json::to_string(detail).unwrap();
    for secret in [
        "MIDDLe",
        "MIDDLE",
        "data:image",
        "data:audio",
        "one.png",
        "tone.wav",
    ] {
        assert!(!serialized.contains(secret));
    }
    assert!(f
        .delete(&[9])
        .unwrap_err()
        .to_string()
        .contains("EDIT_INCONSISTENT"));
    assert!(apply_edit_text_blocks(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        9,
        "",
        Some(&f.revision()),
        Some(&[crate::models::TextBlockEdit {
            content_index: 0,
            text: "new".into()
        }])
    )
    .unwrap_err()
    .to_string()
    .contains("EDIT_INCONSISTENT"));
    assert_eq!(fs::read(&f.path).unwrap(), before);
    f.rewrite(3, "unaffected edit");
}

#[test]
fn native_media_literal_tags_and_whitespace_are_not_normalized() {
    let f = Fixture::native_media("inline-image");
    f.update(|rows, _| {
        rows[8]["payload"]["content"][0]["text"] = json!("<image>");
        rows[9]["payload"]["item"]["content"][0]["text"] = json!("<image>");
    });
    assert!(inspect_edit_capability("codex", f.path.to_str().unwrap())
        .unwrap()
        .diagnostics
        .is_empty());
    let f = Fixture::native_media("literal-tags");
    f.update(|rows, _| rows[8]["payload"]["content"][1]["text"] = json!("KEEP WHITESPACE"));
    assert!(inspect_edit_capability("codex", f.path.to_str().unwrap())
        .unwrap()
        .diagnostics[0]
        .contains("EDIT_INCONSISTENT"));
}

#[test]
fn native_media_unknown_version_source_and_shape_are_unsupported_not_inconsistent() {
    for scenario in 0..3 {
        let f = Fixture::native_media("text-image");
        f.update(|rows, _| match scenario {
            0 => rows[0]["payload"]["cli_version"] = json!("unverified-version"),
            1 => {
                rows[8]["payload"]["internal_chat_message_metadata_passthrough"]
                    ["content_item_kinds"][1] = json!("unknown.source")
            }
            _ => {
                rows[8]["payload"]["content"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"type":"input_text","text":"unmapped extra block"}));
                rows[8]["payload"]["internal_chat_message_metadata_passthrough"]
                    ["content_item_kinds"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("user.text"));
            }
        });
        let capability = inspect_edit_capability("codex", f.path.to_str().unwrap()).unwrap();
        assert!(capability.blocked_reasons.is_empty());
        assert!(capability.diagnostics.is_empty());
        let detail = capability
            .content_mappings
            .iter()
            .find(|m| m.item_id == "user-1")
            .unwrap();
        assert_eq!(detail.status, "unsupported");
        assert!(!detail.operations.edit_text.supported);
        assert!(detail.operations.delete_turn.supported);
        assert_eq!(detail.operations.delete_message.supported, scenario != 1);
        assert!(apply_edit_text(
            "codex",
            f.path.to_str().unwrap(),
            "thread-1",
            f.backup.to_str().unwrap(),
            9,
            "changed",
            Some(&f.revision())
        )
        .is_err());
        f.rewrite(3, "unaffected text");
    }
}

#[test]
fn native_media_duplicate_snapshots_share_the_same_context_mapping() {
    let f = Fixture::native_media("mixed-blocks");
    f.update(|rows, _| rows.insert(10, rows[9].clone()));
    let original = fs::read(&f.path).unwrap();
    let report = apply_edit_text_blocks(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        9,
        "",
        Some(&f.revision()),
        Some(&[crate::models::TextBlockEdit {
            content_index: 3,
            text: "changed middle".into(),
        }]),
    )
    .unwrap();
    assert_eq!(report.changed_lines, 3);
    let loaded = load_file(&f.path).unwrap();
    for i in [9, 10] {
        assert_eq!(
            loaded.parsed[i].as_ref().unwrap()["payload"]["item"]["content"][3]["text"],
            "changed middle"
        );
    }
    assert_eq!(
        loaded.parsed[8].as_ref().unwrap()["payload"]["content"][5]["text"],
        "changed middle"
    );
    undo_last(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read(&f.path).unwrap(), original);
}

#[test]
fn paginated_duplicate_text_and_snapshots_are_selected_by_identity() {
    let f = Fixture::new();
    f.update(|rows, _| {
        for row in rows.iter_mut() {
            if row["payload"]["role"] == "user" {
                row["payload"]["content"][0]["text"] = json!("继续");
            }
            if row["payload"]["item"]["type"] == "UserMessage" {
                row["payload"]["item"]["content"][0]["text"] = json!("继续");
            }
        }
        rows.insert(10, rows[9].clone());
    });
    let target = crate::models::PaginatedItemTarget {
        thread_id: "thread-1".into(),
        turn_id: "turn-1".into(),
        item_id: "user-1".into(),
    };
    let selected = resolve_selection(
        "codex",
        f.path.to_str().unwrap(),
        vec![0],
        Some(&[crate::models::SessionEventTarget::Codex(target)]),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(selected, vec![10]);
    f.delete(&selected).unwrap();
    let loaded = load_file(&f.path).unwrap();
    let h = super::model(&loaded).unwrap();
    assert!(!h.items.iter().any(|i| i.key.id == "user-1"));
    assert!(h.items.iter().any(|i| i.key.id == "user-0"));
    assert!(h.items.iter().any(|i| i.key.id == "user-2"));
    assert_eq!(f.items().matches("继续").count(), 2);
}

#[test]
fn paginated_flat_text_cannot_silently_collapse_multiple_blocks() {
    let f = Fixture::new();
    f.update(|rows, seed| {
        rows[9]["payload"]["item"]["content"] = json!([
            {"type":"text","text":"DELETE-B"},
            {"type":"local_image","path":"synthetic.png"},
            {"type":"text","text":"untouched tail"}
        ]);
        rows[8]["payload"]["content"] = json!([
            {"type":"input_text","text":"DELETE-B"},
            {"type":"input_image","image_url":"synthetic"},
            {"type":"input_text","text":"untouched tail"}
        ]);
        rows[8]["payload"]["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
            json!(["user.text", "user.image", "user.text"]);
        let row = seed
            .rows
            .get_mut("thread_items")
            .unwrap()
            .iter_mut()
            .find(|r| r["item_id"] == "user-1")
            .unwrap();
        let mut native: Value = serde_json::from_str(row["item_json"].as_str().unwrap()).unwrap();
        native["content"] = rows[9]["payload"]["item"]["content"].clone();
        native["content"][1]["type"] = json!("localImage");
        row["item_json"] = json!(native.to_string());
    });
    let before = fs::read(&f.path).unwrap();
    let result = apply_edit_text(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        9,
        "replacement",
        Some(&f.revision()),
    );
    assert!(
        result.is_err(),
        "flat text must not silently move text across images"
    );
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn paginated_user_and_assistant_rewrite_preserve_nontext_blocks() {
    let f = Fixture::new();
    f.update(|rows,seed| {
        rows[9]["payload"]["item"]["content"]=json!([{"type":"text","text":"DELETE-B","text_elements":[{"start":0,"end":8}]},{"type":"local_image","path":"synthetic.png","detail":"original"},{"type":"text","text":"tail","text_elements":[]}]);
        rows[8]["payload"]["content"]=json!([{"type":"input_text","text":"DELETE-B"},{"type":"input_image","image_url":"data:image/png;base64,AA==","detail":"original"},{"type":"input_text","text":"tail"}]);
        rows[8]["payload"]["internal_chat_message_metadata_passthrough"]["content_item_kinds"]=json!(["user.text","user.image","user.text"]);
        let row=seed.rows.get_mut("thread_items").unwrap().iter_mut().find(|r|r["item_id"]=="user-1").unwrap();
        let mut native:Value=serde_json::from_str(row["item_json"].as_str().unwrap()).unwrap();
        native["content"]=rows[9]["payload"]["item"]["content"].clone();native["content"][1]["type"]=json!("localImage");
        row["item_json"]=json!(native.to_string());
    });
    let original = fs::read(&f.path).unwrap();
    let items = f.items();
    let before_rows = load_file(&f.path).unwrap().parsed;
    let report = apply_edit_text_blocks(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        9,
        "",
        Some(&f.revision()),
        Some(&[crate::models::TextBlockEdit {
            content_index: 0,
            text: "changed user".into(),
        }]),
    )
    .unwrap();
    let rows = load_file(&f.path).unwrap().parsed;
    for (row, content_path) in [(9, "/payload/item/content"), (8, "/payload/content")] {
        let before = before_rows[row]
            .as_ref()
            .unwrap()
            .pointer(content_path)
            .unwrap()
            .as_array()
            .unwrap();
        let after = rows[row]
            .as_ref()
            .unwrap()
            .pointer(content_path)
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(after.len(), before.len());
        assert_eq!(
            &after[1..],
            &before[1..],
            "unedited blocks and their relative order must be identical"
        );
        assert_eq!(after[0]["text"], "changed user");
    }
    assert_eq!(
        rows[9].as_ref().unwrap()["payload"]["item"]["content"][1]["path"],
        "synthetic.png"
    );
    assert_eq!(
        rows[9].as_ref().unwrap()["payload"]["item"]["content"][0]["text_elements"],
        json!([])
    );
    assert_eq!(
        rows[8].as_ref().unwrap()["payload"]["content"][1]["image_url"],
        "data:image/png;base64,AA=="
    );
    assert_eq!(
        rows[8].as_ref().unwrap()["payload"]["content"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    f.rewrite(10, "changed assistant");
    assert!(!fs::read_to_string(&f.path).unwrap().contains("reply-1"));
    assert!(f.items().contains("changed assistant"));
    assert!(f.items().contains("localImage"));
    restore_snapshot(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        report.snapshot_created.as_deref().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read(&f.path).unwrap(), original);
    assert_eq!(f.items(), items);
}

#[test]
fn paginated_append_lock_and_interruption_recovery_preserve_history() {
    let f = Fixture::new();
    let original = fs::read(&f.path).unwrap();
    let items = f.items();
    let db = paginated::projection::open(&f.root.join("thread_history_1.sqlite"), true).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(f.delete(&[9]).is_err());
    assert_eq!(fs::read(&f.path).unwrap(), original);
    assert_eq!(f.items(), items);
    db.execute_batch("ROLLBACK").unwrap();
    transaction::FAIL_AFTER_ROLLOUT.with(|flag| flag.set(true));
    let report = f.delete(&[9]).unwrap();
    assert_eq!(report.status, "needs_recovery");
    assert_eq!(f.items(), items);
    let dir = edit_dir(f.backup.to_str().unwrap(), "codex", "thread-1");
    assert!(
        transaction::pending_summary(&dir, &f.path)
            .unwrap()
            .unwrap()
            .can_reconcile
    );
    transaction::reconcile("codex", &f.path, "thread-1", &dir, Some(&f.revision())).unwrap();
    transaction::reconcile("codex", &f.path, "thread-1", &dir, Some(&f.revision())).unwrap();
    assert!(!f.items().contains("DELETE-B"));
    undo_last(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read(&f.path).unwrap(), original);
    assert_eq!(f.items(), items);
    let revision = f.revision();
    fs::OpenOptions::new().append(true).open(&f.path).unwrap().write_all(b"{\"ordinal\":19,\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\"}}\n").unwrap();
    let appended = fs::read(&f.path).unwrap();
    assert!(apply_delete(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        &[9],
        Some(&revision)
    )
    .unwrap_err()
    .to_string()
    .contains("EDIT_CONFLICT"));
    assert_eq!(fs::read(&f.path).unwrap(), appended);
}

#[test]
fn paginated_disk_failure_and_committed_projection_recovery() {
    let f = Fixture::new();
    let original = fs::read(&f.path).unwrap();
    let items = f.items();
    transaction::FAIL_ROLLOUT_WRITE.with(|flag| flag.set(true));
    assert!(f
        .delete(&[9])
        .unwrap_err()
        .to_string()
        .contains("disk write failure"));
    assert_eq!(fs::read(&f.path).unwrap(), original);
    assert_eq!(f.items(), items);
    let dir = edit_dir(f.backup.to_str().unwrap(), "codex", "thread-1");
    assert!(transaction::pending_summary(&dir, &f.path)
        .unwrap()
        .is_none());
    transaction::FAIL_AFTER_PROJECTION.with(|flag| flag.set(true));
    assert_eq!(f.delete(&[9]).unwrap().status, "needs_recovery");
    assert!(!f.items().contains("DELETE-B"));
    transaction::reconcile("codex", &f.path, "thread-1", &dir, Some(&f.revision())).unwrap();
    assert_eq!(read_journal(&dir).unwrap().len(), 1);
    undo_last(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read(&f.path).unwrap(), original);
    assert_eq!(f.items(), items);
}

#[test]
fn paginated_shared_prefix_and_compaction_only_block_affected_range() {
    let f = Fixture::new();
    let child = f.root.join("sessions/child.jsonl");
    let child_bytes=json!({"type":"session_meta","payload":{"id":"child","history_mode":"paginated","history_base":{"thread_id":"thread-1","end_ordinal_exclusive":7}}}).to_string();
    fs::write(&child, &child_bytes).unwrap();
    let db = paginated::projection::open(&f.root.join("thread_history_1.sqlite"), true).unwrap();
    db.execute(
        "INSERT INTO thread_history_projection_state VALUES('child',123,7)",
        [],
    )
    .unwrap();
    let child_projection = paginated::projection::capture(&db, "child").unwrap();
    assert!(f.delete(&[3]).unwrap_err().to_string().contains("继承"));
    f.delete(&[15]).unwrap();
    assert_eq!(fs::read_to_string(&child).unwrap(), child_bytes);
    assert_eq!(
        paginated::projection::capture(&db, "child").unwrap(),
        child_projection
    );
    let c = Fixture::new();
    c.update(|rows,_|rows.insert(7,json!({"type":"compacted","payload":{"message":"KEEP-A compressed","replacement_history":[]}})));
    assert!(c.delete(&[3]).unwrap_err().to_string().contains("摘要"));
    c.delete(&[16]).unwrap();
    assert!(fs::read_to_string(&c.path)
        .unwrap()
        .contains("KEEP-A compressed"));
}

#[test]
fn paginated_tool_turn_and_failed_interrupted_turns_can_be_deleted() {
    let f = Fixture::new();
    f.update(|rows,seed| {
        let tool=json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"thread-1","turn_id":"turn-1","item":{"type":"CommandExecution","id":"tool-1","command":["synthetic-never-execute"],"cwd":url::Url::from_directory_path(&f.root).unwrap().to_string(),"parsed_cmd":[],"source":"agent","status":"completed"}}});
        rows.splice(10..10,[
            json!({"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call-1","arguments":"synthetic-never-execute"}}),
            tool,
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"synthetic output"}}),
        ]);
        seed.rows.get_mut("thread_items").unwrap().push(json!({"thread_id":"thread-1","turn_id":"turn-1","item_id":"tool-1","rollout_ordinal":11,"created_at_ms":1790059470000i64,"item_json":json!({"id":"tool-1","type":"commandExecution","command":"synthetic-never-execute","status":"completed"}).to_string(),"item_type":"commandExecution","updated_at_ordinal":11}));
    });
    assert!(f
        .delete(&[11])
        .unwrap_err()
        .to_string()
        .contains("全部正式记录"));
    let original = fs::read(&f.path).unwrap();
    let items = f.items();
    let partial = plan_delete(
        "codex",
        f.path.to_str().unwrap(),
        &[11],
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(partial.messages.len(), 1);
    assert_eq!(partial.required_turns.len(), 1);
    assert!(!partial.blocked.is_empty());
    assert_eq!(
        fs::read(&f.path).unwrap(),
        original,
        "suggesting a turn must not edit data"
    );
    let targets: Vec<_> = partial.required_turns[0]
        .messages
        .iter()
        .map(|m| m.target.clone().unwrap())
        .collect();
    assert_eq!(
        targets
            .iter()
            .map(|t| t.item_id.as_str())
            .collect::<Vec<_>>(),
        vec!["user-1", "tool-1", "agent-1"]
    );
    let selected = resolve_selection(
        "codex",
        f.path.to_str().unwrap(),
        vec![0; targets.len()],
        Some(
            &targets
                .into_iter()
                .map(crate::models::SessionEventTarget::Codex)
                .collect::<Vec<_>>(),
        ),
        Some(&f.revision()),
    )
    .unwrap();
    let full = plan_delete(
        "codex",
        f.path.to_str().unwrap(),
        &selected,
        Some(&f.revision()),
    )
    .unwrap();
    assert!(full.required_turns.is_empty());
    assert!(full.blocked.is_empty());
    assert_eq!(full.messages.len(), 3);
    f.delete(&selected).unwrap();
    assert!(!fs::read_to_string(&f.path).unwrap().contains("call-1"));
    assert!(!f.items().contains("tool-1"));
    undo_last(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read(&f.path).unwrap(), original);
    assert_eq!(f.items(), items);
    for interrupted in [false, true] {
        let failed = Fixture::new();
        failed.update(|rows, seed| {
            rows[12]["payload"]["type"] = json!(if interrupted {
                "turn_aborted"
            } else {
                "task_complete"
            });
            if interrupted {rows[12]["payload"]["reason"]=json!("interrupted");}
            else {rows[12]["payload"]["error"] = json!({"message":"synthetic failure"});}
            let turn = seed
                .rows
                .get_mut("thread_turns")
                .unwrap()
                .iter_mut()
                .find(|r| r["turn_id"] == "turn-1")
                .unwrap();
            turn["status"] = json!(if interrupted { "interrupted" } else { "failed" });
            turn["error_json"] = if interrupted {Value::Null} else {json!(json!({"message":"synthetic failure","codexErrorInfo":null,"additionalDetails":null}).to_string())};
        });
        failed.delete(&[9, 10]).unwrap();
        assert!(!failed.items().contains("DELETE-B"));
    }
}

#[test]
fn paginated_legacy_damage_is_diagnosed_separately_and_not_auto_repaired() {
    let f = Fixture::new();
    f.update(|rows, _| {
        rows.remove(8);
    }); // old editor removed only the user context
    let damaged = fs::read(&f.path).unwrap();
    let capability = inspect_edit_capability("codex", f.path.to_str().unwrap()).unwrap();
    assert!(
        capability.blocked_reasons.is_empty(),
        "unaffected ranges remain usable"
    );
    assert!(capability.diagnostics.is_empty());
    let detail = capability
        .content_mappings
        .iter()
        .find(|m| m.item_id == "user-1")
        .unwrap();
    assert_eq!(detail.reason_code.as_deref(), Some("MISSING_CONTEXT"));
    assert!(!detail.operations.delete_message.supported);
    assert!(detail.operations.delete_turn.supported);
    assert!(f.delete(&[8]).is_err());
    assert_eq!(fs::read(&f.path).unwrap(), damaged);
    let report = f.rewrite(3, "unaffected edit");
    assert_eq!(
        inspect_edit_capability("codex", f.path.to_str().unwrap())
            .unwrap()
            .content_mappings
            .iter()
            .filter(|m| m.status == "unsupported")
            .count(),
        1
    );
    restore_snapshot(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        report.snapshot_created.as_deref().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(
        fs::read(&f.path).unwrap(),
        damaged,
        "a snapshot of damaged history is not a repair"
    );
    assert!(f.items().contains("DELETE-B"));
}

#[test]
fn paginated_legacy_text_mismatch_is_diagnosed_without_repairing_it() {
    let f = Fixture::new();
    f.update(|rows, _| rows[8]["payload"]["content"][0]["text"] = json!("old edited context"));
    let before = fs::read(&f.path).unwrap();
    let capability = inspect_edit_capability("codex", f.path.to_str().unwrap()).unwrap();
    assert!(capability.blocked_reasons.is_empty());
    assert_eq!(capability.diagnostics.len(), 1);
    assert!(capability.diagnostics[0].contains("user-1"));
    assert!(apply_edit_text(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        9,
        "new text",
        Some(&f.revision())
    )
    .unwrap_err()
    .to_string()
    .contains("EDIT_INCONSISTENT"));
    assert_eq!(fs::read(&f.path).unwrap(), before);
    f.rewrite(3, "unaffected edit");
    assert_eq!(
        inspect_edit_capability("codex", f.path.to_str().unwrap())
            .unwrap()
            .diagnostics
            .len(),
        1
    );
}

#[test]
fn paginated_unknown_turn_and_live_writer_leave_other_ranges_available() {
    let f = Fixture::new();
    f.update(|rows,_|rows.insert(10,json!({"type":"event_msg","payload":{"type":"unknown_future_state","turn_id":"turn-1"}})));
    assert!(f.delete(&[9]).unwrap_err().to_string().contains("尚未映射"));
    f.delete(&[3]).unwrap();
    #[cfg(windows)]
    {
        let writer = fs::OpenOptions::new().append(true).open(&f.path).unwrap();
        let before = fs::read(&f.path).unwrap();
        let index = load_file(&f.path)
            .unwrap()
            .parsed
            .iter()
            .position(|v| v.as_ref().unwrap()["payload"]["item"]["id"] == "user-2")
            .unwrap();
        assert!(f
            .delete(&[index])
            .unwrap_err()
            .to_string()
            .contains("EDIT_BUSY"));
        assert_eq!(fs::read(&f.path).unwrap(), before);
        drop(writer);
    }
}

#[test]
fn paginated_pending_recovery_rejects_external_projection_changes() {
    let f = Fixture::new();
    transaction::FAIL_AFTER_ROLLOUT.with(|flag| flag.set(true));
    assert_eq!(f.delete(&[9]).unwrap().status, "needs_recovery");
    let db = paginated::projection::open(&f.root.join("thread_history_1.sqlite"), true).unwrap();
    db.execute("UPDATE thread_items SET item_json='external-state' WHERE thread_id='thread-1' AND item_id='user-1'",[]).unwrap();
    let dir = edit_dir(f.backup.to_str().unwrap(), "codex", "thread-1");
    let after = fs::read(&f.path).unwrap();
    let items = f.items();
    assert!(
        !transaction::pending_summary(&dir, &f.path)
            .unwrap()
            .unwrap()
            .can_reconcile
    );
    assert!(
        transaction::reconcile("codex", &f.path, "thread-1", &dir, Some(&f.revision()))
            .unwrap_err()
            .to_string()
            .contains("EDIT_CONFLICT")
    );
    assert_eq!(fs::read(&f.path).unwrap(), after);
    assert_eq!(f.items(), items);
    assert!(dir.join("pending-operation.json").exists());
}

#[test]
fn paginated_unphased_assistant_updates_completion_and_projection() {
    let f = Fixture::new();
    f.update(|rows, _| {
        rows[10]["payload"]["item"]["phase"] = Value::Null;
        rows[11]["payload"]["phase"] = Value::Null;
    });
    f.rewrite(10, "unphased changed");
    assert!(!fs::read_to_string(&f.path).unwrap().contains("reply-1"));
    let rows = load_file(&f.path).unwrap().parsed;
    assert_eq!(
        rows[12].as_ref().unwrap()["payload"]["last_agent_message"],
        "unphased changed"
    );
    f.delete(&[10]).unwrap();
    assert!(!fs::read_to_string(&f.path)
        .unwrap()
        .contains("unphased changed"));
}

#[test]
fn paginated_core_summary_and_split_database_recovery_preserve_other_threads() {
    let f = Fixture::new();
    let db = rusqlite::Connection::open(f.root.join("state_5.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,title TEXT,first_user_message TEXT,preview TEXT,project_id TEXT,rollout_path TEXT); INSERT INTO threads VALUES('thread-1','Named thread','KEEP-A','KEEP-A','unchanged-project',NULL),('other','other','other','other','other-project',NULL);").unwrap();
    db.execute(
        "UPDATE threads SET rollout_path=?1 WHERE id='thread-1'",
        [f.path.to_str().unwrap()],
    )
    .unwrap();
    let original = fs::read(&f.path).unwrap();
    transaction::FAIL_AFTER_PROJECTION.with(|flag| flag.set(true));
    let report = f.rewrite(3, "NEW-A");
    assert_eq!(report.status, "needs_recovery");
    let summary: Vec<String> = db
        .query_row(
            "SELECT title,first_user_message,preview,project_id FROM threads WHERE id='thread-1'",
            [],
            |r| Ok(vec![r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?]),
        )
        .unwrap();
    assert_eq!(
        summary,
        vec!["Named thread", "NEW-A", "NEW-A", "unchanged-project"]
    );
    // Simulate power loss between the WAL commits of the two attached databases.
    db.execute(
        "UPDATE threads SET first_user_message='KEEP-A',preview='KEEP-A' WHERE id='thread-1'",
        [],
    )
    .unwrap();
    let dir = edit_dir(f.backup.to_str().unwrap(), "codex", "thread-1");
    assert!(
        transaction::pending_summary(&dir, &f.path)
            .unwrap()
            .unwrap()
            .can_reconcile
    );
    transaction::reconcile("codex", &f.path, "thread-1", &dir, Some(&f.revision())).unwrap();
    assert_eq!(
        db.query_row("SELECT preview FROM threads WHERE id='thread-1'", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "NEW-A"
    );
    undo_last(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read(&f.path).unwrap(), original);
    assert_eq!(
        db.query_row("SELECT preview FROM threads WHERE id='thread-1'", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "KEEP-A"
    );
    assert_eq!(
        db.query_row("SELECT project_id FROM threads WHERE id='other'", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "other-project"
    );
}

#[test]
fn paginated_native_writer_lock_blocks_without_an_open_rollout() {
    let f = Fixture::new();
    let dir = f.root.join("thread-writer-locks");
    fs::create_dir_all(&dir).unwrap();
    let lock = fs::File::create(dir.join("thread-1.lock")).unwrap();
    lock.try_lock().unwrap();
    let before = fs::read(&f.path).unwrap();
    assert!(f
        .delete(&[9])
        .unwrap_err()
        .to_string()
        .contains("EDIT_BUSY"));
    assert_eq!(fs::read(&f.path).unwrap(), before);
    drop(lock);
    f.delete(&[9]).unwrap();
}

#[test]
#[ignore = "requires an explicitly created isolated native fixture"]
fn paginated_native_media_fixture_command() {
    let root =
        PathBuf::from(std::env::var("CC_NATIVE_MEDIA_HOME").expect("isolated native fixture"));
    let capture_file = root
        .join(std::env::var("CC_NATIVE_MEDIA_CAPTURE").unwrap_or_else(|_| "capture.json".into()));
    assert!(capture_file
        .canonicalize()
        .unwrap()
        .starts_with(root.canonicalize().unwrap()));
    let marker: Value = serde_json::from_slice(&fs::read(capture_file).unwrap()).unwrap();
    assert_eq!(marker["nativeGenerated"], true);
    let path = PathBuf::from(marker["path"].as_str().unwrap());
    assert!(path
        .canonicalize()
        .unwrap()
        .starts_with(root.canonicalize().unwrap()));
    let id = marker["threadId"].as_str().unwrap();
    let backup = root.join("edit-backups");
    let capability = inspect_edit_capability("codex", path.to_str().unwrap()).unwrap();
    let action = std::env::var("CC_NATIVE_MEDIA_ACTION").unwrap();
    let result = if action == "diagnose" {
        json!({"capability":capability,"history":history("codex",path.to_str().unwrap(),id,backup.to_str().unwrap()).unwrap()})
    } else {
        let revision = Some(capability.revision);
        let report = if action == "undo" {
            undo_last(
                "codex",
                path.to_str().unwrap(),
                id,
                backup.to_str().unwrap(),
                revision.as_deref(),
            )
        } else {
            let target_id = std::env::var("CC_NATIVE_MEDIA_ITEM").unwrap();
            let loaded = load_file(&path).unwrap();
            let h = super::model(&loaded).unwrap();
            let item = h.items.iter().find(|i| i.key.id == target_id).unwrap();
            let targets: Option<Vec<crate::models::SessionEventTarget>> = Some(
                h.items
                    .iter()
                    .filter(|candidate| {
                        if action == "delete-turn" {
                            candidate.key.turn == item.key.turn
                        } else {
                            candidate.key.id == target_id
                        }
                    })
                    .map(|candidate| {
                        crate::models::SessionEventTarget::Codex(
                            crate::models::PaginatedItemTarget {
                                thread_id: id.into(),
                                turn_id: candidate.key.turn.clone(),
                                item_id: candidate.key.id.clone(),
                            },
                        )
                    })
                    .collect(),
            );
            if matches!(action.as_str(), "delete" | "delete-turn") {
                delete_session_events_with_lock(
                    "codex".into(),
                    path.to_string_lossy().into(),
                    id.into(),
                    backup.to_string_lossy().into(),
                    vec![0; targets.as_ref().unwrap().len()],
                    revision,
                    targets,
                    &crate::family::FamilyLock::default(),
                )
            } else {
                assert_eq!(action, "edit");
                let block = std::env::var("CC_NATIVE_MEDIA_BLOCK")
                    .unwrap()
                    .parse()
                    .unwrap();
                edit_session_event_text_with_lock(
                    "codex".into(),
                    path.to_string_lossy().into(),
                    id.into(),
                    backup.to_string_lossy().into(),
                    0,
                    "".into(),
                    revision,
                    targets,
                    Some(vec![crate::models::TextBlockEdit {
                        content_index: block,
                        text: std::env::var("CC_NATIVE_MEDIA_TEXT")
                            .unwrap_or_else(|_| "NATIVE-MEDIA-EDITED".into()),
                    }]),
                    &crate::family::FamilyLock::default(),
                )
            }
        };
        match report {
            Ok(report) => json!({"ok":true,"report":report}),
            Err(error) => json!({"ok":false,"error":error.to_string()}),
        }
    };
    fs::write(
        root.join("last-operation.json"),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "requires an explicitly created isolated native fixture"]
fn paginated_native_fixture_command() {
    let root = PathBuf::from(std::env::var("CC_SYNTHETIC_HOME").expect("isolated fixture home"));
    let marker: Value =
        serde_json::from_slice(&fs::read(root.join(".cc-synthetic-fixture.json")).unwrap())
            .unwrap();
    let path = PathBuf::from(marker["path"].as_str().unwrap());
    assert!(path
        .canonicalize()
        .unwrap()
        .starts_with(root.canonicalize().unwrap()));
    let id = marker["id"].as_str().unwrap();
    let backup = root.join("edit-backups");
    let revision = inspect_edit_capability("codex", path.to_str().unwrap())
        .unwrap()
        .revision;
    let action = std::env::var("CC_SYNTHETIC_ACTION").unwrap();
    if action == "diagnose-damage" {
        let before = fs::read(&path).unwrap();
        let capability = inspect_edit_capability("codex", path.to_str().unwrap()).unwrap();
        assert!(!capability.diagnostics.is_empty());
        let target = load_file(&path)
            .unwrap()
            .parsed
            .iter()
            .position(|v| {
                v.as_ref()
                    .is_some_and(|v| v["payload"]["item"]["id"] == "user-1")
            })
            .unwrap();
        let deletion = apply_delete(
            "codex",
            path.to_str().unwrap(),
            id,
            backup.to_str().unwrap(),
            &[target],
            Some(&revision),
        )
        .unwrap_err()
        .to_string();
        let journal = read_journal(&edit_dir(backup.to_str().unwrap(), "codex", id)).unwrap();
        let restore = restore_snapshot(
            "codex",
            path.to_str().unwrap(),
            id,
            backup.to_str().unwrap(),
            journal[0].base_snapshot.as_deref().unwrap(),
            Some(&revision),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::write(root.join("diagnose-damage-report.json"), serde_json::to_vec_pretty(&json!({
            "threadId":id, "capability":capability, "deleteRejected":deletion,
            "snapshotRestoreRejected":restore, "rolloutUnchanged":true, "automaticallyRepaired":false,
        })).unwrap()).unwrap();
        return;
    }
    let range_items: Vec<String> = marker["range_items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_else(|| vec!["user-1".into(), "agent-1".into()]);
    let report = match action.as_str() {
        "delete" | "range" | "busy" => delete_session_events_with_lock(
            "codex".into(),
            path.to_string_lossy().into(),
            id.into(),
            backup.to_string_lossy().into(),
            if action == "range" {
                vec![9; range_items.len()]
            } else {
                vec![9]
            },
            Some(revision),
            Some(
                (if action == "range" {
                    range_items
                } else {
                    vec!["user-1".into()]
                })
                .into_iter()
                .map(|item| {
                    crate::models::SessionEventTarget::Codex(crate::models::PaginatedItemTarget {
                        thread_id: id.into(),
                        turn_id: marker["turns"][1].as_str().unwrap().into(),
                        item_id: item.into(),
                    })
                })
                .collect(),
            ),
            &crate::family::FamilyLock::default(),
        ),
        "undo" => undo_last(
            "codex",
            path.to_str().unwrap(),
            id,
            backup.to_str().unwrap(),
            Some(&revision),
        ),
        "rewrite" | "rewrite-assistant" => edit_session_event_text_with_lock(
            "codex".into(),
            path.to_string_lossy().into(),
            id.into(),
            backup.to_string_lossy().into(),
            3,
            if action == "rewrite" {
                "NATIVE-EDITED-A"
            } else {
                "NATIVE-EDITED-ASSISTANT"
            }
            .into(),
            Some(revision),
            Some(vec![crate::models::SessionEventTarget::Codex(
                crate::models::PaginatedItemTarget {
                    thread_id: id.into(),
                    turn_id: marker["turns"][0].as_str().unwrap().into(),
                    item_id: if action == "rewrite" {
                        "user-0"
                    } else {
                        "agent-0"
                    }
                    .into(),
                },
            )]),
            Some(vec![crate::models::TextBlockEdit {
                content_index: 0,
                text: if action == "rewrite" {
                    "NATIVE-EDITED-A"
                } else {
                    "NATIVE-EDITED-ASSISTANT"
                }
                .into(),
            }]),
            &crate::family::FamilyLock::default(),
        ),
        "restore" => {
            let dir = edit_dir(backup.to_str().unwrap(), "codex", id);
            let first = read_journal(&dir).unwrap();
            restore_snapshot(
                "codex",
                path.to_str().unwrap(),
                id,
                backup.to_str().unwrap(),
                first[0].base_snapshot.as_deref().unwrap(),
                Some(&revision),
            )
        }
        _ => panic!("unknown synthetic action"),
    };
    if action == "busy" {
        let error = report.unwrap_err().to_string();
        assert!(error.contains("EDIT_BUSY"), "{error}");
        fs::write(
            root.join("busy-report.json"),
            b"{\"nativeWriterRejected\":true}",
        )
        .unwrap();
        return;
    }
    let report = report.unwrap();
    assert_eq!(report.status, "committed_unverified");
    fs::write(
        root.join(format!("{action}-report.json")),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn paginated_middle_delete_preserves_id_neighbors_and_undo_restores_projection() {
    let f = Fixture::new();
    let original = fs::read(&f.path).unwrap();
    let items = f.items();
    let report = f
        .delete(&[9])
        .expect("normal stopped paginated history must support message deletion");
    assert_ne!(report.status, "needs_recovery");
    let after = fs::read_to_string(&f.path).unwrap();
    assert!(!after.contains("DELETE-B"));
    assert!(after.contains("thread-1"));
    assert!(after.find("KEEP-A").unwrap() < after.find("KEEP-C").unwrap());
    assert!(!f.items().contains("DELETE-B"));
    undo_last(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read(&f.path).unwrap(), original);
    assert_eq!(f.items(), items);
}

#[test]
fn paginated_range_delete_removes_completion_copy_and_supports_redo_and_snapshot() {
    let f = Fixture::new();
    let original = fs::read(&f.path).unwrap();
    let items = f.items();
    let report = f.delete(&[9, 10]).unwrap();
    let after = fs::read(&f.path).unwrap();
    assert!(!String::from_utf8_lossy(&after).contains("reply-1"));
    let undo = || {
        undo_last(
            "codex",
            f.path.to_str().unwrap(),
            "thread-1",
            f.backup.to_str().unwrap(),
            Some(&f.revision()),
        )
        .unwrap()
    };
    undo();
    assert_eq!(fs::read(&f.path).unwrap(), original);
    assert_eq!(f.items(), items);
    undo();
    assert_eq!(fs::read(&f.path).unwrap(), after);
    restore_snapshot(
        "codex",
        f.path.to_str().unwrap(),
        "thread-1",
        f.backup.to_str().unwrap(),
        report.snapshot_created.as_deref().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read(&f.path).unwrap(), original);
    assert_eq!(f.items(), items);
}
