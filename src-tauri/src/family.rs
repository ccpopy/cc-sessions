//! 家族树存储：`~/.codex/session_family.json`
//!
//! 设计要点：
//! - manager 自行维护，Codex 原生不感知。
//! - 同一会话线只有一个 `active` 节点对 Codex app 可见，其他节点落入
//!   `archived_sessions/`。这保证"新对话只写进 active 节点"，切换 provider 时
//!   通过整份复制 + 立即归档做到内容连续。
//! - 每次归档时固化 sha256 + line_count，支持后续完整性校验。

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{ensure_not_cancelled, AppError, AppResult};
use crate::models::{
    ArchiveOrigin, BranchStatus, Family, FamilyBranch, FamilyIntegrityItem, FamilyIntegrityReport,
    FamilyOverlay, FamilyStore,
};
use crate::paths;
use crate::state_db;

static FAMILY_SAVE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 进程内并发保护：所有 family store 的 load → mutate → save 都需要持有这把锁。
/// Tauri 的 command 各自跑在独立线程池里，不加锁会出现"读A→读B→写A→写B"覆盖丢数据。
#[derive(Default)]
pub struct FamilyLock(pub Mutex<()>);

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
fn acquire_cross_process_family_lock() -> AppResult<CrossProcessFamilyGuard> {
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
    const INFINITE: u32 = 0xffff_ffff;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_ABANDONED: u32 = 0x80;
    let name = "Local\\cc-session-manager-family-store-v1"
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
    let wait = unsafe { WaitForSingleObject(handle, INFINITE) };
    if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
        let error = std::io::Error::last_os_error();
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(AppError::Other(format!(
            "获取 family 跨进程锁失败: {error}"
        )));
    }
    Ok(CrossProcessFamilyGuard { handle })
}

#[cfg(not(windows))]
struct CrossProcessFamilyGuard;

#[cfg(not(windows))]
fn acquire_cross_process_family_lock() -> AppResult<CrossProcessFamilyGuard> {
    Ok(CrossProcessFamilyGuard)
}

/// 封装：持锁执行回调。调用方闭包里做 load / mutate / save。
/// 只有 Tauri command 需要持锁；内部辅助函数（已持锁的调用链下层）直接调 load/save 即可。
pub fn with_lock<R>(
    lock: &FamilyLock,
    f: impl FnOnce(MutexGuard<'_, ()>) -> AppResult<R>,
) -> AppResult<R> {
    let g = lock.0.lock().unwrap_or_else(PoisonError::into_inner);
    let _cross_process = acquire_cross_process_family_lock()?;
    f(g)
}

pub fn load(codex_dir: &Path) -> AppResult<FamilyStore> {
    let p = paths::family_store_path(codex_dir);
    let metadata = match fs::metadata(&p) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FamilyStore::default())
        }
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() {
        return Err(AppError::Path(format!(
            "family store 不是普通文件: {}",
            p.to_string_lossy()
        )));
    }
    let raw = fs::read_to_string(&p)?;
    if raw.trim().is_empty() {
        return Err(AppError::Other(format!(
            "family store 内容为空，拒绝按无 family 降级处理: {}",
            p.to_string_lossy()
        )));
    }
    let store: FamilyStore = serde_json::from_str(&raw)?;
    Ok(store)
}

pub fn save(codex_dir: &Path, store: &FamilyStore) -> AppResult<()> {
    let final_path = paths::family_store_path(codex_dir);
    let data = serde_json::to_vec_pretty(store)?;
    let (temp_path, mut temp_file) = create_unique_family_temp(&final_path)?;

    if let Err(error) = write_and_sync_family_temp(&mut temp_file, &data) {
        drop(temp_file);
        return Err(cleanup_family_temp_after_error(
            &temp_path,
            AppError::Other(format!(
                "写入并同步 family 临时文件失败 {}: {error}",
                temp_path.display()
            )),
        ));
    }
    drop(temp_file);

    if let Err(error) = replace_file_atomically(&temp_path, &final_path) {
        return Err(cleanup_family_temp_after_error(
            &temp_path,
            AppError::Other(format!(
                "原子替换 family store 失败 {} -> {}: {error}",
                temp_path.display(),
                final_path.display()
            )),
        ));
    }
    Ok(())
}

fn create_unique_family_temp(final_path: &Path) -> AppResult<(PathBuf, fs::File)> {
    let parent = final_path.parent().ok_or_else(|| {
        AppError::Path(format!("family store 缺少父目录: {}", final_path.display()))
    })?;
    let file_name = final_path.file_name().ok_or_else(|| {
        AppError::Path(format!("family store 缺少文件名: {}", final_path.display()))
    })?;

    loop {
        let sequence = FAMILY_SAVE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = file_name.to_os_string();
        temp_name.push(format!(".{}.{}.tmp", std::process::id(), sequence));
        let temp_path = parent.join(temp_name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(AppError::Other(format!(
                    "创建 family 临时文件失败 {}: {error}",
                    temp_path.display()
                )))
            }
        }
    }
}

fn write_and_sync_family_temp(file: &mut fs::File, data: &[u8]) -> std::io::Result<()> {
    file.write_all(data)?;
    file.flush()?;
    file.sync_all()
}

fn cleanup_family_temp_after_error(temp_path: &Path, original_error: AppError) -> AppError {
    match fs::remove_file(temp_path) {
        Ok(()) => original_error,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => original_error,
        Err(cleanup_error) => AppError::Other(format!(
            "{original_error}; 清理 family 临时文件失败 {}: {cleanup_error}",
            temp_path.display()
        )),
    }
}

#[cfg(not(windows))]
fn replace_file_atomically(temp_path: &Path, final_path: &Path) -> std::io::Result<()> {
    fs::rename(temp_path, final_path)
}

