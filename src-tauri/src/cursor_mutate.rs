//! Cursor 会话的写操作：归档、重命名、删除、压缩数据库。
//!
//! 所有写入都先确认 Cursor 已经完全退出。Cursor 会把会话状态缓存在内存里，运行期间
//! 改库有很大概率被它按旧状态覆盖回去，这和 Codex 桌面端的处理是同一个道理。
//!
//! IDE Composer 会话全部改在 `state.vscdb` 的一个事务里，跨存储补偿（`mutation_journal`）
//! 用不上；cursor-agent 会话各自独占一个目录，删除即删目录。
//!
//! **不碰 `ItemTable` 里的遗留键 `composer.composerHeaders`**：实测本机该数组只有 622 条
//! 而表里有 837 条，且已有 7 条时间戳对不上——新版 Cursor 已经不再维护它，跟着写反而
//! 会让两份数据以新的方式互相矛盾。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

use crate::cursor_blobs;
use crate::cursor_sessions;
use crate::error::{AppError, AppResult};
use crate::models::{
    CursorPruneReport, CursorResidueGroup, CursorResidueReport, CursorResidueSample, DeleteResult,
    ProviderDirs,
};
use crate::paths;

const CURSOR_RUNNING_ERROR: &str =
    "Cursor 正在运行；为避免它用内存中的旧会话状态覆盖本次修改，请完全退出 Cursor（包括后台进程）后重试";

/// 会话头改用独立表之后，Cursor 用这个开关决定读表还是读 `ItemTable` 里的遗留数组。
const HEADER_TABLE_GATE_KEY: &str = "composer.composerHeaders.tableGateEnabled";

const TITLE_MAX_CHARS: usize = 120;

/// `cursorDiskKV` 里按会话 id 归属的全部键空间。
///
/// 前六项抄自 Cursor 自己的 `deleteComposer`（`clearComposerCheckpoints` /
/// `clearComposerDiffs` / `clearComposerMessages` / `clearComposerPartialInlineDiffFates`
/// / `composerDataHandleManager.deleteComposer`，最后一个同时清 `composerData` 与
/// `ofsContent`）。后两项是 Cursor 自己漏掉的：`messageRequestContext` 在当前版本的代码里
/// 已经完全不存在，只剩历史数据；`composerVirtualRowHeights` 只有"整体清空"没有按会话清。
///
/// 键形状有两种，`composerData:<id>` / `composerVirtualRowHeights:<id>` 是精确键，
/// 其余是 `<空间>:<id>:<子 id>`。两种都按 `= ns:id` 或 `ns:id:` 前缀段匹配，
/// 冒号边界保证不会串到别的会话（`bubbleId:task-x:` 不会命中 `bubbleId:x:`）。
const SESSION_NAMESPACES: [&str; 8] = [
    "bubbleId",
    "composerData",
    "checkpointId",
    "codeBlockDiff",
    "codeBlockPartialInlineDiffFates",
    "ofsContent",
    "messageRequestContext",
    "composerVirtualRowHeights",
];

/// 落在会话键空间里、但其实是全局键的例外。
///
/// `composerVirtualRowHeights:_recentIds` 存的是最近会话 id 列表，不属于任何会话；
/// 不排除掉会被当成"没有会话头的孤儿"删掉。
const NON_SESSION_KEYS: [&str; 1] = ["composerVirtualRowHeights:_recentIds"];

/// 一个会话落在哪套存储里。
enum Target {
    Composer { db: PathBuf },
    Agent { dir: PathBuf },
}

#[derive(Debug, Clone, Serialize)]
pub struct CursorCompactReport {
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub bytes_reclaimed: u64,
}

// ---------------------------------------------------------------------------
// 对外操作
// ---------------------------------------------------------------------------

pub fn set_archived(dirs: &ProviderDirs, id: &str, archived: bool) -> AppResult<()> {
    ensure_cursor_not_running()?;
    match locate(dirs, id)? {
        Target::Composer { db } => {
            let mut connection = open_writable(&db)?;
            ensure_header_table_is_authoritative(&connection)?;
            let transaction = connection.transaction()?;
            let header = header_value(&transaction, id)?;
            let updated = merge_header(header, |value| {
                value.insert("isArchived".into(), Value::Bool(archived));
            });
            let changed = transaction.execute(
                "UPDATE composerHeaders SET isArchived = ?1, value = ?2 WHERE composerId = ?3",
                params![i64::from(archived), updated.to_string(), id],
            )?;
            if changed == 0 {
                return Err(AppError::NotFound(format!("Cursor 会话不存在: {id}")));
            }
            transaction.commit()?;
            Ok(())
        }
        // cursor-agent 没有归档这个概念，与其伪造一个本地标记，不如直说。
        Target::Agent { .. } => Err(AppError::Other(
            "cursor-agent 会话不支持归档（Cursor 本身没有这个状态）".into(),
        )),
    }
}

pub fn rename_session(dirs: &ProviderDirs, id: &str, title: &str) -> AppResult<u32> {
    let title = title.trim();
    if title.is_empty() {
        return Err(AppError::Other("会话名称不能为空".into()));
    }
    if title.chars().count() > TITLE_MAX_CHARS {
        return Err(AppError::Other(format!(
            "会话名称过长（最多 {TITLE_MAX_CHARS} 个字符）"
        )));
    }
    ensure_cursor_not_running()?;
    match locate(dirs, id)? {
        Target::Composer { db } => {
            let mut connection = open_writable(&db)?;
            ensure_header_table_is_authoritative(&connection)?;
            let transaction = connection.transaction()?;
            let header = header_value(&transaction, id)?;
            let updated = merge_header(header, |value| {
                value.insert("name".into(), Value::String(title.to_string()));
            });
            let changed = transaction.execute(
                "UPDATE composerHeaders SET value = ?1 WHERE composerId = ?2",
                params![updated.to_string(), id],
            )?;
            if changed == 0 {
                return Err(AppError::NotFound(format!("Cursor 会话不存在: {id}")));
            }
            // 会话正文里也存了一份名字，两处不同步的话 Cursor 打开会话时会显示旧名。
            let key = format!("composerData:{id}");
            if let Some(raw) = read_kv(&transaction, &key)? {
                let data = merge_header(
                    serde_json::from_slice::<Value>(&raw).unwrap_or(Value::Null),
                    |value| {
                        value.insert("name".into(), Value::String(title.to_string()));
                    },
                );
                transaction.execute(
                    "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
                    params![data.to_string(), key],
                )?;
            }
            transaction.commit()?;
            Ok(changed as u32)
        }
        Target::Agent { dir } => {
            rename_agent_session(&dir, title)?;
            Ok(1)
        }
    }
}

pub fn delete_session(dirs: &ProviderDirs, id: &str) -> AppResult<DeleteResult> {
    ensure_cursor_not_running()?;
    match locate(dirs, id)? {
        Target::Composer { db } => delete_composer_session(&db, id),
        Target::Agent { dir } => {
            fs::remove_dir_all(&dir)?;
            Ok(delete_result(
                id,
                Some(dir.to_string_lossy().into_owned()),
                0,
                true,
            ))
        }
    }
}

fn delete_composer_session(db: &Path, id: &str) -> AppResult<DeleteResult> {
    let mut connection = open_writable(db)?;
    ensure_header_table_is_authoritative(&connection)?;
    let targets = cascade_targets(&connection, id)?;
    let transaction = connection.transaction()?;

    let mut removed = 0u32;
    for target in &targets {
        removed = removed.saturating_add(delete_session_rows(&transaction, target)?);
    }
    let mut header_rows = 0usize;
    for target in &targets {
        header_rows += transaction.execute(
            "DELETE FROM composerHeaders WHERE composerId = ?1",
            [target],
        )?;
    }
    transaction.commit()?;

    if header_rows == 0 && removed == 0 {
        return Err(AppError::NotFound(format!("Cursor 会话不存在: {id}")));
    }
    Ok(delete_result(
        id,
        Some(db.to_string_lossy().into_owned()),
        removed as i64,
        header_rows > 0,
    ))
}

