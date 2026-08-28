//! `agentKv:blob:` 与 `composer.content.` 两个内容寻址存储的无引用回收。
//!
//! 这两个空间的键是内容的 sha256，不带会话 id，没法像 `bubbleId:<会话>:…` 那样直接判断
//! 归属，只能做可达性分析。Cursor 自己对 `agentKv` 有同样的机制（命令面板里的
//! `GC Agent KV Blobs`，实现见 bundle 里的 `AgentKvGC`），对 `composer.content` 则完全没有
//! 回收路径——所以后者只会一直涨。
//!
//! # 引用是怎么存的
//!
//! `composerData:<会话>` 和 `bubbleId:<会话>:<气泡>` 的 JSON 里有个 `conversationState`
//! 字段，值是 base64（`~` 开头的要先去掉这个前缀）编码的二进制，blob id 以**原始 32 字节**
//! 出现在里面。按十六进制去搜是搜不到的。
//!
//! `composer.content.<sha256>` 则相反，以十六进制字面量出现在气泡的
//! `toolFormerData.result` 里（`beforeContentId` / `afterContentId`）。
//!
//! # 判定策略：宁可留下，不可误删
//!
//! blob 引用不做协议解析，而是**在字节流里找任何一段等于已知 blob id 的 32 字节窗口**。
//! 这是"按 protobuf 字段解析"的严格超集：字段号、嵌套层数、编码方式怎么变都不影响，
//! 代价只是可能多留几个块，不会少留。blob 之间还会互相引用（实测 376 万条边），
//! 所以要一路做闭包。
//!
//! 与 Cursor 一致的一条安全阀：**只要遍历过程中出现任何一次解码失败就整体放弃删除**，
//! 因为读不出来的那部分可能正引用着别的块。

use std::collections::{HashMap, HashSet, VecDeque};

use base64::engine::general_purpose::{STANDARD, URL_SAFE};
use base64::Engine;
use rusqlite::Connection;
use serde_json::Value;

use crate::error::AppResult;

const BLOB_PREFIX: &str = "agentKv:blob:";
const CHECKPOINT_PREFIX: &str = "agentKv:checkpoint:";
const BUBBLE_CHECKPOINT_PREFIX: &str = "agentKv:bubbleCheckpoint:";
const CONTENT_PREFIX: &str = "composer.content.";

/// 一次可达性扫描的结果。
#[derive(Debug, Default)]
pub struct ContentSweep {
    pub blobs_total: u32,
    pub blobs_live: u32,
    pub blob_orphan_rows: u32,
    pub blob_orphan_bytes: u64,
    pub content_total: u32,
    pub content_live: u32,
    pub content_orphan_rows: u32,
    pub content_orphan_bytes: u64,
    /// 遍历过程中读不动的行数。不为 0 时禁止删除。
    pub errors: u32,
    /// 可以删除的完整键。
    pub orphan_keys: Vec<String>,
}

impl ContentSweep {
    pub fn orphan_rows(&self) -> u32 {
        self.blob_orphan_rows
            .saturating_add(self.content_orphan_rows)
    }

    pub fn orphan_bytes(&self) -> u64 {
        self.blob_orphan_bytes
            .saturating_add(self.content_orphan_bytes)
    }
}

type Id = [u8; 32];

/// 已知 id 的集合，外挂一张 2 MiB 的前 3 字节位图做粗筛。
///
/// 逐字节滑窗要做上亿次查询，先过位图能把绝大多数位置在一次内存访问里排掉。
struct Index {
    sizes: HashMap<Id, u32>,
    prefix: Vec<u64>,
}

impl Index {
    fn new() -> Self {
        Self {
            sizes: HashMap::new(),
            prefix: vec![0u64; 1 << 18],
        }
    }

    fn insert(&mut self, id: Id, bytes: u32) {
        let slot = prefix_slot(&id);
        self.prefix[slot >> 6] |= 1u64 << (slot & 63);
        self.sizes.insert(id, bytes);
    }

