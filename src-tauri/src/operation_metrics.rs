//! Opt-in local workload counters. No paths, session IDs, or conversation data are recorded.
use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Default, serde::Serialize)]
pub(crate) struct Counters {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub parsed_lines: u64,
    pub parsed_files: u64,
    pub sql_rows: u64,
    pub cache_hits: u64,
    pub fsync_count: u64,
    pub fsync_us: u64,
    pub ipc_bytes: u64,
    pub spool_bytes: u64,
    pub snapshot_bytes: u64,
    pub thread_table_scans: u64,
    pub index_scans: u64,
    pub local_thread_queries: u64,
    pub local_index_checks: u64,
    pub hash_bytes: u64,
    pub observation_windows: u64,
    pub lock_wait_us: u64,
    pub lock_hold_us: u64,
}

pub(crate) fn sync_file(file: &std::fs::File) -> std::io::Result<()> {
    let started = Instant::now();
    let result = file.sync_all();
    record(|c| {
        c.fsync_count += 1;
        c.fsync_us += started.elapsed().as_micros() as u64;
    });
    result
}

thread_local! {
    static CURRENT: RefCell<Option<Counters>> = const { RefCell::new(None) };
}

pub(crate) fn record(update: impl FnOnce(&mut Counters)) {
    CURRENT.with(|current| {
        if let Some(counters) = current.borrow_mut().as_mut() {
            update(counters);
        }
    });
}

pub(crate) struct Measurement {
    output: Option<PathBuf>,
    operation: &'static str,
    started: Instant,
    source_kind: &'static str,
    operation_id: String,
}

impl Measurement {
    pub(crate) fn response(&self, value: &impl serde::Serialize) {
        if self.output.is_some() {
            record_response(value);
        }
    }

    pub(crate) fn for_session(operation: &'static str, provider: &str, locator: &str) -> Self {
        let source = if provider == "cursor" {
            match crate::cursor_sessions::decode_locator(locator) {
                Ok(locator) if locator.is_agent() => "cursor_cli",
                Ok(_) => "cursor_ide",
                Err(_) => "cursor",
            }
        } else {
            provider
        };
        Self::for_source(operation, source)
    }
    pub(crate) fn start(operation: &'static str) -> Self {
        Self::for_source(operation, "codex")
    }

    pub(crate) fn for_source(operation: &'static str, source: &str) -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let output = std::env::var_os("CC_SESSIONS_PROFILE")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from);
        let output = output.filter(|_| {
            CURRENT.with(|current| {
                let mut current = current.borrow_mut();
                if current.is_some() {
                    false
                } else {
                    *current = Some(Counters::default());
                    true
                }
            })
        });
        Self {
            operation,
            started: Instant::now(),
            source_kind: match source {
                "codex" => "codex",
                "claude" => "claude",
                "opencode" => "opencode",
                "cursor" => "cursor",
                "cursor_ide" => "cursor_ide",
                "cursor_cli" => "cursor_cli",
                _ => "unknown",
            },
            operation_id: if output.is_some() {
                format!(
                    "{}-{}-{}",
                    std::process::id(),
                    chrono::Utc::now().timestamp_micros(),
                    SEQUENCE.fetch_add(1, Ordering::Relaxed)
                )
            } else {
                String::new()
            },
            output,
        }
    }
}

impl Drop for Measurement {
    fn drop(&mut self) {
        let Some(output) = &self.output else {
            return;
        };
        let counters = CURRENT.with(|current| current.borrow_mut().take());
        let row = serde_json::json!({
            "schema_version": 2,
            "operation_id": self.operation_id,
            "operation": self.operation,
            "source_kind": self.source_kind,
            "thread_id": format!("{:?}", std::thread::current().id()),
            "scope": "instrumented_work_on_current_thread",
            "elapsed_us": self.started.elapsed().as_micros(),
            "counters": counters,
        });
        let write = || -> std::io::Result<()> {
            static OUTPUT_LOCK: Mutex<()> = Mutex::new(());
            let _guard = OUTPUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let mut options = std::fs::OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            writeln!(options.open(output)?, "{row}")
        };
        if let Err(error) = write() {
            eprintln!("写入本地性能计数失败: {error}");
        }
    }
}

fn record_response(value: &impl serde::Serialize) {
    if CURRENT.with(|current| current.borrow().is_some()) {
        struct ByteCount(u64);
        impl Write for ByteCount {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 += bytes.len() as u64;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut counter = ByteCount(0);
        if serde_json::to_writer(&mut counter, value).is_ok() {
            record(|c| c.ipc_bytes += counter.0);
        }
    }
}

#[cfg(test)]
pub(crate) fn measured<T>(run: impl FnOnce() -> T) -> (T, Counters) {
    CURRENT.with(|current| *current.borrow_mut() = Some(Counters::default()));
    let result = run();
    let counters = CURRENT.with(|current| current.borrow_mut().take().unwrap());
    (result, counters)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_bytes_count_encoded_json_without_retaining_payloads() {
        let value = serde_json::json!({"markdown":"中文\n\"quoted\"", "messages":[1, 2, 3]});
        let (_, counters) = measured(|| record_response(&value));
        assert_eq!(
            counters.ipc_bytes,
            serde_json::to_vec(&value).unwrap().len() as u64
        );
    }

    #[test]
    fn worker_counters_are_isolated() {
        let (_, counters) = measured(|| {
            record(|c| c.parsed_lines += 2);
            let worker = std::thread::spawn(|| measured(|| record(|c| c.parsed_lines += 7)));
            assert_eq!(worker.join().unwrap().1.parsed_lines, 7);
        });
        assert_eq!(counters.parsed_lines, 2);
    }
}
