use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFingerprint {
    len: u64,
    sha256: [u8; 32],
}

pub fn fingerprint(path: &Path) -> AppResult<FileFingerprint> {
    let mut file = File::open(path)?;
    fingerprint_open_file(&mut file)
}

fn fingerprint_open_file(file: &mut File) -> AppResult<FileFingerprint> {
    let mut hasher = Sha256::new();
    let len = std::io::copy(file, &mut hasher)?;
    Ok(FileFingerprint {
        len,
        sha256: hasher.finalize().into(),
    })
}

/// Write a same-directory temporary file and replace `path` only if its bytes still match
/// `expected`. This detects concurrent appends by Codex/Claude before the destructive replace.
pub fn replace_with_writer_if_unchanged(
    path: &Path,
    expected: &FileFingerprint,
    writer: impl FnOnce(&mut File) -> AppResult<()>,
) -> AppResult<()> {
    replace_with_writer(path, Some(expected), writer)
}

pub fn create_with_writer_if_absent(
    path: &Path,
    writer: impl FnOnce(&mut File) -> AppResult<()>,
) -> AppResult<()> {
    replace_with_writer(path, None, writer)
}

fn replace_with_writer(
    path: &Path,
    expected: Option<&FileFingerprint>,
    writer: impl FnOnce(&mut File) -> AppResult<()>,
) -> AppResult<()> {
    let (temp_path, mut temp_file) = create_unique_temp(path)?;
    let write_result = writer(&mut temp_file).and_then(|()| {
        temp_file.flush()?;
        temp_file.sync_all()?;
        Ok(())
    });
    drop(temp_file);
    if let Err(error) = write_result {
        return Err(cleanup_after_error(&temp_path, error));
    }

    match expected {
        Some(expected) => {
            if let Err(error) = commit_existing_if_unchanged(&temp_path, path, expected) {
                return Err(cleanup_after_error(&temp_path, error));
            }
        }
        None => match path.try_exists() {
            Ok(false) => {}
            Ok(true) => {
                return Err(cleanup_after_error(
                    &temp_path,
                    AppError::Other(format!(
                        "文件在创建期间已由其他进程生成，已拒绝覆盖: {}",
                        path.to_string_lossy()
                    )),
                ))
            }
            Err(error) => return Err(cleanup_after_error(&temp_path, error.into())),
        },
    }

    if expected.is_none() {
        if let Err(error) = replace_file_atomically(&temp_path, path, false) {
            return Err(cleanup_after_error(&temp_path, error.into()));
        }
    }
    sync_parent(path)?;
    Ok(())
}

fn changed_during_operation(path: &Path) -> AppError {
    AppError::Other(format!(
        "文件在操作期间发生变化，已拒绝覆盖，请停止对应会话后重试: {}",
        path.to_string_lossy()
    ))
}

