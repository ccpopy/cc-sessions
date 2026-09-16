//! Ordered, source-directory-scoped locks shared by desktop, CLI and WebUI writers.
use crate::error::{AppError, AppResult};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, TryLockError, Weak};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct FamilyLock;

static LOCAL_LOCKS: OnceLock<Mutex<BTreeMap<String, Weak<Mutex<()>>>>> = OnceLock::new();

fn local_lock(key: &str) -> Arc<Mutex<()>> {
    let mut locks = LOCAL_LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
        return lock;
    }
    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key.to_owned(), Arc::downgrade(&lock));
    lock
}

fn source_key(root: &Path) -> AppResult<String> {
    let root = crate::path_safety::coordination_path(root)?;
    Ok(hex::encode(Sha256::digest(
        root.as_os_str().as_encoded_bytes(),
    )))
}

#[cfg(test)]
pub(crate) fn test_mutex(root: &Path) -> Arc<Mutex<()>> {
    local_lock(&source_key(root).unwrap())
}

pub fn with_lock<R>(
    lock: &FamilyLock,
    root: &Path,
    f: impl FnOnce(()) -> AppResult<R>,
) -> AppResult<R> {
    with_roots(lock, &[root.to_path_buf()], f)
}

pub fn with_roots<R>(
    _lock: &FamilyLock,
    roots: &[PathBuf],
    f: impl FnOnce(()) -> AppResult<R>,
) -> AppResult<R> {
    with_timeout(roots, Duration::from_secs(30), f)
}

