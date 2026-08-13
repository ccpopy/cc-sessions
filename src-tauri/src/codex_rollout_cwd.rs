use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::atomic_file::{self, FileFingerprint};
use crate::error::{AppError, AppResult};

/// Tracks the cwd contract shared by Core, Desktop import/export, and repair.
///
/// A rollout's latest non-empty `turn_context.cwd` is the current Core working directory. Older
/// rollouts may expose only `session_meta.cwd`, which remains the fallback. Keeping this rule in one
/// place prevents repair/export from undoing a move merely because the two records temporarily
/// disagree.
#[derive(Debug, Default)]
pub(crate) struct EffectiveCwdTracker {
    session_meta_cwds: HashMap<String, String>,
    latest_turn_cwd: Option<String>,
}

impl EffectiveCwdTracker {
    pub(crate) fn observe(&mut self, value: &Value) {
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                let Some(payload) = value.get("payload").and_then(Value::as_object) else {
                    return;
                };
                let Some(id) = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                else {
                    return;
                };
                let Some(cwd) = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|cwd| !cwd.is_empty())
                else {
                    return;
                };
                self.session_meta_cwds
                    .entry(id.to_string())
                    .or_insert_with(|| cwd.to_string());
            }
            Some("turn_context") => {
                if let Some(cwd) = turn_context_cwd(value) {
                    self.latest_turn_cwd = Some(cwd.to_string());
                }
            }
            _ => {}
        }
    }

    pub(crate) fn effective_for(&self, session_id: &str) -> Option<String> {
        self.latest_turn_cwd
            .clone()
            .or_else(|| self.session_meta_cwds.get(session_id).cloned())
    }
}

#[derive(Debug)]
struct CwdInspection {
    source_fingerprint: FileFingerprint,
    latest_turn_line: Option<usize>,
    needs_rewrite: bool,
}

fn json_line_bounds(line: &[u8]) -> (usize, usize) {
    if line.ends_with(b"\r\n") {
        (line.len() - 2, 2)
    } else if line.ends_with(b"\n") {
        (line.len() - 1, 1)
    } else {
        (line.len(), 0)
    }
}

fn matching_session_meta(value: &Value, session_id: &str) -> bool {
    value.get("type").and_then(Value::as_str) == Some("session_meta")
        && value
            .get("payload")
            .and_then(Value::as_object)
            .and_then(|payload| payload.get("id"))
            .and_then(Value::as_str)
            == Some(session_id)
}

fn session_meta_cwd(value: &Value) -> Option<&str> {
    value
        .get("payload")
        .and_then(Value::as_object)
        .and_then(|payload| payload.get("cwd"))
        .and_then(Value::as_str)
}

fn turn_context_cwd(value: &Value) -> Option<&str> {
    if value.get("type").and_then(Value::as_str) != Some("turn_context") {
        return None;
    }
    value
        .get("payload")
        .unwrap_or(value)
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
}