/// 一个会话被删除时要一并带走的全部会话 id（含它自己）。
///
/// Cursor 把子代理记在父会话 `composerData.subagentComposerIds` 上。子代理不在列表里
/// 单独出现，只能随父会话一起删，否则父会话会留下指向已消失会话的引用。
///
/// 两个实测存在的边界都要处理：子代理自己还能再开子代理（需要递归），
/// 以及同一个子代理被两个父会话共享（这种不能删，否则另一个父会话就悬空了）。
pub(crate) fn cascade_targets(connection: &Connection, root: &str) -> AppResult<Vec<String>> {
    let mut targets = vec![root.to_string()];
    let mut seen = BTreeSet::from([root.to_string()]);
    let mut queue = vec![root.to_string()];
    while let Some(current) = queue.pop() {
        for child in subagent_children(connection, &current)? {
            if !seen.insert(child.clone()) {
                continue;
            }
            targets.push(child.clone());
            queue.push(child);
        }
    }
    // 还被删除范围之外的父会话引用的子代理必须留下。
    let shared = shared_children(connection, &seen)?;
    targets.retain(|id| id == root || !shared.contains(id));
    Ok(targets)
}

/// 父会话记子会话的两个字段。
///
/// `subagentComposerIds` 是子代理，`subComposerIds` 是 best-of-N 的分支会话。
/// Cursor 自己的 `deleteComposer` 递归的是后者，前者它压根没处理——两个都要跟。
const CHILD_ID_FIELDS: [&str; 2] = ["subagentComposerIds", "subComposerIds"];

fn child_ids(data: &Value) -> impl Iterator<Item = &str> {
    CHILD_ID_FIELDS.into_iter().flat_map(move |field| {
        data.get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
    })
}

fn subagent_children(connection: &Connection, id: &str) -> AppResult<Vec<String>> {
    let Some(raw) = read_kv(connection, &format!("composerData:{id}"))? else {
        return Ok(Vec::new());
    };
    let data = serde_json::from_slice::<Value>(&raw).unwrap_or(Value::Null);
    Ok(child_ids(&data).map(str::to_string).collect())
}

/// 在删除范围之外，还有哪些会话引用着这批子代理。
fn shared_children(
    connection: &Connection,
    scope: &BTreeSet<String>,
) -> AppResult<BTreeSet<String>> {
    let mut shared = BTreeSet::new();
    let mut statement = connection.prepare(
        "SELECT key, value FROM cursorDiskKV
         WHERE key >= 'composerData:' AND key < 'composerData;'",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, rusqlite::types::Value>(1)?,
        ))
    })?;
    for row in rows {
        let (key, value) = row?;
        let Some(owner) = key.strip_prefix("composerData:") else {
            continue;
        };
        if scope.contains(owner) {
            continue;
        }
        let raw = match value {
            rusqlite::types::Value::Text(text) => text.into_bytes(),
            rusqlite::types::Value::Blob(bytes) => bytes,
            _ => continue,
        };
        let data = serde_json::from_slice::<Value>(&raw).unwrap_or(Value::Null);
        for child in child_ids(&data) {
            if scope.contains(child) {
                shared.insert(child.to_string());
            }
        }
    }
    Ok(shared)
}

/// 回收删除后空出的页。
///
/// 单独做成一个动作而不是删除时顺带执行：`state.vscdb` 实测有 8 GB，VACUUM 需要等量
/// 临时空间，耗时以分钟计，绝不能挂在每次删除后面。
pub fn compact_database(dirs: &ProviderDirs) -> AppResult<CursorCompactReport> {
    ensure_cursor_not_running()?;
    let db = cursor_sessions::state_db_path(&dirs.cursor_path());
    let bytes_before = fs::metadata(&db)?.len();
    let connection = open_writable(&db)?;
    connection.execute_batch("VACUUM")?;
    drop(connection);
    let bytes_after = fs::metadata(&db)?.len();
    Ok(CursorCompactReport {
        bytes_before,
        bytes_after,
        bytes_reclaimed: bytes_before.saturating_sub(bytes_after),
    })
}

// ---------------------------------------------------------------------------
// 定位与读写辅助
// ---------------------------------------------------------------------------

