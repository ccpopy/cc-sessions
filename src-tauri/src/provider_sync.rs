//! Background provider synchronization with pollable progress snapshots.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use crate::error::{AppError, AppResult};
use crate::family::FamilyLock;
use crate::models::{ProviderSyncStart, ProviderSyncStatus, SwitchStrategy};
use crate::repair;

#[derive(Clone)]
struct ProviderSyncJob {
    id: u64,
    status: Arc<Mutex<ProviderSyncStatus>>,
}

struct ProviderSyncManager {
    next_id: AtomicU64,
    active: Mutex<Option<ProviderSyncJob>>,
}

impl ProviderSyncManager {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            active: Mutex::new(None),
        }
    }

    fn start(
        &self,
        codex_dir: String,
        strategy: SwitchStrategy,
        dry_run: bool,
        lock: Arc<FamilyLock>,
    ) -> AppResult<ProviderSyncStart> {
        if codex_dir.trim().is_empty() {
            return Err(AppError::Other("Codex 目录不能为空".to_string()));
        }

        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(job) = active.as_ref() {
            let status = job.status.lock().unwrap_or_else(|error| error.into_inner());
            if status.state == "running" {
                return Err(AppError::Other(
                    "已有 provider 批量同步正在运行".to_string(),
                ));
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let job = ProviderSyncJob {
            id,
            status: Arc::new(Mutex::new(ProviderSyncStatus {
                job_id: id,
                state: "running".to_string(),
                current_provider: None,
                completed: 0,
                total: 0,
                succeeded: 0,
                failed: 0,
                current_session_id: None,
                reports: Vec::new(),
                error: None,
            })),
        };
        let worker_job = job.clone();
        thread::Builder::new()
            .name("cc-sessions-provider-sync".to_string())
            .spawn(move || run_job(worker_job, codex_dir, strategy, dry_run, lock))
            .map_err(|error| AppError::Other(format!("无法启动 provider 同步任务: {error}")))?;
        *active = Some(job);
        Ok(ProviderSyncStart { job_id: id })
    }

    fn status(&self, job_id: u64) -> AppResult<ProviderSyncStatus> {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let job = active
            .as_ref()
            .filter(|job| job.id == job_id)
            .ok_or_else(|| AppError::NotFound(format!("provider 同步任务 {job_id}")))?;
        let status = job
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        Ok(status)
    }

    fn active(&self) -> Option<ProviderSyncStart> {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let job = active.as_ref()?;
        let status = job.status.lock().unwrap_or_else(|error| error.into_inner());
        (status.state == "running").then_some(ProviderSyncStart { job_id: job.id })
    }

    #[cfg(test)]
    fn reset_for_tests(&self) {
        *self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }
}

fn manager() -> &'static ProviderSyncManager {
    static MANAGER: OnceLock<ProviderSyncManager> = OnceLock::new();
    MANAGER.get_or_init(ProviderSyncManager::new)
}

pub fn start_provider_sync(
    codex_dir: String,
    strategy: SwitchStrategy,
    dry_run: bool,
    lock: Arc<FamilyLock>,
) -> AppResult<ProviderSyncStart> {
    manager().start(codex_dir, strategy, dry_run, lock)
}

pub fn provider_sync_status(job_id: u64) -> AppResult<ProviderSyncStatus> {
    manager().status(job_id)
}

pub fn active_provider_sync() -> AppResult<Option<ProviderSyncStart>> {
    Ok(manager().active())
}

fn run_job(
    job: ProviderSyncJob,
    codex_dir: String,
    strategy: SwitchStrategy,
    dry_run: bool,
    lock: Arc<FamilyLock>,
) {
    let result = repair::batch_clone_for_current_provider_with_progress(
        codex_dir,
        strategy,
        dry_run,
        &lock,
        |completed, total, current_session_id, report| {
            let mut status = job.status.lock().unwrap_or_else(|error| error.into_inner());
            status.completed = completed;
            status.total = total;
            status.current_session_id = current_session_id;
            if let Some(report) = report {
                if status.current_provider.is_none() {
                    status.current_provider = Some(report.new_provider.clone());
                }
                if report.ok {
                    status.succeeded += 1;
                } else {
                    status.failed += 1;
                }
                status.reports.push(report);
            }
        },
    );

    let mut status = job.status.lock().unwrap_or_else(|error| error.into_inner());
    status.current_session_id = None;
    match result {
        Ok(reports) => {
            status.completed = reports.len();
            status.total = reports.len();
            status.reports = reports;
            status.succeeded = status.reports.iter().filter(|report| report.ok).count();
            status.failed = status.reports.len().saturating_sub(status.succeeded);
            status.state = "completed".to_string();
        }
        Err(error) => {
            status.state = "failed".to_string();
            status.error = Some(error.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn temp_codex_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ))
    }

    #[test]
    fn background_sync_exposes_a_terminal_progress_snapshot() -> AppResult<()> {
        manager().reset_for_tests();
        let codex = temp_codex_dir("cc-session-manager-provider-sync-job-test");
        fs::create_dir_all(&codex)?;
        fs::write(codex.join("config.toml"), "model_provider = \"openai\"\n")?;
        let started = start_provider_sync(
            codex.to_string_lossy().into_owned(),
            SwitchStrategy::Scatter,
            true,
            Arc::new(FamilyLock::default()),
        )?;

        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            let status = provider_sync_status(started.job_id)?;
            if status.state != "running" {
                break status;
            }
            assert!(Instant::now() < deadline, "provider 同步任务未按时结束");
            thread::sleep(Duration::from_millis(10));
        };

        fs::remove_dir_all(&codex).ok();
        manager().reset_for_tests();
        assert_eq!(status.state, "completed");
        assert_eq!(status.completed, 0);
        assert_eq!(status.total, 0);
        assert!(status.reports.is_empty());
        assert!(active_provider_sync()?.is_none());
        Ok(())
    }
}
