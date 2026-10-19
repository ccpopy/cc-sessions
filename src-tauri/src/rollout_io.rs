//! Read the logical JSONL bytes without materializing Codex's compressed rollouts.
//!
//! Codex may keep a logical `.jsonl` path in SQLite while only `.jsonl.zst` exists.
//! Prefer plain siblings, as Core does, and keep discovery and readers consistent.
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crate::error::{ensure_not_cancelled, AppResult};

mod materialize;
mod writer_lock;
pub(crate) use materialize::materialize_for_write;
pub(crate) use writer_lock::{writer_guard, WriterGuard};

pub(crate) fn codex_root(path: &Path) -> Option<&Path> {
    path.ancestors()
        .find(|p| {
            matches!(
                p.file_name().and_then(|n| n.to_str()),
                Some("sessions" | "archived_sessions")
            )
        })
        .and_then(Path::parent)
}

/// Revisions address original JSONL bytes, independently of physical compression.
pub(crate) fn revision_path(path: &Path) -> AppResult<PathBuf> {
    Ok(plain_path(&resolve(path)?.canonicalize()?))
}

pub(crate) fn logical_fingerprint(path: &Path) -> AppResult<crate::atomic_file::FileFingerprint> {
    let physical = resolve(path)?;
    if is_compressed(&physical) {
        crate::atomic_file::fingerprint_reader(&mut reader_exact(&physical)?)
    } else {
        crate::atomic_file::fingerprint(&physical)
    }
}

pub(crate) fn is_compressed(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl.zst"))
}

pub(crate) fn plain_path(path: &Path) -> PathBuf {
    if is_compressed(path) {
        path.with_extension("")
    } else {
        path.to_path_buf()
    }
}

pub(crate) fn plain_file_name(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    Some(
        name.strip_suffix(".zst")
            .filter(|_| is_compressed(path))
            .unwrap_or(name),
    )
}

pub(crate) fn is_rollout(path: &Path) -> bool {
    plain_file_name(path)
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
}

/// Directory walkers see a logical rollout once, even during representation changes.
pub(crate) fn discover(path: &Path) -> bool {
    is_rollout(path) && discover_transcript(path)
}

/// Dependency checks also cover legacy JSONL names without the rollout prefix.
pub(crate) fn discover_transcript(path: &Path) -> bool {
    plain_file_name(path).is_some_and(|name| name.ends_with(".jsonl"))
        && (!is_compressed(path) || !plain_path(path).is_file())
}

pub(crate) fn resolve(path: &Path) -> io::Result<PathBuf> {
    let plain = plain_path(path);
    match fs::metadata(&plain) {
        Ok(_) => return Ok(plain),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if plain
        .extension()
        .is_some_and(|extension| extension == "jsonl")
    {
        let compressed = plain.with_extension("jsonl.zst");
        fs::metadata(&compressed)?;
        return Ok(compressed);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("会话文件不存在: {}", path.display()),
    ))
}

pub(crate) fn same_file(left: &Path, right: &Path) -> io::Result<bool> {
    Ok(resolve(left)?.canonicalize()? == resolve(right)?.canonicalize()?)
}

pub(crate) fn logical_sha256(path: &Path) -> AppResult<String> {
    use sha2::{Digest, Sha256};
    let mut reader = reader(path)?;
    let mut hash = Sha256::new();
    io::copy(&mut reader, &mut hash)?;
    Ok(hex::encode(hash.finalize()))
}

/// Used by snapshot readers which verify the physical file before and after reading.
pub(crate) fn reader_exact(path: &Path) -> io::Result<Box<dyn BufRead>> {
    let file = File::open(path)?;
    if is_compressed(path) {
        Ok(Box::new(BufReader::new(zstd::stream::read::Decoder::new(
            file,
        )?)))
    } else {
        Ok(Box::new(BufReader::new(file)))
    }
}

