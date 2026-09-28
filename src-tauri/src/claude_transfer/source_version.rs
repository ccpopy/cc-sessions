//! Version the complete move set, including directory entries and companion membership.
use super::*;
use std::collections::BTreeMap;

pub(super) type Version = BTreeMap<PathBuf, Option<String>>;

pub(super) fn capture(path: &Path) -> AppResult<Version> {
    let mut version = BTreeMap::new();
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(|e| AppError::Other(e.to_string()))?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if crate::path_safety::metadata_is_link_or_reparse(&metadata)
            || (!metadata.is_dir() && !metadata.is_file())
        {
            return Err(AppError::Path("Claude 迁移资产包含链接或特殊文件".into()));
        }
        version.insert(
            entry.path().strip_prefix(path).unwrap().to_path_buf(),
            if metadata.is_file() {
                Some(crate::atomic_file::fingerprint(entry.path())?.sha256_hex())
            } else {
                None
            },
        );
    }
    Ok(version)
}

pub(super) fn check(path: &Path, expected: &Version) -> AppResult<()> {
    if capture(path)? != *expected {
        return Err(conflict());
    }
    Ok(())
}

pub(super) fn conflict() -> AppError {
    AppError::Other(
        "[MOVE_CONFLICT] Claude 会话资产在准备期间变化，未用旧暂存覆盖；请停止写入后重新迁移"
            .into(),
    )
}

pub(super) struct Guard {
    // Hold read handles through publication. On Unix they also prove lsof coverage.
    files: Vec<(PathBuf, File)>,
}

pub(super) enum Location {
    Source,
    Backup,
    Destination,
}

impl Guard {
    pub(super) fn recheck(&self) -> AppResult<()> {
        #[cfg(unix)]
        {
            native_activity::ensure_stopped()?;
            native_activity::ensure_no_writers(&self.files)?;
        }
        #[cfg(not(unix))]
        let _ = &self.files;
        Ok(())
    }
}

/// Windows rejects writable handles; Unix requires Claude to be closed and probes
/// all assets for writers at each publication boundary. Neither is a native session
/// lock protocol: retain source backups, and never infer inactivity from timestamps.
pub(super) fn guard(
    artifacts: &[MoveArtifact],
    versions: &[Version],
    location: Location,
) -> AppResult<Guard> {
    let mut handles = Vec::new();
    for (asset, version) in artifacts.iter().zip(versions) {
        for (relative, hash) in version {
            if hash.is_none() {
                continue;
            }
            let root = match location {
                Location::Source => &asset.source,
                Location::Backup => &asset.backup,
                Location::Destination => &asset.destination,
            };
            let path = if relative.as_os_str().is_empty() {
                root.clone()
            } else {
                root.join(relative)
            };
            let mut options = fs::OpenOptions::new();
            options.read(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.share_mode(0x1 | 0x4);
            }
            let file = options.open(&path).map_err(|error| {
                #[cfg(windows)]
                if matches!(error.raw_os_error(), Some(32 | 33)) {
                    return AppError::Other(
                        "[SESSION_BUSY] Claude 会话文件被写入进程占用，请停止对应会话后重试".into(),
                    );
                }
                AppError::Other(format!("无法打开 Claude 迁移资产进行占用检查：{error}"))
            })?;
            handles.push((path, file));
        }
    }
    let guard = Guard { files: handles };
    guard.recheck()?;
    Ok(guard)
}