    fn prefix_hit(&self, window: &[u8]) -> bool {
        let slot = prefix_slot(window);
        self.prefix[slot >> 6] >> (slot & 63) & 1 == 1
    }

    fn is_empty(&self) -> bool {
        self.sizes.is_empty()
    }
}

fn prefix_slot(bytes: &[u8]) -> usize {
    ((bytes[0] as usize) << 16) | ((bytes[1] as usize) << 8) | bytes[2] as usize
}

/// 扫描整库，算出两个内容存储里没有任何引用的行。
pub fn sweep(connection: &Connection) -> AppResult<ContentSweep> {
    let blobs = load_index(connection, BLOB_PREFIX)?;
    let contents = load_index(connection, CONTENT_PREFIX)?;
    let mut sweep = ContentSweep {
        blobs_total: blobs.sizes.len() as u32,
        content_total: contents.sizes.len() as u32,
        ..Default::default()
    };
    if blobs.is_empty() && contents.is_empty() {
        return Ok(sweep);
    }

    let mut live_blobs: HashSet<Id> = HashSet::new();
    let mut live_contents: HashSet<Id> = HashSet::new();
    let mut queue: VecDeque<Id> = VecDeque::new();

    collect_roots(
        connection,
        &blobs,
        &contents,
        &mut live_blobs,
        &mut live_contents,
        &mut queue,
        &mut sweep.errors,
    )?;
    close_over_blobs(connection, &blobs, &mut live_blobs, &mut queue, &mut sweep)?;

    sweep.blobs_live = live_blobs.len() as u32;
    sweep.content_live = live_contents.len() as u32;
    for (id, bytes) in &blobs.sizes {
        if !live_blobs.contains(id) {
            sweep.blob_orphan_rows = sweep.blob_orphan_rows.saturating_add(1);
            sweep.blob_orphan_bytes = sweep.blob_orphan_bytes.saturating_add(*bytes as u64);
            sweep
                .orphan_keys
                .push(format!("{BLOB_PREFIX}{}", hex::encode(id)));
        }
    }
    for (id, bytes) in &contents.sizes {
        if !live_contents.contains(id) {
            sweep.content_orphan_rows = sweep.content_orphan_rows.saturating_add(1);
            sweep.content_orphan_bytes = sweep.content_orphan_bytes.saturating_add(*bytes as u64);
            sweep
                .orphan_keys
                .push(format!("{CONTENT_PREFIX}{}", hex::encode(id)));
        }
    }
    Ok(sweep)
}

/// 删除扫描出来的孤儿行。扫描期间出过错就拒绝执行。
pub fn delete_orphans(connection: &Connection, sweep: &ContentSweep) -> AppResult<u32> {
    if sweep.errors > 0 {
        return Err(crate::error::AppError::Other(format!(
            "有 {} 行内容块读不出来，无法确认剩下的是否还被引用，已放弃删除",
            sweep.errors
        )));
    }
    let mut statement = connection.prepare("DELETE FROM cursorDiskKV WHERE key = ?1")?;
    let mut removed = 0u32;
    for key in &sweep.orphan_keys {
        removed = removed.saturating_add(statement.execute([key])? as u32);
    }
    Ok(removed)
}

fn load_index(connection: &Connection, prefix: &str) -> AppResult<Index> {
    let mut index = Index::new();
    let mut statement = connection.prepare(
        "SELECT key, octet_length(value) FROM cursorDiskKV WHERE key >= ?1 AND key < ?2",
    )?;
    let rows = statement.query_map([prefix, &range_end(prefix)], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
    })?;
    for row in rows {
        let (key, bytes) = row?;
        let Some(id) = key.strip_prefix(prefix).and_then(parse_id) else {
            continue;
        };
        index.insert(id, bytes.unwrap_or(0).max(0) as u32);
    }
    Ok(index)
}