#[cfg(windows)]
fn replace_file_atomically(temp_path: &Path, final_path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let existing: Vec<u16> = temp_path.as_os_str().encode_wide().chain(Some(0)).collect();
    let new: Vec<u16> = final_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // 两个缓冲区在调用期间保持存活且以 NUL 结尾，参数满足 MoveFileExW 的约定。
    let replaced = unsafe {
        MoveFileExW(
            existing.as_ptr(),
            new.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// 计算 rollout 文件的字节级 sha256 + 总行数（与 bundle 导出 sha256_file 语义一致）。
///
/// sha256 对**原字节流**哈希（包括换行符原样、BOM、空行等），因此与 `bundle.rs::sha256_file`
/// 同值；`line_count` 是物理行数（按 `\n` 切分得到的非空片段数），用作参考指标。
/// 两者语义彼此独立。
pub fn compute_integrity(path: &Path) -> AppResult<(String, u64)> {
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    let sha = hex::encode(hasher.finalize());

    let f = fs::File::open(path)?;
    let mut lines: u64 = 0;
    for line in BufReader::new(f).lines() {
        let line = line?;
        if !line.is_empty() {
            lines += 1;
        }
    }
    Ok((sha, lines))
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// 首次遇到的 session 注册为独立家族（root == self），返回 family_id。
pub fn ensure_family_for(
    store: &mut FamilyStore,
    session_id: &str,
    provider: &str,
    rollout_relpath: &str,
    title: &str,
) -> String {
    if let Some(fid) = store.index.get(session_id).cloned() {
        return fid;
    }
    let fid = session_id.to_string();
    let branch = FamilyBranch {
        id: session_id.to_string(),
        provider: provider.to_string(),
        created_at: now_iso(),
        status: BranchStatus::Active,
        rollout_relpath: rollout_relpath.to_string(),
        sha256: None,
        line_count: None,
        note: None,
        archive_origin: None,
    };
    let family = Family {
        family_id: fid.clone(),
        root_id: session_id.to_string(),
        title: title.to_string(),
        chain: vec![branch],
        active_id: session_id.to_string(),
        updated_at: now_iso(),
    };
    store.families.insert(fid.clone(), family);
    store.index.insert(session_id.to_string(), fid.clone());
    fid
}

/// 把某分支设为 active，其他活跃分支一律降级为 archived（同一时刻最多一个 active）。
pub fn set_active(store: &mut FamilyStore, family_id: &str, branch_id: &str) -> AppResult<()> {
    let family = store
        .families
        .get_mut(family_id)
        .ok_or_else(|| AppError::NotFound(format!("family not found: {}", family_id)))?;
    let mut found = false;
    for b in family.chain.iter_mut() {
        if b.id == branch_id {
            b.status = BranchStatus::Active;
            b.sha256 = None;
            b.line_count = None;
            b.archive_origin = None;
            found = true;
        } else if matches!(b.status, BranchStatus::Active) {
            b.status = BranchStatus::Archived;
        }
    }
    if !found {
        return Err(AppError::NotFound(format!(
            "branch not in family {}: {}",
            family_id, branch_id
        )));
    }
    family.active_id = branch_id.to_string();
    family.updated_at = now_iso();
    Ok(())
}

/// 追加一个新分支（默认 status=active，所有其他 active 降级）。
pub fn append_branch(
    store: &mut FamilyStore,
    family_id: &str,
    branch: FamilyBranch,
) -> AppResult<()> {
    let new_id = branch.id.clone();
    {
        let family = store
            .families
            .get_mut(family_id)
            .ok_or_else(|| AppError::NotFound(format!("family not found: {}", family_id)))?;
        for b in family.chain.iter_mut() {
            if matches!(b.status, BranchStatus::Active) {
                b.status = BranchStatus::Archived;
            }
        }
        family.chain.push(branch);
        family.active_id = new_id.clone();
        family.updated_at = now_iso();
    }
    store.index.insert(new_id, family_id.to_string());
    Ok(())
}

/// 严格解析 session 所属 family，供删除等破坏性操作在写入前校验引用完整性。
///
/// `index` 与 `families[*].chain` 必须双向一致；任一侧单独存在、同一分支出现在
/// 多个 family、或目标 family 的 `active_id`/分支状态无效时都会显式报错。
/// 完全不属于任何 family 的普通会话返回 `Ok(None)`。
pub fn resolve_family_id_strict(
    store: &FamilyStore,
    session_id: &str,
) -> AppResult<Option<String>> {
    let chain_family_ids = family_ids_containing_session(store, session_id)?;
    let indexed_family_id = store.index.get(session_id);
    let chain_family_id = chain_family_ids.first();

    let family_id = match (indexed_family_id, chain_family_id) {
        (None, None) => return Ok(None),
        (Some(indexed), Some(chained)) if indexed == chained => indexed.clone(),
        (Some(indexed), None) => {
            return Err(inconsistent_family(format!(
                "index 将分支 {session_id} 指向 {indexed}，但对应 chain 中不存在该分支"
            )))
        }
        (None, Some(chained)) => {
            return Err(inconsistent_family(format!(
                "分支 {session_id} 存在于 family {chained} 的 chain，但缺少 index 记录"
            )))
        }
        (Some(indexed), Some(chained)) => {
            return Err(inconsistent_family(format!(
                "分支 {session_id} 的 index 指向 {indexed}，chain 实际属于 {chained}"
            )))
        }
    };

    validate_family_membership(store, &family_id)?;
    Ok(Some(family_id))
}

/// 整组移除 family，并清掉所有指向它的反向索引，包括旧版本遗留的多余索引。
pub fn remove_family(store: &mut FamilyStore, family_id: &str) -> AppResult<Family> {
    let family = store
        .families
        .remove(family_id)
        .ok_or_else(|| AppError::NotFound(format!("family not found: {family_id}")))?;
    store
        .index
        .retain(|_, mapped_family_id| mapped_family_id != family_id);
    Ok(family)
}

/// 从 family 中移除一个非 active 分支；active 分支必须通过整组删除或先切换。
pub fn remove_non_active_branch(
    store: &mut FamilyStore,
    family_id: &str,
    branch_id: &str,
) -> AppResult<FamilyBranch> {
    let resolved_family_id = resolve_family_id_strict(store, branch_id)?.ok_or_else(|| {
        AppError::NotFound(format!("branch not found in family store: {branch_id}"))
    })?;
    if resolved_family_id != family_id {
        return Err(inconsistent_family(format!(
            "分支 {branch_id} 属于 family {resolved_family_id}，请求却指定了 {family_id}"
        )));
    }

    let family = store
        .families
        .get(family_id)
        .ok_or_else(|| AppError::NotFound(format!("family not found: {family_id}")))?;
    if family.active_id == branch_id {
        return Err(AppError::Other(format!(
            "不能单独删除 active 分支 {branch_id}，请删除整个 family 或先切换分支"
        )));
    }
    let position = family
        .chain
        .iter()
        .position(|branch| branch.id == branch_id)
        .ok_or_else(|| {
            AppError::NotFound(format!("branch not in family {family_id}: {branch_id}"))
        })?;

    let family = store
        .families
        .get_mut(family_id)
        .ok_or_else(|| AppError::NotFound(format!("family not found: {family_id}")))?;
    let removed = family.chain.remove(position);
    if family.root_id == branch_id {
        family.root_id = family
            .chain
            .first()
            .ok_or_else(|| {
                inconsistent_family(format!("family {family_id} 删除分支后 chain 为空"))
            })?
            .id
            .clone();
    }
    family.updated_at = now_iso();
    store.index.remove(branch_id);
    Ok(removed)
}

/// 同步手工归档带来的校验元数据，但不改变 family 的分支角色或恢复路径。
///
/// 归档后的文件视为不可变快照，记录 sha256/line_count；取消归档后文件重新可写，
/// 因此清空旧校验。完全未纳入 family 的普通会话返回 `Ok(false)`。
pub fn update_manual_archive_metadata(
    store: &mut FamilyStore,
    codex_dir: &Path,
    session_id: &str,
    archived: bool,
    rollout_path: &Path,
) -> AppResult<bool> {
    let Some(family_id) = resolve_family_id_strict(store, session_id)? else {
        return Ok(false);
    };
    let integrity = if archived {
        let absolute_path = if rollout_path.is_absolute() {
            rollout_path.to_path_buf()
        } else {
            let relative = paths::checked_relative_path(&rollout_path.to_string_lossy())?;
            codex_dir.join(relative)
        };
        Some(compute_integrity(&absolute_path)?)
    } else {
        None
    };

    let family = store
        .families
        .get_mut(&family_id)
        .ok_or_else(|| AppError::NotFound(format!("family not found: {family_id}")))?;
    let branch = family
        .chain
        .iter_mut()
        .find(|branch| branch.id == session_id)
        .ok_or_else(|| {
            inconsistent_family(format!(
                "index 指向 family {family_id}，但分支 {session_id} 不在 chain 中"
            ))
        })?;
    match integrity {
        Some((sha256, line_count)) => {
            branch.sha256 = Some(sha256);
            branch.line_count = Some(line_count);
        }
        None => {
            branch.sha256 = None;
            branch.line_count = None;
        }
    }
    branch.archive_origin = None;
    family.updated_at = now_iso();
    Ok(true)
}

fn family_ids_containing_session(store: &FamilyStore, session_id: &str) -> AppResult<Vec<String>> {
    let mut family_ids = Vec::new();
    for (family_id, family) in &store.families {
        let matches = family
            .chain
            .iter()
            .filter(|branch| branch.id == session_id)
            .count();
        if matches > 1 {
            return Err(inconsistent_family(format!(
                "分支 {session_id} 在 family {family_id} 的 chain 中重复出现"
            )));
        }
        if matches == 1 {
            family_ids.push(family_id.clone());
        }
    }
    if family_ids.len() > 1 {
        return Err(inconsistent_family(format!(
            "分支 {session_id} 同时出现在多个 family: {}",
            family_ids.join(", ")
        )));
    }
    Ok(family_ids)
}

fn validate_family_membership(store: &FamilyStore, family_id: &str) -> AppResult<()> {
    let family = store
        .families
        .get(family_id)
        .ok_or_else(|| inconsistent_family(format!("index 指向不存在的 family {family_id}")))?;
    if family.family_id != family_id {
        return Err(inconsistent_family(format!(
            "family map key 为 {family_id}，记录内 family_id 却是 {}",
            family.family_id
        )));
    }
    if family.chain.is_empty() {
        return Err(inconsistent_family(format!(
            "family {family_id} 的 chain 为空"
        )));
    }

    let mut branch_ids = std::collections::BTreeSet::new();
    for branch in &family.chain {
        if !branch_ids.insert(branch.id.as_str()) {
            return Err(inconsistent_family(format!(
                "family {family_id} 的分支 {} 重复出现",
                branch.id
            )));
        }
        match store.index.get(&branch.id) {
            Some(indexed_family_id) if indexed_family_id == family_id => {}
            Some(indexed_family_id) => {
                return Err(inconsistent_family(format!(
                    "分支 {} 位于 family {family_id}，index 却指向 {indexed_family_id}",
                    branch.id
                )))
            }
            None => {
                return Err(inconsistent_family(format!(
                    "family {family_id} 的分支 {} 缺少 index 记录",
                    branch.id
                )))
            }
        }
    }

    validate_active_branch(family_id, family)?;
    validate_reverse_indexes(store, family_id, &branch_ids)?;
    validate_cross_family_uniqueness(store, family_id, &branch_ids)
}

fn validate_active_branch(family_id: &str, family: &Family) -> AppResult<()> {
    let active = family
        .chain
        .iter()
        .find(|branch| branch.id == family.active_id)
        .ok_or_else(|| {
            inconsistent_family(format!(
                "family {family_id} 的 active_id {} 不在 chain 中",
                family.active_id
            ))
        })?;
    if !matches!(active.status, BranchStatus::Active) {
        return Err(inconsistent_family(format!(
            "family {family_id} 的 active_id {} 状态不是 active",
            family.active_id
        )));
    }
    if let Some(branch) = family.chain.iter().find(|branch| {
        branch.id != family.active_id && matches!(branch.status, BranchStatus::Active)
    }) {
        return Err(inconsistent_family(format!(
            "family {family_id} 的非 active_id 分支 {} 仍标记为 active",
            branch.id
        )));
    }
    Ok(())
}

fn validate_reverse_indexes(
    store: &FamilyStore,
    family_id: &str,
    branch_ids: &std::collections::BTreeSet<&str>,
) -> AppResult<()> {
    if let Some((indexed_id, _)) = store.index.iter().find(|(indexed_id, mapped_family_id)| {
        mapped_family_id.as_str() == family_id && !branch_ids.contains(indexed_id.as_str())
    }) {
        return Err(inconsistent_family(format!(
            "index 将不存在于 chain 的分支 {indexed_id} 指向 family {family_id}"
        )));
    }
    Ok(())
}

fn validate_cross_family_uniqueness(
    store: &FamilyStore,
    family_id: &str,
    branch_ids: &std::collections::BTreeSet<&str>,
) -> AppResult<()> {
    for (other_family_id, other_family) in &store.families {
        if other_family_id == family_id {
            continue;
        }
        if let Some(branch) = other_family
            .chain
            .iter()
            .find(|branch| branch_ids.contains(branch.id.as_str()))
        {
            return Err(inconsistent_family(format!(
                "分支 {} 同时存在于 family {family_id} 和 {other_family_id}",
                branch.id
            )));
        }
    }
    Ok(())
}

fn inconsistent_family(message: impl Into<String>) -> AppError {
    AppError::Other(format!("session_family 数据不一致: {}", message.into()))
}

/// 归档指定分支时固化 sha256 + line_count（rollout 文件必须存在）。
pub fn archive_with_integrity(
    store: &mut FamilyStore,
    codex_dir: &Path,
    family_id: &str,
    branch_id: &str,
) -> AppResult<()> {
    let branch = store
        .families
        .get(family_id)
        .ok_or_else(|| AppError::NotFound(format!("family not found: {family_id}")))?
        .chain
        .iter()
        .find(|branch| branch.id == branch_id)
        .ok_or_else(|| {
            AppError::NotFound(format!("branch not in family {family_id}: {branch_id}"))
        })?;
    let rel = paths::checked_relative_path(&branch.rollout_relpath)?;
    let abs = codex_dir.join(rel);
    if !abs.is_file() {
        return Err(AppError::NotFound(format!(
            "待归档 rollout 不存在: {}",
            abs.to_string_lossy()
        )));
    }
    let (sha, lines) = compute_integrity(&abs)?;

    let family = store
        .families
        .get_mut(family_id)
        .ok_or_else(|| AppError::NotFound(format!("family not found: {family_id}")))?;
    let branch = family
        .chain
        .iter_mut()
        .find(|branch| branch.id == branch_id)
        .ok_or_else(|| {
            AppError::NotFound(format!("branch not in family {family_id}: {branch_id}"))
        })?;
    branch.sha256 = Some(sha);
    branch.line_count = Some(lines);
    branch.status = BranchStatus::Archived;
    branch.archive_origin = None;
    family.updated_at = now_iso();
    Ok(())
}

pub fn set_archive_origin(
    store: &mut FamilyStore,
    family_id: &str,
    branch_id: &str,
    origin: ArchiveOrigin,
) -> AppResult<()> {
    let branch = store
        .families
        .get_mut(family_id)
        .ok_or_else(|| AppError::NotFound(format!("family not found: {family_id}")))?
        .chain
        .iter_mut()
        .find(|branch| branch.id == branch_id)
        .ok_or_else(|| {
            AppError::NotFound(format!("branch not in family {family_id}: {branch_id}"))
        })?;
    branch.archive_origin = Some(origin);
    Ok(())
}

/// 按 session_id 定位 family 分支并设置归档来源（用户手动切换来源时同步分支字段）。
/// 会话不属于任何 family 时 no-op（返回 false）。调用方必须持有 FamilyLock。
pub fn set_archive_origin_for_session(
    codex_dir: &Path,
    session_id: &str,
    origin: ArchiveOrigin,
) -> AppResult<bool> {
    let mut store = load(codex_dir)?;
    let family_ids = family_ids_containing_session(&store, session_id)?;
    let Some(family_id) = family_ids.first() else {
        return Ok(false);
    };
    set_archive_origin(&mut store, family_id, session_id, origin)?;
    if let Some(family) = store.families.get_mut(family_id) {
        family.updated_at = now_iso();
    }
    save(codex_dir, &store)?;
    Ok(true)
}

/// 扫描 family store，对每个已固化的分支比对 rollout 文件。
pub fn verify_integrity(codex_dir: &Path) -> AppResult<FamilyIntegrityReport> {
    let store = load(codex_dir)?;
    let mut items: Vec<FamilyIntegrityItem> = Vec::new();
    let mut all_ok = true;
    for (fid, family) in store.families.iter() {
        for b in family.chain.iter() {
            let expected_sha = b.sha256.clone();
            let expected_lines = b.line_count;
            let unsealed = expected_sha.is_none();
            let rel = paths::checked_relative_path(&b.rollout_relpath)?;
            let abs_main = codex_dir.join(&rel);
            let abs_archived =
                paths::archived_sessions_dir(codex_dir).join(rel.file_name().unwrap_or_default());
            let candidate = if abs_main.is_file() {
                abs_main
            } else if abs_archived.is_file() {
                abs_archived
            } else {
                all_ok = false;
                items.push(FamilyIntegrityItem {
                    family_id: fid.clone(),
                    branch_id: b.id.clone(),
                    ok: false,
                    expected_sha,
                    actual_sha: None,
                    expected_lines,
                    actual_lines: None,
                    missing: true,
                });
                continue;
            };
            if unsealed {
                continue; // 未固化 sha256 的分支（当前可写分支、迁移前的旧数据）只检查是否存在，不做 sha256/行数校验
            }
            match compute_integrity(&candidate) {
                Ok((sha, lines)) => {
                    let sha_ok = expected_sha.as_deref() == Some(sha.as_str());
                    let lines_ok = expected_lines.map(|l| l == lines).unwrap_or(true);
                    let ok = sha_ok && lines_ok;
                    if !ok {
                        all_ok = false;
                    }
                    items.push(FamilyIntegrityItem {
                        family_id: fid.clone(),
                        branch_id: b.id.clone(),
                        ok,
                        expected_sha,
                        actual_sha: Some(sha),
                        expected_lines,
                        actual_lines: Some(lines),
                        missing: false,
                    });
                }
                Err(_) => {
                    all_ok = false;
                    items.push(FamilyIntegrityItem {
                        family_id: fid.clone(),
                        branch_id: b.id.clone(),
                        ok: false,
                        expected_sha,
                        actual_sha: None,
                        expected_lines,
                        actual_lines: None,
                        missing: false,
                    });
                }
            }
        }
    }
    Ok(FamilyIntegrityReport { items, all_ok })
}

/// 从 rollout 文件读第一行 session_meta 的 payload（id / model_provider / cwd / originator）。
pub fn read_session_meta(rollout: &Path) -> AppResult<Value> {
    let f = fs::File::open(rollout)?;
    let mut reader = BufReader::new(f);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let v: Value = serde_json::from_str(first.trim())?;
    Ok(v)
}

fn scan_rollouts_in(root: PathBuf, cancel: Option<&AtomicBool>) -> AppResult<Vec<PathBuf>> {
    let mut out = Vec::new();
    ensure_not_cancelled(cancel)?;
    let metadata = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        return Err(AppError::Path(format!(
            "rollout 根目录不是普通目录或属于链接/junction: {}",
            root.to_string_lossy()
        )));
    }
    for entry in walkdir::WalkDir::new(&root).follow_links(false) {
        ensure_not_cancelled(cancel)?;
        let entry = entry.map_err(|error| {
            AppError::Other(format!(
                "扫描 rollout 目录失败 {}: {error}",
                root.to_string_lossy()
            ))
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "rollout 目录包含链接/junction: {}",
                entry.path().to_string_lossy()
            )));
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if name.starts_with("rollout-") && name.ends_with(".jsonl") {
            out.push(entry.path().to_path_buf());
        }
    }
    Ok(out)
}

/// 扫描 sessions/ 目录下所有 active `rollout-*.jsonl`。
pub fn scan_rollouts(codex_dir: &Path) -> AppResult<Vec<PathBuf>> {
    scan_rollouts_in(paths::sessions_dir(codex_dir), None)
}

/// 扫描 archived_sessions/ 目录下所有 archived `rollout-*.jsonl`。
pub fn scan_archived_rollouts(codex_dir: &Path) -> AppResult<Vec<PathBuf>> {
    scan_rollouts_in(paths::archived_sessions_dir(codex_dir), None)
}

pub(crate) fn scan_archived_rollouts_cancellable(
    codex_dir: &Path,
    cancel: &AtomicBool,
) -> AppResult<Vec<PathBuf>> {
    scan_rollouts_in(paths::archived_sessions_dir(codex_dir), Some(cancel))
}

pub fn get_family_store_with_lock(codex_dir: String, lock: &FamilyLock) -> AppResult<FamilyStore> {
    with_lock(lock, |_g| {
        let p = PathBuf::from(&codex_dir);
        load(&p)
    })
}

pub fn verify_family_integrity_with_lock(
    codex_dir: String,
    lock: &FamilyLock,
) -> AppResult<FamilyIntegrityReport> {
    with_lock(lock, |_g| {
        let p = PathBuf::from(&codex_dir);
        verify_integrity(&p)
    })
}

/// 把 threads 表 + family store + current provider 聚合成 per-session 覆盖信息，
/// 用于 Sessions 列表的 Badge 与 provider / 本地索引维护提示。
pub fn get_session_family_overlay_with_lock(
    codex_dir: String,
    lock: &FamilyLock,
) -> AppResult<Vec<FamilyOverlay>> {
    let codex = PathBuf::from(&codex_dir);
    let _g = lock.0.lock().unwrap_or_else(PoisonError::into_inner);
    let _cross_process = acquire_cross_process_family_lock()?;

    // 1) 读 threads 表（id, model_provider, source, archived）
    let mut thread_state_of: std::collections::BTreeMap<
        String,
        (Option<String>, Option<String>, Option<String>, bool),
    > = std::collections::BTreeMap::new();
    if paths::state_db_path(&codex).is_file() {
        let conn = state_db::open_ro(&codex)?;
        let mut stmt = conn.prepare(
            "SELECT id, rollout_path, model_provider, source, COALESCE(archived,0) FROM threads",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)? != 0,
                ),
            ))
        })?;
        for row in rows {
            let (id, state) = row?;
            thread_state_of.insert(id, state);
        }
    }

    // 2) 读 family store
    let store = load(&codex)?;
    let index_ids = crate::repair::read_session_index_ids(&codex)?;

    // 3) 读 current provider
    let cur = Some(crate::repair::read_current_provider_export(&codex)?);
    let sessions_root = paths::sessions_dir(&codex);

    let mut out: Vec<FamilyOverlay> = Vec::with_capacity(thread_state_of.len());
    for (id, (rollout_path, provider, source, archived)) in &thread_state_of {
        let rollout = rollout_path.as_deref().map(|raw| {
            PathBuf::from(paths::strip_verbatim(
                &paths::host_path_string_from_codex_record(&codex, raw),
            ))
        });
        let recorded_rollout_available = rollout
            .as_ref()
            .is_some_and(|path| path.starts_with(&sessions_root) && path.is_file());
        let record_usable =
            if let (Some(provider), Some(rollout)) = (provider.as_deref(), rollout.as_deref()) {
                crate::repair::rollout_record_is_usable_provider(
                    &codex,
                    id,
                    provider,
                    rollout,
                    rollout_path.as_deref(),
                    Some(provider),
                    source.as_deref(),
                    *archived,
                    index_ids.contains(id),
                )?
            } else {
                false
            };
        let family_id = store.index.get(id).cloned();
        let family_rollout_available = if let Some(branch) = family_id
            .as_ref()
            .and_then(|family_id| store.families.get(family_id))
            .and_then(|family| family.chain.iter().find(|branch| branch.id == *id))
        {
            let relative = paths::checked_relative_path(&branch.rollout_relpath)?;
            relative.starts_with("sessions") && codex.join(relative).is_file()
        } else {
            false
        };
        let source_rollout_available = recorded_rollout_available || family_rollout_available;
        let archive_origin = family_id
            .as_ref()
            .and_then(|family_id| store.families.get(family_id))
            .and_then(|family| family.chain.iter().find(|branch| branch.id == *id))
            .and_then(|branch| branch.archive_origin.clone());
        let (branch_count, is_active_branch, clone_state) = match family_id.as_ref() {
            None => {
                let cs = compute_clone_state(
                    provider.as_deref(),
                    None,
                    cur.as_deref(),
                    false,
                    source.as_deref(),
                    *archived,
                    record_usable,
                    source_rollout_available,
                );
                (0u32, false, cs)
            }
            Some(fid) => {
                let family = store.families.get(fid);
                let branch_count = family.map(|f| f.chain.len() as u32).unwrap_or(0);
                let is_active = family.map(|f| f.active_id == *id).unwrap_or(false);
                let mut has_clone_in_current = false;
                if let (Some(family), Some(current_provider)) = (family, cur.as_deref()) {
                    for branch in &family.chain {
                        if branch.provider != current_provider {
                            continue;
                        }
                        let Some((branch_rollout, branch_provider, branch_source, branch_archived)) =
                            thread_state_of.get(&branch.id)
                        else {
                            continue;
                        };
                        let relative = paths::checked_relative_path(&branch.rollout_relpath)?;
                        if !relative.starts_with("sessions") {
                            continue;
                        }
                        if crate::repair::rollout_record_is_usable_provider(
                            &codex,
                            &branch.id,
                            current_provider,
                            &codex.join(relative),
                            branch_rollout.as_deref(),
                            branch_provider.as_deref(),
                            branch_source.as_deref(),
                            *branch_archived,
                            index_ids.contains(&branch.id),
                        )? {
                            has_clone_in_current = true;
                            break;
                        }
                    }
                }
                let cs = compute_clone_state(
                    provider.as_deref(),
                    family,
                    cur.as_deref(),
                    has_clone_in_current,
                    source.as_deref(),
                    *archived,
                    record_usable,
                    source_rollout_available,
                );
                (branch_count, is_active, cs)
            }
        };
        out.push(FamilyOverlay {
            session_id: id.clone(),
            provider: provider.clone(),
            family_id,
            branch_count,
            is_active_branch,
            archive_origin,
            clone_state,
        });
    }
    Ok(out)
}

