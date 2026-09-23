//! Schema v6, observed in Codex 0.155.0-alpha.9.2 and alpha.16. Preserve native
//! item encodings; update only the target thread and recompute its byte checkpoints.
use super::*;
use rusqlite::OptionalExtension;
use rusqlite::{types::Value as SqlValue, Connection, OpenFlags};

mod observation;
pub(in crate::edit) use observation::{observe, rollout_identity};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(in crate::edit) struct ProjectionIdentity {
    pub thread_id: String,
    pub rollout_id: String,
    pub rollout_path: String,
    pub selected_rollout_path: Option<String>,
}

const TABLES: &[(&str,&str)] = &[
    ("thread_items","thread_id,turn_id,item_id,rollout_ordinal,created_at_ms,item_json,item_type,updated_at_ordinal"),
    ("thread_turns","thread_id,turn_id,rollout_ordinal,status,error_json,started_at,completed_at,duration_ms,first_user_item_id,final_agent_item_id,rollout_byte_offset,rollout_end_ordinal,rollout_end_byte_offset"),
    ("thread_realtime_items","thread_id,item_id,rollout_ordinal,created_at_ms,item_type,item_json"),
    ("thread_history_projection_state","thread_id,next_rollout_byte_offset,next_rollout_ordinal"),
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(in crate::edit) struct HistoryImage {
    pub rows: BTreeMap<String, Vec<Value>>,
    #[serde(default)]
    pub summary: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ProjectionIdentity>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(in crate::edit) struct HistoryChange {
    pub path: PathBuf,
    pub before: HistoryImage,
    pub after: HistoryImage,
}

pub(in crate::edit) fn path(rollout: &Path) -> AppResult<PathBuf> {
    Ok(safety::codex_root(rollout)
        .ok_or_else(|| unsupported("分页日志必须位于已识别的数据根 sessions/archived_sessions 中"))?
        .join("thread_history_1.sqlite"))
}
fn identity(loaded: &LoadedFile) -> AppResult<&str> {
    loaded
        .parsed
        .iter()
        .flatten()
        .find(|v| codex_outer(v) == "session_meta")
        .and_then(|v| v["payload"]["id"].as_str())
        .ok_or_else(|| unsupported("缺少 thread ID"))
}
pub(in crate::edit) fn open(path: &Path, write: bool) -> AppResult<Connection> {
    let conn = Connection::open_with_flags(
        path,
        if write {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI
        },
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(250))?;
    for (table, columns) in TABLES {
        let mut q = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let found = q
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<Result<BTreeSet<_>, _>>()?;
        let expected = columns.split(',').map(str::to_owned).collect();
        if found != expected {
            return Err(unsupported(format!(
                "历史库 {table} 不是已验证的 schema v6；本次不迁移数据库"
            )));
        }
    }
    let unknown_triggers:i64=conn.query_row("SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND name!='thread_realtime_items_projection_cleanup'",[],|r|r.get(0))?;
    if unknown_triggers > 0 {
        return Err(unsupported("历史库含未知触发器，无法确认受影响线程范围"));
    }
    let state = path.parent().unwrap().join("state_5.sqlite");
    if state.is_file() {
        let state = PathBuf::from(paths::strip_verbatim(
            &state.canonicalize()?.to_string_lossy(),
        ));
        let mut uri = url::Url::from_file_path(&state)
            .map_err(|_| unsupported("无法解析 Core 数据库的绝对路径"))?;
        uri.query_pairs_mut()
            .append_pair("mode", if write { "rw" } else { "ro" });
        conn.execute("ATTACH DATABASE ?1 AS core_state", [uri.as_str()])?;
        conn.prepare("SELECT id,title,first_user_message,preview FROM core_state.threads LIMIT 0")?;
    }
    Ok(conn)
}

#[cfg(test)]
pub(in crate::edit) fn capture(conn: &Connection, id: &str) -> AppResult<HistoryImage> {
    capture_for(conn, id, None)
}

fn capture_for(
    conn: &Connection,
    id: &str,
    identity: Option<&ProjectionIdentity>,
) -> AppResult<HistoryImage> {
    if !conn.is_autocommit() {
        return capture_rows(conn, id, identity);
    }
    // Pin all history tables in one SQLite read transaction. Attached WAL databases
    // do not share a commit clock; data_version fences also reject cross-DB races.
    for _ in 0..3 {
        let versions = data_versions(conn)?;
        let tx = conn.unchecked_transaction()?;
        let image = capture_rows(&tx, id, identity)?;
        tx.rollback()?;
        if data_versions(conn)? == versions {
            return Ok(image);
        }
    }
    Err(AppError::Other(
        "[PROJECTION_CHANGING] 数据库在读取期间持续更新，请刷新".into(),
    ))
}

fn attached(conn: &Connection) -> AppResult<bool> {
    Ok(conn
        .prepare("PRAGMA database_list")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == "core_state"))
}

fn data_versions(conn: &Connection) -> AppResult<(i64, Option<i64>)> {
    Ok((
        conn.query_row("PRAGMA main.data_version", [], |r| r.get(0))?,
        if attached(conn)? {
            Some(conn.query_row("PRAGMA core_state.data_version", [], |r| r.get(0))?)
        } else {
            None
        },
    ))
}

fn capture_rows(
    conn: &Connection,
    id: &str,
    identity: Option<&ProjectionIdentity>,
) -> AppResult<HistoryImage> {
    let mut rows = BTreeMap::new();
    for (table, columns) in TABLES {
        let names: Vec<&str> = columns.split(',').collect();
        let mut q = conn.prepare(&format!(
            "SELECT {columns} FROM {table} WHERE thread_id=?1 ORDER BY {}",
            if *table == "thread_history_projection_state" {
                "thread_id"
            } else {
                "rollout_ordinal"
            }
        ))?;
        let values = q
            .query_map([id], |r| {
                let mut value = serde_json::Map::new();
                for (i, name) in names.iter().enumerate() {
                    let v = match r.get::<_, SqlValue>(i)? {
                        SqlValue::Null => Value::Null,
                        SqlValue::Integer(n) => Value::from(n),
                        SqlValue::Real(n) => Value::from(n),
                        SqlValue::Text(s) => Value::from(s),
                        SqlValue::Blob(_) => return Err(rusqlite::Error::InvalidQuery),
                    };
                    value.insert((*name).into(), v);
                }
                Ok(Value::Object(value))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.insert((*table).into(), values);
        #[cfg(test)]
        if *table == "thread_items" {
            AFTER_ITEMS_READ.with(|hook| {
                if let Some(hook) = hook.borrow_mut().take() {
                    hook();
                }
            });
        }
    }
    let attached = attached(conn)?;
    let logical_id = identity.map_or(id, |v| v.thread_id.as_str());
    let summary = if attached {
        conn.query_row("SELECT title,first_user_message,preview FROM core_state.threads WHERE id=?1",[logical_id],|r|Ok(serde_json::json!({"title":r.get::<_,String>(0)?,"first_user_message":r.get::<_,String>(1)?,"preview":r.get::<_,String>(2)?}))).optional()?
    } else {
        None
    };
    let mut identity = identity.cloned();
    if let Some(identity) = &mut identity {
        identity.selected_rollout_path = if attached {
            conn.query_row(
                "SELECT rollout_path FROM core_state.threads WHERE id=?1",
                [logical_id],
                |r| r.get(0),
            )
            .optional()?
        } else {
            None
        };
    }
    Ok(HistoryImage {
        rows,
        summary,
        identity,
    })
}

#[cfg(test)]
thread_local! {
    pub(in crate::edit) static AFTER_ITEMS_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

pub(in crate::edit) fn read(rollout: &Path, loaded: &LoadedFile) -> AppResult<HistoryImage> {
    observe(rollout, loaded).map(|(image, _)| image)
}

pub(in crate::edit) fn prepare(
    rollout: &Path,
    before: &LoadedFile,
    after: &LoadedFile,
    seed: Option<&HistoryImage>,
) -> AppResult<HistoryChange> {
    let before_image = read(rollout, before)?;
    if identity(before)? != identity(after)? {
        return Err(unsupported("恢复数据的 thread ID 与原会话不一致"));
    }
    let mut seed = seed.cloned();
    if let Some(seed) = &mut seed {
        // Snapshots from before physical identities were recorded are valid
        // only for an ordinary rollout whose physical and logical IDs coincide.
        if seed.identity.is_none()
            && before_image.identity.as_ref().is_some_and(|id| {
                id.thread_id == id.rollout_id
                    && seed
                        .rows
                        .values()
                        .flatten()
                        .all(|row| row["thread_id"] == id.thread_id)
            })
        {
            seed.identity = before_image.identity.clone();
        }
        if seed.identity != before_image.identity {
            return Err(unsupported(
                "恢复快照来自不同 rollout 或旧身份格式，请在隔离副本核对；未覆盖当前投影",
            ));
        }
    }
    let mut after_image = project(after, seed.as_ref().unwrap_or(&before_image))?;
    // A suffix after revert/fork is not the first user message of the logical
    // thread. Never derive its Core title/preview from the suffix alone.
    let inherited = before
        .parsed
        .iter()
        .flatten()
        .any(|v| codex_outer(v) == "session_meta" && !v["payload"]["history_base"].is_null());
    if seed.is_none() && !inherited {
        let old = first_user_summary(before)?;
        let new = first_user_summary(after)?;
        if old != new {
            if let Some(summary) = &mut after_image.summary {
                // Core metadata, not Desktop project state. Preserve an explicit title
                // and a goal-derived preview, as native metadata reconciliation does.
                if summary["title"].as_str().unwrap_or("").trim() == old.0 || summary["title"] == ""
                {
                    summary["title"] = Value::String(new.0.clone());
                }
                if summary["preview"] == old.1 || summary["preview"] == "" {
                    summary["preview"] = Value::String(new.1.clone());
                }
                summary["first_user_message"] = Value::String(new.1);
            }
        }
    }
    Ok(HistoryChange {
        path: path(rollout)?,
        before: before_image,
        after: after_image,
    })
}

pub(super) fn project(loaded: &LoadedFile, seed: &HistoryImage) -> AppResult<HistoryImage> {
    let h = model(loaded)?;
    let id = seed
        .identity
        .as_ref()
        .map(|v| v.rollout_id.as_str())
        .or_else(|| {
            seed.rows["thread_history_projection_state"]
                .first()
                .and_then(|r| r["thread_id"].as_str())
        })
        .unwrap_or(identity(loaded)?);
    let mut image = seed.clone();
    let mut positions = BTreeMap::new();
    let mut offset = 0u64;
    let mut next = 0u64;
    for (i, line) in loaded.lines.iter().enumerate() {
        let v = loaded.parsed[i].as_ref().unwrap();
        let ord = v["ordinal"].as_u64().unwrap();
        let end = offset
            + line.len() as u64
            + u64::from(i + 1 < loaded.lines.len() || loaded.trailing_newline);
        positions.insert(ord, (offset, end));
        offset = end;
        next = ord + 1;
    }
    let mut rows = Vec::new();
    for item in &h.items {
        let mut row = seed.rows["thread_items"]
            .iter()
            .find(|r| r["turn_id"] == item.key.turn && r["item_id"] == item.key.id)
            .cloned()
            .ok_or_else(|| {
                unsupported(format!(
                    "item {} 缺少原生投影或恢复快照；请先完成原生历史读取",
                    item.key.id
                ))
            })?;
        let raw = &loaded.parsed[*item.records.last().unwrap()]
            .as_ref()
            .unwrap()["payload"]["item"];
        let mut native: Value = serde_json::from_str(
            row["item_json"]
                .as_str()
                .ok_or_else(|| unsupported("原生 item_json 无效"))?,
        )?;
        let original = native.clone();
        if native["id"] != item.key.id {
            return Err(unsupported("原生投影 item ID 不一致"));
        }
        let text = raw["content"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        if item.kind == "UserMessage" {
            let content = raw["content"]
                .as_array()
                .ok_or_else(|| unsupported("正式用户消息内容无效"))?;
            let projected = native["content"]
                .as_array_mut()
                .ok_or_else(|| unsupported("原生用户消息内容无效"))?;
            if content.len() != projected.len() {
                return Err(unsupported("原生用户消息内容块数量与正式消息不一致"));
            }
            for (source, target) in content.iter().zip(projected) {
                if source["type"] == "text" {
                    if target["type"] != "text" {
                        return Err(unsupported("原生用户消息内容块类型不一致"));
                    }
                    target["text"] = source["text"].clone();
                    if let Some(elements) = source.get("text_elements") {
                        target["text_elements"] = elements.clone();
                    }
                }
            }
        } else if item.kind == "AgentMessage" {
            native["text"] = Value::String(text);
            native["phase"] = raw["phase"].clone();
            if let Some(questions) = raw.get("questions") {
                native["questions"] = questions.clone();
            }
        }
        if native != original {
            row["item_json"] = Value::String(serde_json::to_string(&native)?);
        }
        row["rollout_ordinal"] =
            loaded.parsed[item.records[0]].as_ref().unwrap()["ordinal"].clone();
        row["updated_at_ordinal"] = loaded.parsed[*item.records.last().unwrap()]
            .as_ref()
            .unwrap()["ordinal"]
            .clone();
        rows.push(row);
    }
    rows.sort_by_key(|r| r["rollout_ordinal"].as_u64().unwrap_or(0));
    image.rows.insert("thread_items".into(), rows);
    let items = image.rows["thread_items"].clone();
    for turn in image.rows.get_mut("thread_turns").unwrap() {
        let tid = turn["turn_id"]
            .as_str()
            .ok_or_else(|| unsupported("投影回合 ID 无效"))?
            .to_owned();
        let lifecycle: Vec<&Value> = loaded
            .parsed
            .iter()
            .flatten()
            .filter(|v| {
                v["payload"]["turn_id"] == tid
                    && matches!(
                        codex_ptype(v),
                        "task_started" | "task_complete" | "turn_aborted"
                    )
            })
            .collect();
        let first = lifecycle
            .first()
            .ok_or_else(|| unsupported(format!("回合 {tid} 缺少生命周期记录")))?;
        let last = lifecycle.last().unwrap();
        turn["rollout_ordinal"] = first["ordinal"].clone();
        turn["rollout_byte_offset"] = Value::from(positions[&first["ordinal"].as_u64().unwrap()].0);
        if matches!(codex_ptype(last), "task_complete" | "turn_aborted") {
            turn["rollout_end_ordinal"] = last["ordinal"].clone();
            turn["rollout_end_byte_offset"] =
                Value::from(positions[&last["ordinal"].as_u64().unwrap()].1);
        }
        turn["first_user_item_id"] = items
            .iter()
            .find(|r| r["turn_id"] == tid && r["item_type"] == "userMessage")
            .map(|r| r["item_id"].clone())
            .unwrap_or(Value::Null);
        turn["final_agent_item_id"] = final_agent(&h, loaded, &tid, &BTreeMap::new(), true)
            .filter(|item| {
                turn["status"] != "inProgress"
                    || loaded.parsed[*item.records.last().unwrap()]
                        .as_ref()
                        .unwrap()["payload"]["item"]["phase"]
                        == "final_answer"
            })
            .map(|item| Value::String(item.key.id.clone()))
            .unwrap_or(Value::Null);
    }
    image.rows.insert("thread_history_projection_state".into(),vec![serde_json::json!({"thread_id":id,"next_rollout_byte_offset":offset,"next_rollout_ordinal":next})]);
    Ok(image)
}

pub(in crate::edit) fn replace(conn: &Connection, id: &str, image: &HistoryImage) -> AppResult<()> {
    let logical_id = id;
    if image
        .identity
        .as_ref()
        .is_some_and(|identity| identity.thread_id != logical_id)
    {
        return Err(unsupported("投影快照与逻辑会话身份不一致"));
    }
    let id = image
        .identity
        .as_ref()
        .map_or(id, |v| v.rollout_id.as_str());
    for (table, _) in TABLES {
        conn.execute(&format!("DELETE FROM {table} WHERE thread_id=?1"), [id])?;
    }
    for (table, columns) in TABLES {
        let names: Vec<&str> = columns.split(',').collect();
        let placeholders = vec!["?"; names.len()].join(",");
        for row in &image.rows[*table] {
            if row["thread_id"] != id {
                return Err(unsupported("投影快照包含其他线程"));
            }
            let values: Vec<SqlValue> = names
                .iter()
                .map(|n| match &row[*n] {
                    Value::Null => SqlValue::Null,
                    Value::String(s) => SqlValue::Text(s.clone()),
                    Value::Number(n) => SqlValue::Integer(n.as_i64().unwrap_or_default()),
                    _ => SqlValue::Null,
                })
                .collect();
            conn.execute(
                &format!("INSERT INTO {table} ({columns}) VALUES ({placeholders})"),
                rusqlite::params_from_iter(values),
            )?;
        }
    }
    if let Some(summary) = &image.summary {
        if conn.execute(
            "UPDATE core_state.threads SET title=?2,first_user_message=?3,preview=?4 WHERE id=?1",
            rusqlite::params![
                logical_id,
                summary["title"].as_str(),
                summary["first_user_message"].as_str(),
                summary["preview"].as_str()
            ],
        )? != 1
        {
            return Err(unsupported("目标线程元数据已不存在"));
        }
    }
    Ok(())
}

impl HistoryChange {
    pub(in crate::edit) fn capture_current(
        &self,
        conn: &Connection,
        id: &str,
    ) -> AppResult<HistoryImage> {
        if self.before.identity != self.after.identity
            || self
                .before
                .identity
                .as_ref()
                .is_some_and(|v| v.thread_id != id)
        {
            return Err(unsupported("操作清单的投影身份不一致"));
        }
        capture_for(
            conn,
            self.before
                .identity
                .as_ref()
                .map_or(id, |v| v.rollout_id.as_str()),
            self.before.identity.as_ref(),
        )
    }
    pub(in crate::edit) fn can_reconcile(&self, current: &HistoryImage) -> bool {
        // Attached SQLite databases in WAL mode may commit separately on a crash.
        // Only finish components equal to the saved before/after images.
        (current.rows == self.before.rows || current.rows == self.after.rows)
            && (current.summary == self.before.summary || current.summary == self.after.summary)
            && current.identity == self.before.identity
    }
    pub(in crate::edit) fn begin(&self, id: &str) -> AppResult<Connection> {
        let conn = open(&self.path, true)?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        if self.capture_current(&conn, id)? != self.before {
            return Err(AppError::Other(
                "[EDIT_CONFLICT] 原生历史在预览后变化，未执行修改".into(),
            ));
        }
        Ok(conn)
    }
}

fn first_user_summary(loaded: &LoadedFile) -> AppResult<(String, String)> {
    let h = model(loaded)?;
    let mut title = String::new();
    let mut preview = String::new();
    for item in h.items.iter().filter(|it| it.kind == "UserMessage") {
        let raw = &loaded.parsed[*item.records.last().unwrap()]
            .as_ref()
            .unwrap()["payload"]["item"];
        let content = raw["content"]
            .as_array()
            .ok_or_else(|| unsupported("正式用户消息内容无效"))?;
        let text = content
            .iter()
            .filter_map(|c| c["text"].as_str())
            .collect::<String>();
        let text = text
            .split_once("## My request for Codex:")
            .map_or(text.as_str(), |(_, request)| request)
            .trim();
        if title.is_empty() {
            title = text.into();
        }
        if preview.is_empty() {
            preview = if !text.is_empty() {
                text.into()
            } else if content.iter().any(|c| {
                matches!(
                    c["type"].as_str(),
                    Some("image" | "local_image" | "file_id")
                )
            }) {
                "[Image]".into()
            } else if content
                .iter()
                .any(|c| matches!(c["type"].as_str(), Some("audio" | "local_audio")))
            {
                "[Audio]".into()
            } else {
                String::new()
            };
        }
        if !title.is_empty() && !preview.is_empty() {
            break;
        }
    }
    Ok((title, preview))
}
