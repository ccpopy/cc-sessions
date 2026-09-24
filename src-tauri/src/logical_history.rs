//! Read-only Codex history. Lineage follows the alpha.16 rollout_lineage contract:
//! history_base names a physical rollout and bounds it by both byte offset and ordinal.
use crate::error::{ensure_not_cancelled, AppError, AppResult};
use crate::models::PreviewEvent;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

pub(crate) struct History {
    pub records: Vec<Value>,
    pub paginated: bool,
    pub inherited_records: usize,
}

fn invalid(reason: &str) -> AppError {
    AppError::Other(format!("[HISTORY_READ] {reason}；未返回不完整历史"))
}

pub(crate) fn read(path: &Path, cancel: Option<&AtomicBool>) -> AppResult<History> {
    let mut seen = HashSet::new();
    let root = path
        .ancestors()
        .find(|p| {
            matches!(
                p.file_name().and_then(|s| s.to_str()),
                Some("sessions" | "archived_sessions")
            )
        })
        .and_then(Path::parent);
    read_segment(path, root, None, &mut seen, cancel)
}

fn read_segment(
    path: &Path,
    root: Option<&Path>,
    end: Option<(u64, u64)>,
    seen: &mut HashSet<PathBuf>,
    cancel: Option<&AtomicBool>,
) -> AppResult<History> {
    ensure_not_cancelled(cancel)?;
    let physical = path.canonicalize()?;
    if !seen.insert(physical.clone()) || seen.len() > 128 {
        return Err(invalid("继承历史存在环或超过 128 层"));
    }
    let bytes = std::fs::read(&physical)?;
    crate::operation_metrics::record(|c| c.read_bytes += bytes.len() as u64);
    let length = end.map(|e| e.1).unwrap_or(bytes.len() as u64);
    if length > bytes.len() as u64
        || (length > 0 && length != bytes.len() as u64 && bytes[length as usize - 1] != b'\n')
    {
        return Err(invalid("继承字节边界越界或未落在完整记录边界"));
    }
    let text = std::str::from_utf8(&bytes[..length as usize])
        .map_err(|_| invalid("历史不是有效 UTF-8"))?;
    let mut records = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        ensure_not_cancelled(cancel)?;
        crate::operation_metrics::record(|c| c.parsed_lines += 1);
        let record: Value =
            serde_json::from_str(line).map_err(|_| invalid("历史包含不完整或无法解析的记录"))?;
        records.push(record);
    }
    resolve_records(records, root, end, seen, cancel)
}

pub(crate) fn from_records(
    path: &Path,
    records: Vec<Value>,
    cancel: Option<&AtomicBool>,
) -> AppResult<History> {
    let root = path
        .ancestors()
        .find(|p| {
            matches!(
                p.file_name().and_then(|s| s.to_str()),
                Some("sessions" | "archived_sessions")
            )
        })
        .and_then(Path::parent);
    let mut seen = HashSet::from([path.canonicalize()?]);
    resolve_records(records, root, None, &mut seen, cancel)
}

