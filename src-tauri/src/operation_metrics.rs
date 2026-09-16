//! Opt-in local workload counters. No paths, session IDs, or conversation data are recorded.
use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Default, serde::Serialize)]
pub(crate) struct Counters {
    pub read_bytes: u64,
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
}

impl Measurement {
    pub(crate) fn start(operation: &'static str) -> Self {
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
            output,
            operation,
            started: Instant::now(),
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
            "operation": self.operation,
            "elapsed_us": self.started.elapsed().as_micros(),
            "counters": counters,
        });
        let write = || -> std::io::Result<()> {
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

#[cfg(test)]
pub(crate) fn measured<T>(run: impl FnOnce() -> T) -> (T, Counters) {
    CURRENT.with(|current| *current.borrow_mut() = Some(Counters::default()));
    let result = run();
    let counters = CURRENT.with(|current| current.borrow_mut().take().unwrap());
    (result, counters)
}