fn with_timeout<R>(
    roots: &[PathBuf],
    timeout: Duration,
    f: impl FnOnce(()) -> AppResult<R>,
) -> AppResult<R> {
    let started = Instant::now();
    let mut keys = roots
        .iter()
        .map(|root| source_key(root))
        .collect::<AppResult<Vec<_>>>()?;
    keys.sort();
    keys.dedup();
    let locks: Vec<_> = keys.iter().map(|key| local_lock(key)).collect();
    let mut guards = Vec::with_capacity(locks.len());
    let mut processes = Vec::with_capacity(locks.len());
    for (key, lock) in keys.iter().zip(&locks) {
        loop {
            match lock.try_lock() {
                Ok(guard) => {
                    guards.push(guard);
                    break;
                }
                Err(TryLockError::Poisoned(error)) => {
                    guards.push(error.into_inner());
                    break;
                }
                Err(TryLockError::WouldBlock) => {
                    if started.elapsed() >= timeout {
                        return Err(AppError::Other(
                            "等待 CC Sessions 写锁超时，请稍后重试".into(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        processes.push(acquire_cross_process_family_lock(
            key,
            timeout.saturating_sub(started.elapsed()),
        )?);
    }
    crate::operation_metrics::record(|c| c.lock_wait_us += started.elapsed().as_micros() as u64);
    let held = Instant::now();
    let result = f(());
    crate::operation_metrics::record(|c| c.lock_hold_us += held.elapsed().as_micros() as u64);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round2_source_lock_child() -> AppResult<()> {
        let Some(root) = std::env::var_os("CC_SOURCE_LOCK_TEST_ROOT") else {
            return Ok(());
        };
        let result = with_timeout(&[PathBuf::from(root)], Duration::from_millis(100), |_| {
            Ok(())
        });
        if std::env::var_os("CC_SOURCE_LOCK_TEST_BLOCKED").is_some() {
            assert!(result.unwrap_err().to_string().contains("写锁超时"));
        } else {
            result?;
        }
        Ok(())
    }

    #[test]
    fn round2_source_locks_coordinate_processes_and_aliases() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "cc-source-lock-{}",
            crate::repair::new_session_id()
        ));
        std::fs::create_dir_all(&root)?;
        let missing = root.join("source");
        let key_before = source_key(&missing)?;
        std::fs::create_dir_all(&missing)?;
        assert_eq!(source_key(&missing)?, key_before);
        assert_eq!(source_key(&missing.canonicalize()?)?, key_before);
        let alias = missing.join("..").join("source");
        assert_eq!(source_key(&missing)?, source_key(&alias)?);
        #[cfg(windows)]
        assert_eq!(
            source_key(&missing)?,
            source_key(&PathBuf::from(missing.to_string_lossy().to_uppercase()))?
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&missing, root.join("alias"))?;
            assert_eq!(source_key(&missing)?, source_key(&root.join("alias"))?);
        }
        let child = |path: &Path, blocked: bool| -> AppResult<()> {
            let mut command = std::process::Command::new(std::env::current_exe()?);
            command
                .args([
                    "--exact",
                    "family::locking::tests::round2_source_lock_child",
                ])
                .env("CC_SOURCE_LOCK_TEST_ROOT", path)
                .env_remove("CC_SOURCE_LOCK_TEST_BLOCKED");
            if blocked {
                command.env("CC_SOURCE_LOCK_TEST_BLOCKED", "1");
            }
            let output = command.output()?;
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(())
        };
        with_timeout(
            std::slice::from_ref(&missing),
            Duration::from_secs(5),
            |_| {
                child(&alias, true)?;
                child(&root.join("independent"), false)
            },
        )?;
        child(&missing, false)?;
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn round2_multi_root_order_and_in_process_timeout() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "cc-source-order-{}",
            crate::repair::new_session_id()
        ));
        let a = root.join("a");
        let b = root.join("b");
        let mutex = test_mutex(&a);
        let guard = mutex.lock().unwrap();
        let timeout = with_timeout(std::slice::from_ref(&a), Duration::from_millis(30), |_| {
            Ok(())
        });
        assert!(timeout.unwrap_err().to_string().contains("写锁超时"));
        drop(guard);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut workers = Vec::new();
        for roots in [vec![a.clone(), b.clone(), a], vec![b, root.join("a")]] {
            let barrier = barrier.clone();
            let entered = entered.clone();
            workers.push(std::thread::spawn(move || -> AppResult<()> {
                barrier.wait();
                for _ in 0..10 {
                    with_timeout(&roots, Duration::from_secs(5), |_| {
                        assert!(!entered.swap(true, std::sync::atomic::Ordering::SeqCst));
                        std::thread::yield_now();
                        entered.store(false, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    })?;
                }
                Ok(())
            }));
        }
        for worker in workers {
            worker.join().unwrap()?;
        }
        assert!(!root.exists(), "locking must not create a data source");
        Ok(())
    }
}

#[cfg(windows)]
struct CrossProcessFamilyGuard {
    handle: *mut std::ffi::c_void,
}

#[cfg(windows)]
impl Drop for CrossProcessFamilyGuard {
    fn drop(&mut self) {
        #[link(name = "kernel32")]
        extern "system" {
            fn ReleaseMutex(handle: *mut std::ffi::c_void) -> i32;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        // The handle is created and acquired by `acquire_cross_process_family_lock` and remains
        // owned by this guard until drop.
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
fn acquire_cross_process_family_lock(
    key: &str,
    timeout: std::time::Duration,
) -> AppResult<CrossProcessFamilyGuard> {
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(
            attributes: *const std::ffi::c_void,
            initial_owner: i32,
            name: *const u16,
        ) -> *mut std::ffi::c_void;
        fn WaitForSingleObject(handle: *mut std::ffi::c_void, milliseconds: u32) -> u32;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_ABANDONED: u32 = 0x80;
    const WAIT_TIMEOUT: u32 = 0x102;
    let name = format!("Local\\cc-session-manager-source-v2-{key}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // The UTF-16 name is NUL terminated and remains alive for the call. A non-null handle is
    // closed either on an acquisition error or by the returned guard.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(AppError::Other(format!(
            "创建 family 跨进程锁失败: {}",
            std::io::Error::last_os_error()
        )));
    }
    let wait =
        unsafe { WaitForSingleObject(handle, timeout.as_millis().min(u32::MAX as u128) as u32) };
    if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
        let error = if wait == WAIT_TIMEOUT {
            "等待 CC Sessions 写锁超时（30 秒），请稍后重试".to_string()
        } else {
            format!(
                "获取 family 跨进程锁失败: {}",
                std::io::Error::last_os_error()
            )
        };
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(AppError::Other(error));
    }
    Ok(CrossProcessFamilyGuard { handle })
}

#[cfg(unix)]
pub(super) struct CrossProcessFamilyGuard(fs::File);

#[cfg(unix)]
fn acquire_cross_process_family_lock(
    key: &str,
    timeout: std::time::Duration,
) -> AppResult<CrossProcessFamilyGuard> {
    let root = dirs::cache_dir()
        .ok_or_else(|| AppError::Path("无法确定本地锁目录".into()))?
        .join("cc-sessions");
    fs::create_dir_all(&root)?;
    acquire_family_file_lock(&root.join(format!("source-v2-{key}.lock")), timeout)
}

#[cfg(unix)]
pub(super) fn acquire_family_file_lock(
    path: &Path,
    timeout: std::time::Duration,
) -> AppResult<CrossProcessFamilyGuard> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let started = std::time::Instant::now();
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            file.set_len(0)?;
            writeln!(file, "{}", std::process::id())?;
            return Ok(CrossProcessFamilyGuard(file));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Err(error.into());
        }
        if started.elapsed() >= timeout {
            let owner = fs::read_to_string(path).unwrap_or_default();
            return Err(AppError::Other(format!(
                "等待 CC Sessions 写锁超时（进程 {}）",
                owner.trim()
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[cfg(unix)]
impl Drop for CrossProcessFamilyGuard {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