fn compute_clone_state(
    provider: Option<&str>,
    _family: Option<&Family>,
    current: Option<&str>,
    has_clone_in_current: bool,
    source: Option<&str>,
    archived: bool,
    record_usable: bool,
    source_rollout_available: bool,
) -> String {
    if archived {
        return "matches".into();
    }
    if crate::repair::is_subagent_source(source) {
        return "subagent".into();
    }
    if !source_rollout_available {
        return "unknown".into();
    }
    match (provider, current) {
        (Some(p), Some(cur)) if p == cur => {
            if record_usable {
                "matches".into()
            } else {
                "resync".into()
            }
        }
        (Some(_), Some(_)) if has_clone_in_current => "has_clone".into(),
        (Some(_), Some(_)) => "clonable".into(),
        _ => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{BranchStatus, Family, FamilyBranch, FamilyStore};
    use rusqlite::params;
    use std::collections::BTreeMap;

    fn temp_codex_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ))
    }

    fn branch(id: &str, status: BranchStatus) -> FamilyBranch {
        FamilyBranch {
            id: id.to_string(),
            provider: format!("provider-{id}"),
            created_at: "2026-04-24T00:00:00Z".to_string(),
            status,
            rollout_relpath: format!("sessions/2026/04/24/rollout-{id}.jsonl"),
            sha256: None,
            line_count: None,
            note: None,
            archive_origin: None,
        }
    }

    fn two_branch_store() -> FamilyStore {
        let family_id = "family-a";
        let active_id = "active-a";
        let history_id = "history-a";
        let family = Family {
            family_id: family_id.to_string(),
            root_id: active_id.to_string(),
            title: "family fixture".to_string(),
            chain: vec![
                branch(active_id, BranchStatus::Active),
                branch(history_id, BranchStatus::Archived),
            ],
            active_id: active_id.to_string(),
            updated_at: "2026-04-24T00:00:00Z".to_string(),
        };
        let mut families = BTreeMap::new();
        families.insert(family_id.to_string(), family);
        let mut index = BTreeMap::new();
        index.insert(active_id.to_string(), family_id.to_string());
        index.insert(history_id.to_string(), family_id.to_string());
        FamilyStore {
            version: 1,
            families,
            index,
        }
    }

    #[test]
    fn set_active_clears_restored_branch_integrity_snapshot() -> AppResult<()> {
        let mut store = two_branch_store();
        let history = store
            .families
            .get_mut("family-a")
            .expect("family fixture")
            .chain
            .iter_mut()
            .find(|branch| branch.id == "history-a")
            .expect("history branch");
        history.sha256 = Some("archived-sha".to_string());
        history.line_count = Some(42);

        set_active(&mut store, "family-a", "history-a")?;

        let family = store.families.get("family-a").expect("family remains");
        let restored = family
            .chain
            .iter()
            .find(|branch| branch.id == "history-a")
            .expect("restored branch");
        assert_eq!(family.active_id, "history-a");
        assert!(matches!(restored.status, BranchStatus::Active));
        assert_eq!(restored.sha256, None);
        assert_eq!(restored.line_count, None);
        assert!(family.chain.iter().any(
            |branch| branch.id == "active-a" && matches!(branch.status, BranchStatus::Archived)
        ));
        Ok(())
    }

    #[test]
    fn family_store_without_archive_origin_remains_compatible() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-family-origin-compat-test");
        fs::create_dir_all(&codex)?;
        fs::write(
            paths::family_store_path(&codex),
            serde_json::json!({
                "version": 1,
                "families": {
                    "legacy": {
                        "family_id": "legacy",
                        "root_id": "legacy",
                        "title": "legacy family",
                        "chain": [{
                            "id": "legacy",
                            "provider": "openai",
                            "created_at": "2026-04-24T00:00:00Z",
                            "status": "archived",
                            "rollout_relpath": "archived_sessions/rollout-legacy.jsonl",
                            "sha256": null,
                            "line_count": null,
                            "note": null
                        }],
                        "active_id": "legacy",
                        "updated_at": "2026-04-24T00:00:00Z"
                    }
                },
                "index": {"legacy": "legacy"}
            })
            .to_string(),
        )?;

        let store = load(&codex)?;
        assert_eq!(store.families["legacy"].chain[0].archive_origin, None);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    fn family_save_temp_files(codex_dir: &Path) -> AppResult<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in fs::read_dir(codex_dir)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if name.contains("session_family.json") && name.ends_with(".tmp") {
                files.push(path);
            }
        }
        Ok(files)
    }

    #[test]
    fn save_atomically_replaces_existing_family_store_repeatedly() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-family-save-replace-test");
        fs::create_dir_all(&codex)?;
        let mut store = two_branch_store();

        for revision in 0..5 {
            let title = format!("family revision {revision}");
            store
                .families
                .get_mut("family-a")
                .expect("family fixture")
                .title
                .clone_from(&title);
            save(&codex, &store)?;

            let loaded = load(&codex)?;
            assert_eq!(
                loaded.families.get("family-a").expect("saved family").title,
                title
            );
        }

        assert!(family_save_temp_files(&codex)?.is_empty());
        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn save_removes_unique_temp_file_when_atomic_replace_fails() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-family-save-cleanup-test");
        fs::create_dir_all(paths::family_store_path(&codex))?;

        let error = save(&codex, &two_branch_store())
            .expect_err("a directory cannot be replaced by the family store file");

        assert!(!error.to_string().is_empty());
        assert!(paths::family_store_path(&codex).is_dir());
        assert!(family_save_temp_files(&codex)?.is_empty());
        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn strict_family_resolution_accepts_consistent_membership_and_orphan() -> AppResult<()> {
        let store = two_branch_store();

        assert_eq!(
            resolve_family_id_strict(&store, "active-a")?,
            Some("family-a".to_string())
        );
        assert_eq!(
            resolve_family_id_strict(&store, "history-a")?,
            Some("family-a".to_string())
        );
        assert_eq!(resolve_family_id_strict(&store, "orphan")?, None);
        Ok(())
    }

    #[test]
    fn strict_family_resolution_rejects_index_and_active_drift() {
        let mut missing_index = two_branch_store();
        missing_index.index.remove("history-a");
        let error = resolve_family_id_strict(&missing_index, "history-a")
            .expect_err("chain membership without an index entry must fail");
        assert!(error.to_string().contains("index"));

        let mut dangling_active = two_branch_store();
        dangling_active
            .families
            .get_mut("family-a")
            .expect("family fixture")
            .active_id = "missing-active".to_string();
        let error = resolve_family_id_strict(&dangling_active, "active-a")
            .expect_err("a dangling active_id must fail before destructive work");
        assert!(error.to_string().contains("active_id"));
    }

    #[test]
    fn remove_family_cleans_every_reverse_index_for_that_family() -> AppResult<()> {
        let mut store = two_branch_store();
        store
            .index
            .insert("stale-family-index".to_string(), "family-a".to_string());
        store
            .index
            .insert("unrelated".to_string(), "family-b".to_string());

        let removed = remove_family(&mut store, "family-a")?;

        assert_eq!(removed.active_id, "active-a");
        assert!(!store.families.contains_key("family-a"));
        assert!(store
            .index
            .values()
            .all(|family_id| family_id != "family-a"));
        assert_eq!(
            store.index.get("unrelated").map(String::as_str),
            Some("family-b")
        );
        Ok(())
    }

    #[test]
    fn remove_non_active_branch_preserves_active_and_rejects_active() -> AppResult<()> {
        let mut store = two_branch_store();
        let before = serde_json::to_value(&store)?;

        let error = remove_non_active_branch(&mut store, "family-a", "active-a")
            .expect_err("the active branch must never be removed individually");
        assert!(error.to_string().contains("active"));
        assert_eq!(serde_json::to_value(&store)?, before);

        let removed = remove_non_active_branch(&mut store, "family-a", "history-a")?;
        assert_eq!(removed.id, "history-a");
        let family = store.families.get("family-a").expect("family remains");
        assert_eq!(family.active_id, "active-a");
        assert_eq!(family.chain.len(), 1);
        assert_eq!(family.chain[0].id, "active-a");
        assert!(!store.index.contains_key("history-a"));
        assert_eq!(
            store.index.get("active-a").map(String::as_str),
            Some("family-a")
        );
        Ok(())
    }

    #[test]
    fn remove_non_active_branch_repairs_root_when_root_branch_is_removed() -> AppResult<()> {
        let mut store = two_branch_store();
        let family = store.families.get_mut("family-a").expect("family fixture");
        family.root_id = "history-a".to_string();
        family.chain.swap(0, 1);

        let removed = remove_non_active_branch(&mut store, "family-a", "history-a")?;

        assert_eq!(removed.id, "history-a");
        let family = store.families.get("family-a").expect("family remains");
        assert_eq!(family.root_id, "active-a");
        assert_eq!(family.active_id, "active-a");
        assert_eq!(family.chain.len(), 1);
        Ok(())
    }

    #[test]
    fn verify_integrity_reports_missing_unsealed_active_branch() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-family-missing-active-integrity-test");
        fs::create_dir_all(&codex)?;
        let mut store = FamilyStore::default();
        ensure_family_for(
            &mut store,
            "missing-active",
            "openai",
            "sessions/2026/04/24/rollout-missing-active.jsonl",
            "missing active",
        );
        save(&codex, &store)?;

        let report = verify_integrity(&codex)?;

        assert!(!report.all_ok);
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].branch_id, "missing-active");
        assert!(report.items[0].missing);
        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn verify_integrity_skips_unsealed_non_active_branch() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-family-unsealed-history-integrity-test");
        let store = two_branch_store();
        for b in &store.families["family-a"].chain {
            let abs = codex.join(&b.rollout_relpath);
            fs::create_dir_all(abs.parent().unwrap())?;
            fs::write(&abs, "line\n")?;
        }
        save(&codex, &store)?;

        let report = verify_integrity(&codex)?;

        // history-a 已归档但从未固化 sha256（旧数据/外部写入），只检查存在性，不算失败。
        assert!(report.all_ok);
        assert!(report.items.is_empty());
        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn manual_archive_metadata_preserves_family_role_and_tracks_mutability() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-family-archive-metadata-test");
        fs::create_dir_all(&codex)?;
        let rollout = codex.join("rollout-active-a.jsonl");
        fs::write(&rollout, "first\n\nsecond\n")?;
        let expected = compute_integrity(&rollout)?;
        let mut store = two_branch_store();

        assert!(update_manual_archive_metadata(
            &mut store, &codex, "active-a", true, &rollout,
        )?);
        let family = store.families.get("family-a").expect("family remains");
        let active = family
            .chain
            .iter()
            .find(|item| item.id == "active-a")
            .expect("active branch remains");
        assert_eq!(family.active_id, "active-a");
        assert!(matches!(active.status, BranchStatus::Active));
        assert_eq!(active.sha256.as_deref(), Some(expected.0.as_str()));
        assert_eq!(active.line_count, Some(expected.1));
        assert_eq!(
            active.rollout_relpath,
            "sessions/2026/04/24/rollout-active-a.jsonl"
        );

        let missing = codex.join("does-not-need-to-exist-on-restore.jsonl");
        assert!(update_manual_archive_metadata(
            &mut store, &codex, "active-a", false, &missing,
        )?);
        let family = store.families.get("family-a").expect("family remains");
        let active = family
            .chain
            .iter()
            .find(|item| item.id == "active-a")
            .expect("active branch remains");
        assert_eq!(family.active_id, "active-a");
        assert!(matches!(active.status, BranchStatus::Active));
        assert_eq!(active.sha256, None);
        assert_eq!(active.line_count, None);
        assert!(!update_manual_archive_metadata(
            &mut store, &codex, "orphan", true, &missing,
        )?);

        fs::remove_dir_all(codex).ok();
        Ok(())
    }

    #[test]
    fn overlay_counts_only_unarchived_visible_target_clone() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-overlay-clone-test");
        let sessions = codex.join("sessions");
        fs::create_dir_all(&sessions)?;
        let source_rollout = sessions.join("rollout-overlay-source.jsonl");
        let target_rollout = sessions.join("rollout-overlay-target.jsonl");
        for (path, id, provider) in [
            (&source_rollout, "overlay-source", "custom"),
            (&target_rollout, "overlay-target", "openai"),
        ] {
            let line = serde_json::json!({
                "timestamp": "2026-04-24T00:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "model_provider": provider,
                    "cwd": "F:\\project\\example"
                }
            });
            fs::write(path, format!("{}\n", serde_json::to_string(&line)?))?;
        }
        fs::write(
            paths::session_index_path(&codex),
            "{\"id\":\"overlay-source\"}\n{\"id\":\"overlay-target\"}\n",
        )?;
        let conn = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        conn.execute(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT,
                model_provider TEXT,
                source TEXT,
                archived INTEGER
            )",
            [],
        )?;
        conn.execute(
            "INSERT INTO threads (id, rollout_path, model_provider, source, archived)
             VALUES (?1, ?2, ?3, ?4, 0)",
            params![
                "overlay-source",
                source_rollout.to_string_lossy(),
                "custom",
                "cli"
            ],
        )?;
        conn.execute(
            "INSERT INTO threads (id, rollout_path, model_provider, source, archived)
             VALUES (?1, ?2, ?3, ?4, 1)",
            params![
                "overlay-target",
                target_rollout.to_string_lossy(),
                "openai",
                "cli"
            ],
        )?;

        let family = Family {
            family_id: "overlay-source".to_string(),
            root_id: "overlay-source".to_string(),
            title: "overlay".to_string(),
            chain: vec![
                FamilyBranch {
                    id: "overlay-source".to_string(),
                    provider: "custom".to_string(),
                    created_at: "2026-04-24T00:00:00Z".to_string(),
                    status: BranchStatus::Active,
                    rollout_relpath: "sessions/rollout-overlay-source.jsonl".to_string(),
                    sha256: None,
                    line_count: None,
                    note: None,
                    archive_origin: None,
                },
                FamilyBranch {
                    id: "overlay-target".to_string(),
                    provider: "openai".to_string(),
                    created_at: "2026-04-24T00:00:00Z".to_string(),
                    status: BranchStatus::Archived,
                    rollout_relpath: "sessions/rollout-overlay-target.jsonl".to_string(),
                    sha256: None,
                    line_count: None,
                    note: None,
                    archive_origin: None,
                },
            ],
            active_id: "overlay-source".to_string(),
            updated_at: "2026-04-24T00:00:00Z".to_string(),
        };
        let mut families = BTreeMap::new();
        families.insert("overlay-source".to_string(), family);
        let mut index = BTreeMap::new();
        index.insert("overlay-source".to_string(), "overlay-source".to_string());
        index.insert("overlay-target".to_string(), "overlay-source".to_string());
        save(
            &codex,
            &FamilyStore {
                version: 1,
                families,
                index,
            },
        )?;
        drop(conn);

        let lock = FamilyLock::default();
        let overlay =
            get_session_family_overlay_with_lock(codex.to_string_lossy().into_owned(), &lock)?;
        let source = overlay
            .iter()
            .find(|item| item.session_id == "overlay-source")
            .expect("source overlay");
        assert_eq!(source.clone_state, "clonable");

        let conn = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        conn.execute(
            "UPDATE threads SET archived = 0 WHERE id = ?",
            ["overlay-target"],
        )?;
        drop(conn);
        let overlay =
            get_session_family_overlay_with_lock(codex.to_string_lossy().into_owned(), &lock)?;
        let source = overlay
            .iter()
            .find(|item| item.session_id == "overlay-source")
            .expect("source overlay");
        assert_eq!(source.clone_state, "has_clone");

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn overlay_does_not_offer_provider_sync_when_source_rollout_is_missing() -> AppResult<()> {
        let codex = temp_codex_dir("cc-session-manager-overlay-missing-rollout-test");
        fs::create_dir_all(&codex)?;
        fs::write(
            paths::session_index_path(&codex),
            "{\"id\":\"missing-active\"}\n",
        )?;
        let conn = rusqlite::Connection::open(paths::state_db_path(&codex))?;
        conn.execute(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT,
                model_provider TEXT,
                source TEXT,
                archived INTEGER
            )",
            [],
        )?;
        conn.execute(
            "INSERT INTO threads (id, rollout_path, model_provider, source, archived)
             VALUES (?1, ?2, 'openai', 'cli', 0)",
            params![
                "missing-active",
                codex
                    .join("sessions/2026/04/24/rollout-missing-active.jsonl")
                    .to_string_lossy()
            ],
        )?;
        let family = Family {
            family_id: "missing-family".to_string(),
            root_id: "missing-active".to_string(),
            title: "missing rollout".to_string(),
            chain: vec![FamilyBranch {
                id: "missing-active".to_string(),
                provider: "openai".to_string(),
                created_at: "2026-04-24T00:00:00Z".to_string(),
                status: BranchStatus::Active,
                rollout_relpath: "sessions/2026/04/24/rollout-missing-active.jsonl".to_string(),
                sha256: None,
                line_count: None,
                note: None,
                archive_origin: None,
            }],
            active_id: "missing-active".to_string(),
            updated_at: "2026-04-24T00:00:00Z".to_string(),
        };
        let mut families = BTreeMap::new();
        families.insert("missing-family".to_string(), family);
        let mut index = BTreeMap::new();
        index.insert("missing-active".to_string(), "missing-family".to_string());
        save(
            &codex,
            &FamilyStore {
                version: 1,
                families,
                index,
            },
        )?;
        drop(conn);

        let overlay = get_session_family_overlay_with_lock(
            codex.to_string_lossy().into_owned(),
            &FamilyLock::default(),
        )?;
        let item = overlay
            .iter()
            .find(|item| item.session_id == "missing-active")
            .expect("missing thread overlay");
        assert_eq!(
            item.clone_state, "unknown",
            "缺少源 rollout 的 orphan 记录应交给 orphan 清理，而不是展示必然失败的同步操作"
        );

        fs::remove_dir_all(&codex).ok();
        Ok(())
    }
}