/// Pin an open representation. A concurrent compression cannot invalidate this reader.
pub(crate) fn reader(path: &Path) -> io::Result<Box<dyn BufRead>> {
    let mut last_error = None;
    for attempt in 0..4 {
        let opened =
            resolve(path).and_then(|physical| File::open(&physical).map(|file| (physical, file)));
        match opened {
            Ok((physical, file)) => {
                if is_compressed(&physical) {
                    return Ok(Box::new(BufReader::new(zstd::stream::read::Decoder::new(
                        file,
                    )?)));
                }
                return Ok(Box::new(BufReader::new(file)));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => last_error = Some(error),
            Err(error) => return Err(error),
        }
        if attempt < 3 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    Err(last_error.unwrap())
}

pub(crate) fn read_exact(path: &Path, cancel: Option<&AtomicBool>) -> AppResult<Vec<u8>> {
    ensure_not_cancelled(cancel)?;
    let mut reader = reader_exact(path)?;
    let mut bytes = Vec::new();
    let mut chunk = [0; 64 * 1024];
    loop {
        ensure_not_cancelled(cancel)?;
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        crate::operation_metrics::record(|c| c.read_bytes += n as u64);
        bytes.extend_from_slice(&chunk[..n]);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{backup, edit, family, logical_history, repair, rollout, sessions};
    use serde_json::{json, Value};

    const ID: &str = "01900000-0000-7000-8000-000000000053";

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> AppResult<Self> {
            let root = std::env::temp_dir().join(format!("cc-zstd-{}", repair::new_session_id()));
            fs::create_dir_all(root.join("sessions"))?;
            fs::create_dir_all(root.join("archived_sessions"))?;
            let db = rusqlite::Connection::open(root.join("state_5.sqlite"))?;
            db.execute_batch(
                "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT, source TEXT,
                cwd TEXT, title TEXT, first_user_message TEXT, model TEXT, reasoning_effort TEXT,
                tokens_used INTEGER, created_at INTEGER, updated_at INTEGER, archived INTEGER,
                archived_at INTEGER, git_branch TEXT, agent_nickname TEXT, agent_role TEXT
            );",
            )?;
            Ok(Self(root))
        }
        fn path(&self, id: &str) -> PathBuf {
            self.0
                .join("sessions")
                .join(format!("rollout-2026-10-01T12-00-00-{id}.jsonl"))
        }
        fn record(&self, path: &Path) -> AppResult<()> {
            let db = rusqlite::Connection::open(self.0.join("state_5.sqlite"))?;
            db.execute("INSERT OR REPLACE INTO threads
                (id,rollout_path,model_provider,source,cwd,title,first_user_message,tokens_used,created_at,updated_at,archived)
                VALUES (?1,?2,'openai','cli','/tmp/project','test','needle 中文',0,1,1,0)",
                rusqlite::params![ID, path.to_string_lossy()])?;
            fs::write(
                self.0.join("session_index.jsonl"),
                format!("{}\n", json!({"id":ID,"thread_name":"test"})),
            )?;
            Ok(())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn jsonl(records: &[Value]) -> Vec<u8> {
        records
            .iter()
            .map(|v| format!("{v}\n"))
            .collect::<String>()
            .into_bytes()
    }
    fn compress(path: &Path, bytes: &[u8]) -> AppResult<PathBuf> {
        let packed = plain_path(path).with_extension("jsonl.zst");
        fs::write(&packed, zstd::stream::encode_all(bytes, 3)?)?;
        Ok(packed)
    }
    fn legacy() -> Vec<u8> {
        jsonl(&[
            json!({"type":"session_meta","payload":{"id":ID,"source":"cli","model_provider":"openai","cwd":"/tmp/project"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"needle 中文"}]}}),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":321}}}}),
        ])
    }

    #[test]
    fn compressed_rollout_supports_logical_and_physical_database_paths() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        let bytes = legacy();
        fs::write(&plain, &bytes)?;
        let baseline =
            rollout::preview_session_range(None, plain.to_string_lossy().into_owned(), 0, 10)?;
        let packed = compress(&plain, &bytes)?;
        let packed_before = fs::read(&packed)?;
        fs::remove_file(&plain)?;
        for recorded in [&plain, &packed] {
            fixture.record(recorded)?;
            let codex = fixture.0.to_string_lossy().into_owned();
            let list = sessions::list_sessions(None, codex.clone(), None)?;
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].tokens_used, 321);
            assert_eq!(Path::new(&list[0].rollout_path), packed);
            let lock = family::FamilyLock::default();
            let overlays = family::get_session_family_overlay_with_lock(codex.clone(), &lock)?;
            assert_eq!(overlays[0].clone_state, "matches");
            assert!(repair::get_provider_sync_plan_with_lock(codex.clone(), &lock)?.is_empty());
            let diagnostic = repair::diagnose_codex_state(codex)?;
            assert_eq!(diagnostic.rollout_count, 1);
            assert!(diagnostic.orphan_in_threads.is_empty());
            assert!(diagnostic.orphan_in_index.is_empty());
            let preview = rollout::preview_session_range(
                None,
                recorded.to_string_lossy().into_owned(),
                0,
                10,
            )?;
            assert_eq!(
                serde_json::to_value(preview)?,
                serde_json::to_value(&baseline)?
            );
            let page = rollout::preview_session_page(
                "codex".into(),
                recorded.to_string_lossy().into_owned(),
                0,
                10,
                None,
            )?;
            assert_eq!(
                serde_json::to_value(page.events)?,
                serde_json::to_value(&baseline)?
            );
            assert!(page.capability.unwrap().blocked_reasons.is_empty());
        }
        assert_eq!(fs::read(&packed)?, packed_before);
        assert!(
            !plain.exists(),
            "read-only operations must not materialize source JSONL"
        );
        Ok(())
    }

    #[test]
    fn discovery_prefers_plain_and_reads_concatenated_zstd_frames() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        let bytes = legacy();
        let split = bytes.len() / 2;
        let mut frames = zstd::stream::encode_all(&bytes[..split], 3)?;
        frames.extend(zstd::stream::encode_all(&bytes[split..], 3)?);
        let packed = plain.with_extension("jsonl.zst");
        fs::write(&packed, frames)?;
        fs::write(&plain, b"plain wins\n")?;
        assert_eq!(resolve(&packed)?, plain);
        assert_eq!(family::scan_rollouts(&fixture.0)?, vec![plain.clone()]);
        let mut text = String::new();
        reader(&packed)?.read_to_string(&mut text)?;
        assert_eq!(text, "plain wins\n");
        fs::remove_file(&plain)?;
        assert_eq!(resolve(&plain)?, packed);
        assert_eq!(family::scan_rollouts(&fixture.0)?, vec![packed.clone()]);
        assert_eq!(read_exact(&packed, None)?, bytes);
        Ok(())
    }

    #[test]
    fn archived_compressed_rollouts_are_supplemented_without_database_rows() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture
            .0
            .join("archived_sessions")
            .join(fixture.path(ID).file_name().unwrap());
        compress(&plain, &legacy())?;
        let list = sessions::list_sessions(None, fixture.0.to_string_lossy().into_owned(), None)?;
        assert_eq!(list.len(), 1);
        assert!(list[0].archived);
        assert_eq!(list[0].tokens_used, 321);
        Ok(())
    }

    #[test]
    fn compressed_paginated_ancestry_uses_decoded_byte_boundaries() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let base = fixture.path(ID);
        let prefix = jsonl(&[
            json!({"type":"session_meta","ordinal":0,"payload":{"id":ID,"history_mode":"paginated"}}),
            json!({"type":"response_item","ordinal":1,"payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"继承 needle 中文"}]}}),
        ]);
        let mut base_bytes = prefix.clone();
        base_bytes.extend(jsonl(&[json!({"type":"event_msg","ordinal":2,"payload":{"type":"agent_message","message":"excluded tail"}})]));
        compress(&base, &base_bytes)?;
        let child = fixture.path("01900000-0000-7000-8000-000000000054");
        let records = vec![
            json!({"type":"session_meta","ordinal":0,"payload":{"id":"child","history_mode":"paginated","history_base":{"thread_id":ID,"end_ordinal_exclusive":2,"end_byte_offset":prefix.len()}}}),
            json!({"type":"event_msg","ordinal":2,"payload":{"type":"agent_message","message":"child answer"}}),
        ];
        compress(&child, &jsonl(&records))?;
        let history = logical_history::read(&child, None)?;
        assert!(history.paginated);
        assert_eq!(history.inherited_records, 1);
        assert_eq!(history.records.len(), 3);
        assert!(!serde_json::to_string(&history.records)?.contains("excluded tail"));
        // An uncompressed sibling is the same base, not an ambiguous second ancestor.
        fs::write(&base, &base_bytes)?;
        assert_eq!(
            logical_history::read(&child, None)?.records,
            history.records
        );
        let mut invalid = records;
        invalid[0]["payload"]["history_base"]["end_byte_offset"] = json!(prefix.len() - 1);
        compress(&child, &jsonl(&invalid))?;
        assert!(logical_history::read(&child, None).is_err());
        Ok(())
    }

    #[test]
    fn compressed_cache_validates_bytes_and_rejects_truncated_streams() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        let first = legacy();
        let next = String::from_utf8(first.clone())
            .unwrap()
            .replace("needle", "NEEDLE")
            .into_bytes();
        let packed = compress(&plain, &first)?;
        let before = edit::read_preview_snapshot(&plain, None)?;
        compress(&plain, &next)?;
        let after = edit::read_preview_snapshot(&plain, None)?;
        assert_ne!(before.hash, after.hash);
        assert!(after.lines.iter().any(|line| line.contains("NEEDLE")));
        assert!(matches!(
            read_exact(&packed, Some(&AtomicBool::new(true))),
            Err(crate::error::AppError::Cancelled)
        ));
        let mut broken = fs::read(&packed)?;
        broken.truncate(broken.len() - 3);
        fs::write(&packed, &broken)?;
        assert!(edit::read_preview_snapshot(&plain, None).is_err());
        assert!(materialize_for_write(&plain).is_err());
        assert_eq!(fs::read(&packed)?, broken);
        Ok(())
    }

    #[test]
    fn compressed_backup_materializes_only_the_backup_copy() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        let bytes = legacy();
        let packed = compress(&plain, &bytes)?;
        let original = fs::read(&packed)?;
        fixture.record(&plain)?;
        let backup_dir = fixture.0.join("backups").to_string_lossy().into_owned();
        let summary = backup::create_backup(
            None,
            fixture.0.to_string_lossy().into_owned(),
            None,
            backup_dir.clone(),
            vec![ID.into()],
            None,
            Some("compressed".into()),
            None,
        )?;
        let manifest: Value =
            serde_json::from_slice(&fs::read(Path::new(&summary.path).join("manifest.json"))?)?;
        let relative = manifest["sessions"][0]["rollout_relpath"].as_str().unwrap();
        assert!(relative.ends_with(".jsonl"));
        assert_eq!(fs::read(Path::new(&summary.path).join(relative))?, bytes);
        assert!(backup::verify_backup(backup_dir, summary.path)?.all_ok);
        assert_eq!(fs::read(&packed)?, original);
        assert!(!plain.exists());
        Ok(())
    }

    #[test]
    fn compressed_export_preserves_source_and_edits_materialize_on_demand() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        let bytes = legacy();
        let packed = compress(&plain, &bytes)?;
        let original = fs::read(&packed)?;
        fixture.record(&plain)?;
        let reports = crate::bundle::export_session_bundles(
            None,
            fixture.0.to_string_lossy().into_owned(),
            None,
            fixture.0.join("exports").to_string_lossy().into_owned(),
            vec![ID.into()],
            None,
            None,
            None,
        )?;
        assert!(reports[0].ok, "{:?}", reports[0].error);
        let exported = PathBuf::from(reports[0].bundle_path.as_ref().unwrap());
        let manifest: Value = serde_json::from_slice(&fs::read(exported.join("manifest.json"))?)?;
        let relative = manifest["rollout_relpath"].as_str().unwrap();
        assert_eq!(fs::read(exported.join("codex").join(relative))?, bytes);
        let capability = edit::inspect_edit_capability("codex", &packed.to_string_lossy())?;
        assert!(capability.blocked_reasons.is_empty());
        let edits = fixture.0.join("edits");
        edit::apply_edit_text(
            "codex",
            &packed.to_string_lossy(),
            ID,
            &edits.to_string_lossy(),
            1,
            "replacement",
            Some(&capability.revision),
        )?;
        assert!(!packed.exists());
        assert!(fs::read_to_string(&plain)?.contains("replacement"));
        let capability = edit::inspect_edit_capability("codex", &packed.to_string_lossy())?;
        edit::undo_last(
            "codex",
            &packed.to_string_lossy(),
            ID,
            &edits.to_string_lossy(),
            Some(&capability.revision),
        )?;
        assert_eq!(fs::read(&plain)?, bytes);
        let originals = walkdir::WalkDir::new(fixture.0.join("backups/rollout-materializations"))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| is_compressed(entry.path()))
            .collect::<Vec<_>>();
        assert_eq!(originals.len(), 1);
        assert_eq!(fs::read(originals[0].path())?, original);
        Ok(())
    }

    #[test]
    fn compressed_shared_children_protect_parent_edits_and_deletion() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        fs::write(&plain, legacy())?;
        fixture.record(&plain)?;
        let child = fixture.0.join("sessions/child.jsonl");
        compress(
            &child,
            &jsonl(&[
                json!({"type":"session_meta","payload":{"id":"child","history_base":{"thread_id":ID}}}),
            ]),
        )?;
        let capability = edit::inspect_edit_capability("codex", &plain.to_string_lossy())?;
        let plan = edit::plan_delete(
            "codex",
            &plain.to_string_lossy(),
            &[1],
            Some(&capability.revision),
        )?;
        assert!(!plan.blocked.is_empty());
        // Deletion scans official rollout names, while editing also protects legacy names.
        fs::rename(
            child.with_extension("jsonl.zst"),
            fixture.path("child").with_extension("jsonl.zst"),
        )?;
        assert!(sessions::delete_codex_artifacts(&fixture.0, ID).is_err());
        assert!(plain.is_file());
        Ok(())
    }

    #[test]
    fn delete_removes_both_representations_without_resurrecting_compressed_copy() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        fs::write(&plain, legacy())?;
        let packed = compress(&plain, &legacy())?;
        fixture.record(&plain)?;
        let outcome = sessions::delete_codex_artifacts(&fixture.0, ID)?;
        assert!(outcome.result.ok, "{:?}", outcome.result.error);
        assert!(!plain.exists());
        assert!(!packed.exists());
        Ok(())
    }

    fn family_for_compressed(fixture: &Fixture, packed: &Path) -> AppResult<()> {
        let mut store = crate::models::FamilyStore::default();
        family::ensure_family_for(
            &mut store,
            ID,
            "openai",
            &format!("sessions/{}", packed.file_name().unwrap().to_string_lossy()),
            "test",
        );
        family::save(&fixture.0, &store)
    }

    #[test]
    fn materialization_preserves_revisions_updates_paths_and_retires_stale_sibling() -> AppResult<()>
    {
        for existing_plain in [false, true] {
            let fixture = Fixture::new()?;
            let plain = fixture.path(ID);
            let bytes = legacy();
            let packed = compress(&plain, &bytes)?;
            let source = fs::read(&packed)?;
            fixture.record(&packed)?;
            family_for_compressed(&fixture, &packed)?;
            if existing_plain {
                fs::write(&plain, &bytes)?;
            }
            let before =
                edit::inspect_edit_capability("codex", &packed.to_string_lossy())?.revision;
            assert_eq!(materialize_for_write(&packed)?, plain);
            assert_eq!(fs::read(&plain)?, bytes);
            assert!(!packed.exists());
            assert_eq!(
                edit::inspect_edit_capability("codex", &packed.to_string_lossy())?.revision,
                before
            );
            let db = rusqlite::Connection::open(fixture.0.join("state_5.sqlite"))?;
            let selected: String =
                db.query_row("SELECT rollout_path FROM threads WHERE id=?1", [ID], |r| {
                    r.get(0)
                })?;
            assert_eq!(Path::new(&selected), plain);
            let store = family::load(&fixture.0)?;
            assert!(store
                .families
                .values()
                .flat_map(|f| &f.chain)
                .all(|b| b.rollout_relpath.ends_with(".jsonl")));
            let saved = walkdir::WalkDir::new(fixture.0.join("backups/rollout-materializations"))
                .into_iter()
                .filter_map(Result::ok)
                .find(|e| is_compressed(e.path()))
                .unwrap();
            assert_eq!(fs::read(saved.path())?, source);
        }
        Ok(())
    }

    #[test]
    fn materialization_rolls_back_files_family_and_sqlite_at_each_publication_stage(
    ) -> AppResult<()> {
        for stage in ["before_publish", "after_publish", "after_retire"] {
            let fixture = Fixture::new()?;
            let plain = fixture.path(ID);
            let packed = compress(&plain, &legacy())?;
            let source = fs::read(&packed)?;
            fixture.record(&packed)?;
            family_for_compressed(&fixture, &packed)?;
            let family_before = fs::read(crate::paths::family_store_path(&fixture.0))?;
            materialize::during(stage, || {
                Err(crate::error::AppError::Other(
                    "injected materialization failure".into(),
                ))
            });
            assert!(materialize_for_write(&packed).is_err(), "{stage}");
            assert_eq!(fs::read(&packed)?, source, "{stage}");
            assert!(!plain.exists(), "{stage}");
            assert_eq!(
                fs::read(crate::paths::family_store_path(&fixture.0))?,
                family_before,
                "{stage}"
            );
            let db = rusqlite::Connection::open(fixture.0.join("state_5.sqlite"))?;
            let selected: String =
                db.query_row("SELECT rollout_path FROM threads WHERE id=?1", [ID], |r| {
                    r.get(0)
                })?;
            assert_eq!(Path::new(&selected), packed, "{stage}");
        }
        Ok(())
    }

    #[test]
    fn materialization_respects_native_writer_lock_and_foreign_publication() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        let packed = compress(&plain, &legacy())?;
        let source = fs::read(&packed)?;
        let directory = fixture.0.join("thread-writer-locks");
        fs::create_dir_all(&directory)?;
        let native = File::create(directory.join(format!("{ID}.lock")))?;
        native.try_lock().unwrap();
        assert!(materialize_for_write(&packed)
            .unwrap_err()
            .to_string()
            .contains("EDIT_BUSY"));
        assert_eq!(fs::read(&packed)?, source);
        assert!(!plain.exists());
        drop(native);
        let foreign = plain.clone();
        materialize::during("before_publish", move || {
            fs::write(foreign, b"foreign writer")?;
            Ok(())
        });
        assert!(materialize_for_write(&packed).is_err());
        assert_eq!(fs::read(&plain)?, b"foreign writer");
        assert_eq!(fs::read(&packed)?, source);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn materialization_rejects_an_open_rollout_writer() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        let bytes = legacy();
        let packed = compress(&plain, &bytes)?;
        let source = fs::read(&packed)?;
        let writer = fs::OpenOptions::new().append(true).open(&packed)?;
        let error = materialize_for_write(&packed).unwrap_err().to_string();
        assert!(error.contains("EDIT_BUSY"), "{error}");
        assert_eq!(fs::read(&packed)?, source);
        assert!(!plain.exists());
        drop(writer);
        assert_eq!(materialize_for_write(&packed)?, plain);
        assert_eq!(fs::read(&plain)?, bytes);
        Ok(())
    }

    #[test]
    fn compressed_directory_move_materializes_and_keeps_family_metadata_current() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        let packed = compress(&plain, &legacy())?;
        fixture.record(&packed)?;
        family_for_compressed(&fixture, &packed)?;
        let target = fixture.0.join("new-project");
        fs::create_dir_all(&target)?;
        let result = sessions::move_session_cwd_with_lock(
            None,
            fixture.0.to_string_lossy().into_owned(),
            ID.into(),
            target.to_string_lossy().into_owned(),
            &family::FamilyLock::default(),
        )?;
        assert!(result.rollout_rewritten);
        assert!(!packed.exists());
        let meta = family::read_session_meta(&plain)?;
        let cwd = meta["payload"]["cwd"].as_str().unwrap();
        assert_eq!(Path::new(cwd).canonicalize()?, target.canonicalize()?);
        let store = family::load(&fixture.0)?;
        assert!(store
            .families
            .values()
            .flat_map(|f| &f.chain)
            .all(|b| b.rollout_relpath.ends_with(".jsonl")));
        Ok(())
    }

    #[test]
    fn compressed_preview_remains_valid_for_delete_and_snapshot_restore() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        let bytes = legacy();
        let packed = compress(&plain, &bytes)?;
        fixture.record(&packed)?;
        let edits = fixture.0.join("edits");
        let capability = edit::inspect_edit_capability("codex", &packed.to_string_lossy())?;
        let plan = edit::plan_delete(
            "codex",
            &packed.to_string_lossy(),
            &[1],
            Some(&capability.revision),
        )?;
        assert!(plan.blocked.is_empty());
        assert!(!plain.exists(), "planning must remain read-only");
        edit::apply_delete(
            "codex",
            &packed.to_string_lossy(),
            ID,
            &edits.to_string_lossy(),
            &[1],
            Some(&capability.revision),
        )?;
        assert!(!fs::read_to_string(&plain)?.contains("needle"));
        let history = edit::history(
            "codex",
            &packed.to_string_lossy(),
            ID,
            &edits.to_string_lossy(),
        )?;
        assert!(history.restore_available);
        let snapshot = &history.snapshots[0].name;
        edit::restore_snapshot(
            "codex",
            &packed.to_string_lossy(),
            ID,
            &edits.to_string_lossy(),
            snapshot,
            history.revision.as_deref(),
        )?;
        assert_eq!(fs::read(&plain)?, bytes);
        Ok(())
    }

    #[test]
    fn materialization_never_overwrites_source_recreated_during_rollback() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        let packed = compress(&plain, &legacy())?;
        let original = fs::read(&packed)?;
        fixture.record(&packed)?;
        let newer = zstd::stream::encode_all(
            String::from_utf8(legacy())
                .unwrap()
                .replace("needle", "newer")
                .as_bytes(),
            3,
        )?;
        let source = packed.clone();
        let foreign = newer.clone();
        materialize::during("after_retire", move || {
            fs::write(source, foreign)?;
            Ok(())
        });
        assert!(materialize_for_write(&packed).is_err());
        assert_eq!(fs::read(&packed)?, newer);
        assert!(!plain.exists());
        let saved = walkdir::WalkDir::new(fixture.0.join("backups/rollout-materializations"))
            .into_iter()
            .filter_map(Result::ok)
            .find(|entry| is_compressed(entry.path()))
            .unwrap();
        assert_eq!(fs::read(saved.path())?, original);
        Ok(())
    }

    #[test]
    fn stale_revisions_and_shared_children_block_before_materialization() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        let packed = compress(&plain, &legacy())?;
        let original = fs::read(&packed)?;
        fixture.record(&packed)?;
        let capability = edit::inspect_edit_capability("codex", &packed.to_string_lossy())?;
        let edits = fixture.0.join("edits");
        let stale = edit::apply_edit_text(
            "codex",
            &packed.to_string_lossy(),
            ID,
            &edits.to_string_lossy(),
            1,
            "new",
            Some("stale-version"),
        )
        .unwrap_err();
        assert!(stale.to_string().contains("EDIT_CONFLICT"));
        compress(
            &fixture.path("child"),
            &jsonl(&[
                json!({"type":"session_meta","payload":{"id":"child","history_base":{"thread_id":ID}}}),
            ]),
        )?;
        let shared = edit::apply_edit_text(
            "codex",
            &packed.to_string_lossy(),
            ID,
            &edits.to_string_lossy(),
            1,
            "new",
            Some(&capability.revision),
        )
        .unwrap_err();
        assert!(shared.to_string().contains("共享历史"));
        assert_eq!(fs::read(&packed)?, original);
        assert!(!plain.exists());
        assert!(!fixture.0.join("backups/rollout-materializations").exists());
        Ok(())
    }

    #[test]
    fn archiving_plain_and_compressed_siblings_does_not_resurrect_the_session() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        fs::write(&plain, legacy())?;
        compress(&plain, &legacy())?;
        fixture.record(&plain)?;
        sessions::set_archived_with_lock(
            None,
            fixture.0.to_string_lossy().into_owned(),
            ID.into(),
            true,
            &family::FamilyLock::default(),
        )?;
        assert!(family::scan_rollouts(&fixture.0)?.is_empty());
        assert_eq!(family::scan_archived_rollouts(&fixture.0)?.len(), 1);
        Ok(())
    }

    #[test]
    fn materialization_preserves_external_family_changes_and_rolls_back_its_log() -> AppResult<()> {
        let fixture = Fixture::new()?;
        let plain = fixture.path(ID);
        let packed = compress(&plain, &legacy())?;
        fixture.record(&packed)?;
        family_for_compressed(&fixture, &packed)?;
        let root = fixture.0.clone();
        materialize::during("after_publish", move || {
            let mut store = family::load(&root)?;
            store.families.values_mut().next().unwrap().title = "external update".into();
            family::save(&root, &store)
        });
        assert!(materialize_for_write(&packed).is_err());
        assert!(packed.exists());
        assert!(!plain.exists());
        assert_eq!(
            family::load(&fixture.0)?
                .families
                .values()
                .next()
                .unwrap()
                .title,
            "external update"
        );
        Ok(())
    }
}