fn inspect(
    source: &Path,
    session_id: &str,
    target_cwd: Option<&str>,
    expected_source_sha256: Option<&str>,
) -> AppResult<(CwdInspection, EffectiveCwdTracker)> {
    let source_fingerprint = atomic_file::fingerprint(source)?;
    let mut reader = BufReader::new(File::open(source)?);
    let mut tracker = EffectiveCwdTracker::default();
    let mut latest_turn_line = None;
    let mut matching_meta_found = false;
    let mut meta_needs_rewrite = false;
    let mut latest_turn_cwd = None;
    let mut source_hasher = Sha256::new();
    let mut line_number = 0usize;

    loop {
        let mut line = Vec::new();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        source_hasher.update(&line);
        let (json_end, _) = json_line_bounds(&line);
        let Ok(value) = serde_json::from_slice::<Value>(&line[..json_end]) else {
            line_number += 1;
            continue;
        };
        tracker.observe(&value);
        if matching_session_meta(&value, session_id) {
            matching_meta_found = true;
            if let Some(target_cwd) = target_cwd {
                meta_needs_rewrite |= session_meta_cwd(&value) != Some(target_cwd);
            }
        }
        if let Some(cwd) = turn_context_cwd(&value) {
            latest_turn_line = Some(line_number);
            latest_turn_cwd = Some(cwd.to_string());
        }
        line_number += 1;
    }

    if !matching_meta_found {
        return Err(AppError::Other(format!(
            "Codex rollout 缺少会话 {session_id} 的 session_meta: {}",
            source.to_string_lossy()
        )));
    }
    if let Some(expected) = expected_source_sha256 {
        let actual = hex::encode(source_hasher.finalize());
        if actual != expected {
            return Err(AppError::Other(format!(
                "Bundle 源文件在导入期间发生变化，已拒绝提交: expected={expected} actual={actual} source={}",
                source.to_string_lossy()
            )));
        }
    }
    if atomic_file::fingerprint(source)? != source_fingerprint {
        return Err(AppError::Other(format!(
            "Codex rollout 在读取 cwd 期间发生变化: {}",
            source.to_string_lossy()
        )));
    }

    let turn_needs_rewrite = target_cwd.is_some_and(|target| {
        latest_turn_line.is_some() && latest_turn_cwd.as_deref() != Some(target)
    });
    Ok((
        CwdInspection {
            source_fingerprint,
            latest_turn_line,
            needs_rewrite: meta_needs_rewrite || turn_needs_rewrite,
        },
        tracker,
    ))
}

/// Read the rollout's current cwd using the same rule consumed by repair and rewrite operations.
pub(crate) fn read_effective_cwd(source: &Path, session_id: &str) -> AppResult<Option<String>> {
    let (_, tracker) = inspect(source, session_id, None, None)?;
    Ok(tracker.effective_for(session_id))
}

fn write_from_inspection(
    source: &Path,
    destination: &mut dyn Write,
    session_id: &str,
    target_cwd: &str,
    inspection: &CwdInspection,
) -> AppResult<()> {
    let mut reader = BufReader::new(File::open(source)?);
    let mut line_number = 0usize;
    loop {
        let mut line = Vec::new();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let (json_end, ending_len) = json_line_bounds(&line);
        let Ok(mut value) = serde_json::from_slice::<Value>(&line[..json_end]) else {
            destination.write_all(&line)?;
            line_number += 1;
            continue;
        };

        let rewrite_meta = matching_session_meta(&value, session_id)
            && session_meta_cwd(&value) != Some(target_cwd);
        let rewrite_turn = inspection.latest_turn_line == Some(line_number)
            && turn_context_cwd(&value) != Some(target_cwd);
        if rewrite_meta {
            let payload = value
                .get_mut("payload")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| {
                    AppError::Other(format!(
                        "Codex rollout 的 session_meta.payload 不是对象: {}",
                        source.to_string_lossy()
                    ))
                })?;
            payload.insert("cwd".to_string(), Value::String(target_cwd.to_string()));
        }
        if rewrite_turn {
            let payload = if value.get("payload").is_some() {
                value.get_mut("payload").and_then(Value::as_object_mut)
            } else {
                value.as_object_mut()
            }
            .ok_or_else(|| {
                AppError::Other(format!(
                    "Codex rollout 的 turn_context.payload 不是对象: {}",
                    source.to_string_lossy()
                ))
            })?;
            payload.insert("cwd".to_string(), Value::String(target_cwd.to_string()));
        }

        if rewrite_meta || rewrite_turn {
            destination.write_all(&serde_json::to_vec(&value)?)?;
            if ending_len > 0 {
                destination.write_all(&line[line.len() - ending_len..])?;
            }
        } else {
            destination.write_all(&line)?;
        }
        line_number += 1;
    }
    destination.flush()?;
    if atomic_file::fingerprint(source)? != inspection.source_fingerprint {
        return Err(AppError::Other(format!(
            "Codex rollout 在重写 cwd 期间发生变化: {}",
            source.to_string_lossy()
        )));
    }
    Ok(())
}

