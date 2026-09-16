//! Display-only summaries. Mutation target validation always bypasses this cache.
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use crate::error::AppResult;
use crate::models::SessionSummary;

const PARSER_VERSION: u32 = 1;
const MAX_ENTRIES: usize = 2048;
const MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(PartialEq, Eq)]
pub(super) struct Stamp {
    identity: u64,
    len: u64,
    modified: SystemTime,
    version: u32,
}

pub(super) fn stamp(path: &Path) -> AppResult<Stamp> {
    let identity = same_file::Handle::from_path(path)?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    identity.hash(&mut hash);
    let metadata = std::fs::metadata(path)?;
    Ok(Stamp {
        identity: hash.finish(),
        len: metadata.len(),
        modified: metadata.modified()?,
        version: PARSER_VERSION,
    })
}

struct Entry {
    stamp: Stamp,
    summary: Option<SessionSummary>,
    bytes: usize,
    used: u64,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<PathBuf, Entry>,
    bytes: usize,
    clock: u64,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

pub(super) fn get(key: &Path, stamp: &Stamp) -> Option<Option<SessionSummary>> {
    let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
    cache.clock += 1;
    let used = cache.clock;
    let entry = cache.entries.get_mut(key)?;
    if entry.stamp != *stamp {
        return None;
    }
    entry.used = used;
    crate::operation_metrics::record(|c| c.cache_hits += 1);
    Some(entry.summary.clone())
}

pub(super) fn insert(key: PathBuf, stamp: Stamp, summary: Option<SessionSummary>) {
    let bytes = serde_json::to_vec(&summary)
        .map(|v| v.len())
        .unwrap_or(MAX_BYTES + 1);
    let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = cache.entries.remove(&key) {
        cache.bytes -= old.bytes;
    }
    if bytes > MAX_BYTES {
        return;
    }
    while cache.entries.len() >= MAX_ENTRIES || cache.bytes + bytes > MAX_BYTES {
        let Some(oldest) = cache
            .entries
            .iter()
            .min_by_key(|(_, e)| e.used)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        let old = cache.entries.remove(&oldest).unwrap();
        cache.bytes -= old.bytes;
    }
    cache.clock += 1;
    let used = cache.clock;
    cache.bytes += bytes;
    cache.entries.insert(
        key,
        Entry {
            stamp,
            summary,
            bytes,
            used,
        },
    );
}

pub(super) fn invalidate(path: &Path) {
    let Ok(key) = path.canonicalize() else { return };
    let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = cache.entries.remove(&key) {
        cache.bytes -= old.bytes;
    }
}
