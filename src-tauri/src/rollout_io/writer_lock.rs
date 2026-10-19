//! Share Core's publication lock across materialization and paginated edits.
use super::*;
use crate::error::AppError;
use crate::path_safety::{validate_descendant, EntryKind};

pub(crate) struct WriterGuard {
    _rollout: File,
    _thread: Option<File>,
    _coordination: File,
}

pub(crate) fn writer_guard(path: &Path) -> AppResult<WriterGuard> {
    let physical = resolve(path)?;
    let root = codex_root(&physical).ok_or_else(|| AppError::Path("缺少 Codex 数据根".into()))?;
    validate_descendant(root, &physical, EntryKind::File, false, "会话日志")?;
    let meta = crate::family::read_physical_session_meta(&physical)?;
    let id = meta["payload"]["id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .ok_or_else(|| AppError::Other("[EDIT_IDENTITY] 原生写入锁身份无效".into()))?;
    let directory = root.join("thread-writer-locks");
    validate_descendant(
        root,
        &directory,
        EntryKind::Directory,
        true,
        "原生写入锁目录",
    )?;
    fs::create_dir_all(&directory)?;
    let coordination_path = directory.join(".coordination.lock");
    validate_descendant(
        root,
        &coordination_path,
        EntryKind::File,
        true,
        "原生协调锁",
    )?;
    let coordination = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(coordination_path)?;
    coordination.try_lock().map_err(|e| {
        AppError::Other(format!(
            "[EDIT_BUSY] 原生写入协调锁正被使用：{e}；停止会话写入后重试"
        ))
    })?;
    let thread_path = directory.join(format!("{id}.lock"));
    validate_descendant(root, &thread_path, EntryKind::File, true, "原生会话锁")?;
    let thread = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(thread_path)
    {
        Ok(file) => {
            file.try_lock().map_err(|e| {
                AppError::Other(format!(
                    "[EDIT_BUSY] 会话仍由原生进程持有：{e}；停止该会话后重试"
                ))
            })?;
            Some(file)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1 | 4);
    }
    let rollout = options.open(&physical)?;
    Ok(WriterGuard {
        _rollout: rollout,
        _thread: thread,
        _coordination: coordination,
    })
}