fn locate(dirs: &ProviderDirs, id: &str) -> AppResult<Target> {
    if id.trim().is_empty() {
        return Err(AppError::Other("会话 id 不能为空".into()));
    }
    let db = cursor_sessions::state_db_path(&dirs.cursor_path());
    if db.is_file() {
        let connection = cursor_sessions::open_readonly(&db)?;
        if cursor_sessions::table_exists(&connection, "composerHeaders")? {
            let found = connection
                .query_row(
                    "SELECT 1 FROM composerHeaders WHERE composerId = ?1",
                    [id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if found.is_some() {
                return Ok(Target::Composer { db });
            }
        }
    }
    // cursor-agent 的会话目录名就是 agentId。
    let chats = paths::cursor_agent_chats_dir(&dirs.cursor_agent_path());
    if chats.is_dir() {
        for entry in fs::read_dir(&chats)? {
            let candidate = entry?.path().join(id);
            if candidate.is_dir() {
                return Ok(Target::Agent { dir: candidate });
            }
        }
    }
    Err(AppError::NotFound(format!("Cursor 会话不存在: {id}")))
}

/// 拒绝在旧版 Cursor 上写入。
///
/// 开关为 false 时 Cursor 读的是 `ItemTable` 里的遗留数组，改表不会有任何效果，
/// 与其静默无效不如直接报错。
fn ensure_header_table_is_authoritative(connection: &Connection) -> AppResult<()> {
    if !cursor_sessions::table_exists(connection, "composerHeaders")? {
        return Err(AppError::Other(
            "这个 Cursor 版本还没有 composerHeaders 表，暂不支持修改会话".into(),
        ));
    }
    let gate = connection
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            [HEADER_TABLE_GATE_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match gate.as_deref().map(str::trim) {
        // 键不存在的版本本来就只有表这一份数据。
        None => Ok(()),
        Some("true") => Ok(()),
        Some(other) => Err(AppError::Other(format!(
            "这个 Cursor 版本以 ItemTable 中的遗留会话头为准（{HEADER_TABLE_GATE_KEY}={other}），改表不会生效，已拒绝写入"
        ))),
    }
}

fn header_value(connection: &Connection, id: &str) -> AppResult<Value> {
    let raw = connection
        .query_row(
            "SELECT value FROM composerHeaders WHERE composerId = ?1",
            [id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(raw
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null))
}

/// 只改动指定字段，其余原样保留——会话头里还有几十个 Cursor 自己用的字段。
fn merge_header(header: Value, edit: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Value {
    let mut object = match header {
        Value::Object(object) => object,
        _ => serde_json::Map::new(),
    };
    edit(&mut object);
    Value::Object(object)
}

fn read_kv(connection: &Connection, key: &str) -> AppResult<Option<Vec<u8>>> {
    use rusqlite::types::Value as SqlValue;
    let value = connection
        .query_row(
            "SELECT value FROM cursorDiskKV WHERE key = ?1",
            [key],
            |row| row.get::<_, SqlValue>(0),
        )
        .optional()?;
    Ok(match value {
        Some(SqlValue::Text(text)) => Some(text.into_bytes()),
        Some(SqlValue::Blob(bytes)) => Some(bytes),
        _ => None,
    })
}

fn rename_agent_session(dir: &Path, title: &str) -> AppResult<()> {
    let store = crate::cursor_agent_store::store_path(dir);
    let connection = open_writable(&store)?;
    let row = connection
        .query_row("SELECT key, value FROM meta ORDER BY key ASC", [], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .optional()?;
    let Some((key, encoded)) = row else {
        return Err(AppError::NotFound("Cursor CLI 会话缺少会话头".into()));
    };
    let bytes = hex::decode(encoded.trim())
        .map_err(|error| AppError::Other(format!("Cursor CLI 会话头无法解码: {error}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|error| AppError::Other(format!("Cursor CLI 会话头不是合法 UTF-8: {error}")))?;
    let header = merge_header(
        serde_json::from_str::<Value>(&text).unwrap_or(Value::Null),
        |value| {
            value.insert("name".into(), Value::String(title.to_string()));
        },
    );
    connection.execute(
        "UPDATE meta SET value = ?1 WHERE key = ?2",
        params![hex::encode(header.to_string()), key],
    )?;
    Ok(())
}

fn delete_result(id: &str, path: Option<String>, rows: i64, removed: bool) -> DeleteResult {
    DeleteResult {
        id: id.into(),
        rollout_path: path,
        threads_rows_deleted: rows as u32,
        logs_rows_deleted: 0,
        history_rows_deleted: 0,
        rollout_deleted: removed,
        rollout_missing: !removed,
        sidecar_deleted: false,
        tasks_deleted: false,
        file_history_deleted: false,
        shared_data_preserved: false,
        desktop_restart_required: false,
        ok: true,
        error: None,
    }
}

fn open_writable(path: &Path) -> AppResult<Connection> {
    if !path.is_file() {
        return Err(AppError::NotFound(format!(
            "Cursor 数据库不存在: {}",
            path.to_string_lossy()
        )));
    }
    Connection::open(path).map_err(Into::into)
}

// ---------------------------------------------------------------------------
// 残留清理
// ---------------------------------------------------------------------------

/// 只有会话头没有内容的会话。列表里已经把它们藏起来了，但行还留在库里。
pub const RESIDUE_EMPTY_SESSIONS: &str = "empty_sessions";
/// 会话头已经不在，但 [`SESSION_NAMESPACES`] 里还留着行的孤儿数据。
pub const RESIDUE_ORPHAN_RECORDS: &str = "orphan_records";
/// 有真实对话、但项目目录已经不存在的会话。
pub const RESIDUE_MISSING_PROJECT: &str = "missing_project";
/// 没有任何父会话引用的子代理。子代理只随父会话管理，父没了它就永远不会再出现。
pub const RESIDUE_ORPHAN_SUBAGENTS: &str = "orphan_subagents";
/// `agentKv:blob:` / `composer.content.` 里没有任何会话引用得到的内容块。
pub const RESIDUE_ORPHAN_BLOBS: &str = "orphan_blobs";

/// 扫描 Cursor 数据库里可清理的残留。
///
/// 孤儿气泡那一类必须整段扫 `bubbleId:` 前缀（实测 9 秒 / 31 万行），所以这是一个
/// 显式触发的诊断动作，不会挂在列表刷新上。
pub fn diagnose_residue(dirs: &ProviderDirs) -> AppResult<CursorResidueReport> {
    let cursor_dir = dirs.cursor_path();
    let db = cursor_sessions::state_db_path(&cursor_dir);
    let connection = cursor_sessions::open_readonly(&db)?;
    if !cursor_sessions::table_exists(&connection, "composerHeaders")? {
        return Err(AppError::Other(
            "这个 Cursor 版本还没有 composerHeaders 表，无法诊断".into(),
        ));
    }

    let headers = read_headers(&connection)?;
    let counts = cursor_sessions::bubble_counts(&connection, headers.keys().map(String::as_str))?;

    let referenced = referenced_subagents(&connection)?;
    let mut empty = Vec::new();
    let mut missing_project = Vec::new();
    let mut orphan_subagents = Vec::new();
    let mut visible = 0u32;
    for (id, header) in &headers {
        if counts.get(id).copied().unwrap_or(0) == 0 {
            empty.push(id.clone());
            continue;
        }
        if header.is_subagent {
            // 子代理不进列表，只随父会话管理；没有父引用的就再也访问不到了。
            if !referenced.contains(id) {
                orphan_subagents.push(id.clone());
            }
            continue;
        }
        visible += 1;
        let cwd = header_cwd(&header.value);
        // 空路径说明这个会话本来就没绑定项目，不算"目录消失"。
        if !cwd.is_empty() && !Path::new(&cwd).is_dir() {
            missing_project.push(id.clone());
        }
    }

    let orphans = scan_orphan_records(&connection, &headers)?;
    let blobs = cursor_blobs::sweep(&connection)?;

    let sample = |ids: &[String], with_bubbles: bool| -> Vec<CursorResidueSample> {
        ids.iter()
            .take(RESIDUE_SAMPLE_LIMIT)
            .map(|id| CursorResidueSample {
                id: id.clone(),
                title: headers
                    .get(id)
                    .map(|header| header_title(&header.value, id))
                    .unwrap_or_else(|| id.clone()),
                cwd: headers
                    .get(id)
                    .map(|header| header_cwd(&header.value))
                    .unwrap_or_default(),
                bubbles: if with_bubbles {
                    counts.get(id).copied().unwrap_or(0).max(0) as u32
                } else {
                    0
                },
            })
            .collect()
    };

    let groups = vec![
        CursorResidueGroup {
            kind: RESIDUE_EMPTY_SESSIONS.into(),
            label: "空会话".into(),
            description: "只留下会话头、一条消息都没有。列表里已经不显示，删掉不会丢内容。".into(),
            sessions: empty.len() as u32,
            rows: empty.len() as u32,
            bytes: 0,
            destructive: false,
            samples: sample(&empty, false),
        },
        CursorResidueGroup {
            kind: RESIDUE_ORPHAN_RECORDS.into(),
            label: "孤儿记录".into(),
            description:
                "会话头已经不在，但检查点、气泡、代码差异等数据还留在库里。Cursor 自己删会话时就漏清这些，是数据库变大的主因。"
                    .into(),
            sessions: orphans.sessions.len() as u32,
            rows: orphans.rows,
            bytes: orphans.bytes,
            destructive: false,
            samples: orphans
                .sessions
                .iter()
                .take(RESIDUE_SAMPLE_LIMIT)
                .map(|id| CursorResidueSample {
                    id: id.clone(),
                    title: "（会话头已丢失）".into(),
                    cwd: String::new(),
                    bubbles: 0,
                })
                .collect(),
        },
        CursorResidueGroup {
            kind: RESIDUE_ORPHAN_BLOBS.into(),
            label: "无引用的内容块".into(),
            description: blob_group_description(&blobs),
            sessions: 0,
            rows: blobs.orphan_rows(),
            bytes: blobs.orphan_bytes(),
            destructive: false,
            samples: Vec::new(),
        },
        CursorResidueGroup {
            kind: RESIDUE_ORPHAN_SUBAGENTS.into(),
            label: "无主子会话".into(),
            description: "子会话只跟着父会话管理，但这些的父会话已经不在了，界面上再也访问不到。"
                .into(),
            sessions: orphan_subagents.len() as u32,
            rows: orphan_subagents.len() as u32,
            bytes: 0,
            destructive: true,
            samples: sample(&orphan_subagents, true),
        },
        CursorResidueGroup {
            kind: RESIDUE_MISSING_PROJECT.into(),
            label: "项目目录已不存在".into(),
            description: "会话本身有完整对话，只是当初的项目目录被删或改名了。删除会丢失这些对话。"
                .into(),
            sessions: missing_project.len() as u32,
            rows: missing_project.len() as u32,
            bytes: 0,
            destructive: true,
            samples: sample(&missing_project, true),
        },
    ];

    Ok(CursorResidueReport {
        database_path: db.to_string_lossy().into_owned(),
        database_bytes: fs::metadata(&db).map(|meta| meta.len()).unwrap_or(0),
        header_rows: headers.len() as u32,
        visible_sessions: visible,
        groups,
    })
}

/// 每类残留展示的样例条数。
const RESIDUE_SAMPLE_LIMIT: usize = 8;

fn blob_group_description(sweep: &cursor_blobs::ContentSweep) -> String {
    if sweep.errors > 0 {
        return format!(
            "有 {} 行读不出来，无法确认剩下的内容块是否还被引用，这一类已锁定不可清理。",
            sweep.errors
        );
    }
    format!(
        "会话正文引用的文件快照与代理消息，按内容哈希存放。当前 {} 个块里有 {} 个已经没有任何会话引用得到（Cursor 删会话时不回收它们）。",
        sweep.blobs_total + sweep.content_total,
        sweep.orphan_rows()
    )
}

/// 按选定的类别清理残留。
///
/// `kinds` 必须由调用方显式给出：删除有内容的会话是不可逆的，绝不默认包含。
pub fn prune_residue(
    dirs: &ProviderDirs,
    kinds: &[String],
    dry_run: bool,
) -> AppResult<CursorPruneReport> {
    let known = [
        RESIDUE_EMPTY_SESSIONS,
        RESIDUE_ORPHAN_RECORDS,
        RESIDUE_ORPHAN_SUBAGENTS,
        RESIDUE_MISSING_PROJECT,
        RESIDUE_ORPHAN_BLOBS,
    ];
    for kind in kinds {
        if !known.contains(&kind.as_str()) {
            return Err(AppError::Other(format!("未知的清理类别: {kind}")));
        }
    }
    if kinds.is_empty() {
        return Err(AppError::Other("请至少选择一类要清理的残留".into()));
    }
    if !dry_run {
        ensure_cursor_not_running()?;
    }

    let cursor_dir = dirs.cursor_path();
    let db = cursor_sessions::state_db_path(&cursor_dir);
    let mut connection = open_writable(&db)?;
    ensure_header_table_is_authoritative(&connection)?;

    let headers = read_headers(&connection)?;
    let counts = cursor_sessions::bubble_counts(&connection, headers.keys().map(String::as_str))?;

    let mut sessions_to_drop: Vec<String> = Vec::new();
    if kinds.iter().any(|kind| kind == RESIDUE_EMPTY_SESSIONS) {
        sessions_to_drop.extend(
            headers
                .keys()
                .filter(|id| counts.get(*id).copied().unwrap_or(0) == 0)
                .cloned(),
        );
    }
    if kinds.iter().any(|kind| kind == RESIDUE_MISSING_PROJECT) {
        for (id, header) in &headers {
            if counts.get(id).copied().unwrap_or(0) == 0 || header.is_subagent {
                continue;
            }
            let cwd = header_cwd(&header.value);
            if !cwd.is_empty() && !Path::new(&cwd).is_dir() {
                sessions_to_drop.push(id.clone());
            }
        }
    }
    if kinds.iter().any(|kind| kind == RESIDUE_ORPHAN_SUBAGENTS) {
        let referenced = referenced_subagents(&connection)?;
        for (id, header) in &headers {
            if counts.get(id).copied().unwrap_or(0) == 0 || !header.is_subagent {
                continue;
            }
            if !referenced.contains(id) {
                sessions_to_drop.push(id.clone());
            }
        }
    }
    // 删除主会话时要连它的子会话一起带走，否则父没了、子会话再也访问不到。
    let mut expanded = Vec::new();
    for id in &sessions_to_drop {
        expanded.extend(cascade_targets(&connection, id)?);
    }
    sessions_to_drop = expanded;
    sessions_to_drop.sort();
    sessions_to_drop.dedup();

    let orphans = if kinds.iter().any(|kind| kind == RESIDUE_ORPHAN_RECORDS) {
        scan_orphan_records(&connection, &headers)?
    } else {
        OrphanScan::default()
    };

    let mut report = CursorPruneReport {
        database_path: db.to_string_lossy().into_owned(),
        dry_run,
        removed_header_rows: sessions_to_drop.len() as u32,
        removed_kv_rows: orphans.rows,
        freed_bytes: orphans.bytes,
        kinds: kinds.to_vec(),
        blob_scan_errors: 0,
    };
    let sweep_blobs = kinds.iter().any(|kind| kind == RESIDUE_ORPHAN_BLOBS);
    if dry_run {
        // 试运行也要把待删会话自身的体积算出来，否则报的数会偏小。
        for id in &sessions_to_drop {
            let (rows, bytes) = session_footprint(&connection, id);
            report.freed_bytes = report.freed_bytes.saturating_add(bytes);
            report.removed_kv_rows = report.removed_kv_rows.saturating_add(rows);
        }
        if sweep_blobs {
            // 按当前状态估算。真正执行时会话行会先被删掉，实际回收只多不少。
            let sweep = cursor_blobs::sweep(&connection)?;
            report.blob_scan_errors = sweep.errors;
            if sweep.errors == 0 {
                report.removed_kv_rows = report.removed_kv_rows.saturating_add(sweep.orphan_rows());
                report.freed_bytes = report.freed_bytes.saturating_add(sweep.orphan_bytes());
            }
        }
        return Ok(report);
    }

    let transaction = connection.transaction()?;
    let mut removed_kv = orphans.rows;
    let mut freed = orphans.bytes;
    for id in &orphans.sessions {
        delete_session_rows(&transaction, id)?;
    }
    for id in &sessions_to_drop {
        freed = freed.saturating_add(session_bytes(&transaction, id));
        removed_kv = removed_kv.saturating_add(delete_session_rows(&transaction, id)?);
        transaction.execute("DELETE FROM composerHeaders WHERE composerId = ?1", [id])?;
    }
    if sweep_blobs {
        // 必须放在会话行删完之后：那些行还在的时候，它们引用的内容块仍然算可达。
        let sweep = cursor_blobs::sweep(&transaction)?;
        report.blob_scan_errors = sweep.errors;
        // 有行读不出来就整体跳过内容块，但别把已经算准的会话级清理一起回滚掉。
        if sweep.errors == 0 {
            freed = freed.saturating_add(sweep.orphan_bytes());
            removed_kv =
                removed_kv.saturating_add(cursor_blobs::delete_orphans(&transaction, &sweep)?);
        }
    }
    transaction.commit()?;

    report.removed_kv_rows = removed_kv;
    report.freed_bytes = freed;
    Ok(report)
}

#[derive(Default)]
struct OrphanScan {
    sessions: Vec<String>,
    rows: u32,
    bytes: u64,
}

/// 找出全部键空间里会话头已经不存在的行。
fn scan_orphan_records(
    connection: &Connection,
    headers: &BTreeMap<String, HeaderRow>,
) -> AppResult<OrphanScan> {
    let mut out = OrphanScan::default();
    let mut seen = BTreeSet::new();
    for namespace in SESSION_NAMESPACES {
        let mut statement = connection.prepare(
            "SELECT key, octet_length(value) FROM cursorDiskKV
             WHERE key >= ?1 AND key < ?2",
        )?;
        let rows = statement
            .query_map([format!("{namespace}:"), format!("{namespace};")], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
            })?;
        for row in rows {
            let (key, bytes) = row?;
            let Some(id) = key_session_id(&key) else {
                continue;
            };
            if headers.contains_key(id) {
                continue;
            }
            out.rows = out.rows.saturating_add(1);
            out.bytes = out.bytes.saturating_add(bytes.unwrap_or(0).max(0) as u64);
            if seen.insert(id.to_string()) {
                out.sessions.push(id.to_string());
            }
        }
    }
    Ok(out)
}

/// 从会话级的键里取出会话 id，例如 `composerData:<id>`、`checkpointId:<id>:<检查点>`。
fn key_session_id(key: &str) -> Option<&str> {
    if NON_SESSION_KEYS.contains(&key) {
        return None;
    }
    for namespace in SESSION_NAMESPACES {
        let Some(rest) = key
            .strip_prefix(namespace)
            .and_then(|rest| rest.strip_prefix(':'))
        else {
            continue;
        };
        let id = rest.split(':').next().unwrap_or(rest);
        return (!id.is_empty()).then_some(id);
    }
    None
}

/// 一个会话在 `cursorDiskKV` 里占的行数与字节数。
fn session_footprint(connection: &Connection, id: &str) -> (u32, u64) {
    let mut rows = 0u32;
    let mut bytes = 0u64;
    for namespace in SESSION_NAMESPACES {
        let measured = connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(octet_length(value)), 0) FROM cursorDiskKV
                 WHERE key = ?1 OR (key >= ?2 AND key < ?3)",
                [
                    format!("{namespace}:{id}"),
                    format!("{namespace}:{id}:"),
                    format!("{namespace}:{id};"),
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap_or((0, 0));
        rows = rows.saturating_add(measured.0.max(0) as u32);
        bytes = bytes.saturating_add(measured.1.max(0) as u64);
    }
    (rows, bytes)
}

fn session_bytes(connection: &Connection, id: &str) -> u64 {
    session_footprint(connection, id).1
}

/// 删掉一个会话在 `cursorDiskKV` 里的全部行，返回删除行数。
///
/// 只删 `bubbleId` + `composerData` 是不够的：实测本机库里 306 个已删会话留下了
/// 15174 行、1.9 GB 的检查点与差异数据，全部来自这里没覆盖到的键空间。
fn delete_session_rows(connection: &Connection, id: &str) -> AppResult<u32> {
    let mut removed = 0usize;
    for namespace in SESSION_NAMESPACES {
        removed += connection.execute(
            "DELETE FROM cursorDiskKV WHERE key = ?1 OR (key >= ?2 AND key < ?3)",
            [
                format!("{namespace}:{id}"),
                format!("{namespace}:{id}:"),
                format!("{namespace}:{id};"),
            ],
        )?;
    }
    Ok(removed as u32)
}

/// 一行会话头：`isSubagent` 只在列上，`value` 的 JSON 里并没有这个字段，
/// 所以两者都要带出来，光看 JSON 会把全部子代理漏掉。
struct HeaderRow {
    value: Value,
    is_subagent: bool,
}

fn read_headers(connection: &Connection) -> AppResult<BTreeMap<String, HeaderRow>> {
    let mut statement =
        connection.prepare("SELECT composerId, value, isSubagent FROM composerHeaders")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<i64>>(2)?,
        ))
    })?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (id, value, is_subagent) = row?;
        out.insert(
            id,
            HeaderRow {
                value: value
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .unwrap_or(Value::Null),
                is_subagent: is_subagent == Some(1),
            },
        );
    }
    Ok(out)
}

/// 库里所有被某个父会话引用着的子代理 id。
fn referenced_subagents(connection: &Connection) -> AppResult<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    let mut statement = connection.prepare(
        "SELECT value FROM cursorDiskKV
         WHERE key >= 'composerData:' AND key < 'composerData;'",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, rusqlite::types::Value>(0))?;
    for row in rows {
        let raw = match row? {
            rusqlite::types::Value::Text(text) => text.into_bytes(),
            rusqlite::types::Value::Blob(bytes) => bytes,
            _ => continue,
        };
        let data = serde_json::from_slice::<Value>(&raw).unwrap_or(Value::Null);
        for child in child_ids(&data) {
            out.insert(child.to_string());
        }
    }
    Ok(out)
}

fn header_cwd(header: &Value) -> String {
    header
        .get("workspaceIdentifier")
        .and_then(|value| value.get("uri"))
        .and_then(|uri| uri.get("fsPath"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .unwrap_or_default()
}

fn header_title(header: &Value, id: &str) -> String {
    for key in ["name", "subtitle"] {
        if let Some(text) = header.get(key).and_then(Value::as_str) {
            if !text.trim().is_empty() {
                return text.trim().to_string();
            }
        }
    }
    id.to_string()
}

// ---------------------------------------------------------------------------
// 进程守卫
// ---------------------------------------------------------------------------

// 刻意不与 `codex_projects::desktop_guard` 共用实现：那边在 Windows 上还要额外比对
// Microsoft Store 的包族名（Codex 有 Store 版本），而 Cursor 是普通桌面应用，只需要
// 比对进程名。把两套匹配规则硬塞进一个抽象，只会让一段本来就难以在本地验证的
// unsafe 代码更难读。
#[cfg(test)]
thread_local! {
    static TEST_RUNNING_PROBE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// 测试用注入：让下一次探测返回指定结果。
#[cfg(test)]
pub(crate) struct CursorRunningProbe(Option<bool>);

#[cfg(test)]
impl CursorRunningProbe {
    pub(crate) fn running() -> Self {
        Self(TEST_RUNNING_PROBE.replace(Some(true)))
    }

    pub(crate) fn not_running() -> Self {
        Self(TEST_RUNNING_PROBE.replace(Some(false)))
    }
}

#[cfg(test)]
impl Drop for CursorRunningProbe {
    fn drop(&mut self) {
        TEST_RUNNING_PROBE.set(self.0.take());
    }
}

pub fn ensure_cursor_not_running() -> AppResult<()> {
    if cursor_is_running()? {
        return Err(AppError::Other(CURSOR_RUNNING_ERROR.to_string()));
    }
    Ok(())
}

pub fn cursor_is_running() -> AppResult<bool> {
    #[cfg(test)]
    {
        return Ok(TEST_RUNNING_PROBE
            .with(|probe| probe.get())
            .unwrap_or(false));
    }
    #[cfg(all(not(test), target_os = "macos"))]
    {
        return macos_cursor_is_running();
    }
    #[cfg(all(not(test), target_os = "linux"))]
    {
        return linux_cursor_is_running();
    }
    #[cfg(all(not(test), windows))]
    {
        return windows_cursor_is_running();
    }
    #[cfg(all(
        not(test),
        not(windows),
        not(target_os = "linux"),
        not(target_os = "macos")
    ))]
    {
        Err(AppError::Other(
            "当前平台无法安全确认 Cursor 是否运行，已拒绝修改会话".to_string(),
        ))
    }
}

/// `<...>/Cursor.app/Contents/MacOS/Cursor`。
///
/// 只认主进程：Electron 的 Helper 进程在主进程退出后不会长期存活，拿它们做判据
/// 反而容易把残留子进程误判成"应用还开着"。
#[cfg(any(target_os = "macos", test))]
fn is_macos_cursor_executable(executable: &str) -> bool {
    let Some((bundle_prefix, binary)) = executable.rsplit_once("/Contents/MacOS/") else {
        return false;
    };
    let bundle = bundle_prefix.rsplit('/').next().unwrap_or("");
    bundle == "Cursor.app" && binary == "Cursor"
}

#[cfg(all(target_os = "macos", not(test)))]
fn macos_cursor_is_running() -> AppResult<bool> {
    unsafe {
        let needed = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if needed <= 0 {
            return Err(AppError::Other(format!(
                "无法确认 Cursor 是否运行（枚举进程失败: {}），已拒绝修改会话",
                std::io::Error::last_os_error()
            )));
        }
        // 两次调用之间可能有新进程产生，预留余量。
        let capacity = (needed as usize) + 64;
        let mut pids = vec![0 as libc::pid_t; capacity];
        let count = libc::proc_listallpids(
            pids.as_mut_ptr() as *mut libc::c_void,
            (capacity * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        );
        if count <= 0 {
            return Err(AppError::Other(format!(
                "无法确认 Cursor 是否运行（枚举进程失败: {}），已拒绝修改会话",
                std::io::Error::last_os_error()
            )));
        }
        let mut path_buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        for &pid in &pids[..(count as usize).min(capacity)] {
            if pid <= 0 {
                continue;
            }
            // 已退出或属于其他用户的进程读不到路径，它们也不可能持有本用户的会话状态。
            let len = libc::proc_pidpath(
                pid,
                path_buffer.as_mut_ptr() as *mut libc::c_void,
                path_buffer.len() as u32,
            );
            if len <= 0 {
                continue;
            }
            if is_macos_cursor_executable(&String::from_utf8_lossy(&path_buffer[..len as usize])) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Linux 版 Cursor 既有 AppImage 也有 deb 安装，可执行文件名统一是 `cursor`。
#[cfg(any(target_os = "linux", test))]
fn is_linux_cursor_executable(executable: &Path) -> bool {
    let name = executable
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = name.strip_suffix(" (deleted)").unwrap_or(&name);
    // `cursor-agent` 是 CLI，不会缓存 IDE 的会话状态，不能算作"Cursor 在运行"。
    name == "cursor" || name == "Cursor"
}

#[cfg(all(target_os = "linux", not(test)))]
fn linux_cursor_is_running() -> AppResult<bool> {
    let entries = fs::read_dir("/proc").map_err(|error| {
        AppError::Other(format!(
            "无法确认 Cursor 是否运行（读取 /proc 失败: {error}），已拒绝修改会话"
        ))
    })?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        // 进程随时可能退出，读不到 exe 就跳过。
        let Ok(executable) = fs::read_link(entry.path().join("exe")) else {
            continue;
        };
        if is_linux_cursor_executable(&executable) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(any(windows, test))]
fn is_windows_cursor_process(name: &str) -> bool {
    name.eq_ignore_ascii_case("Cursor.exe")
}

#[cfg(all(windows, not(test)))]
fn windows_cursor_is_running() -> AppResult<bool> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    struct OwnedHandle(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(AppError::Other(format!(
            "无法确认 Cursor 是否运行（进程快照失败，Windows 错误 {}），已拒绝修改会话",
            unsafe { GetLastError() }
        )));
    }
    let snapshot = OwnedHandle(snapshot);
    let mut entry = PROCESSENTRY32W::default();
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    if unsafe { Process32FirstW(snapshot.0, &mut entry) } == 0 {
        let error = unsafe { GetLastError() };
        if error == ERROR_NO_MORE_FILES {
            return Ok(false);
        }
        return Err(AppError::Other(format!(
            "无法确认 Cursor 是否运行（枚举进程失败，Windows 错误 {error}），已拒绝修改会话"
        )));
    }
    loop {
        let nul = entry
            .szExeFile
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(entry.szExeFile.len());
        if is_windows_cursor_process(&String::from_utf16_lossy(&entry.szExeFile[..nul])) {
            return Ok(true);
        }
        if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
            let error = unsafe { GetLastError() };
            if error == ERROR_NO_MORE_FILES {
                return Ok(false);
            }
            return Err(AppError::Other(format!(
                "无法确认 Cursor 是否运行（枚举进程失败，Windows 错误 {error}），已拒绝修改会话"
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        dirs: ProviderDirs,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn fixture(name: &str) -> AppResult<Fixture> {
        let root = std::env::temp_dir().join(format!(
            "cc-sessions-cursor-mutate-{name}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let storage = root.join("globalStorage");
        fs::create_dir_all(&storage)?;
        let connection = Connection::open(storage.join("state.vscdb"))?;
        connection.execute_batch(
            "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT,
                createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER,
                isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT);
             CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
             CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);",
        )?;
        connection.execute(
            "INSERT INTO ItemTable VALUES (?1, 'true')",
            [HEADER_TABLE_GATE_KEY],
        )?;
        connection.execute(
            "INSERT INTO composerHeaders VALUES ('s1', 'ws', 1000, 2000, 0, 0, 2000, NULL, ?1)",
            [json!({"name": "原名", "subtitle": "概述", "totalLinesAdded": 12}).to_string()],
        )?;
        connection.execute(
            "INSERT INTO cursorDiskKV VALUES ('composerData:s1', ?1)",
            [json!({
                "name": "原名",
                "fullConversationHeadersOnly": [{"bubbleId": "b1"}, {"bubbleId": "b2"}]
            })
            .to_string()],
        )?;
        for bubble in ["b1", "b2"] {
            connection.execute(
                "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
                params![
                    format!("bubbleId:s1:{bubble}"),
                    json!({"type": 1}).to_string()
                ],
            )?;
        }
        // 另一个会话的气泡，必须完好无损。
        connection.execute(
            "INSERT INTO cursorDiskKV VALUES ('bubbleId:s2:b1', ?1)",
            [json!({"type": 1}).to_string()],
        )?;
        drop(connection);
        let dirs = ProviderDirs {
            codex_dir: root.join("codex").to_string_lossy().into_owned(),
            cursor_dir: Some(root.to_string_lossy().into_owned()),
            cursor_agent_dir: Some(root.join("cursor-agent").to_string_lossy().into_owned()),
            ..ProviderDirs::default()
        };
        Ok(Fixture { root, dirs })
    }

    fn insert_header(
        connection: &Connection,
        id: &str,
        archived: i64,
        subagent: i64,
        value: Value,
    ) -> AppResult<()> {
        connection.execute(
            "INSERT INTO composerHeaders VALUES (?1, 'ws', 1000, 2000, ?2, ?3, 2000, NULL, ?4)",
            params![id, archived, subagent, value.to_string()],
        )?;
        Ok(())
    }

    /// 列声明是 BLOB，但 Cursor 实际写 TEXT，夹具照做。
    fn insert_kv(connection: &Connection, key: &str, value: Value) -> AppResult<()> {
        connection.execute(
            "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
            params![key, value.to_string()],
        )?;
        Ok(())
    }

    fn connect(fixture: &Fixture) -> AppResult<Connection> {
        Ok(Connection::open(cursor_sessions::state_db_path(
            &fixture.root,
        ))?)
    }

    #[test]
    fn writes_are_refused_while_cursor_is_running() -> AppResult<()> {
        let fixture = fixture("running")?;
        let _probe = CursorRunningProbe::running();
        let error = set_archived(&fixture.dirs, "s1", true).unwrap_err();
        assert!(error.to_string().contains("请完全退出 Cursor"));
        assert!(rename_session(&fixture.dirs, "s1", "新名").is_err());
        assert!(delete_session(&fixture.dirs, "s1").is_err());

        // 库必须原封不动。
        let connection = connect(&fixture)?;
        let archived: i64 = connection.query_row(
            "SELECT isArchived FROM composerHeaders WHERE composerId = 's1'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(archived, 0);
        Ok(())
    }

    #[test]
    fn archiving_updates_both_the_column_and_the_header_json() -> AppResult<()> {
        let fixture = fixture("archive")?;
        let _probe = CursorRunningProbe::not_running();
        set_archived(&fixture.dirs, "s1", true)?;

        let connection = connect(&fixture)?;
        let (archived, value): (i64, String) = connection.query_row(
            "SELECT isArchived, value FROM composerHeaders WHERE composerId = 's1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(archived, 1);
        let header: Value = serde_json::from_str(&value)?;
        assert_eq!(header["isArchived"], true);
        // 其余字段不能被顺手抹掉。
        assert_eq!(header["name"], "原名");
        assert_eq!(header["totalLinesAdded"], 12);
        Ok(())
    }

    #[test]
    fn renaming_syncs_the_header_and_the_conversation_copy() -> AppResult<()> {
        let fixture = fixture("rename")?;
        let _probe = CursorRunningProbe::not_running();
        assert_eq!(rename_session(&fixture.dirs, "s1", "  新名字  ")?, 1);

        let connection = connect(&fixture)?;
        let value: String = connection.query_row(
            "SELECT value FROM composerHeaders WHERE composerId = 's1'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(serde_json::from_str::<Value>(&value)?["name"], "新名字");
        let data: String = connection.query_row(
            "SELECT value FROM cursorDiskKV WHERE key = 'composerData:s1'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(serde_json::from_str::<Value>(&data)?["name"], "新名字");
        Ok(())
    }

    #[test]
    fn renaming_rejects_blank_and_overlong_titles() -> AppResult<()> {
        let fixture = fixture("rename-invalid")?;
        let _probe = CursorRunningProbe::not_running();
        assert!(rename_session(&fixture.dirs, "s1", "   ").is_err());
        assert!(rename_session(&fixture.dirs, "s1", &"字".repeat(121)).is_err());
        assert!(rename_session(&fixture.dirs, "s1", &"字".repeat(120)).is_ok());
        Ok(())
    }

    #[test]
    fn deleting_removes_the_header_index_and_every_bubble() -> AppResult<()> {
        let fixture = fixture("delete")?;
        let _probe = CursorRunningProbe::not_running();
        let report = delete_session(&fixture.dirs, "s1")?;
        assert!(report.ok);
        // 2 条气泡 + composerData；messageRequestContext 不存在所以不计。
        assert_eq!(report.threads_rows_deleted, 3);

        let connection = connect(&fixture)?;
        let headers: i64 =
            connection.query_row("SELECT COUNT(*) FROM composerHeaders", [], |row| row.get(0))?;
        assert_eq!(headers, 0);
        let leftovers: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key LIKE 'bubbleId:s1:%' OR key = 'composerData:s1'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(leftovers, 0);
        // 同库里其它会话的气泡不能受影响。
        let others: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key = 'bubbleId:s2:b1'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(others, 1);
        Ok(())
    }

    /// 删一个会话要把它在全部键空间里的行都带走。
    ///
    /// Cursor 自己的删除只清了一部分，实测本机因此攒下 1.9 GB 已删会话的检查点数据。
    #[test]
    fn deleting_clears_every_session_scoped_namespace() -> AppResult<()> {
        let fixture = fixture("delete-namespaces")?;
        let connection = connect(&fixture)?;
        for namespace in SESSION_NAMESPACES {
            // 两种键形状都铺一遍：精确键与 `<空间>:<id>:<子 id>`。
            insert_kv(&connection, &format!("{namespace}:s1"), json!({"x": 1}))?;
            insert_kv(&connection, &format!("{namespace}:s1:sub"), json!({"x": 1}))?;
            // 同名前缀的另一个会话不能被误删。
            insert_kv(&connection, &format!("{namespace}:s2:sub"), json!({"x": 1}))?;
        }
        // 落在会话键空间里的全局键必须留下。
        insert_kv(
            &connection,
            "composerVirtualRowHeights:_recentIds",
            json!(["s1"]),
        )?;
        drop(connection);

        let _probe = CursorRunningProbe::not_running();
        delete_session(&fixture.dirs, "s1")?;

        let connection = connect(&fixture)?;
        for namespace in SESSION_NAMESPACES {
            let left: i64 = connection.query_row(
                "SELECT COUNT(*) FROM cursorDiskKV WHERE key = ?1 OR (key >= ?2 AND key < ?3)",
                [
                    format!("{namespace}:s1"),
                    format!("{namespace}:s1:"),
                    format!("{namespace}:s1;"),
                ],
                |row| row.get(0),
            )?;
            assert_eq!(left, 0, "{namespace} 还有 s1 的残留");
            let other: i64 = connection.query_row(
                "SELECT COUNT(*) FROM cursorDiskKV WHERE key = ?1",
                [format!("{namespace}:s2:sub")],
                |row| row.get(0),
            )?;
            assert_eq!(other, 1, "{namespace} 误删了 s2");
        }
        let recent: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key = 'composerVirtualRowHeights:_recentIds'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(recent, 1, "全局键被当成孤儿删掉了");
        Ok(())
    }

    /// 会话头没了但辅助数据还在的行，要被算成孤儿并清掉。
    #[test]
    fn orphan_scan_covers_auxiliary_namespaces() -> AppResult<()> {
        let fixture = fixture("orphan-namespaces")?;
        let connection = connect(&fixture)?;
        for namespace in ["checkpointId", "codeBlockDiff", "messageRequestContext"] {
            insert_kv(
                &connection,
                &format!("{namespace}:gone:x"),
                json!({"payload": "1234567890"}),
            )?;
        }
        insert_kv(&connection, "composerVirtualRowHeights:gone", json!([1, 2]))?;
        insert_kv(
            &connection,
            "composerVirtualRowHeights:_recentIds",
            json!(["s1"]),
        )?;
        drop(connection);

        let report = diagnose_residue(&fixture.dirs)?;
        let orphans = report
            .groups
            .iter()
            .find(|group| group.kind == RESIDUE_ORPHAN_RECORDS)
            .expect("孤儿记录分组");
        // `gone` 的 4 行，外加夹具里本来就没有会话头的 `bubbleId:s2:b1`。
        assert_eq!(orphans.sessions, 2);
        assert_eq!(orphans.rows, 5);
        assert!(orphans.bytes > 0);

        let _probe = CursorRunningProbe::not_running();
        prune_residue(&fixture.dirs, &[RESIDUE_ORPHAN_RECORDS.to_string()], false)?;
        let connection = connect(&fixture)?;
        let left: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key LIKE '%:gone%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(left, 0);
        let recent: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key = 'composerVirtualRowHeights:_recentIds'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(recent, 1, "全局键被当成孤儿删掉了");
        Ok(())
    }

    /// 索引缺失时不能留下孤儿气泡。
    #[test]
    fn deleting_sweeps_bubbles_even_without_a_usable_index() -> AppResult<()> {
        let fixture = fixture("delete-orphan")?;
        let connection = connect(&fixture)?;
        connection.execute(
            "UPDATE cursorDiskKV SET value = ?1 WHERE key = 'composerData:s1'",
            [json!({"name": "原名"}).to_string()],
        )?;
        drop(connection);

        let _probe = CursorRunningProbe::not_running();
        delete_session(&fixture.dirs, "s1")?;
        let connection = connect(&fixture)?;
        let leftovers: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key LIKE 'bubbleId:s1:%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(leftovers, 0);
        Ok(())
    }

    /// 删除主会话必须把它的子会话一起带走，否则父没了、子会话再也访问不到。
    #[test]
    fn deleting_a_parent_cascades_to_its_subagents() -> AppResult<()> {
        let fixture = fixture("cascade")?;
        let connection = connect(&fixture)?;
        // parent -> kid -> grandkid，实测存在这种多层嵌套。
        for (id, kids) in [
            ("parent", vec!["kid"]),
            ("kid", vec!["grandkid"]),
            ("grandkid", Vec::new()),
        ] {
            let subagent = i64::from(id != "parent");
            insert_header(&connection, id, 0, subagent, json!({ "name": id }))?;
            insert_kv(
                &connection,
                &format!("composerData:{id}"),
                json!({
                    "fullConversationHeadersOnly": [{"bubbleId": "b1"}],
                    "subagentComposerIds": kids,
                }),
            )?;
            insert_kv(
                &connection,
                &format!("bubbleId:{id}:b1"),
                json!({"type": 1, "text": id}),
            )?;
        }
        drop(connection);

        let _probe = CursorRunningProbe::not_running();
        delete_session(&fixture.dirs, "parent")?;

        let connection = connect(&fixture)?;
        let ids: Vec<String> = connection
            .prepare("SELECT composerId FROM composerHeaders ORDER BY composerId")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        // 整条链都应消失，夹具自带的 s1 不受影响。
        assert_eq!(ids, vec!["s1".to_string()]);
        let leftovers: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV
             WHERE key LIKE 'bubbleId:parent:%' OR key LIKE 'bubbleId:kid:%'
                OR key LIKE 'bubbleId:grandkid:%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(leftovers, 0);
        Ok(())
    }

    /// 同一个子会话被两个父引用时，删其中一个父不能把它带走。
    #[test]
    fn a_subagent_shared_with_another_parent_survives() -> AppResult<()> {
        let fixture = fixture("cascade-shared")?;
        let connection = connect(&fixture)?;
        for id in ["p1", "p2", "shared"] {
            let subagent = i64::from(id == "shared");
            insert_header(&connection, id, 0, subagent, json!({ "name": id }))?;
            insert_kv(
                &connection,
                &format!("bubbleId:{id}:b1"),
                json!({"type": 1, "text": id}),
            )?;
        }
        for parent in ["p1", "p2"] {
            insert_kv(
                &connection,
                &format!("composerData:{parent}"),
                json!({
                    "fullConversationHeadersOnly": [{"bubbleId": "b1"}],
                    "subagentComposerIds": ["shared"],
                }),
            )?;
        }
        insert_kv(
            &connection,
            "composerData:shared",
            json!({"fullConversationHeadersOnly": [{"bubbleId": "b1"}]}),
        )?;
        drop(connection);

        let _probe = CursorRunningProbe::not_running();
        delete_session(&fixture.dirs, "p1")?;

        let connection = connect(&fixture)?;
        let ids: Vec<String> = connection
            .prepare("SELECT composerId FROM composerHeaders ORDER BY composerId")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        // p2 还引用着 shared，它必须留下。
        assert_eq!(
            ids,
            vec!["p2".to_string(), "s1".to_string(), "shared".to_string()]
        );
        Ok(())
    }

    /// 没有父引用的子会话是访问不到的死数据，应当被诊断出来。
    #[test]
    fn subagents_without_a_parent_are_reported_as_residue() -> AppResult<()> {
        let fixture = fixture("orphan-subagent")?;
        let connection = connect(&fixture)?;
        insert_header(
            &connection,
            "lonely",
            0,
            1,
            json!({ "name": "没爹的子会话" }),
        )?;
        insert_kv(
            &connection,
            "composerData:lonely",
            json!({"fullConversationHeadersOnly": [{"bubbleId": "b1"}]}),
        )?;
        insert_kv(
            &connection,
            "bubbleId:lonely:b1",
            json!({"type": 1, "text": "x"}),
        )?;
        // 有父引用的子会话不该被算进来。
        insert_header(&connection, "owned", 0, 1, json!({ "name": "有爹的" }))?;
        insert_kv(
            &connection,
            "composerData:owned",
            json!({"fullConversationHeadersOnly": [{"bubbleId": "b1"}]}),
        )?;
        insert_kv(
            &connection,
            "bubbleId:owned:b1",
            json!({"type": 1, "text": "y"}),
        )?;
        insert_kv(
            &connection,
            "composerData:s1",
            json!({
                "fullConversationHeadersOnly": [{"bubbleId": "b1"}, {"bubbleId": "b2"}],
                "subagentComposerIds": ["owned"],
            }),
        )?;
        drop(connection);

        let report = diagnose_residue(&fixture.dirs)?;
        let group = report
            .groups
            .iter()
            .find(|g| g.kind == RESIDUE_ORPHAN_SUBAGENTS)
            .expect("缺少无主子会话分组");
        assert_eq!(group.sessions, 1);
        assert_eq!(group.samples[0].id, "lonely");
        assert!(group.destructive);
        // 子会话不计入可见会话，只有 s1 是主会话。
        assert_eq!(report.visible_sessions, 1);
        Ok(())
    }

    #[test]
    fn unknown_sessions_are_reported_as_missing() -> AppResult<()> {
        let fixture = fixture("missing")?;
        let _probe = CursorRunningProbe::not_running();
        let error = delete_session(&fixture.dirs, "does-not-exist").unwrap_err();
        assert!(error.to_string().contains("不存在"));
        Ok(())
    }

    #[test]
    fn writes_are_refused_when_the_legacy_header_store_is_authoritative() -> AppResult<()> {
        let fixture = fixture("legacy-gate")?;
        let connection = connect(&fixture)?;
        connection.execute(
            "UPDATE ItemTable SET value = 'false' WHERE key = ?1",
            [HEADER_TABLE_GATE_KEY],
        )?;
        drop(connection);

        let _probe = CursorRunningProbe::not_running();
        let error = set_archived(&fixture.dirs, "s1", true).unwrap_err();
        assert!(error.to_string().contains("已拒绝写入"));
        Ok(())
    }

    /// 内容块读不动的时候，跳过那一类就行，会话级的清理不能跟着回滚。
    #[test]
    fn an_unreadable_content_block_only_skips_its_own_kind() -> AppResult<()> {
        let fixture = fixture("blob-scan-error")?;
        let connection = connect(&fixture)?;
        // 一个没人引用的内容块。
        insert_kv(
            &connection,
            &format!("agentKv:blob:{}", "ab".repeat(32)),
            json!({"role": "user"}),
        )?;
        // 一行解不开的 conversationState，让可达性扫描报错。
        connection.execute(
            "UPDATE cursorDiskKV SET value = ?1 WHERE key = 'composerData:s1'",
            [json!({"conversationState": "~不是 base64"}).to_string()],
        )?;
        drop(connection);

        let _probe = CursorRunningProbe::not_running();
        let report = prune_residue(
            &fixture.dirs,
            &[
                RESIDUE_ORPHAN_RECORDS.to_string(),
                RESIDUE_ORPHAN_BLOBS.to_string(),
            ],
            false,
        )?;
        assert_eq!(report.blob_scan_errors, 1);

        let connection = connect(&fixture)?;
        let blobs: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key LIKE 'agentKv:blob:%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(blobs, 1, "扫描出错时内容块一个都不许删");
        // 夹具里 `bubbleId:s2:b1` 没有会话头，属于孤儿记录，这一类必须照常清掉。
        let orphan: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key = 'bubbleId:s2:b1'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(orphan, 0, "会话级清理被内容块的错误连累回滚了");
        Ok(())
    }

    /// 诊断要能分清三类残留，且不把正常会话算进去。
    #[test]
    fn residue_diagnosis_separates_empty_orphan_and_missing_project() -> AppResult<()> {
        let fixture = fixture("residue")?;
        let connection = connect(&fixture)?;
        // s1 是夹具自带的正常会话（2 条气泡、无项目路径）。
        // 空会话：有标题没内容。
        insert_header(&connection, "empty1", 0, 0, json!({ "name": "空的" }))?;
        // 项目目录已不存在，但有内容。
        insert_header(
            &connection,
            "gone",
            0,
            0,
            json!({ "name": "目录没了", "workspaceIdentifier": {"uri": {"fsPath": "/definitely/not/here"}} }),
        )?;
        insert_kv(
            &connection,
            "composerData:gone",
            json!({"fullConversationHeadersOnly": [{"bubbleId": "b1"}]}),
        )?;
        insert_kv(
            &connection,
            "bubbleId:gone:b1",
            json!({"type": 1, "text": "有内容"}),
        )?;
        // 孤儿：没有会话头，但索引和气泡都在。
        insert_kv(
            &connection,
            "composerData:orphan",
            json!({"fullConversationHeadersOnly": [{"bubbleId": "b1"}]}),
        )?;
        insert_kv(
            &connection,
            "bubbleId:orphan:b1",
            json!({"type": 1, "text": "孤儿"}),
        )?;
        drop(connection);

        let report = diagnose_residue(&fixture.dirs)?;
        let group = |kind: &str| {
            report
                .groups
                .iter()
                .find(|g| g.kind == kind)
                .unwrap_or_else(|| panic!("缺少分组 {kind}"))
        };
        assert_eq!(group(RESIDUE_EMPTY_SESSIONS).sessions, 1);
        assert_eq!(group(RESIDUE_MISSING_PROJECT).sessions, 1);
        assert!(group(RESIDUE_MISSING_PROJECT).destructive);
        // orphan（索引 + 气泡）与夹具自带的 s2（只有一条气泡）都没有会话头。
        assert_eq!(group(RESIDUE_ORPHAN_RECORDS).sessions, 2);
        assert_eq!(group(RESIDUE_ORPHAN_RECORDS).rows, 3);
        assert!(group(RESIDUE_ORPHAN_RECORDS).bytes > 0);
        // s1 与 gone 有内容，都算可见会话；empty1 不算。
        assert_eq!(report.visible_sessions, 2);
        Ok(())
    }

    /// 默认只清安全的两类，有内容的会话必须显式勾选才会删。
    #[test]
    fn pruning_only_touches_the_selected_kinds() -> AppResult<()> {
        let fixture = fixture("residue-prune")?;
        let connection = connect(&fixture)?;
        insert_header(&connection, "empty1", 0, 0, json!({ "name": "空的" }))?;
        insert_header(
            &connection,
            "gone",
            0,
            0,
            json!({ "name": "目录没了", "workspaceIdentifier": {"uri": {"fsPath": "/definitely/not/here"}} }),
        )?;
        insert_kv(
            &connection,
            "composerData:gone",
            json!({"fullConversationHeadersOnly": [{"bubbleId": "b1"}]}),
        )?;
        insert_kv(
            &connection,
            "bubbleId:gone:b1",
            json!({"type": 1, "text": "有内容"}),
        )?;
        insert_kv(
            &connection,
            "bubbleId:orphan:b1",
            json!({"type": 1, "text": "孤儿"}),
        )?;
        drop(connection);

        let _probe = CursorRunningProbe::not_running();
        let report = prune_residue(
            &fixture.dirs,
            &[
                RESIDUE_EMPTY_SESSIONS.to_string(),
                RESIDUE_ORPHAN_RECORDS.to_string(),
            ],
            false,
        )?;
        assert_eq!(report.removed_header_rows, 1);
        assert!(report.freed_bytes > 0);

        let connection = connect(&fixture)?;
        let ids: Vec<String> = connection
            .prepare("SELECT composerId FROM composerHeaders ORDER BY composerId")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        // 目录不存在的会话没被勾选，必须原样保留。
        assert_eq!(ids, vec!["gone".to_string(), "s1".to_string()]);
        let orphan: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key = 'bubbleId:orphan:b1'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(orphan, 0);
        // 正常会话的气泡不能受影响。
        let kept: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key LIKE 'bubbleId:s1:%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(kept, 2);
        Ok(())
    }

    #[test]
    fn a_dry_run_reports_without_changing_anything() -> AppResult<()> {
        let fixture = fixture("residue-dry")?;
        let connection = connect(&fixture)?;
        insert_header(&connection, "empty1", 0, 0, json!({ "name": "空的" }))?;
        drop(connection);

        // 试运行不需要 Cursor 退出。
        let _probe = CursorRunningProbe::running();
        let report = prune_residue(&fixture.dirs, &[RESIDUE_EMPTY_SESSIONS.to_string()], true)?;
        assert!(report.dry_run);
        assert_eq!(report.removed_header_rows, 1);

        let connection = connect(&fixture)?;
        let rows: i64 =
            connection.query_row("SELECT COUNT(*) FROM composerHeaders", [], |row| row.get(0))?;
        assert_eq!(rows, 2);
        Ok(())
    }

    #[test]
    fn pruning_rejects_an_empty_or_unknown_selection() -> AppResult<()> {
        let fixture = fixture("residue-args")?;
        let _probe = CursorRunningProbe::not_running();
        assert!(prune_residue(&fixture.dirs, &[], false).is_err());
        assert!(prune_residue(&fixture.dirs, &["whatever".to_string()], false).is_err());
        Ok(())
    }

    #[test]
    fn residue_pruning_is_refused_while_cursor_is_running() -> AppResult<()> {
        let fixture = fixture("residue-running")?;
        let _probe = CursorRunningProbe::running();
        let error =
            prune_residue(&fixture.dirs, &[RESIDUE_EMPTY_SESSIONS.to_string()], false).unwrap_err();
        assert!(error.to_string().contains("请完全退出 Cursor"));
        Ok(())
    }

    #[test]
    fn session_ids_are_parsed_out_of_both_key_shapes() {
        assert_eq!(key_session_id("composerData:abc"), Some("abc"));
        assert_eq!(key_session_id("bubbleId:abc:b1"), Some("abc"));
        assert_eq!(key_session_id("other:abc"), None);
    }

    #[test]
    fn only_the_cursor_bundle_main_binary_counts_as_running() {
        assert!(is_macos_cursor_executable(
            "/Applications/Cursor.app/Contents/MacOS/Cursor"
        ));
        // 同名目录、其它应用、Helper 进程都不算。
        assert!(!is_macos_cursor_executable(
            "/Applications/Cursor.app/Contents/Frameworks/Cursor Helper.app/Contents/MacOS/Cursor Helper"
        ));
        assert!(!is_macos_cursor_executable(
            "/Applications/Visual Studio Code.app/Contents/MacOS/Electron"
        ));
        assert!(!is_macos_cursor_executable("/usr/local/bin/Cursor"));

        assert!(is_linux_cursor_executable(Path::new("/opt/cursor/cursor")));
        assert!(is_linux_cursor_executable(Path::new(
            "/opt/cursor/cursor (deleted)"
        )));
        // CLI 不缓存 IDE 会话状态。
        assert!(!is_linux_cursor_executable(Path::new(
            "/home/u/.local/bin/cursor-agent"
        )));

        assert!(is_windows_cursor_process("Cursor.exe"));
        assert!(is_windows_cursor_process("cursor.exe"));
        assert!(!is_windows_cursor_process("cursor-agent.exe"));
    }
}
