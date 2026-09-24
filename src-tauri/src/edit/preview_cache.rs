//! Display-only immutable parses. Mutations always use load_file and full hashes.
use super::*;
use crate::error::ensure_not_cancelled;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::sync::{atomic::AtomicBool, Arc, Mutex, OnceLock};
use std::time::SystemTime;

const MAX_ENTRIES: usize = 8;
const MAX_BYTES: usize = 96 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    identity: u64,
    len: u64,
    modified: SystemTime,
}

fn stamp(path: &Path) -> AppResult<Stamp> {
    let handle = same_file::Handle::from_path(path)?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    handle.hash(&mut hash);
    let metadata = fs::metadata(path)?;
    Ok(Stamp {
        identity: hash.finish(),
        len: metadata.len(),
        modified: metadata.modified()?,
    })
}

struct Entry {
    stamp: Stamp,
    loaded: Arc<LoadedFile>,
    bytes: usize,
    used: u64,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<PathBuf, Entry>,
    bytes: usize,
    clock: u64,
}

impl Cache {
    fn insert(&mut self, path: PathBuf, mut entry: Entry) {
        if let Some(old) = self.entries.remove(&path) {
            self.bytes -= old.bytes;
        }
        if entry.bytes > MAX_BYTES {
            return;
        }
        while self.entries.len() >= MAX_ENTRIES || self.bytes + entry.bytes > MAX_BYTES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .unwrap()
                .0
                .clone();
            self.bytes -= self.entries.remove(&oldest).unwrap().bytes;
        }
        self.clock += 1;
        entry.used = self.clock;
        self.bytes += entry.bytes;
        self.entries.insert(path, entry);
    }
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

// Conservative accounting of owned buffers and JSON containers, excluding shared callers.
fn value_bytes(value: &Value) -> usize {
    std::mem::size_of::<Value>()
        + match value {
            Value::String(s) => s.capacity(),
            Value::Array(a) => {
                a.capacity() * std::mem::size_of::<Value>()
                    + a.iter().map(value_bytes).sum::<usize>()
            }
            Value::Object(o) => o
                .iter()
                .map(|(k, v)| k.capacity() + 128 + value_bytes(v))
                .sum(),
            _ => 0,
        }
}

fn retained_bytes(loaded: &LoadedFile) -> usize {
    loaded.lines.capacity() * std::mem::size_of::<String>()
        + loaded.lines.iter().map(String::capacity).sum::<usize>()
        + loaded.parsed.capacity() * std::mem::size_of::<Option<Value>>()
        + loaded
            .parsed
            .iter()
            .flatten()
            .map(value_bytes)
            .sum::<usize>()
}

