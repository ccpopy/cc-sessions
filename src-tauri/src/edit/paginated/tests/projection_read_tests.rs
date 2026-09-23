use super::*;

fn status(f: &Fixture) -> Value {
    serde_json::to_value(inspect_edit_capability("codex", f.path.to_str().unwrap()).unwrap())
        .unwrap()["projection"]
        .clone()
}

#[test]
fn projection_read_lag_is_updating_then_sync_restores_editing() {
    let f = Fixture::new();
    let loaded = load_file(&f.path).unwrap();
    let image = paginated::projection::read(&f.path, &loaded).unwrap();
    let db = paginated::projection::open(&f.root.join("thread_history_1.sqlite"), true).unwrap();
    let prefix = loaded.lines[..9].join("\n").len() + 1;
    db.execute("DELETE FROM thread_items WHERE rollout_ordinal >= 9", [])
        .unwrap();
    db.execute("UPDATE thread_history_projection_state SET next_rollout_byte_offset=?1,next_rollout_ordinal=9", [prefix]).unwrap();
    let detail = status(&f);
    assert_eq!(detail["state"], "updating", "{detail}");
    assert_eq!(detail["reason_code"], "PROJECTION_BEHIND");
    assert_eq!(detail["item"]["item_id"], "user-1");
    assert_eq!(detail["item"]["covered"], false);
    assert!(f.delete(&[9]).is_err());
    paginated::projection::replace(&db, "thread-1", &image).unwrap();
    assert_eq!(status(&f)["state"], "ready");
    f.delete(&[9]).unwrap();
    assert!(!f.items().contains("DELETE-B"));
}

#[test]
fn projection_read_covered_missing_keeps_protection() {
    let f = Fixture::new();
    let db = paginated::projection::open(&f.root.join("thread_history_1.sqlite"), true).unwrap();
    db.execute("DELETE FROM thread_items WHERE item_id='user-1'", [])
        .unwrap();
    let detail = status(&f);
    assert_eq!(detail["state"], "inconsistent", "{detail}");
    assert_eq!(detail["reason_code"], "PROJECTED_ITEM_MISSING");
    assert_eq!(detail["item"]["covered"], true);
    assert!(f.delete(&[9]).is_err());
}

#[test]
fn projection_read_concurrent_commit_never_mixes_table_generations() {
    let f = Fixture::new();
    let path = f.root.join("thread_history_1.sqlite");
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL").unwrap();
    let reader = paginated::projection::open(&path, false).unwrap();
    paginated::projection::AFTER_ITEMS_READ.with(|hook| *hook.borrow_mut() = Some(Box::new(move || {
        writer.execute_batch("BEGIN IMMEDIATE; DELETE FROM thread_items WHERE item_id='user-1'; UPDATE thread_history_projection_state SET next_rollout_byte_offset=123; COMMIT").unwrap();
    })));
    let captured = paginated::projection::capture(&reader, "thread-1").unwrap();
    assert_eq!(
        captured.rows["thread_history_projection_state"][0]["next_rollout_byte_offset"],
        123
    );
    assert!(!captured.rows["thread_items"]
        .iter()
        .any(|r| r["item_id"] == "user-1"));
}

#[test]
fn projection_read_native_filtered_history_is_not_called_corruption() {
    let f = Fixture::new();
    f.update(|rows, _| rows[0]["payload"]["subagent_history_start_ordinal"] = json!(10));
    let db = paginated::projection::open(&f.root.join("thread_history_1.sqlite"), true).unwrap();
    db.execute("DELETE FROM thread_items WHERE item_id='user-1'", [])
        .unwrap();
    let detail = status(&f);
    assert_eq!(detail["state"], "identity_pending");
    assert_eq!(detail["reason_code"], "NATIVE_FILTERED_RANGE");
    assert!(f.delete(&[9]).is_err());
}

#[test]
fn projection_read_pre_identity_snapshot_remains_restorable_for_ordinary_thread() {
    let f = Fixture::new();
    let original = load_file(&f.path).unwrap();
    let mut snapshot = paginated::projection::read(&f.path, &original).unwrap();
    snapshot.identity = None;
    f.rewrite(9, "EDITED-B");
    let current = load_file(&f.path).unwrap();
    let change =
        paginated::projection::prepare(&f.path, &current, &original, Some(&snapshot)).unwrap();
    let db = change.begin("thread-1").unwrap();
    paginated::projection::replace(&db, "thread-1", &change.after).unwrap();
    db.execute_batch("ROLLBACK").unwrap();
    assert_eq!(change.after.identity, change.before.identity);
}

