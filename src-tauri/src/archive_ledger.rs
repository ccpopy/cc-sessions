//! 归档来源账本（archive_ledger.json）：CC Sessions 自己维护的"归档会话来源"登记，
//! 记录每个已归档会话的产生原因（手动/官方/分支/同步/恢复/导入），Codex/Claude 原生均不读取。
//!
//! 并发纪律：所有写操作（record/remove/save）的调用方**必须持有 `family::FamilyLock`**
//! （与 family store 相同的纪律，见 family.rs `with_lock`）；load/origin_for 为只读，无需锁。

use std::io::Write;
use std::path::Path;

use crate::atomic_file;
use crate::error::{AppError, AppResult};
use crate::models::{ArchiveLedger, ArchiveLedgerEntry, ArchiveOrigin};
use crate::paths;

/// 读取账本。文件不存在时返回空账本（升级前没有 ledger 属正常）；
/// 空文件/非法 JSON 拒绝静默降级——避免损坏时把所有归档误判为"无记录"。
pub fn load(codex_dir: &Path) -> AppResult<ArchiveLedger> {
    let p = paths::archive_ledger_path(codex_dir);
    let metadata = match std::fs::metadata(&p) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ArchiveLedger::default())
        }
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() {
        return Err(AppError::Path(format!(
            "归档来源账本不是普通文件: {}",
            p.to_string_lossy()
        )));
    }
    let raw = std::fs::read_to_string(&p)?;
    if raw.trim().is_empty() {
        return Err(AppError::Other(format!(
            "归档来源账本内容为空，拒绝按空账本降级处理: {}",
            p.to_string_lossy()
        )));
    }
    serde_json::from_str(&raw).map_err(|error| {
        AppError::Other(format!(
            "解析归档来源账本失败 {}: {error}",
            p.to_string_lossy()
        ))
    })
}

/// 原子写账本（覆盖或新建）。调用方必须持有 FamilyLock。
pub fn save(codex_dir: &Path, ledger: &ArchiveLedger) -> AppResult<()> {
    let p = paths::archive_ledger_path(codex_dir);
    let data = serde_json::to_vec_pretty(ledger)?;
    atomic_file::overwrite_with_writer(&p, |file| {
        file.write_all(&data)?;
        Ok(())
    })
}

/// D13 来源优先级：Manual(5) > Official/Fork(4) > ProviderSync(3) > Restore/Import(2) > Unknown(0)。
/// 新 origin 优先级**高于**旧值才覆盖；Unknown 永不覆盖已有值。
fn should_record_over(existing: &ArchiveOrigin, new: &ArchiveOrigin) -> bool {
    if matches!(new, ArchiveOrigin::Unknown) {
        return false; // Unknown 只是"无法确定"的兜底，不得抹掉已有标记
    }
    new.priority() > existing.priority()
}

/// 记录（或按 D13 规则决定是否覆盖）一个归档来源。调用方必须持有 FamilyLock。
/// `archived_at` 为归档操作时刻（与官方 threads.archived_at 同语义，D15）。
pub fn record(
    codex_dir: &Path,
    session_id: &str,
    origin: ArchiveOrigin,
    archived_at: Option<i64>,
    source_path: Option<String>,
    sha256: Option<String>,
) -> AppResult<()> {
    let mut ledger = load(codex_dir)?;
    let overwrite = match ledger.entries.get(session_id) {
        Some(existing) => should_record_over(&existing.origin, &origin),
        None => true,
    };
    if !overwrite {
        return Ok(());
    }
    ledger.entries.insert(
        session_id.to_string(),
        ArchiveLedgerEntry {
            session_id: session_id.to_string(),
            origin,
            archived_at,
            source_path,
            sha256,
        },
    );
    save(codex_dir, &ledger)
}

/// 用户显式指定的归档来源（前端"来源未知"徽标下拉手动切换）：**绕过 D13 优先级**，
/// 无条件覆盖 origin 字段。与 `record` 的区别：record 只在优先级更高时覆盖；本函数是
/// 用户显式操作，直接写入目标 origin，保留已有 archived_at/source_path/sha256；
/// 无记录时新建一条（archive 相关字段为 None）。调用方必须持有 FamilyLock。
pub fn set_archive_origin(
    codex_dir: &Path,
    session_id: &str,
    origin: ArchiveOrigin,
) -> AppResult<()> {
    let mut ledger = load(codex_dir)?;
    match ledger.entries.get_mut(session_id) {
        Some(entry) => {
            entry.origin = origin;
        }
        None => {
            ledger.entries.insert(
                session_id.to_string(),
                ArchiveLedgerEntry {
                    session_id: session_id.to_string(),
                    origin,
                    archived_at: None,
                    source_path: None,
                    sha256: None,
                },
            );
        }
    }
    save(codex_dir, &ledger)
}

