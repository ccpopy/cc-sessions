//! CC Sessions 自己维护的会话转换来源登记。
//!
//! 来源信息刻意不写入 Codex/Claude 的会话 JSONL、history、session_meta 或工具块，
//! 避免改变 CLI resume、tool_use/tool_result 和官方客户端对会话格式的解释。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::atomic_file;
use crate::error::{AppError, AppResult};
use crate::models::{SessionConversionOrigin, SessionSummary};
use crate::paths;

const STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProvenanceStore {
    #[serde(default = "store_version")]
    version: u32,
    #[serde(default)]
    sessions: BTreeMap<String, SessionConversionOrigin>,
}

impl Default for ProvenanceStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            sessions: BTreeMap::new(),
        }
    }
}

fn store_version() -> u32 {
    STORE_VERSION
}

fn session_key(provider: &str, session_id: &str) -> String {
    format!("{provider}:{session_id}")
}

fn load(codex_dir: &Path) -> AppResult<ProvenanceStore> {
    let path = paths::session_provenance_path(codex_dir);
    if !path.is_file() {
        return Ok(ProvenanceStore::default());
    }
    let store: ProvenanceStore = serde_json::from_slice(&fs::read(&path)?)?;
    if store.version != STORE_VERSION {
        return Err(AppError::Other(format!(
            "不支持的会话来源登记版本 {}: {}",
            store.version,
            path.to_string_lossy()
        )));
    }
    Ok(store)
}

fn save(codex_dir: &Path, store: &ProvenanceStore) -> AppResult<()> {
    fs::create_dir_all(codex_dir)?;
    let path = paths::session_provenance_path(codex_dir);
    let data = serde_json::to_vec_pretty(store)?;
    let writer = |file: &mut fs::File| -> AppResult<()> {
        file.write_all(&data)?;
        file.write_all(b"\n")?;
        Ok(())
    };
    if path.is_file() {
        let expected = atomic_file::fingerprint(&path)?;
        atomic_file::replace_with_writer_if_unchanged(&path, &expected, writer)
    } else {
        atomic_file::create_with_writer_if_absent(&path, writer)
    }
}

pub fn record_conversion(
    codex_dir: &Path,
    target_provider: &str,
    target_id: &str,
    source_provider: &str,
    source_id: &str,
    conversion_mode: Option<&str>,
) -> AppResult<()> {
    if codex_dir.as_os_str().is_empty()
        || !matches!(target_provider, "codex" | "claude")
        || !matches!(source_provider, "codex" | "claude")
        || target_id.trim().is_empty()
        || source_id.trim().is_empty()
    {
        return Err(AppError::Other("会话来源登记参数无效".into()));
    }

    let mut store = load(codex_dir)?;
    store.sessions.insert(
        session_key(target_provider, target_id),
        SessionConversionOrigin {
            source_provider: source_provider.to_string(),
            source_id: source_id.to_string(),
            conversion_mode: conversion_mode.map(String::from),
            converted_at: chrono::Utc::now().to_rfc3339(),
        },
    );
    save(codex_dir, &store)
}

/// 来源登记属于可选展示信息；文件缺失或损坏不能阻断主会话列表。
pub fn annotate_sessions(codex_dir: &Path, sessions: &mut [SessionSummary]) {
    let Ok(store) = load(codex_dir) else {
        return;
    };
    for session in sessions {
        session.conversion_origin = store
            .sessions
            .get(&session_key(&session.provider, &session.id))
            .cloned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cc-sessions-provenance-{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn summary(provider: &str, id: &str) -> SessionSummary {
        SessionSummary {
            provider: provider.into(),
            id: id.into(),
            rollout_path: String::new(),
            cwd: String::new(),
            cwd_display: String::new(),
            title: String::new(),
            first_user_message: String::new(),
            model: None,
            reasoning_effort: None,
            source: None,
            agent_nickname: None,
            agent_role: None,
            conversion_origin: None,
            tokens_used: 0,
            created_at: 0,
            updated_at: 0,
            archived: false,
            git_branch: None,
            rollout_bytes: 0,
            logs_count: 0,
            has_backup: false,
            resume_command: String::new(),
        }
    }

    #[test]
    fn records_and_annotates_conversion_without_touching_session_files() -> AppResult<()> {
        let codex = temp_dir("roundtrip");
        record_conversion(
            &codex,
            "claude",
            "target-session",
            "codex",
            "source-session",
            Some("native"),
        )?;

        let mut sessions = vec![
            summary("claude", "target-session"),
            summary("codex", "target-session"),
        ];
        annotate_sessions(&codex, &mut sessions);

        let origin = sessions[0].conversion_origin.as_ref().expect("origin");
        assert_eq!(origin.source_provider, "codex");
        assert_eq!(origin.source_id, "source-session");
        assert_eq!(origin.conversion_mode.as_deref(), Some("native"));
        assert!(sessions[1].conversion_origin.is_none());
        assert!(paths::session_provenance_path(&codex).is_file());

        fs::remove_dir_all(codex).ok();
        Ok(())
    }
}