/// 从会话数据里找出根引用。
///
/// 覆盖 `agentKv:blob:` 之外的**全部**行：blob 引用只可能出现在 `conversationState` 里，
/// 但 `composer.content.` 的十六进制字面量出现在哪一类行里没法先验排除，索性全扫。
fn collect_roots(
    connection: &Connection,
    blobs: &Index,
    contents: &Index,
    live_blobs: &mut HashSet<Id>,
    live_contents: &mut HashSet<Id>,
    queue: &mut VecDeque<Id>,
    errors: &mut u32,
) -> AppResult<()> {
    let end = range_end(BLOB_PREFIX);
    let mut statement = connection.prepare(
        "SELECT key, value FROM cursorDiskKV WHERE key < ?1
         UNION ALL
         SELECT key, value FROM cursorDiskKV WHERE key >= ?2",
    )?;
    let rows = statement.query_map([BLOB_PREFIX, end.as_str()], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, rusqlite::types::Value>(1)?,
        ))
    })?;
    for row in rows {
        let (key, value) = row?;
        let Some(key) = key else { continue };
        let Some(raw) = as_bytes(value) else { continue };

        // `composer.content.<十六进制>` 在任何行里都算引用。
        for id in scan_content_refs(&raw, contents) {
            live_contents.insert(id);
        }

        if let Some(pointer) = checkpoint_pointer(&key, &raw, errors) {
            if blobs.sizes.contains_key(&pointer) && live_blobs.insert(pointer) {
                queue.push_back(pointer);
            }
            continue;
        }
        if !(key.starts_with("composerData:") || key.starts_with("bubbleId:")) {
            continue;
        }
        let Ok(json) = serde_json::from_slice::<Value>(&raw) else {
            continue;
        };
        let Some(state) = json.get("conversationState").and_then(Value::as_str) else {
            continue;
        };
        if state.is_empty() {
            continue;
        }
        let Some(decoded) = decode_base64(state) else {
            // 有这个字段却解不开，说明这一行的引用没能算进来。
            *errors = errors.saturating_add(1);
            continue;
        };
        for id in scan_blob_refs(&decoded, blobs) {
            if live_blobs.insert(id) {
                queue.push_back(id);
            }
        }
    }
    Ok(())
}

/// 顺着 blob 之间的引用做闭包。
fn close_over_blobs(
    connection: &Connection,
    blobs: &Index,
    live_blobs: &mut HashSet<Id>,
    queue: &mut VecDeque<Id>,
    sweep: &mut ContentSweep,
) -> AppResult<()> {
    let mut statement = connection.prepare("SELECT value FROM cursorDiskKV WHERE key = ?1")?;
    while let Some(current) = queue.pop_front() {
        let key = format!("{BLOB_PREFIX}{}", hex::encode(current));
        let value = statement
            .query_row([&key], |row| row.get::<_, rusqlite::types::Value>(0))
            .ok();
        let Some(raw) = value.and_then(as_bytes) else {
            // 键在索引里却读不出内容，剩下的可达性无从判断。
            sweep.errors = sweep.errors.saturating_add(1);
            continue;
        };
        for id in scan_blob_refs(&raw, blobs) {
            if live_blobs.insert(id) {
                queue.push_back(id);
            }
        }
    }
    Ok(())
}

/// `agentKv:checkpoint:<会话>` / `agentKv:bubbleCheckpoint:<会话>:<气泡>` 存的是
/// base64 后的 32 字节根指针。本机库里这两类键已经被 Cursor 清空，但别的安装上可能还有。
fn checkpoint_pointer(key: &str, raw: &[u8], errors: &mut u32) -> Option<Id> {
    if !(key.starts_with(CHECKPOINT_PREFIX) || key.starts_with(BUBBLE_CHECKPOINT_PREFIX)) {
        return None;
    }
    let text = std::str::from_utf8(raw).ok()?.trim();
    match decode_base64(text).and_then(|bytes| bytes.try_into().ok()) {
        Some(id) => Some(id),
        None => {
            *errors = errors.saturating_add(1);
            None
        }
    }
}

