use std::fs;
use std::path::Path;

use chrono::Utc;
use serde_json::{json, Map, Value};

use crate::error::{AppError, AppResult};
use crate::paths;

mod desktop_guard;
mod desktop_thread_state;
mod state_store;

pub(crate) use state_store::StateMutationReceipt;
#[cfg(test)]
pub(crate) use state_store::StateWriteConflictTestGuard;
use state_store::{load_state, mutate_existing_state_with_receipt, validate_state_file_metadata};
#[cfg(test)]
use state_store::{write_state_bytes_if_unchanged, StatePostCommitErrorTestGuard};

#[cfg(test)]
use crate::atomic_file;
#[cfg(test)]
pub(crate) use desktop_guard::DesktopTestProbeGuard;
#[cfg(test)]
use desktop_guard::{
    is_linux_desktop_candidate, is_linux_desktop_executable, is_macos_desktop_executable,
    is_official_windows_desktop, is_windows_desktop_candidate, official_desktop_is_running,
    TestDesktopProbe,
};
#[cfg(test)]
use std::path::PathBuf;

const LOCAL_PROJECTS: &str = "local-projects";
const PROJECT_ORDER: &str = "project-order";
const THREAD_ASSIGNMENTS: &str = "thread-project-assignments";
const PROJECTLESS_THREADS: &str = "projectless-thread-ids";