#[cfg(windows)]
fn commit_existing_if_unchanged(
    temp_path: &Path,
    final_path: &Path,
    expected: &FileFingerprint,
) -> AppResult<()> {
    use std::os::windows::fs::OpenOptionsExt;

    // Exclude FILE_SHARE_WRITE while keeping FILE_SHARE_DELETE. Windows therefore refuses this
    // open when another process already owns a writable handle, and refuses new writable opens
    // until MoveFileEx has committed the replacement. The fingerprint check and replacement are
    // consequently one write-exclusion window instead of two racy path operations.
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    let mut guarded = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(final_path)
        .map_err(|error| {
            AppError::Other(format!(
                "文件正在被其他进程写入，已拒绝覆盖，请停止对应会话后重试: {} ({error})",
                final_path.to_string_lossy()
            ))
        })?;
    if &fingerprint_open_file(&mut guarded)? != expected {
        return Err(changed_during_operation(final_path));
    }
    let backup_path = loop {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let file_name = final_path.file_name().ok_or_else(|| {
            AppError::Path(format!(
                "待替换文件缺少文件名: {}",
                final_path.to_string_lossy()
            ))
        })?;
        let mut backup_name = file_name.to_os_string();
        backup_name.push(format!(
            ".{}.{}.compare-swap.old",
            std::process::id(),
            sequence
        ));
        let candidate = final_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .join(backup_name);
        match replace_file_atomically(final_path, &candidate, false) {
            Ok(()) => break candidate,
            Err(error) if matches!(error.raw_os_error(), Some(80) | Some(183)) => {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
    };

    if let Err(install_error) = replace_file_atomically(temp_path, final_path, false) {
        let rollback = replace_file_atomically(&backup_path, final_path, false);
        drop(guarded);
        return match rollback {
            Ok(()) => Err(install_error.into()),
            Err(rollback_error) => Err(AppError::Other(format!(
                "安装替换文件失败: {install_error}; 恢复旧文件也失败，旧数据保留在 {}: {rollback_error}",
                backup_path.to_string_lossy()
            ))),
        };
    }
    drop(guarded);
    fs::remove_file(&backup_path).map_err(|error| {
        AppError::Other(format!(
            "文件已替换，但清理旧快照失败 {}: {error}",
            backup_path.to_string_lossy()
        ))
    })?;
    Ok(())
}

#[cfg(not(windows))]
fn commit_existing_if_unchanged(
    temp_path: &Path,
    final_path: &Path,
    expected: &FileFingerprint,
) -> AppResult<()> {
    if &fingerprint(final_path)? != expected {
        return Err(changed_during_operation(final_path));
    }
    replace_file_atomically(temp_path, final_path, true)?;
    Ok(())
}

fn create_unique_temp(path: &Path) -> AppResult<(PathBuf, File)> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        AppError::Path(format!("待替换文件缺少文件名: {}", path.to_string_lossy()))
    })?;
    loop {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = file_name.to_os_string();
        temp_name.push(format!(".{}.{}.rewrite.tmp", std::process::id(), sequence));
        let temp_path = parent.join(temp_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn cleanup_after_error(temp_path: &Path, original: AppError) -> AppError {
    match fs::remove_file(temp_path) {
        Ok(()) => original,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => original,
        Err(error) => AppError::Other(format!(
            "{original}; 清理临时文件失败 {}: {error}",
            temp_path.to_string_lossy()
        )),
    }
}

#[cfg(windows)]
fn move_file_api_path(path: &Path) -> std::io::Result<Vec<u16>> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    const BACKSLASH: u16 = b'\\' as u16;
    let absolute = std::path::absolute(path)?;
    let raw = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
    let is_verbatim = raw.starts_with(&[BACKSLASH, BACKSLASH, b'?' as u16, BACKSLASH]);
    let is_device = raw.starts_with(&[BACKSLASH, BACKSLASH, b'.' as u16, BACKSLASH]);

    let mut wide = if is_verbatim || is_device {
        raw
    } else if raw.starts_with(&[BACKSLASH, BACKSLASH]) {
        let mut prefixed = OsStr::new(r"\\?\UNC\").encode_wide().collect::<Vec<_>>();
        prefixed.extend_from_slice(&raw[2..]);
        prefixed
    } else {
        let mut prefixed = OsStr::new(r"\\?\").encode_wide().collect::<Vec<_>>();
        prefixed.extend_from_slice(&raw);
        prefixed
    };
    wide.push(0);
    Ok(wide)
}

#[cfg(windows)]
fn replace_file_atomically(
    temp_path: &Path,
    final_path: &Path,
    replace_existing: bool,
) -> std::io::Result<()> {
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }

    // Raw Win32 APIs still enforce the legacy MAX_PATH limit unless absolute paths use the
    // extended-length namespace. Claude project directory encoding can easily push a transcript
    // beyond that boundary after a cwd move, even though Rust's normal filesystem APIs can open it.
    let existing = move_file_api_path(temp_path)?;
    let replacement = move_file_api_path(final_path)?;
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace_existing {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    let moved = unsafe { MoveFileExW(existing.as_ptr(), replacement.as_ptr(), flags) };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file_atomically(
    temp_path: &Path,
    final_path: &Path,
    replace_existing: bool,
) -> std::io::Result<()> {
    if replace_existing {
        fs::rename(temp_path, final_path)
    } else {
        fs::hard_link(temp_path, final_path)?;
        fs::remove_file(temp_path)
    }
}

fn sync_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_to_replace_a_file_changed_after_snapshot() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "cc-session-manager-atomic-race-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&root)?;
        let path = root.join("history.jsonl");
        fs::write(&path, b"before\n")?;
        let expected = fingerprint(&path)?;

        let error = replace_with_writer_if_unchanged(&path, &expected, |temp| {
            temp.write_all(b"replacement\n")?;
            fs::write(&path, b"before\nconcurrent\n")?;
            Ok(())
        })
        .expect_err("concurrent write must abort replacement");

        assert!(error.to_string().contains("发生变化"));
        assert_eq!(fs::read(&path)?, b"before\nconcurrent\n");
        let temp_files = fs::read_dir(&root)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|entry| entry.file_name().to_string_lossy().contains("rewrite.tmp"))
            .count();
        assert_eq!(temp_files, 0);
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn absent_create_commit_never_replaces_a_concurrent_destination() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "cc-session-manager-atomic-create-race-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&root)?;
        let temp = root.join("pending.tmp");
        let destination = root.join("session.jsonl");
        fs::write(&temp, b"pending\n")?;
        fs::write(&destination, b"concurrent\n")?;

        replace_file_atomically(&temp, &destination, false)
            .expect_err("an absent-only commit must fail when the destination already exists");

        assert_eq!(fs::read(&destination)?, b"concurrent\n");
        assert_eq!(fs::read(&temp)?, b"pending\n");
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn refuses_replacement_while_another_writable_handle_is_open() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "cc-session-manager-atomic-open-writer-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&root)?;
        let path = root.join("history.jsonl");
        fs::write(&path, b"before\n")?;
        let expected = fingerprint(&path)?;
        let writer = OpenOptions::new().append(true).open(&path)?;

        let error = replace_with_writer_if_unchanged(&path, &expected, |temp| {
            temp.write_all(b"replacement\n")?;
            Ok(())
        })
        .expect_err("an open writable handle must abort replacement");

        assert!(error.to_string().contains("正在被其他进程写入"));
        assert_eq!(fs::read(&path)?, b"before\n");
        drop(writer);
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replaces_an_unchanged_existing_file_with_write_exclusion() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "cc-session-manager-atomic-success-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&root)?;
        let path = root.join("history.jsonl");
        fs::write(&path, b"before\n")?;
        let expected = fingerprint(&path)?;

        replace_with_writer_if_unchanged(&path, &expected, |temp| {
            temp.write_all(b"replacement\n")?;
            Ok(())
        })?;

        assert_eq!(fs::read(&path)?, b"replacement\n");
        let leftovers = fs::read_dir(&root)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.contains("rewrite.tmp") || name.contains("compare-swap.old")
            })
            .count();
        assert_eq!(leftovers, 0);
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replaces_an_unchanged_file_beyond_legacy_max_path() -> AppResult<()> {
        let cleanup_root = std::env::temp_dir().join(format!(
            "cc-session-manager-atomic-long-path-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let mut root = cleanup_root.clone();
        while root.to_string_lossy().encode_utf16().count() < 280 {
            root.push("claude-project-directory-segment-0123456789");
        }
        fs::create_dir_all(&root)?;
        let path = root.join("session.jsonl");
        fs::write(&path, b"before\n")?;
        let expected = fingerprint(&path)?;

        replace_with_writer_if_unchanged(&path, &expected, |temp| {
            temp.write_all(b"replacement\n")?;
            Ok(())
        })?;

        assert_eq!(fs::read(&path)?, b"replacement\n");
        fs::remove_dir_all(cleanup_root).ok();
        Ok(())
    }
}