/// 找出字节流里任何一段等于已知 blob id 的 32 字节窗口。
fn scan_blob_refs(buf: &[u8], index: &Index) -> Vec<Id> {
    let mut out = Vec::new();
    if buf.len() < 32 || index.is_empty() {
        return out;
    }
    for start in 0..=buf.len() - 32 {
        if !index.prefix_hit(&buf[start..start + 3]) {
            continue;
        }
        let mut id = [0u8; 32];
        id.copy_from_slice(&buf[start..start + 32]);
        if index.sizes.contains_key(&id) {
            out.push(id);
        }
    }
    out
}

/// 找出 `composer.content.<64 位十六进制>` 字面量。
fn scan_content_refs(buf: &[u8], index: &Index) -> Vec<Id> {
    let mut out = Vec::new();
    if index.is_empty() {
        return out;
    }
    let needle = CONTENT_PREFIX.as_bytes();
    let span = needle.len() + 64;
    if buf.len() < span {
        return out;
    }
    for start in 0..=buf.len() - span {
        if buf[start] != needle[0] || &buf[start..start + needle.len()] != needle {
            continue;
        }
        let digits = &buf[start + needle.len()..start + span];
        let Ok(text) = std::str::from_utf8(digits) else {
            continue;
        };
        if let Some(id) = parse_id(text) {
            if index.sizes.contains_key(&id) {
                out.push(id);
            }
        }
    }
    out
}

fn parse_id(hex_text: &str) -> Option<Id> {
    if hex_text.len() != 64 || !hex_text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    hex::decode(hex_text).ok()?.try_into().ok()
}

fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let body = text.strip_prefix('~').unwrap_or(text);
    STANDARD
        .decode(body)
        .or_else(|_| URL_SAFE.decode(body))
        .ok()
}

fn as_bytes(value: rusqlite::types::Value) -> Option<Vec<u8>> {
    match value {
        rusqlite::types::Value::Text(text) => Some(text.into_bytes()),
        rusqlite::types::Value::Blob(bytes) => Some(bytes),
        _ => None,
    }
}