/// 删除一条记录（取消归档/删除会话时调用）。无记录时 no-op。调用方必须持有 FamilyLock。
pub fn remove(codex_dir: &Path, session_id: &str) -> AppResult<()> {
    let mut ledger = load(codex_dir)?;
    if ledger.entries.remove(session_id).is_none() {
        return Ok(()); // 本来就没有记录，不需要写盘
    }
    save(codex_dir, &ledger)
}

/// 只读查询单个会话的来源。账本缺失/损坏时返回 None（与"无记录"同语义，
/// 由前端降级为"未知来源"，损坏可由 backfill 修复工具重建）。
pub fn origin_for(codex_dir: &Path, session_id: &str) -> Option<ArchiveOrigin> {
    let ledger = load(codex_dir).ok()?;
    ledger
        .entries
        .get(session_id)
        .map(|entry| entry.origin.clone())
}

/// 只读返回全部记录（供 Tauri 命令 get_archive_ledger 与 backfill 使用）。
pub fn entries(codex_dir: &Path) -> AppResult<Vec<ArchiveLedgerEntry>> {
    Ok(load(codex_dir)?.entries.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_codex_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cc-session-manager-{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&dir).expect("create temp codex dir");
        dir
    }

    fn assert_ledger_empty(codex: &Path) {
        let ledger = load(codex).expect("load ledger");
        assert!(ledger.entries.is_empty());
    }

    #[test]
    fn roundtrip_record_and_remove() -> AppResult<()> {
        let codex = temp_codex_dir("ledger-roundtrip");
        assert_ledger_empty(&codex);

        record(
            &codex,
            "sess-1",
            ArchiveOrigin::Manual,
            Some(1000),
            Some("archived_sessions/rollout-1.jsonl".into()),
            Some("abc".into()),
        )?;
        let entry = load(&codex)?.entries.get("sess-1").cloned().unwrap();
        assert_eq!(entry.origin, ArchiveOrigin::Manual);
        assert_eq!(entry.archived_at, Some(1000));
        assert_eq!(entry.sha256.as_deref(), Some("abc"));

        remove(&codex, "sess-1")?;
        assert_ledger_empty(&codex);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn remove_without_record_is_noop() -> AppResult<()> {
        let codex = temp_codex_dir("ledger-remove-noop");
        remove(&codex, "never-recorded")?; // 不写盘也不报错
        assert!(!paths::archive_ledger_path(&codex).exists());
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn missing_file_loads_default() -> AppResult<()> {
        let codex = temp_codex_dir("ledger-missing-default");
        assert_ledger_empty(&codex);
        assert_eq!(origin_for(&codex, "sess-1"), None);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn corrupt_file_reports_error_with_path() -> AppResult<()> {
        let codex = temp_codex_dir("ledger-corrupt");
        fs::write(paths::archive_ledger_path(&codex), "{not-json")?;
        let error = load(&codex).expect_err("corrupt ledger must error");
        assert!(
            error.to_string().contains("archive_ledger.json"),
            "error should mention the file path: {error}"
        );
        assert_eq!(origin_for(&codex, "sess-1"), None);
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn empty_file_reports_error() -> AppResult<()> {
        let codex = temp_codex_dir("ledger-empty-file");
        fs::write(paths::archive_ledger_path(&codex), "")?;
        let error = load(&codex).expect_err("empty ledger must error");
        assert!(error.to_string().contains("为空"), "{error}");
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }

    #[test]
    fn record_overwrite_follows_priority() -> AppResult<()> {
        let codex = temp_codex_dir("ledger-priority");

        // Unknown 不覆盖已有值：先有 Manual，后 Unknown 保留 Manual
        record(&codex, "s1", ArchiveOrigin::Manual, None, None, None)?;
        record(&codex, "s1", ArchiveOrigin::Unknown, None, None, None)?;
        assert_eq!(
            origin_for(&codex, "s1"),
            Some(ArchiveOrigin::Manual),
            "Unknown must not overwrite Manual"
        );

        // Restore(2) 不覆盖 Manual(5)
        record(&codex, "s1", ArchiveOrigin::Restore, None, None, None)?;
        assert_eq!(origin_for(&codex, "s1"), Some(ArchiveOrigin::Manual));

        // Manual(5) 覆盖 ProviderSync(3)
        record(&codex, "s2", ArchiveOrigin::ProviderSync, None, None, None)?;
        record(&codex, "s2", ArchiveOrigin::Manual, None, None, None)?;
        assert_eq!(origin_for(&codex, "s2"), Some(ArchiveOrigin::Manual));

        // 同级 Fork(4) 不覆盖 Official(4)
        record(&codex, "s3", ArchiveOrigin::Official, None, None, None)?;
        record(&codex, "s3", ArchiveOrigin::Fork, None, None, None)?;
        assert_eq!(origin_for(&codex, "s3"), Some(ArchiveOrigin::Official));

        // 无记录时任何 origin 都写入（含 Unknown 兜底）
        record(&codex, "s4", ArchiveOrigin::Unknown, None, None, None)?;
        assert_eq!(origin_for(&codex, "s4"), Some(ArchiveOrigin::Unknown));
        fs::remove_dir_all(&codex).ok();
        Ok(())
    }
}