/// Refuse external writes while the official Desktop process owns this state in memory.
///
/// Codex Desktop does not reload `.codex-global-state.json` after another process changes it and
/// can later overwrite that change from its stale in-memory copy. This check intentionally looks
/// only for the packaged GUI executable, never `codex`/`codex.exe`, so CLI and app-server
/// processes do not block session management.
pub(crate) fn ensure_desktop_not_running(codex: &Path) -> AppResult<()> {
    let state_path = paths::codex_global_state_json_path(codex);
    match fs::symlink_metadata(&state_path) {
        Ok(metadata) => {
            validate_state_file_metadata(&state_path, &metadata)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    desktop_guard::ensure_official_desktop_not_running()
}

/// Whether this Codex home has Desktop-owned state that would be mutated by these workflows.
pub(crate) fn desktop_state_initialized(codex: &Path) -> AppResult<bool> {
    let state_path = paths::codex_global_state_json_path(codex);
    match fs::symlink_metadata(&state_path) {
        Ok(metadata) => {
            validate_state_file_metadata(&state_path, &metadata)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Delete/prune may safely defer Desktop-owned project-state cleanup. If Desktop is running or
/// its process state cannot be determined, callers should remove Core data without writing the
/// private global state and tell the user to restart Desktop before relying on its cached list.
pub(crate) fn should_defer_desktop_state_cleanup() -> bool {
    desktop_guard::official_desktop_is_running().unwrap_or(true)
}

/// Remove current Desktop catalog/summary rows after Core deletion.
///
/// Unlike the global JSON state, these SQLite stores coordinate concurrent writers. They can be
/// updated while Desktop is running; its already-rendered list still requires a restart to refresh.
pub(crate) fn clear_deleted_thread_cache_rows(
    codex: &Path,
    thread_ids: &[String],
) -> AppResult<()> {
    desktop_thread_state::clear_deleted_thread_rows(codex, thread_ids)
}

/// Validate the Desktop state that a delete/prune operation will later clean up, without writing.
///
/// These workflows remove Core data before their project-state cleanup. Loading and applying the
/// same cleanup to an in-memory clone surfaces malformed JSON/current field shapes before that
/// first destructive Core write. A missing state file intentionally remains a no-op.
pub(crate) fn preflight_thread_project_state_cleanup(
    codex: &Path,
    thread_ids: &[String],
) -> AppResult<()> {
    for thread_id in thread_ids {
        validate_thread_id(thread_id)?;
    }
    let Some(snapshot) = load_state(codex)? else {
        return Ok(());
    };
    let mut root = snapshot.root;
    let state = root.as_object_mut().ok_or_else(|| {
        AppError::Other(format!(
            "Codex 全局状态必须是 JSON 对象: {}",
            snapshot.path.to_string_lossy()
        ))
    })?;
    validate_thread_project_state_cleanup(state, thread_ids)
}

/// Synchronize a Codex thread's Desktop project membership with its persisted cwd.
///
/// Newer Codex Desktop builds group threads by `thread-project-assignments`, not by the
/// rollout/SQLite cwd alone. Existing local projects are matched by their `rootPaths` first. If
/// no project covers `cwd`, this registers a local project using Desktop's current UUID ID
/// convention, then assigns the thread to it. Workspace-root hints are deliberately left alone:
/// current Desktop move operations do not use that independent cache. A missing global-state
/// file means Desktop has not initialized this Codex home yet and is intentionally a no-op.
#[cfg(test)]
fn sync_thread_project_assignment(codex: &Path, thread_id: &str, cwd: &str) -> AppResult<()> {
    sync_thread_project_assignments(codex, &[thread_id.to_string()], cwd)?;
    Ok(())
}

/// Synchronize one thread and return a receipt that can conditionally undo exactly this write.
///
/// The receipt snapshots the bytes used by the successful compare-and-swap attempt, rather than
/// an earlier caller snapshot. Compensation therefore preserves Desktop changes that won a retry
/// race before our write, and refuses to overwrite changes made after our write.
pub(crate) fn sync_thread_project_assignment_with_receipt(
    codex: &Path,
    thread_id: &str,
    cwd: &str,
) -> AppResult<Option<StateMutationReceipt>> {
    sync_thread_project_assignments_with_receipt(codex, &[thread_id.to_string()], cwd)
}

/// Assign several related threads through one read and one compare-and-swap write.
#[cfg(test)]
fn sync_thread_project_assignments(
    codex: &Path,
    thread_ids: &[String],
    cwd: &str,
) -> AppResult<bool> {
    if thread_ids.is_empty() {
        return Ok(false);
    }
    for thread_id in thread_ids {
        validate_thread_id(thread_id)?;
    }
    let records = thread_ids
        .iter()
        .map(|thread_id| (thread_id.clone(), cwd.to_string()))
        .collect::<Vec<_>>();
    sync_thread_project_assignment_records(codex, &records)
}

/// Assign several related threads and return a conditional compensation receipt.
pub(crate) fn sync_thread_project_assignments_with_receipt(
    codex: &Path,
    thread_ids: &[String],
    cwd: &str,
) -> AppResult<Option<StateMutationReceipt>> {
    if thread_ids.is_empty() {
        return Ok(None);
    }
    for thread_id in thread_ids {
        validate_thread_id(thread_id)?;
    }
    let records = thread_ids
        .iter()
        .map(|thread_id| (thread_id.clone(), cwd.to_string()))
        .collect::<Vec<_>>();
    sync_project_assignment_records_with_receipt(codex, &records, false)
}

/// Synchronize several independent thread/cwd pairs with one global-state compare-and-swap.
/// Callers must pass Desktop-visible host paths (for WSL, the host UNC path), not Core paths.
#[cfg(test)]
fn sync_thread_project_assignment_records(
    codex: &Path,
    records: &[(String, String)],
) -> AppResult<bool> {
    sync_project_assignment_records(codex, records, false)
}

/// Validate that the current Desktop state can accept an assignment without writing it.
///
/// Bundle import calls this before touching rollout/history/SQLite so malformed current state or a
/// malformed project collection cannot turn a visibility failure into a partial import.
pub(crate) fn validate_thread_project_assignment(
    codex: &Path,
    thread_id: &str,
    cwd: &str,
) -> AppResult<()> {
    validate_thread_project_assignments(codex, &[thread_id.to_string()], cwd)
}

/// Validate a related thread batch with the exact all-assignment semantics used by move.
pub(crate) fn validate_thread_project_assignments(
    codex: &Path,
    thread_ids: &[String],
    cwd: &str,
) -> AppResult<()> {
    let records = thread_ids
        .iter()
        .map(|thread_id| (thread_id.clone(), cwd.to_string()))
        .collect::<Vec<_>>();
    validate_project_assignment_records(codex, &records, false)
}

/// Validate a repair batch using the exact missing-only mutation semantics, without writing.
pub(crate) fn validate_missing_thread_project_assignment_records(
    codex: &Path,
    records: &[(String, String)],
) -> AppResult<()> {
    validate_project_assignment_records(codex, records, true)
}

/// Fill only threads that have neither a valid project membership nor an explicit projectless state.
///
/// Rollout cwd may lag behind Desktop-owned state. Existing assignments, including pending ones,
/// and an explicit `projectless-thread-ids` entry are therefore preserved byte-for-byte. A
/// malformed current field aborts the mutation instead of being silently overwritten.
#[cfg(test)]
fn sync_missing_thread_project_assignment_records(
    codex: &Path,
    records: &[(String, String)],
) -> AppResult<bool> {
    sync_project_assignment_records(codex, records, true)
}

/// Fill missing memberships and return a receipt for transaction compensation.
pub(crate) fn sync_missing_thread_project_assignment_records_with_receipt(
    codex: &Path,
    records: &[(String, String)],
) -> AppResult<Option<StateMutationReceipt>> {
    sync_project_assignment_records_with_receipt(codex, records, true)
}

#[cfg(test)]
fn sync_project_assignment_records(
    codex: &Path,
    records: &[(String, String)],
    only_missing: bool,
) -> AppResult<bool> {
    Ok(sync_project_assignment_records_with_receipt(codex, records, only_missing)?.is_some())
}

fn sync_project_assignment_records_with_receipt(
    codex: &Path,
    records: &[(String, String)],
    only_missing: bool,
) -> AppResult<Option<StateMutationReceipt>> {
    if records.is_empty() {
        return Ok(None);
    }
    for (thread_id, _) in records {
        validate_thread_id(thread_id)?;
    }
    let result = mutate_existing_state_with_receipt(codex, |state| {
        apply_project_assignment_records(state, records, only_missing)
    })?;
    Ok(result.and_then(|(_, receipt)| receipt))
}

fn validate_project_assignment_records(
    codex: &Path,
    records: &[(String, String)],
    only_missing: bool,
) -> AppResult<()> {
    if records.is_empty() {
        return Ok(());
    }
    for (thread_id, _) in records {
        validate_thread_id(thread_id)?;
    }
    let Some(mut snapshot) = load_state(codex)? else {
        return Ok(());
    };
    let state = snapshot.root.as_object_mut().ok_or_else(|| {
        AppError::Other(format!(
            "Codex 全局状态必须是 JSON 对象: {}",
            snapshot.path.to_string_lossy()
        ))
    })?;
    apply_project_assignment_records(state, records, only_missing)
}

fn apply_project_assignment_records(
    state: &mut Map<String, Value>,
    records: &[(String, String)],
    only_missing: bool,
) -> AppResult<()> {
    if only_missing {
        let mut all_assigned = true;
        for (thread_id, raw_cwd) in records {
            if !should_preserve_thread_project_assignment(state, thread_id, raw_cwd)? {
                all_assigned = false;
            }
        }
        if all_assigned {
            return Ok(());
        }
    }
    ensure_current_local_projects(state)?;
    for (thread_id, raw_cwd) in records {
        if only_missing && should_preserve_thread_project_assignment(state, thread_id, raw_cwd)? {
            continue;
        }
        apply_thread_project_assignment(state, thread_id, raw_cwd)?;
    }
    Ok(())
}

fn apply_thread_project_assignment(
    state: &mut Map<String, Value>,
    thread_id: &str,
    raw_cwd: &str,
) -> AppResult<()> {
    ensure_current_local_projects(state)?;
    let Some(cwd) = prepare_stored_path(raw_cwd) else {
        return mark_thread_projectless(state, thread_id);
    };
    let project_id = match find_local_project(state, &cwd)? {
        Some(project) => project.id,
        None => register_local_project(state, &cwd)?,
    };
    assign_thread(state, thread_id, &project_id, &cwd)?;
    ensure_project_order(state, &project_id)
}

fn should_preserve_thread_project_assignment(
    state: &Map<String, Value>,
    thread_id: &str,
    core_cwd: &str,
) -> AppResult<bool> {
    match state.get(PROJECTLESS_THREADS) {
        Some(Value::Array(projectless)) => {
            if projectless
                .iter()
                .any(|entry| entry.as_str() == Some(thread_id))
            {
                return Ok(true);
            }
        }
        Some(_) => {
            return Err(AppError::Other(format!(
                "Codex 全局状态字段 {PROJECTLESS_THREADS} 必须是数组"
            )))
        }
        None => {}
    }

    let Some(assignments) = state.get(THREAD_ASSIGNMENTS) else {
        return Ok(false);
    };
    let assignments = assignments.as_object().ok_or_else(|| {
        AppError::Other(format!(
            "Codex 全局状态字段 {THREAD_ASSIGNMENTS} 必须是对象"
        ))
    })?;
    let Some(assignment) = assignments.get(thread_id) else {
        return Ok(false);
    };
    let assignment = assignment
        .as_object()
        .ok_or_else(|| AppError::Other(format!("Codex 会话 {thread_id} 的项目归属必须是对象")))?;
    // ChatGPT/cloud workspace membership is owned by Desktop even when it carries fields that
    // resemble a local assignment. Core repair must never coerce it into a local project.
    if assignment.get("projectOrigin").and_then(Value::as_str) == Some("chatgpt") {
        return Ok(true);
    }
    let Some(project_kind) = assignment
        .get("projectKind")
        .and_then(Value::as_str)
        .filter(|kind| !kind.trim().is_empty())
    else {
        return Ok(false);
    };
    // Unknown/non-local kinds are owned by Desktop or future versions. Never coerce them into a
    // local project during a Core index repair, and do not require local-only fields from them.
    if project_kind != "local" {
        return Ok(true);
    }
    let Some(project_id) = assignment
        .get("projectId")
        .and_then(Value::as_str)
        .filter(|project_id| !project_id.trim().is_empty())
    else {
        return Ok(false);
    };

    // An official Desktop move may intentionally lead rollout/SQLite until the next Core turn.
    if assignment.get("pendingCoreUpdate").and_then(Value::as_bool) == Some(true) {
        return Ok(true);
    }
    if assignment.get("pendingCoreUpdate").and_then(Value::as_bool) != Some(false) {
        return Ok(false);
    }

    let Some(expected_cwd) = prepare_stored_path(core_cwd) else {
        return Ok(false);
    };
    let Some(assigned_cwd) = assignment
        .get("cwd")
        .and_then(Value::as_str)
        .and_then(prepare_stored_path)
    else {
        return Ok(false);
    };
    if normalize_path_for_compare(&assigned_cwd) != normalize_path_for_compare(&expected_cwd) {
        return Ok(false);
    }

    let Some(projects) = state.get(LOCAL_PROJECTS) else {
        return Ok(false);
    };
    let projects = projects.as_object().ok_or_else(|| {
        AppError::Other(format!("Codex 全局状态字段 {LOCAL_PROJECTS} 必须是对象"))
    })?;
    let Some(project) = projects.get(project_id) else {
        return Ok(false);
    };
    let Some(project) = project.as_object() else {
        return Ok(false);
    };
    if project.get("id").and_then(Value::as_str) != Some(project_id) {
        return Ok(false);
    }
    let Some(roots) = project.get("rootPaths").and_then(Value::as_array) else {
        return Ok(false);
    };
    Ok(roots
        .iter()
        .filter_map(Value::as_str)
        .any(|root| matching_root_len(&assigned_cwd, root).is_some()))
}

/// Remove current-protocol per-thread Desktop project metadata after a thread is deleted.
///
/// Project definitions and project ordering are deliberately retained because other threads may
/// still use them. Unknown or legacy fields are deliberately left untouched. Returns whether an
/// existing global-state file was changed.
#[cfg(test)]
pub(crate) fn clear_thread_project_state(codex: &Path, thread_id: &str) -> AppResult<bool> {
    clear_thread_project_states(codex, &[thread_id.to_string()])
}

/// Clear project metadata for a complete thread family as one state-file mutation.
#[cfg(test)]
pub(crate) fn clear_thread_project_states(codex: &Path, thread_ids: &[String]) -> AppResult<bool> {
    Ok(clear_thread_project_states_with_receipt(codex, thread_ids)?.is_some())
}

/// Clear project metadata and return an exact CAS compensation receipt when bytes changed.
pub(crate) fn clear_thread_project_states_with_receipt(
    codex: &Path,
    thread_ids: &[String],
) -> AppResult<Option<StateMutationReceipt>> {
    if thread_ids.is_empty() {
        return Ok(None);
    }
    for thread_id in thread_ids {
        validate_thread_id(thread_id)?;
    }
    let result = mutate_existing_state_with_receipt(codex, |state| {
        clear_thread_project_state_fields(state, thread_ids)
    })?;
    Ok(result.and_then(|(_, receipt)| receipt))
}

fn validate_thread_project_state_cleanup(
    state: &mut Map<String, Value>,
    thread_ids: &[String],
) -> AppResult<()> {
    clear_thread_project_state_fields(state, thread_ids).map(|_| ())
}

fn clear_thread_project_state_fields(
    state: &mut Map<String, Value>,
    thread_ids: &[String],
) -> AppResult<bool> {
    let mut changed = false;
    for thread_id in thread_ids {
        changed |= remove_object_member(state, THREAD_ASSIGNMENTS, thread_id)?;
        changed |= remove_string_from_array(state, PROJECTLESS_THREADS, thread_id)?;
    }
    Ok(changed)
}

/// Read Desktop's pending cwd override for a thread.
///
/// Only a valid local assignment with `pendingCoreUpdate: true` can be newer than Core. Ordinary
/// assignments may be stale after an older manager changed rollout/SQLite without updating
/// Desktop, so backup/export callers must not prefer their cwd. A missing or non-pending
/// assignment is represented as `None`; malformed current-state fields are surfaced as errors.
/// Current Desktop assignments may expose the pending location as `cwd` or `path`; prefer a
/// non-empty `cwd` and fall back to a non-empty `path` when `cwd` is absent or empty.
pub(crate) fn pending_thread_project_assignment_cwd(
    codex: &Path,
    thread_id: &str,
) -> AppResult<Option<String>> {
    validate_thread_id(thread_id)?;
    let Some(snapshot) = load_state(codex)? else {
        return Ok(None);
    };
    let state = snapshot.root.as_object().ok_or_else(|| {
        AppError::Other(format!(
            "Codex 全局状态必须是 JSON 对象: {}",
            snapshot.path.to_string_lossy()
        ))
    })?;
    let Some(assignments) = state.get(THREAD_ASSIGNMENTS) else {
        return Ok(None);
    };
    let assignments = assignments.as_object().ok_or_else(|| {
        AppError::Other(format!(
            "Codex 全局状态字段 {THREAD_ASSIGNMENTS} 必须是对象"
        ))
    })?;
    let Some(assignment) = assignments.get(thread_id) else {
        return Ok(None);
    };
    let assignment = assignment
        .as_object()
        .ok_or_else(|| AppError::Other(format!("Codex 会话 {thread_id} 的项目归属必须是对象")))?;
    let project_kind = assignment
        .get("projectKind")
        .and_then(Value::as_str)
        .filter(|kind| !kind.trim().is_empty())
        .ok_or_else(|| {
            AppError::Other(format!(
                "Codex 会话 {thread_id} 的 projectKind 必须是字符串"
            ))
        })?;
    if project_kind != "local" {
        return Ok(None);
    }
    assignment
        .get("projectId")
        .and_then(Value::as_str)
        .filter(|project_id| !project_id.trim().is_empty())
        .ok_or_else(|| {
            AppError::Other(format!("Codex 会话 {thread_id} 的 projectId 必须是字符串"))
        })?;
    let pending = assignment
        .get("pendingCoreUpdate")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            AppError::Other(format!(
                "Codex 会话 {thread_id} 的 pendingCoreUpdate 必须是布尔值"
            ))
        })?;
    if !pending {
        return Ok(None);
    }
    match assignment.get("cwd") {
        Some(Value::String(cwd)) if !cwd.trim().is_empty() => return Ok(Some(cwd.clone())),
        None | Some(Value::Null) | Some(Value::String(_)) => {}
        Some(_) => {
            return Err(AppError::Other(format!(
                "Codex 会话 {thread_id} 的项目归属 cwd 必须是字符串"
            )));
        }
    }
    match assignment.get("path") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(path)) if path.trim().is_empty() => Ok(None),
        Some(Value::String(path)) => Ok(Some(path.clone())),
        Some(_) => Err(AppError::Other(format!(
            "Codex 会话 {thread_id} 的项目归属 path 必须是字符串"
        ))),
    }
}

fn validate_thread_id(thread_id: &str) -> AppResult<()> {
    if thread_id.trim().is_empty() {
        Err(AppError::Path("Codex 会话 ID 不能为空".to_string()))
    } else {
        Ok(())
    }
}

#[derive(Debug)]
struct LocalProjectMatch {
    id: String,
    matched_root_len: usize,
}

fn find_local_project(
    state: &Map<String, Value>,
    cwd: &str,
) -> AppResult<Option<LocalProjectMatch>> {
    let Some(projects) = state.get(LOCAL_PROJECTS) else {
        return Ok(None);
    };
    let mut best = None;
    match projects {
        Value::Object(projects) => {
            for (map_id, project) in projects {
                consider_project(&mut best, map_id, project, cwd);
            }
        }
        _ => {
            return Err(AppError::Other(format!(
                "Codex 全局状态字段 {LOCAL_PROJECTS} 必须是对象"
            )))
        }
    }
    Ok(best)
}

fn ensure_current_local_projects(state: &mut Map<String, Value>) -> AppResult<()> {
    let Some(current) = state.get_mut(LOCAL_PROJECTS) else {
        state.insert(LOCAL_PROJECTS.to_string(), Value::Object(Map::new()));
        return Ok(());
    };
    match current {
        Value::Object(_) => Ok(()),
        _ => Err(AppError::Other(format!(
            "Codex 全局状态字段 {LOCAL_PROJECTS} 必须是对象"
        ))),
    }
}

fn consider_project(
    best: &mut Option<LocalProjectMatch>,
    map_id: &String,
    project: &Value,
    cwd: &str,
) {
    if !project_has_identity(project, map_id) {
        return;
    }
    let project = project
        .as_object()
        .expect("identity check requires an object");
    let Some(roots) = project.get("rootPaths").and_then(Value::as_array) else {
        return;
    };
    for root in roots.iter().filter_map(Value::as_str) {
        let Some(root_len) = matching_root_len(cwd, root) else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|current| root_len > current.matched_root_len)
        {
            *best = Some(LocalProjectMatch {
                id: map_id.to_string(),
                matched_root_len: root_len,
            });
        }
    }
}