/// 前缀区间的右端。`:` 的下一个字符是 `;`，`.` 的下一个是 `/`。
fn range_end(prefix: &str) -> String {
    let mut end = prefix.to_string();
    let last = end.pop().unwrap_or(':');
    end.push((last as u8 + 1) as char);
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;

    fn memory_db() -> AppResult<Connection> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
        )?;
        Ok(connection)
    }

    fn put_blob(connection: &Connection, body: &[u8]) -> AppResult<Id> {
        use sha2::{Digest, Sha256};
        let id: Id = Sha256::digest(body).into();
        connection.execute(
            "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
            rusqlite::params![format!("{BLOB_PREFIX}{}", hex::encode(id)), body],
        )?;
        Ok(id)
    }

    fn put_content(connection: &Connection, body: &str) -> AppResult<Id> {
        use sha2::{Digest, Sha256};
        let id: Id = Sha256::digest(body.as_bytes()).into();
        connection.execute(
            "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
            rusqlite::params![format!("{CONTENT_PREFIX}{}", hex::encode(id)), body],
        )?;
        Ok(id)
    }

    fn conversation_state(ids: &[Id]) -> String {
        // 真实数据是 protobuf，这里只关心"原始 32 字节出现在里面"这一点。
        let mut raw = Vec::new();
        for id in ids {
            raw.push(0x0a);
            raw.push(0x20);
            raw.extend_from_slice(id);
        }
        format!("~{}", STANDARD.encode(raw))
    }

    #[test]
    fn blobs_reachable_through_the_reference_chain_survive() -> AppResult<()> {
        let connection = memory_db()?;
        let leaf = put_blob(&connection, b"leaf payload")?;
        // 中间节点把叶子的 id 以原始字节嵌进去。
        let mut middle_body = b"header".to_vec();
        middle_body.push(0x12);
        middle_body.push(0x20);
        middle_body.extend_from_slice(&leaf);
        let middle = put_blob(&connection, &middle_body)?;
        let unreachable = put_blob(&connection, b"nobody points at me")?;

        connection.execute(
            "INSERT INTO cursorDiskKV VALUES ('composerData:s1', ?1)",
            [json!({ "conversationState": conversation_state(&[middle]) }).to_string()],
        )?;

        let sweep = sweep(&connection)?;
        assert_eq!(sweep.errors, 0);
        assert_eq!(sweep.blobs_total, 3);
        assert_eq!(sweep.blobs_live, 2, "叶子必须跟着中间节点一起活下来");
        assert_eq!(sweep.blob_orphan_rows, 1);
        assert_eq!(
            sweep.orphan_keys,
            vec![format!("{BLOB_PREFIX}{}", hex::encode(unreachable))]
        );

        delete_orphans(&connection, &sweep)?;
        let left: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key LIKE 'agentKv:blob:%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(left, 2);
        Ok(())
    }

    #[test]
    fn bubbles_keep_the_file_snapshots_they_mention() -> AppResult<()> {
        let connection = memory_db()?;
        let kept = put_content(&connection, "kept file body")?;
        let dropped = put_content(&connection, "stale file body")?;
        connection.execute(
            "INSERT INTO cursorDiskKV VALUES ('bubbleId:s1:b1', ?1)",
            [json!({
                "toolFormerData": {
                    "result": format!(
                        "{{\"beforeContentId\":\"{CONTENT_PREFIX}{}\"}}",
                        hex::encode(kept)
                    )
                }
            })
            .to_string()],
        )?;

        let sweep = sweep(&connection)?;
        assert_eq!(sweep.content_total, 2);
        assert_eq!(sweep.content_live, 1);
        assert_eq!(sweep.content_orphan_rows, 1);
        assert_eq!(
            sweep.orphan_keys,
            vec![format!("{CONTENT_PREFIX}{}", hex::encode(dropped))]
        );
        Ok(())
    }

    /// 遗留的 `agentKv:checkpoint:` 指针也算根。
    #[test]
    fn legacy_checkpoint_pointers_are_roots() -> AppResult<()> {
        let connection = memory_db()?;
        let pinned = put_blob(&connection, b"pinned by a legacy pointer")?;
        connection.execute(
            "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
            rusqlite::params![format!("{CHECKPOINT_PREFIX}s1"), STANDARD.encode(pinned)],
        )?;
        let sweep = sweep(&connection)?;
        assert_eq!(sweep.errors, 0);
        assert_eq!(sweep.blobs_live, 1);
        assert_eq!(sweep.blob_orphan_rows, 0);
        Ok(())
    }

    /// 解不开的 `conversationState` 意味着这一行的引用没算进来，必须整体放弃删除。
    #[test]
    fn an_undecodable_state_blocks_deletion() -> AppResult<()> {
        let connection = memory_db()?;
        put_blob(&connection, b"payload")?;
        connection.execute(
            "INSERT INTO cursorDiskKV VALUES ('composerData:s1', ?1)",
            [json!({ "conversationState": "~这不是 base64" }).to_string()],
        )?;
        let sweep = sweep(&connection)?;
        assert_eq!(sweep.errors, 1);
        assert_eq!(sweep.blob_orphan_rows, 1);
        assert!(delete_orphans(&connection, &sweep).is_err());
        let left: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key LIKE 'agentKv:blob:%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(left, 1, "出错时一行都不许删");
        Ok(())
    }

    #[test]
    fn an_empty_store_is_not_an_error() -> AppResult<()> {
        let connection = memory_db()?;
        let sweep = sweep(&connection)?;
        assert_eq!(sweep.blobs_total, 0);
        assert_eq!(sweep.orphan_rows(), 0);
        assert!(sweep.orphan_keys.is_empty());
        Ok(())
    }
}