#[test]
fn projection_read_uncommitted_rows_remain_updating_until_atomic_commit() {
    let f = Fixture::new();
    let path = f.root.join("thread_history_1.sqlite");
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL").unwrap();
    let complete = paginated::projection::read(&f.path, &load_file(&f.path).unwrap()).unwrap();
    writer.execute_batch("DELETE FROM thread_items WHERE item_id='user-1'; UPDATE thread_history_projection_state SET next_rollout_byte_offset=0,next_rollout_ordinal=0; BEGIN IMMEDIATE").unwrap();
    paginated::projection::replace(&writer, "thread-1", &complete).unwrap();
    assert_eq!(status(&f)["state"], "updating");
    writer.execute_batch("COMMIT").unwrap();
    assert_eq!(status(&f)["state"], "ready");
}

#[test]
fn projection_read_failure_with_growing_log_does_not_claim_missing_history() {
    let f = Fixture::new();
    let db = rusqlite::Connection::open(f.root.join("thread_history_1.sqlite")).unwrap();
    db.execute_batch("BEGIN EXCLUSIVE").unwrap();
    for ordinal in 19..21 {
        fs::OpenOptions::new().append(true).open(&f.path).unwrap().write_all(
            format!("{{\"ordinal\":{ordinal},\"type\":\"event_msg\",\"payload\":{{\"type\":\"thread_settings_applied\"}}}}\n").as_bytes()).unwrap();
        let detail = status(&f);
        assert_eq!(detail["state"], "failed", "{detail}");
        assert_eq!(detail["reason_code"], "PROJECTION_READ_FAILED");
    }
    db.execute_batch("ROLLBACK").unwrap();
    assert_eq!(status(&f)["state"], "updating");
    let loaded = load_file(&f.path).unwrap();
    let seed = paginated::projection::capture(&db, "thread-1").unwrap();
    let image = paginated::projection::project(&loaded, &seed).unwrap();
    paginated::projection::replace(&db, "thread-1", &image).unwrap();
    assert_eq!(status(&f)["state"], "ready");
    f.rewrite(9, "SYNCED-B");
}

#[test]
fn projection_read_preview_during_continuous_generation_has_coherent_revision() {
    let f = Fixture::new();
    let initial = load_file(&f.path).unwrap();
    let db = paginated::projection::open(&f.root.join("thread_history_1.sqlite"), true).unwrap();
    let complete = paginated::projection::read(&f.path, &initial).unwrap();
    let first = initial.lines[0].clone() + "\n";
    fs::write(&f.path, &first).unwrap();
    db.execute_batch("DELETE FROM thread_items; DELETE FROM thread_turns")
        .unwrap();
    db.execute("UPDATE thread_history_projection_state SET next_rollout_byte_offset=?1,next_rollout_ordinal=1", [first.len()]).unwrap();
    let stale_loaded = load_file(&f.path).unwrap();
    let new_records = initial.lines[1..].to_vec();
    let path = f.path.clone();
    let (send, receive) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        for (i, record) in new_records.into_iter().enumerate() {
            file.write_all((record + "\n").as_bytes()).unwrap();
            if i == 0 {
                send.send(()).unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });
    receive.recv().unwrap();
    let stale = paginated::projection::read(&f.path, &stale_loaded).unwrap_err();
    assert!(stale.to_string().contains("ROLLOUT_CHANGING"));
    for _ in 0..3 {
        let page = crate::rollout::preview_session_page(
            "codex".into(),
            f.path.to_string_lossy().into(),
            0,
            usize::MAX,
            None,
        )
        .unwrap();
        assert!(page.events.len() >= 2);
        let cap = page.capability.unwrap();
        assert_eq!(cap.projection.unwrap().state, "updating");
        let lines = page
            .events
            .iter()
            .map(|e| e.raw.to_string())
            .collect::<Vec<_>>();
        assert_eq!(cap.file_sha256, transaction::lines_hash(&lines, true));
    }
    writer.join().unwrap();
    paginated::projection::replace(&db, "thread-1", &complete).unwrap();
    assert_eq!(status(&f)["state"], "ready");
    f.delete(&[9]).unwrap();
}