fn matching_root_len(cwd: &str, root: &str) -> Option<usize> {
    let cwd = normalize_path_for_compare(cwd);
    let root = normalize_path_for_compare(root);
    if cwd == root {
        return Some(root.len());
    }
    let prefix = format!("{}/", root.trim_end_matches('/'));
    cwd.starts_with(&prefix).then_some(root.len())
}

fn register_local_project(state: &mut Map<String, Value>, cwd: &str) -> AppResult<String> {
    let id = new_local_project_id()?;
    let now = Utc::now().timestamp_millis();
    let project = json!({
        "id": id,
        "name": project_name(cwd),
        "rootPaths": [cwd],
        "createdAt": now,
        "updatedAt": now,
    });

    match state
        .entry(LOCAL_PROJECTS.to_string())
        .or_insert_with(|| Value::Object(Map::new()))
    {
        Value::Object(projects) => {
            if projects.contains_key(&id) {
                return Err(AppError::Other(format!(
                    "生成的 Codex 本地项目 UUID 已存在，已拒绝覆盖现有项目: {id}"
                )));
            }
            projects.insert(id.clone(), project);
        }
        _ => {
            return Err(AppError::Other(format!(
                "Codex 全局状态字段 {LOCAL_PROJECTS} 必须是对象"
            )))
        }
    }
    Ok(id)
}

fn project_has_identity(project: &Value, expected_id: &str) -> bool {
    project.get("id").and_then(Value::as_str) == Some(expected_id)
}

fn new_local_project_id() -> AppResult<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| AppError::Other(format!("生成 Codex 本地项目 UUID 失败: {error}")))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

fn project_name(cwd: &str) -> String {
    let unified = cwd.replace('\\', "/");
    let trimmed = unified.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .map(str::to_string)
        .filter(|name| !name.ends_with(':'))
        .unwrap_or_else(|| cwd.to_string())
}

fn assign_thread(
    state: &mut Map<String, Value>,
    thread_id: &str,
    project_id: &str,
    cwd: &str,
) -> AppResult<()> {
    let assignments = object_field_mut(state, THREAD_ASSIGNMENTS)?;
    let assignment = assignments
        .entry(thread_id.to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| AppError::Other(format!("Codex 会话 {thread_id} 的项目归属必须是对象")))?;
    // Desktop owns this object and may add fields in later releases. Update only the current
    // local membership contract so forward-compatible metadata survives a move/import/repair.
    // Known remote/ChatGPT discriminators are mutually exclusive with a local assignment and
    // must not survive a conversion.
    for field in ["projectOrigin", "hostId", "path"] {
        assignment.remove(field);
    }
    assignment.insert(
        "projectKind".to_string(),
        Value::String("local".to_string()),
    );
    assignment.insert(
        "projectId".to_string(),
        Value::String(project_id.to_string()),
    );
    assignment.insert("cwd".to_string(), Value::String(cwd.to_string()));
    assignment.insert("pendingCoreUpdate".to_string(), Value::Bool(false));
    remove_string_from_array(state, PROJECTLESS_THREADS, thread_id)?;
    Ok(())
}