/// Copy a rollout while normalizing both current cwd records in the destination only.
pub(crate) fn copy_with_effective_cwd(
    source: &Path,
    destination: &mut dyn Write,
    session_id: &str,
    target_cwd: &str,
    expected_source_sha256: Option<&str>,
) -> AppResult<bool> {
    let (inspection, _) = inspect(source, session_id, Some(target_cwd), expected_source_sha256)?;
    write_from_inspection(source, destination, session_id, target_cwd, &inspection)?;
    Ok(inspection.needs_rewrite)
}

/// Atomically update the current session metadata and the last effective turn context in place.
pub(crate) fn rewrite_effective_cwd(
    source: &Path,
    session_id: &str,
    target_cwd: &str,
) -> AppResult<bool> {
    let (inspection, _) = inspect(source, session_id, Some(target_cwd), None)?;
    if !inspection.needs_rewrite {
        return Ok(false);
    }
    atomic_file::replace_with_writer_if_unchanged(
        source,
        &inspection.source_fingerprint,
        |destination| {
            write_from_inspection(source, destination, session_id, target_cwd, &inspection)
        },
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn effective_cwd_prefers_and_rewrites_only_latest_turn_context() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "ccsm-rollout-cwd-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&root)?;
        let rollout = root.join("rollout-thread-1.jsonl");
        let lines = [
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-1","cwd":"F:\\meta-old"}}),
            serde_json::json!({"type":"turn_context","payload":{"cwd":"F:\\historical"}}),
            serde_json::json!({"type":"turn_context","payload":{"cwd":"F:\\current"}}),
        ];
        fs::write(
            &rollout,
            format!(
                "{}\n",
                lines
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )?;

        assert_eq!(
            read_effective_cwd(&rollout, "thread-1")?,
            Some(r"F:\current".to_string())
        );
        assert!(rewrite_effective_cwd(&rollout, "thread-1", r"F:\moved")?);
        let rewritten = fs::read_to_string(&rollout)?
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(rewritten[0]["payload"]["cwd"], r"F:\moved");
        assert_eq!(rewritten[1]["payload"]["cwd"], r"F:\historical");
        assert_eq!(rewritten[2]["payload"]["cwd"], r"F:\moved");
        assert_eq!(
            read_effective_cwd(&rollout, "thread-1")?,
            Some(r"F:\moved".to_string())
        );

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn copy_normalizes_matching_metadata_without_touching_ancestor_or_history() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "ccsm-rollout-cwd-copy-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&root)?;
        let source = root.join("source.jsonl");
        let destination = root.join("destination.jsonl");
        let lines = [
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-1","cwd":"F:\\old-a"}}),
            serde_json::json!({"type":"session_meta","payload":{"id":"ancestor","cwd":"F:\\ancestor"}}),
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-1","cwd":"F:\\old-b"}}),
            serde_json::json!({"type":"turn_context","payload":{"cwd":"F:\\historical"}}),
            serde_json::json!({"type":"turn_context","payload":{"cwd":"F:\\current"}}),
        ];
        fs::write(
            &source,
            format!(
                "{}\n",
                lines
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )?;
        let mut output = File::create(&destination)?;

        assert!(copy_with_effective_cwd(
            &source,
            &mut output,
            "thread-1",
            r"F:\moved",
            None,
        )?);
        drop(output);
        let copied = fs::read_to_string(&destination)?
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(copied[0]["payload"]["cwd"], r"F:\moved");
        assert_eq!(copied[1]["payload"]["cwd"], r"F:\ancestor");
        assert_eq!(copied[2]["payload"]["cwd"], r"F:\moved");
        assert_eq!(copied[3]["payload"]["cwd"], r"F:\historical");
        assert_eq!(copied[4]["payload"]["cwd"], r"F:\moved");

        fs::remove_dir_all(root).ok();
        Ok(())
    }
}