#[test]
#[ignore = "read-only structural diagnosis of an explicitly supplied native rollout"]
fn projection_read_native_audit() {
    let path = std::env::var("CC_PROJECTION_AUDIT_PATH").unwrap();
    let capability = inspect_edit_capability("codex", &path).unwrap();
    let result = json!({"projection":capability.projection,"blocked_reasons":capability.blocked_reasons,"revision":capability.revision});
    fs::write(
        std::env::var("CC_PROJECTION_AUDIT_OUTPUT").unwrap(),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
}

#[test]
fn projection_read_switched_rollout_uses_physical_key_and_undo() {
    let mut f = Fixture::new();
    let logical = "00000000-0000-4000-8000-000000000001";
    let physical = "00000000-0000-4000-8000-000000000002";
    let path = f.root.join(format!(
        "sessions/rollout-2026-09-23T16-59-25-{logical}_{physical}.jsonl"
    ));
    let original = fs::read_to_string(&f.path)
        .unwrap()
        .replace("thread-1", logical);
    fs::write(&path, original.as_bytes()).unwrap();
    fs::remove_file(&f.path).unwrap();
    f.path = path;
    let db = paginated::projection::open(&f.root.join("thread_history_1.sqlite"), true).unwrap();
    for table in [
        "thread_items",
        "thread_turns",
        "thread_realtime_items",
        "thread_history_projection_state",
    ] {
        db.execute(&format!("UPDATE {table} SET thread_id=?1"), [physical])
            .unwrap();
    }
    // Recompute byte positions after replacing the logical ID; preserve native rows.
    let mut seed = paginated::projection::capture(&db, physical).unwrap();
    seed = paginated::projection::project(&load_file(&f.path).unwrap(), &seed).unwrap();
    seed.rows
        .get_mut("thread_history_projection_state")
        .unwrap()[0]["thread_id"] = json!(physical);
    paginated::projection::replace(&db, physical, &seed).unwrap();
    // The logical ID has an older projection, as on a reverted native thread.
    db.execute(
        "INSERT INTO thread_history_projection_state VALUES(?1,999999,100)",
        [logical],
    )
    .unwrap();
    let core = rusqlite::Connection::open(f.root.join("state_5.sqlite")).unwrap();
    core.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT,title TEXT,first_user_message TEXT,preview TEXT)").unwrap();
    core.execute(
        "INSERT INTO threads VALUES(?1,?2,'original','KEEP-A','KEEP-A')",
        rusqlite::params![logical, f.path.to_str().unwrap()],
    )
    .unwrap();
    assert_eq!(status(&f)["state"], "ready");
    assert_eq!(status(&f)["rollout_id"], physical);
    apply_edit_text(
        "codex",
        f.path.to_str().unwrap(),
        logical,
        f.backup.to_str().unwrap(),
        9,
        "EDITED-B",
        Some(&f.revision()),
    )
    .unwrap();
    undo_last(
        "codex",
        f.path.to_str().unwrap(),
        logical,
        f.backup.to_str().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&f.path).unwrap(), original);
    transaction::FAIL_AFTER_ROLLOUT.with(|flag| flag.set(true));
    let interrupted = apply_delete(
        "codex",
        f.path.to_str().unwrap(),
        logical,
        f.backup.to_str().unwrap(),
        &[9],
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(interrupted.status, "needs_recovery");
    let directory = edit_dir(f.backup.to_str().unwrap(), "codex", logical);
    assert!(
        transaction::pending_summary(&directory, &f.path)
            .unwrap()
            .unwrap()
            .can_reconcile
    );
    transaction::reconcile("codex", &f.path, logical, &directory, Some(&f.revision())).unwrap();
    undo_last(
        "codex",
        f.path.to_str().unwrap(),
        logical,
        f.backup.to_str().unwrap(),
        Some(&f.revision()),
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&f.path).unwrap(), original);
    assert_eq!(db.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id=?1",[logical],|r|r.get::<_,i64>(0)).unwrap(),999999);
    core.execute(
        "UPDATE threads SET rollout_path='another-rollout.jsonl'",
        [],
    )
    .unwrap();
    assert_eq!(status(&f)["state"], "identity_pending");
    assert!(apply_delete(
        "codex",
        f.path.to_str().unwrap(),
        logical,
        f.backup.to_str().unwrap(),
        &[9],
        Some(&f.revision())
    )
    .is_err());
}