fn resolve_records(
    mut records: Vec<Value>,
    root: Option<&Path>,
    end: Option<(u64, u64)>,
    seen: &mut HashSet<PathBuf>,
    cancel: Option<&AtomicBool>,
) -> AppResult<History> {
    let Some(meta) = records.first().filter(|v| v["type"] == "session_meta") else {
        // Legacy read-only exports may lack metadata.
        if end.is_some() {
            return Err(invalid("继承源缺少会话元数据"));
        }
        return Ok(History {
            records,
            paginated: false,
            inherited_records: 0,
        });
    };
    let paginated = meta["payload"]["history_mode"] == "paginated";
    let base = meta["payload"]
        .get("history_base")
        .filter(|v| !v.is_null())
        .cloned();
    let start = base
        .as_ref()
        .and_then(|v| v["end_ordinal_exclusive"].as_u64())
        .unwrap_or(0);
    if let Some((ordinal, _)) = end {
        if !paginated
            || ordinal == 0
            || records
                .iter()
                .enumerate()
                .skip(1)
                .any(|(i, v)| v["ordinal"].as_u64().unwrap_or(start + i as u64) >= ordinal)
        {
            return Err(invalid("继承 ordinal 与字节边界不一致"));
        }
        let last = records
            .last()
            .and_then(|v| v["ordinal"].as_u64())
            .unwrap_or(start + records.len() as u64 - 1);
        if last.checked_add(1) != Some(ordinal) {
            return Err(invalid("继承范围记录缺失"));
        }
    }
    let Some(base) = base else {
        return Ok(History {
            records,
            paginated,
            inherited_records: 0,
        });
    };
    if !paginated {
        return Err(invalid("非分页历史携带继承指针"));
    }
    let id = base["thread_id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .ok_or_else(|| invalid("继承 rollout ID 无效"))?;
    let ordinal = base["end_ordinal_exclusive"]
        .as_u64()
        .ok_or_else(|| invalid("继承 ordinal 缺失"))?;
    let offset = base["end_byte_offset"]
        .as_u64()
        .ok_or_else(|| invalid("继承字节边界缺失"))?;
    let root = root.ok_or_else(|| invalid("无法确定继承历史所属的数据根"))?;
    let mut candidates = Vec::new();
    for directory in ["sessions", "archived_sessions"] {
        let managed = root.join(directory);
        if !managed.is_dir() {
            continue;
        }
        let managed = managed.canonicalize()?;
        for entry in walkdir::WalkDir::new(&managed).follow_links(false) {
            ensure_not_cancelled(cancel)?;
            let entry = entry.map_err(|_| invalid("无法遍历继承历史目录"))?;
            let name = entry.file_name().to_string_lossy();
            if entry.file_type().is_file()
                && name.starts_with("rollout-")
                && name.ends_with(&format!("-{id}.jsonl"))
            {
                let resolved = entry.path().canonicalize()?;
                if !resolved.starts_with(&managed) {
                    return Err(invalid("继承源逃出数据根"));
                }
                candidates.push(resolved);
            }
        }
    }
    if candidates.len() != 1 {
        return Err(invalid("继承源缺失、存在歧义或为尚未支持的压缩文件"));
    }
    let inherited = read_segment(
        &candidates[0],
        Some(root),
        Some((ordinal, offset)),
        seen,
        cancel,
    )?;
    let prefix: Vec<_> = inherited
        .records
        .into_iter()
        .filter(|v| v["type"] != "session_meta")
        .collect();
    let count = prefix.len();
    records.splice(1..1, prefix);
    Ok(History {
        records,
        paginated,
        inherited_records: count,
    })
}

pub(crate) fn item_key(value: &Value) -> Option<(String, String, String)> {
    if value["type"] != "event_msg" || value["payload"]["type"] != "item_completed" {
        return None;
    }
    let p = &value["payload"];
    Some((
        p["thread_id"].as_str()?.into(),
        p["turn_id"].as_str()?.into(),
        p["item"]["id"].as_str()?.into(),
    ))
}

/// Latest snapshot at the first logical position, matching native item ordering.
pub(crate) fn latest(records: &[Value]) -> Vec<(usize, &Value)> {
    let mut positions = HashMap::new();
    let mut out = Vec::new();
    for (i, record) in records.iter().enumerate() {
        if let Some(key) = item_key(record) {
            if let Some(&position) = positions.get(&key) {
                let (first, _) = out[position];
                out[position] = (first, record);
                continue;
            }
            positions.insert(key, out.len());
        }
        out.push((i, record));
    }
    out
}

impl History {
    pub fn raw_events(&self) -> Vec<PreviewEvent> {
        self.records
            .iter()
            .enumerate()
            .map(|(i, v)| crate::rollout::classify_history(i, v.clone(), self.paginated))
            .collect()
    }
    pub fn events(&self) -> Vec<PreviewEvent> {
        latest(&self.records)
            .into_iter()
            .map(|(index, v)| crate::rollout::classify_history(index, v.clone(), self.paginated))
            .collect()
    }
}