fn mark_thread_projectless(state: &mut Map<String, Value>, thread_id: &str) -> AppResult<()> {
    remove_object_member(state, THREAD_ASSIGNMENTS, thread_id)?;
    let projectless = array_field_mut(state, PROJECTLESS_THREADS)?;
    if !projectless
        .iter()
        .any(|entry| entry.as_str() == Some(thread_id))
    {
        projectless.push(Value::String(thread_id.to_string()));
    }
    Ok(())
}

fn ensure_project_order(state: &mut Map<String, Value>, project_id: &str) -> AppResult<()> {
    let order = array_field_mut(state, PROJECT_ORDER)?;
    if !order.iter().any(|entry| entry.as_str() == Some(project_id)) {
        order.insert(0, Value::String(project_id.to_string()));
    }
    Ok(())
}

fn object_field_mut<'a>(
    state: &'a mut Map<String, Value>,
    key: &str,
) -> AppResult<&'a mut Map<String, Value>> {
    let value = state
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    value
        .as_object_mut()
        .ok_or_else(|| AppError::Other(format!("Codex 全局状态字段 {key} 必须是对象")))
}

fn array_field_mut<'a>(
    state: &'a mut Map<String, Value>,
    key: &str,
) -> AppResult<&'a mut Vec<Value>> {
    let value = state
        .entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    value
        .as_array_mut()
        .ok_or_else(|| AppError::Other(format!("Codex 全局状态字段 {key} 必须是数组")))
}

fn remove_object_member(
    state: &mut Map<String, Value>,
    key: &str,
    member: &str,
) -> AppResult<bool> {
    let Some(value) = state.get_mut(key) else {
        return Ok(false);
    };
    let object = value
        .as_object_mut()
        .ok_or_else(|| AppError::Other(format!("Codex 全局状态字段 {key} 必须是对象")))?;
    Ok(object.remove(member).is_some())
}

fn remove_string_from_array(
    state: &mut Map<String, Value>,
    key: &str,
    target: &str,
) -> AppResult<bool> {
    let Some(value) = state.get_mut(key) else {
        return Ok(false);
    };
    let entries = value
        .as_array_mut()
        .ok_or_else(|| AppError::Other(format!("Codex 全局状态字段 {key} 必须是数组")))?;
    let before = entries.len();
    entries.retain(|entry| entry.as_str() != Some(target));
    Ok(entries.len() != before)
}

fn prepare_stored_path(raw: &str) -> Option<String> {
    let stripped = paths::strip_verbatim(raw.trim());
    if stripped.is_empty() {
        return None;
    }
    let mut stored = if looks_like_windows_path(&stripped) {
        stripped.replace('/', "\\")
    } else {
        stripped
    };
    if stored.as_bytes().get(1) == Some(&b':') {
        stored.replace_range(0..1, &stored[..1].to_ascii_uppercase());
    }
    trim_trailing_separators(&mut stored);
    Some(stored)
}

fn trim_trailing_separators(path: &mut String) {
    let is_unix_root = path == "/";
    let is_drive_root = path.len() == 3
        && path.as_bytes().get(1) == Some(&b':')
        && matches!(path.as_bytes().get(2), Some(b'\\') | Some(b'/'));
    if !is_unix_root && !is_drive_root {
        while path.len() > 1 && (path.ends_with('/') || path.ends_with('\\')) {
            path.pop();
        }
    }
}

fn normalize_path_for_compare(raw: &str) -> String {
    let stripped = paths::strip_verbatim(raw.trim());
    let windows = looks_like_windows_path(&stripped);
    let mut normalized = stripped.replace('\\', "/");
    trim_trailing_separators(&mut normalized);
    if windows {
        normalized.make_ascii_lowercase();
    }
    normalized
}