pub(super) fn read(path: &Path, cancel: Option<&AtomicBool>) -> AppResult<Arc<LoadedFile>> {
    ensure_not_cancelled(cancel)?;
    let path = path.canonicalize()?;
    for _ in 0..3 {
        let before = stamp(&path)?;
        let prior = {
            let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
            cache.clock += 1;
            let used = cache.clock;
            cache.entries.get_mut(&path).map(|e| {
                e.used = used;
                (e.stamp.clone(), e.loaded.clone())
            })
        };
        if let Some((previous, loaded)) = &prior {
            if *previous == before {
                crate::operation_metrics::record(|c| c.cache_hits += 1);
                return Ok(loaded.clone());
            }
        }
        let mut file = fs::File::open(&path)?;
        let mut raw = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            ensure_not_cancelled(cancel)?;
            let n = file.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            crate::operation_metrics::record(|c| c.read_bytes += n as u64);
            raw.extend_from_slice(&chunk[..n]);
        }
        if before != stamp(&path)? || before.len != raw.len() as u64 {
            continue;
        }
        let hash = sha_hex(&raw);
        let append = prior.as_ref().filter(|(previous, loaded)| {
            previous.identity == before.identity
                && previous.len < before.len
                && loaded.trailing_newline
                && sha_hex(&raw[..previous.len as usize]) == loaded.hash
        });
        let offset = append.map(|(s, _)| s.len as usize).unwrap_or(0);
        let text = std::str::from_utf8(&raw[offset..])
            .map_err(|_| AppError::Other("会话文件不是有效的 UTF-8".into()))?;
        let trailing_newline = text.ends_with('\n');
        let mut loaded = append
            .map(|(_, old)| (**old).clone())
            .unwrap_or_else(|| LoadedFile {
                lines: Vec::new(),
                parsed: Vec::new(),
                trailing_newline: false,
                hash: String::new(),
            });
        for line in text.split_terminator('\n') {
            ensure_not_cancelled(cancel)?;
            crate::operation_metrics::record(|c| c.parsed_lines += 1);
            loaded.lines.push(line.to_owned());
            loaded.parsed.push(serde_json::from_str(line.trim()).ok());
        }
        loaded.trailing_newline = trailing_newline;
        loaded.hash = hash;
        // A writer may have changed the file during JSON parsing. Do not cache that observation.
        if before != stamp(&path)? {
            continue;
        }
        let bytes = retained_bytes(&loaded);
        let loaded = Arc::new(loaded);
        cache().lock().unwrap_or_else(|e| e.into_inner()).insert(
            path.clone(),
            Entry {
                stamp: before,
                loaded: loaded.clone(),
                bytes,
                used: 0,
            },
        );
        return Ok(loaded);
    }
    Err(AppError::Other(
        "[EDIT_CONFLICT] 会话正在持续更新，请稍后刷新预览".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "isolated fixed-size performance measurement; writes only an explicitly supplied report"]
    fn r07_fixed_preview_measurement() {
        let root = super::super::tests::temp_dir("preview-measurement");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("rollout.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "{}", serde_json::json!({"type":"session_meta","payload":{"id":"fixture","history_mode":"paginated"}})).unwrap();
        for i in 0..6000 {
            writeln!(file, "{}", serde_json::json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"fixture","turn_id":format!("turn-{i}"),"item":{"type":"UserMessage","id":format!("item-{i}"),"content":[{"type":"text","text":"fixture ".repeat(64)}]}}})).unwrap();
        }
        drop(file);
        let mut measurements = Vec::new();
        for (name, offset, limit) in [
            ("first_page", 0, 80),
            ("second_page", 80, 80),
            ("full_history", 0, usize::MAX),
        ] {
            let start = std::time::Instant::now();
            let (page, work) = crate::operation_metrics::measured(|| {
                codex_preview_page(path.to_str().unwrap(), offset, limit, None).unwrap()
            });
            if name == "second_page" {
                assert_eq!(work.parsed_lines, 0);
                assert!(work.cache_hits > 0);
            }
            measurements.push(serde_json::json!({"scenario":name,"elapsed_us":start.elapsed().as_micros(),"events":page.events.len(),"counters":work}));
        }
        let start = std::time::Instant::now();
        assert!(read(&path, Some(&AtomicBool::new(true))).is_err());
        let cancelled_us = start.elapsed().as_micros();
        let retained = retained_bytes(&read(&path, None).unwrap());
        #[cfg(windows)]
        let peak: Option<u64> = {
            use std::os::windows::process::CommandExt;
            std::process::Command::new("powershell.exe")
                .creation_flags(0x08000000)
                .args([
                    "-NoProfile",
                    "-Command",
                    &format!("(Get-Process -Id {}).PeakWorkingSet64", std::process::id()),
                ])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.trim().parse().ok())
        };
        #[cfg(not(windows))]
        let peak: Option<u64> = None;
        let report = serde_json::json!({"records":6001,"file_bytes":fs::metadata(&path).unwrap().len(),"measurements":measurements,"cancelled_us":cancelled_us,"cache_retained_bytes":retained,"cache_budget_bytes":MAX_BYTES,"process_lifetime_peak_working_set_bytes":peak,"scope":"synthetic paginated preview; no native projection database; desktop latency not measured"});
        fs::write(
            std::env::var("CC_PREVIEW_PERF_REPORT").unwrap(),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn r07_append_rewrite_replace_cancel_and_bounds() {
        let root = super::super::tests::temp_dir("cache-invalidation");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("rollout.jsonl");
        fs::write(&path, "{\"a\":1}\n").unwrap();
        let first = read(&path, None).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"b\":2}\n")
            .unwrap();
        let (appended, work) = crate::operation_metrics::measured(|| read(&path, None).unwrap());
        assert_eq!(work.parsed_lines, 1);
        assert_eq!(appended.parsed.len(), 2);
        assert_eq!(first.parsed.len(), 1);
        // Growing rewrites must not be mistaken for an append.
        fs::write(&path, "{\"changed\":true}\n{\"c\":3}\n").unwrap();
        assert_eq!(
            read(&path, None).unwrap().parsed[0].as_ref().unwrap()["changed"],
            true
        );
        let replacement = root.join("replacement");
        fs::write(&replacement, "{\"new\":true}\n").unwrap();
        fs::remove_file(&path).unwrap();
        fs::rename(replacement, &path).unwrap();
        assert_eq!(
            read(&path, None).unwrap().parsed[0].as_ref().unwrap()["new"],
            true
        );
        assert!(read(&path, Some(&AtomicBool::new(true))).is_err());
        let mut bounded = Cache::default();
        for i in 0..20 {
            bounded.insert(
                PathBuf::from(i.to_string()),
                Entry {
                    stamp: stamp(&path).unwrap(),
                    loaded: first.clone(),
                    bytes: MAX_BYTES / 3,
                    used: 0,
                },
            );
            assert!(bounded.bytes <= MAX_BYTES && bounded.entries.len() <= MAX_ENTRIES);
        }
        fs::remove_dir_all(root).unwrap();
    }
}