fn looks_like_windows_path(path: &str) -> bool {
    (path.as_bytes().get(1) == Some(&b':')
        && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic))
        || path.starts_with("\\\\")
        || path.starts_with("//")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> AppResult<Self> {
            let path = std::env::temp_dir().join(format!(
                "cc-session-manager-{label}-{}-{}",
                std::process::id(),
                Utc::now().timestamp_nanos_opt().unwrap()
            ));
            fs::create_dir_all(&path)?;
            Ok(Self(path))
        }

        fn state_path(&self) -> PathBuf {
            paths::codex_global_state_json_path(&self.0)
        }

        fn write_state(&self, value: &Value) -> AppResult<()> {
            fs::write(self.state_path(), serde_json::to_vec(value)?)?;
            Ok(())
        }

        fn read_state(&self) -> AppResult<Value> {
            Ok(serde_json::from_slice(&fs::read(self.state_path())?)?)
        }
    }

    fn create_state_path_link(target: &Path, link: &Path) -> AppResult<()> {
        #[cfg(windows)]
        {
            match std::os::windows::fs::symlink_dir(target, link) {
                Ok(()) => return Ok(()),
                Err(error) if error.raw_os_error() == Some(1314) => {
                    let output = std::process::Command::new("pwsh")
                        .args([
                            "-NoProfile",
                            "-NonInteractive",
                            "-Command",
                            "$ErrorActionPreference = 'Stop'; New-Item -ItemType Junction -Path $env:CC_TEST_LINK -Target $env:CC_TEST_TARGET | Out-Null",
                        ])
                        .env("CC_TEST_LINK", link)
                        .env("CC_TEST_TARGET", target)
                        .output()?;
                    if output.status.success() {
                        return Ok(());
                    }
                    return Err(AppError::Other(format!(
                        "无法创建全局状态 junction 测试夹具: {}",
                        String::from_utf8_lossy(&output.stderr)
                    )));
                }
                Err(error) => Err(error.into()),
            }
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)?;
            Ok(())
        }
    }

    fn remove_state_path_link(link: &Path) -> AppResult<()> {
        #[cfg(windows)]
        fs::remove_dir(link)?;
        #[cfg(unix)]
        fs::remove_file(link)?;
        Ok(())
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn desktop_process_candidates_are_exact_and_do_not_match_cli() {
        assert!(is_windows_desktop_candidate("ChatGPT.exe"));
        assert!(is_windows_desktop_candidate("chatgpt.EXE"));
        assert!(!is_windows_desktop_candidate("codex.exe"));
        assert!(!is_windows_desktop_candidate("cc-session-manager.exe"));
        assert!(!is_windows_desktop_candidate("ChatGPT-helper.exe"));
        assert!(is_official_windows_desktop(
            "ChatGPT.exe",
            Some("OpenAI.Codex_2p2nqsd0c76g0")
        ));
        assert!(!is_official_windows_desktop("ChatGPT.exe", None));
        assert!(!is_official_windows_desktop(
            "ChatGPT.exe",
            Some("Unrelated.Package_family")
        ));
        assert!(!is_official_windows_desktop(
            "codex.exe",
            Some("OpenAI.Codex_2p2nqsd0c76g0")
        ));

        assert!(is_linux_desktop_candidate("ChatGPT\n"));
        assert!(!is_linux_desktop_candidate("chatgpt\n"));
        assert!(!is_linux_desktop_candidate("codex\n"));

        assert!(is_macos_desktop_executable(
            "/Applications/Codex.app/Contents/MacOS/Codex"
        ));
        assert!(is_macos_desktop_executable(
            "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"
        ));
        assert!(is_macos_desktop_executable(
            "/Users/dev/Applications/Codex.app/Contents/MacOS/ChatGPT"
        ));
        assert!(!is_macos_desktop_executable(
            "/Applications/Codex.app/Contents/MacOS/Codex Helper"
        ));
        assert!(!is_macos_desktop_executable(
            "/Applications/Other.app/Contents/MacOS/Codex"
        ));
        assert!(!is_macos_desktop_executable("/usr/local/bin/codex"));
    }

    #[test]
    fn linux_desktop_executable_requires_official_install_path() {
        let expected = "/usr/lib/chatgpt/ChatGPT";
        assert!(is_linux_desktop_executable(Path::new(expected), expected));
        assert!(is_linux_desktop_executable(
            Path::new("/usr/lib/chatgpt/ChatGPT (deleted)"),
            expected
        ));
        assert!(!is_linux_desktop_executable(
            Path::new("/tmp/ChatGPT"),
            expected
        ));
    }

    #[test]
    fn running_after_not_running_probe_is_deterministic() -> AppResult<()> {
        let _desktop = DesktopTestProbeGuard::running_after_not_running(2);
        assert!(!official_desktop_is_running()?);
        assert!(!official_desktop_is_running()?);
        assert!(official_desktop_is_running()?);
        Ok(())
    }

    #[test]
    fn running_desktop_rejects_state_mutation_without_changing_bytes() -> AppResult<()> {
        let codex = TestDir::new("desktop-running")?;
        codex.write_state(&json!({"future-field": {"keep": true}}))?;
        let before = fs::read(codex.state_path())?;
        let _desktop = DesktopTestProbeGuard::running();

        let error = sync_thread_project_assignment(&codex.0, "thread-1", r"F:\work")
            .expect_err("running Desktop must own its global state");

        assert!(error.to_string().contains("完全退出桌面应用"), "{error}");
        assert_eq!(fs::read(codex.state_path())?, before);
        Ok(())
    }

    #[test]
    fn missing_global_state_is_noop_even_when_desktop_is_running() -> AppResult<()> {
        let codex = TestDir::new("desktop-running-no-state")?;
        let _desktop = DesktopTestProbeGuard::running();

        assert!(!sync_thread_project_assignments(
            &codex.0,
            &["thread-1".to_string()],
            r"F:\work",
        )?);
        assert!(!codex.state_path().exists());
        Ok(())
    }

    #[test]
    fn linked_global_state_is_rejected_without_touching_target() -> AppResult<()> {
        let codex = TestDir::new("linked-global-state")?;
        let external = codex.0.join("external-state");
        fs::create_dir(&external)?;
        let sentinel = external.join("sentinel.json");
        fs::write(&sentinel, serde_json::to_vec(&json!({"external": "keep"}))?)?;
        create_state_path_link(&external, &codex.state_path())?;
        let before = fs::read(&sentinel)?;

        let error = sync_thread_project_assignment(&codex.0, "thread-1", r"F:\work")
            .expect_err("Desktop global state must never be read or rewritten through a link");

        assert!(
            error.to_string().contains("链接") || error.to_string().contains("junction"),
            "{error}"
        );
        assert_eq!(fs::read(&sentinel)?, before);
        remove_state_path_link(&codex.state_path())?;
        Ok(())
    }

    #[test]
    fn desktop_starting_before_commit_rejects_write() -> AppResult<()> {
        let codex = TestDir::new("desktop-start-before-commit")?;
        codex.write_state(&json!({}))?;
        let before = fs::read(codex.state_path())?;
        let _desktop = DesktopTestProbeGuard::sequence([
            TestDesktopProbe::Running(false),
            TestDesktopProbe::Running(true),
        ]);

        let error = sync_thread_project_assignment(&codex.0, "thread-1", r"F:\work")
            .expect_err("second guard must catch Desktop starting before CAS");

        assert!(error.to_string().contains("完全退出桌面应用"), "{error}");
        assert_eq!(fs::read(codex.state_path())?, before);
        Ok(())
    }

    #[test]
    fn desktop_probe_error_fails_closed() -> AppResult<()> {
        let codex = TestDir::new("desktop-probe-error")?;
        codex.write_state(&json!({}))?;
        let _desktop = DesktopTestProbeGuard::sequence([TestDesktopProbe::Error("probe failed")]);

        let error = sync_thread_project_assignment(&codex.0, "thread-1", r"F:\work")
            .expect_err("unknown Desktop state must reject writes");

        assert!(error.to_string().contains("probe failed"), "{error}");
        Ok(())
    }

    #[test]
    fn assigns_existing_project_and_repairs_new_desktop_fields() -> AppResult<()> {
        let codex = TestDir::new("project-assignment")?;
        codex.write_state(&json!({
            "untouched": {"keep": true},
            LOCAL_PROJECTS: {
                "project-existing": {
                    "id": "project-existing",
                    "name": "Work",
                    "rootPaths": [r"f:/Work/Repo"]
                }
            },
            THREAD_ASSIGNMENTS: {
                "thread-1": {
                    "projectKind": "remote",
                    "projectId": "old",
                    "cwd": r"C:\old",
                    "projectOrigin": "chatgpt",
                    "hostId": "old-host",
                    "path": "/remote/old",
                    "futureField": {"keep": true}
                }
            },
            PROJECTLESS_THREADS: ["thread-1", "thread-1", "other"],
            "thread-workspace-root-hints": {"thread-1": r"C:\old"},
            PROJECT_ORDER: [r"F:\WORK\REPO", "other-project"]
        }))?;

        sync_thread_project_assignment(&codex.0, "thread-1", r"\\?\F:\Work\Repo\")?;

        let state = codex.read_state()?;
        assert_eq!(state["untouched"]["keep"], true);
        let assignment = &state[THREAD_ASSIGNMENTS]["thread-1"];
        assert_eq!(assignment["projectKind"], "local");
        assert_eq!(assignment["projectId"], "project-existing");
        assert_eq!(assignment["cwd"], r"F:\Work\Repo");
        assert_eq!(assignment["pendingCoreUpdate"], false);
        for field in ["projectOrigin", "hostId", "path"] {
            assert!(
                assignment.get(field).is_none(),
                "stale mutually exclusive field {field} must be removed"
            );
        }
        assert_eq!(assignment["futureField"], json!({"keep": true}));
        assert_eq!(state["thread-workspace-root-hints"]["thread-1"], r"C:\old");
        assert_eq!(state[PROJECTLESS_THREADS], json!(["other"]));
        assert_eq!(
            state[PROJECT_ORDER],
            json!(["project-existing", r"F:\WORK\REPO", "other-project"])
        );
        assert!(state.get("electron-saved-workspace-roots").is_none());
        assert!(state.get("active-workspace-roots").is_none());
        Ok(())
    }

    #[test]
    fn longest_root_wins_for_nested_current_projects() -> AppResult<()> {
        let codex = TestDir::new("nested-project-assignment")?;
        codex.write_state(&json!({
            LOCAL_PROJECTS: {
                "parent": {"id": "parent", "rootPaths": [r"F:\work"]},
                "nested": {"id": "nested", "rootPaths": [r"F:\work\repo"]}
            }
        }))?;

        sync_thread_project_assignment(&codex.0, "thread-1", r"f:\WORK\repo\src")?;

        let state = codex.read_state()?;
        assert_eq!(state[THREAD_ASSIGNMENTS]["thread-1"]["projectId"], "nested");
        assert!(state[LOCAL_PROJECTS].is_object());
        Ok(())
    }

    #[test]
    fn mismatched_project_map_key_and_embedded_id_is_not_reused() -> AppResult<()> {
        let codex = TestDir::new("mismatched-local-project-id")?;
        codex.write_state(&json!({
            LOCAL_PROJECTS: {
                "actual-map-key": {
                    "id": "different-embedded-id",
                    "rootPaths": [r"F:\work\repo"]
                }
            }
        }))?;

        sync_thread_project_assignment(&codex.0, "thread-1", r"F:\work\repo")?;

        let state = codex.read_state()?;
        let project_id = state[THREAD_ASSIGNMENTS]["thread-1"]["projectId"]
            .as_str()
            .expect("new project id");
        assert_ne!(project_id, "different-embedded-id");
        assert_ne!(project_id, "actual-map-key");
        assert!(state[LOCAL_PROJECTS].get(project_id).is_some());
        Ok(())
    }

    #[test]
    fn missing_embedded_project_id_is_not_reused() -> AppResult<()> {
        let codex = TestDir::new("missing-local-project-id")?;
        let cwd = r"F:\work\repo";
        codex.write_state(&json!({
            LOCAL_PROJECTS: {
                "project-without-id": {
                    "rootPaths": [cwd],
                    "futureField": {"keep": true}
                }
            }
        }))?;

        sync_thread_project_assignment(&codex.0, "thread-1", cwd)?;

        let state = codex.read_state()?;
        let project_id = state[THREAD_ASSIGNMENTS]["thread-1"]["projectId"]
            .as_str()
            .expect("new project id");
        assert_ne!(project_id, "project-without-id");
        assert_eq!(state[LOCAL_PROJECTS][project_id]["id"], project_id);
        assert_eq!(
            state[LOCAL_PROJECTS]["project-without-id"]["futureField"],
            json!({"keep": true})
        );
        Ok(())
    }

    #[test]
    fn missing_current_fields_are_initialized_without_touching_old_or_unknown_fields(
    ) -> AppResult<()> {
        let codex = TestDir::new("missing-current-project-fields")?;
        let cwd = r"F:\current\Repo";
        codex.write_state(&json!({
            "electron-saved-workspace-roots": [r"F:\old"],
            "active-workspace-roots": [r"F:\old"],
            "unknown-future-field": {"keep": true}
        }))?;

        sync_thread_project_assignment(&codex.0, "thread-1", cwd)?;

        let state = codex.read_state()?;
        assert_eq!(state["electron-saved-workspace-roots"], json!([r"F:\old"]));
        assert_eq!(state["active-workspace-roots"], json!([r"F:\old"]));
        assert_eq!(state["unknown-future-field"], json!({"keep": true}));
        assert!(state[LOCAL_PROJECTS].is_object());
        assert!(state[PROJECT_ORDER].is_array());
        assert!(state[THREAD_ASSIGNMENTS]["thread-1"].is_object());
        Ok(())
    }

    #[test]
    fn rejects_non_current_local_projects_schema_without_rewriting_it() -> AppResult<()> {
        let codex = TestDir::new("reject-old-local-projects-schema")?;
        codex.write_state(&json!({LOCAL_PROJECTS: []}))?;
        let before = fs::read(codex.state_path())?;

        let error = sync_thread_project_assignment(&codex.0, "thread-1", r"F:\work")
            .expect_err("array schema is not part of the current state protocol");

        assert!(error.to_string().contains("必须是对象"));
        assert_eq!(fs::read(codex.state_path())?, before);
        Ok(())
    }

    #[test]
    fn rejects_null_current_project_fields_without_rewriting_state() -> AppResult<()> {
        for field in [
            LOCAL_PROJECTS,
            PROJECT_ORDER,
            THREAD_ASSIGNMENTS,
            PROJECTLESS_THREADS,
        ] {
            let codex = TestDir::new(&format!("reject-null-{field}"))?;
            let mut state = Map::new();
            state.insert(field.to_string(), Value::Null);
            codex.write_state(&Value::Object(state))?;
            let before = fs::read(codex.state_path())?;

            let error = sync_thread_project_assignment(&codex.0, "thread-1", r"F:\work")
                .expect_err("a present current-protocol field must keep its declared JSON type");

            assert!(error.to_string().contains(field), "{field}: {error}");
            assert_eq!(fs::read(codex.state_path())?, before, "{field}");
        }
        Ok(())
    }

    #[test]
    fn batch_assignment_and_cleanup_share_one_project() -> AppResult<()> {
        let codex = TestDir::new("batch-project-assignment")?;
        codex.write_state(&json!({LOCAL_PROJECTS: {}}))?;
        let ids = vec!["thread-1".to_string(), "thread-2".to_string()];

        assert!(sync_thread_project_assignments(
            &codex.0,
            &ids,
            r"F:\work\repo"
        )?);
        let assigned = codex.read_state()?;
        let project_id = assigned[THREAD_ASSIGNMENTS]["thread-1"]["projectId"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            assigned[THREAD_ASSIGNMENTS]["thread-2"]["projectId"],
            project_id
        );

        assert!(clear_thread_project_states(&codex.0, &ids)?);
        let cleared = codex.read_state()?;
        assert!(cleared[THREAD_ASSIGNMENTS].as_object().unwrap().is_empty());
        assert!(cleared[LOCAL_PROJECTS].get(&project_id).is_some());
        Ok(())
    }

    #[test]
    fn missing_only_batch_preserves_pending_official_move_and_repairs_missing_membership(
    ) -> AppResult<()> {
        let codex = TestDir::new("missing-only-project-assignment")?;
        let pending = json!({
            "projectKind": "local",
            "projectId": "official-target",
            "cwd": r"F:\official\target",
            "pendingCoreUpdate": true,
            "futureField": {"keep": true}
        });
        codex.write_state(&json!({
            LOCAL_PROJECTS: {
                "official-target": {
                    "id": "official-target",
                    "rootPaths": [r"F:\official\target"]
                }
            },
            THREAD_ASSIGNMENTS: {
                "pending-thread": pending,
                "invalid-thread": {"cwd": r"F:\stale"}
            }
        }))?;
        let records = vec![
            (
                "pending-thread".to_string(),
                r"F:\stale\rollout".to_string(),
            ),
            ("missing-thread".to_string(), r"F:\new\repo".to_string()),
            (
                "invalid-thread".to_string(),
                r"F:\repaired\repo".to_string(),
            ),
        ];

        assert!(sync_missing_thread_project_assignment_records(
            &codex.0, &records
        )?);

        let state = codex.read_state()?;
        assert_eq!(state[THREAD_ASSIGNMENTS]["pending-thread"], pending);
        assert_eq!(
            state[THREAD_ASSIGNMENTS]["missing-thread"]["cwd"],
            r"F:\new\repo"
        );
        assert_eq!(
            state[THREAD_ASSIGNMENTS]["invalid-thread"]["cwd"],
            r"F:\repaired\repo"
        );
        Ok(())
    }

    #[test]
    fn missing_only_batch_preserves_explicit_projectless_state() -> AppResult<()> {
        let codex = TestDir::new("preserve-explicit-projectless-state")?;
        codex.write_state(&json!({
            LOCAL_PROJECTS: {},
            PROJECT_ORDER: [],
            PROJECTLESS_THREADS: ["projectless-thread"]
        }))?;
        let before = fs::read(codex.state_path())?;

        assert!(!sync_missing_thread_project_assignment_records(
            &codex.0,
            &[(
                "projectless-thread".to_string(),
                r"F:\should-not-become-a-project".to_string(),
            )]
        )?);

        assert_eq!(fs::read(codex.state_path())?, before);
        Ok(())
    }

    #[test]
    fn missing_only_batch_repairs_settled_stale_and_dangling_local_assignments() -> AppResult<()> {
        let codex = TestDir::new("repair-stale-local-project-assignment")?;
        codex.write_state(&json!({
            LOCAL_PROJECTS: {
                "old-project": {"id": "old-project", "rootPaths": [r"F:\old"]},
                "current-project": {"id": "current-project", "rootPaths": [r"F:\current"]}
            },
            THREAD_ASSIGNMENTS: {
                "stale-thread": {
                    "projectKind": "local",
                    "projectId": "old-project",
                    "cwd": r"F:\old",
                    "pendingCoreUpdate": false,
                    "futureField": "keep"
                },
                "dangling-thread": {
                    "projectKind": "local",
                    "projectId": "missing-project",
                    "cwd": r"F:\current",
                    "pendingCoreUpdate": false
                },
                "unrelated-thread": {
                    "projectKind": "local",
                    "projectId": "old-project",
                    "cwd": r"F:\current",
                    "pendingCoreUpdate": false
                }
            }
        }))?;
        let records = vec![
            ("stale-thread".to_string(), r"F:\current".to_string()),
            ("dangling-thread".to_string(), r"F:\current".to_string()),
            ("unrelated-thread".to_string(), r"F:\current".to_string()),
        ];

        assert!(sync_missing_thread_project_assignment_records(
            &codex.0, &records
        )?);

        let state = codex.read_state()?;
        for id in ["stale-thread", "dangling-thread", "unrelated-thread"] {
            assert_eq!(
                state[THREAD_ASSIGNMENTS][id]["projectId"],
                "current-project"
            );
            assert_eq!(state[THREAD_ASSIGNMENTS][id]["cwd"], r"F:\current");
            assert_eq!(state[THREAD_ASSIGNMENTS][id]["pendingCoreUpdate"], false);
        }
        assert_eq!(
            state[THREAD_ASSIGNMENTS]["stale-thread"]["futureField"],
            "keep"
        );
        Ok(())
    }

    #[test]
    fn missing_only_batch_preserves_valid_parent_and_non_local_assignments() -> AppResult<()> {
        let codex = TestDir::new("preserve-explicit-project-assignment")?;
        let parent_assignment = json!({
            "projectKind": "local",
            "projectId": "parent-project",
            "cwd": r"F:\work\repo",
            "pendingCoreUpdate": false
        });
        let cloud_assignment = json!({
            "projectKind": "future-cloud-kind",
            "futureField": true
        });
        let chatgpt_assignment = json!({
            "projectKind": "local",
            "projectId": "stale-local-project",
            "cwd": r"F:\stale",
            "pendingCoreUpdate": false,
            "projectOrigin": "chatgpt",
            "hostId": "chatgpt-host",
            "path": "/workspace"
        });
        codex.write_state(&json!({
            LOCAL_PROJECTS: {
                "parent-project": {"id": "parent-project", "rootPaths": [r"F:\work"]},
                "nested-project": {"id": "nested-project", "rootPaths": [r"F:\work\repo"]}
            },
            THREAD_ASSIGNMENTS: {
                "parent-thread": parent_assignment,
                "cloud-thread": cloud_assignment,
                "chatgpt-thread": chatgpt_assignment
            }
        }))?;
        let records = vec![
            ("parent-thread".to_string(), r"F:\work\repo".to_string()),
            ("cloud-thread".to_string(), r"F:\work\repo".to_string()),
            ("chatgpt-thread".to_string(), r"F:\work\repo".to_string()),
        ];

        assert!(!sync_missing_thread_project_assignment_records(
            &codex.0, &records
        )?);
        let state = codex.read_state()?;
        assert_eq!(
            state[THREAD_ASSIGNMENTS]["parent-thread"],
            parent_assignment
        );
        assert_eq!(state[THREAD_ASSIGNMENTS]["cloud-thread"], cloud_assignment);
        assert_eq!(
            state[THREAD_ASSIGNMENTS]["chatgpt-thread"],
            chatgpt_assignment
        );
        Ok(())
    }

    #[test]
    fn missing_only_batch_repairs_local_assignment_when_project_id_disagrees_with_map_key(
    ) -> AppResult<()> {
        let codex = TestDir::new("repair-mismatched-settled-project-id")?;
        let cwd = r"F:\work\repo";
        codex.write_state(&json!({
            LOCAL_PROJECTS: {
                "map-key": {
                    "id": "different-embedded-id",
                    "rootPaths": [cwd]
                }
            },
            THREAD_ASSIGNMENTS: {
                "thread-1": {
                    "projectKind": "local",
                    "projectId": "map-key",
                    "cwd": cwd,
                    "pendingCoreUpdate": false
                }
            }
        }))?;

        assert!(sync_missing_thread_project_assignment_records(
            &codex.0,
            &[("thread-1".to_string(), cwd.to_string())]
        )?);

        let state = codex.read_state()?;
        let repaired_id = state[THREAD_ASSIGNMENTS]["thread-1"]["projectId"]
            .as_str()
            .expect("repaired local project id");
        assert_ne!(repaired_id, "map-key");
        assert_eq!(state[LOCAL_PROJECTS][repaired_id]["id"], repaired_id);
        assert_eq!(state[THREAD_ASSIGNMENTS]["thread-1"]["cwd"], cwd);
        Ok(())
    }

    #[test]
    fn missing_only_batch_rejects_non_object_assignment_without_partial_write() -> AppResult<()> {
        let codex = TestDir::new("malformed-missing-project-assignment")?;
        codex.write_state(&json!({
            LOCAL_PROJECTS: {},
            THREAD_ASSIGNMENTS: {
                "malformed-thread": "not-an-object"
            }
        }))?;
        let before = fs::read(codex.state_path())?;
        let records = vec![
            ("new-thread".to_string(), r"F:\new\repo".to_string()),
            ("malformed-thread".to_string(), r"F:\other\repo".to_string()),
        ];

        let error = sync_missing_thread_project_assignment_records(&codex.0, &records)
            .expect_err("malformed explicit assignment must abort the batch");

        assert!(error.to_string().contains("项目归属必须是对象"));
        assert_eq!(fs::read(codex.state_path())?, before);
        Ok(())
    }

    #[test]
    fn creates_a_uuid_local_project_at_the_front_when_cwd_is_new() -> AppResult<()> {
        let codex = TestDir::new("create-local-project")?;
        codex.write_state(&json!({
            LOCAL_PROJECTS: {},
            PROJECT_ORDER: [r"F:\project\sessions-management\codex-session-manager"]
        }))?;
        let cwd = r"F:\project\sessions-management\codex-session-manager";

        sync_thread_project_assignment(&codex.0, "thread-new", cwd)?;

        let state = codex.read_state()?;
        let project_id = state[THREAD_ASSIGNMENTS]["thread-new"]["projectId"]
            .as_str()
            .expect("new local project UUID");
        let id_bytes = project_id.as_bytes();
        assert_eq!(project_id.len(), 36);
        assert_eq!(id_bytes[8], b'-');
        assert_eq!(id_bytes[13], b'-');
        assert_eq!(id_bytes[18], b'-');
        assert_eq!(id_bytes[23], b'-');
        assert_eq!(id_bytes[14], b'4');
        assert!(matches!(id_bytes[19], b'8' | b'9' | b'a' | b'b'));
        assert!(!project_id.starts_with("local-"));
        let project = &state[LOCAL_PROJECTS][project_id];
        assert_eq!(project["id"], project_id);
        assert_eq!(project["name"], "codex-session-manager");
        assert_eq!(project["rootPaths"], json!([cwd]));
        assert!(project["createdAt"].as_i64().is_some());
        assert_eq!(state[PROJECT_ORDER], json!([project_id, cwd]));
        Ok(())
    }

    #[test]
    fn empty_cwd_clears_stale_assignment_and_marks_thread_projectless() -> AppResult<()> {
        let codex = TestDir::new("projectless")?;
        codex.write_state(&json!({
            THREAD_ASSIGNMENTS: {"thread-1": {"projectId": "stale"}},
            "thread-workspace-root-hints": {"thread-1": r"C:\stale"},
            PROJECTLESS_THREADS: []
        }))?;

        sync_thread_project_assignment(&codex.0, "thread-1", "  ")?;

        let state = codex.read_state()?;
        assert!(state[THREAD_ASSIGNMENTS].get("thread-1").is_none());
        assert_eq!(
            state["thread-workspace-root-hints"]["thread-1"],
            r"C:\stale"
        );
        assert_eq!(state[PROJECTLESS_THREADS], json!(["thread-1"]));
        Ok(())
    }

    #[test]
    fn reads_explicit_assignment_cwd_for_pending_official_moves() -> AppResult<()> {
        let codex = TestDir::new("read-project-assignment")?;
        codex.write_state(&json!({
            THREAD_ASSIGNMENTS: {
                "thread-1": {
                    "projectKind": "local",
                    "projectId": "project-new",
                    "cwd": r"F:\new\repo",
                    "pendingCoreUpdate": true
                },
                "thread-path-only": {
                    "projectKind": "local",
                    "projectId": "project-path-only",
                    "path": r"F:\path-only\repo",
                    "pendingCoreUpdate": true
                },
                "thread-empty-cwd": {
                    "projectKind": "local",
                    "projectId": "project-empty-cwd",
                    "cwd": "  ",
                    "path": r"F:\fallback\repo",
                    "pendingCoreUpdate": true
                },
                "thread-both": {
                    "projectKind": "local",
                    "projectId": "project-both",
                    "cwd": r"F:\preferred\cwd",
                    "path": r"F:\fallback\path",
                    "pendingCoreUpdate": true
                },
                "thread-without-cwd": {"projectKind": "local"}
            }
        }))?;

        assert_eq!(
            pending_thread_project_assignment_cwd(&codex.0, "thread-1")?,
            Some(r"F:\new\repo".to_string())
        );
        assert_eq!(
            pending_thread_project_assignment_cwd(&codex.0, "thread-path-only")?,
            Some(r"F:\path-only\repo".to_string())
        );
        assert_eq!(
            pending_thread_project_assignment_cwd(&codex.0, "thread-empty-cwd")?,
            Some(r"F:\fallback\repo".to_string())
        );
        assert_eq!(
            pending_thread_project_assignment_cwd(&codex.0, "thread-both")?,
            Some(r"F:\preferred\cwd".to_string())
        );
        assert!(pending_thread_project_assignment_cwd(&codex.0, "thread-without-cwd").is_err());
        assert_eq!(
            pending_thread_project_assignment_cwd(&codex.0, "missing-thread")?,
            None
        );
        Ok(())
    }

    #[test]
    fn assignment_cwd_reader_distinguishes_missing_and_malformed_state() -> AppResult<()> {
        let codex = TestDir::new("read-project-assignment-errors")?;
        assert_eq!(
            pending_thread_project_assignment_cwd(&codex.0, "thread-1")?,
            None
        );
        codex.write_state(&json!({THREAD_ASSIGNMENTS: {"thread-1": {
            "projectKind": "local",
            "projectId": "project-1",
            "pendingCoreUpdate": true,
            "cwd": 42
        }}}))?;
        assert!(pending_thread_project_assignment_cwd(&codex.0, "thread-1").is_err());
        Ok(())
    }

    #[test]
    fn assignment_cwd_reader_ignores_non_pending_or_non_local_membership() -> AppResult<()> {
        let codex = TestDir::new("read-non-pending-project-assignment")?;
        codex.write_state(&json!({
            THREAD_ASSIGNMENTS: {
                "settled-thread": {
                    "projectKind": "local",
                    "projectId": "stale-project",
                    "cwd": r"F:\stale",
                    "pendingCoreUpdate": false
                },
                "cloud-thread": {
                    "projectKind": "future-cloud-kind"
                }
            }
        }))?;

        assert_eq!(
            pending_thread_project_assignment_cwd(&codex.0, "settled-thread")?,
            None
        );
        assert_eq!(
            pending_thread_project_assignment_cwd(&codex.0, "cloud-thread")?,
            None
        );
        Ok(())
    }

    #[test]
    fn clear_removes_current_project_membership_and_preserves_unknown_fields() -> AppResult<()> {
        let codex = TestDir::new("clear-project-state")?;
        codex.write_state(&json!({
            THREAD_ASSIGNMENTS: {"thread-1": {}, "other": {}},
            "thread-workspace-root-hints": {"thread-1": "one", "other": "two"},
            "thread-writable-roots": {"thread-1": ["one"], "other": ["two"]},
            PROJECTLESS_THREADS: ["thread-1", "other", "thread-1"],
            "thread-projectless-output-directories": {
                "thread-1": r"F:\projectless\one",
                "other": r"F:\projectless\two"
            },
            "electron-persisted-atom-state": {
                "thread-workspace-state-v1:thread-1": {"cwd": "one"},
                "future-atom": {"keep": true}
            },
            LOCAL_PROJECTS: {"keep": {"id": "keep", "rootPaths": ["one"]}}
        }))?;

        assert!(clear_thread_project_state(&codex.0, "thread-1")?);
        let state = codex.read_state()?;
        assert!(state[THREAD_ASSIGNMENTS].get("thread-1").is_none());
        assert_eq!(state[PROJECTLESS_THREADS], json!(["other"]));
        assert_eq!(
            state["thread-workspace-root-hints"],
            json!({"thread-1": "one", "other": "two"})
        );
        assert_eq!(
            state["thread-writable-roots"],
            json!({"thread-1": ["one"], "other": ["two"]})
        );
        assert_eq!(
            state["thread-projectless-output-directories"],
            json!({
                "thread-1": r"F:\projectless\one",
                "other": r"F:\projectless\two"
            })
        );
        assert_eq!(
            state["electron-persisted-atom-state"],
            json!({
                "thread-workspace-state-v1:thread-1": {"cwd": "one"},
                "future-atom": {"keep": true}
            })
        );
        assert!(state[LOCAL_PROJECTS].get("keep").is_some());
        assert!(!clear_thread_project_state(&codex.0, "thread-1")?);
        Ok(())
    }

    #[test]
    fn missing_state_is_a_noop_but_malformed_state_is_an_error() -> AppResult<()> {
        let codex = TestDir::new("missing-project-state")?;
        sync_thread_project_assignment(&codex.0, "thread-1", r"F:\work")?;
        assert!(!codex.state_path().exists());
        assert!(!clear_thread_project_state(&codex.0, "thread-1")?);

        fs::write(codex.state_path(), b"{broken")?;
        let before = fs::read(codex.state_path())?;
        assert!(sync_thread_project_assignment(&codex.0, "thread-1", r"F:\work").is_err());
        assert_eq!(fs::read(codex.state_path())?, before);
        Ok(())
    }

    #[test]
    fn state_write_uses_fingerprint_compare_and_swap() -> AppResult<()> {
        let codex = TestDir::new("project-state-cas")?;
        codex.write_state(&json!({"version": 1}))?;
        let expected = atomic_file::fingerprint(&codex.state_path())?;
        fs::write(
            codex.state_path(),
            serde_json::to_vec(&json!({"version": 2}))?,
        )?;

        let replacement = serde_json::to_vec(&json!({"version": 3}))?;
        let error = write_state_bytes_if_unchanged(&codex.state_path(), &expected, &replacement)
            .expect_err("stale snapshot must not replace newer Desktop state");

        assert!(error.to_string().contains("发生变化") || error.to_string().contains("正在被"));
        assert_eq!(codex.read_state()?["version"], 2);
        Ok(())
    }

    #[test]
    fn state_write_post_commit_failure_restores_original_bytes() -> AppResult<()> {
        let codex = TestDir::new("project-state-post-commit-failure")?;
        codex.write_state(&json!({
            "untouched": {"keep": true},
            LOCAL_PROJECTS: {}
        }))?;
        let before = fs::read(codex.state_path())?;
        let _failure = StatePostCommitErrorTestGuard::once();

        let error = sync_thread_project_assignment(&codex.0, "thread-1", r"F:\work")
            .expect_err("a post-commit failure must be surfaced after compensation");

        assert!(error.to_string().contains("已写入但收尾失败"), "{error}");
        assert_eq!(fs::read(codex.state_path())?, before);
        Ok(())
    }

    #[test]
    fn mutation_receipt_restores_only_its_own_write() -> AppResult<()> {
        let codex = TestDir::new("project-state-receipt")?;
        codex.write_state(&json!({
            LOCAL_PROJECTS: {
                "project": {"id": "project", "rootPaths": [r"F:\repo"]}
            },
            "desktop-field": "before"
        }))?;
        let before = fs::read(codex.state_path())?;

        let receipt =
            sync_thread_project_assignment_with_receipt(&codex.0, "thread-1", r"F:\repo")?
                .expect("assignment must mutate state");
        assert!(codex.read_state()?[THREAD_ASSIGNMENTS]
            .get("thread-1")
            .is_some());

        receipt.compensate()?;
        assert_eq!(fs::read(codex.state_path())?, before);
        Ok(())
    }

    #[test]
    fn mutation_receipt_refuses_compensation_while_desktop_runs() -> AppResult<()> {
        let codex = TestDir::new("project-state-receipt-running")?;
        codex.write_state(&json!({
            LOCAL_PROJECTS: {
                "project": {"id": "project", "rootPaths": [r"F:\repo"]}
            }
        }))?;
        let receipt =
            sync_thread_project_assignment_with_receipt(&codex.0, "thread-1", r"F:\repo")?
                .expect("assignment must mutate state");
        let after = fs::read(codex.state_path())?;
        let _desktop = DesktopTestProbeGuard::running();

        let error = receipt
            .compensate()
            .expect_err("compensation must not overwrite Desktop-owned state");

        assert!(error.to_string().contains("完全退出桌面应用"), "{error}");
        assert_eq!(fs::read(codex.state_path())?, after);
        Ok(())
    }

    #[test]
    fn mutation_receipt_never_overwrites_a_later_desktop_write() -> AppResult<()> {
        let codex = TestDir::new("project-state-receipt-concurrent")?;
        codex.write_state(&json!({
            LOCAL_PROJECTS: {
                "project": {"id": "project", "rootPaths": [r"F:\repo"]}
            },
            "desktop-field": "before"
        }))?;
        let receipt =
            sync_thread_project_assignment_with_receipt(&codex.0, "thread-1", r"F:\repo")?
                .expect("assignment must mutate state");
        let mut concurrent = codex.read_state()?;
        concurrent["desktop-field"] = Value::String("after assignment".to_string());
        codex.write_state(&concurrent)?;
        let concurrent_bytes = fs::read(codex.state_path())?;

        let error = receipt
            .compensate()
            .expect_err("later Desktop state must win over compensation");

        assert!(error.to_string().contains("并发数据"), "{error}");
        assert_eq!(fs::read(codex.state_path())?, concurrent_bytes);
        Ok(())
    }

    #[test]
    fn unix_paths_remain_case_sensitive_while_windows_paths_do_not() {
        assert_eq!(
            normalize_path_for_compare(r"\\?\C:\Work\Repo\"),
            "c:/work/repo"
        );
        assert_ne!(
            normalize_path_for_compare("/Users/Alice/Repo"),
            normalize_path_for_compare("/users/alice/repo")
        );
    }
}
