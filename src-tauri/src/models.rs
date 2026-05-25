use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub codex_dir: String,
    #[serde(default = "default_claude_dir")]
    pub claude_dir: String,
    #[serde(default = "default_gemini_dir")]
    pub gemini_dir: String,
    pub backup_dir: String,
    #[serde(default = "default_open_cmd")]
    pub open_command: String,
    #[serde(default = "default_refresh_ms")]
    pub refresh_interval_ms: u64,
}

fn default_open_cmd() -> String {
    "auto".into()
}

fn default_claude_dir() -> String {
    crate::paths::default_claude_dir()
        .to_string_lossy()
        .into_owned()
}

fn default_gemini_dir() -> String {
    crate::paths::default_gemini_dir()
        .to_string_lossy()
        .into_owned()
}

fn default_refresh_ms() -> u64 {
    5000
}

impl Default for Settings {
    fn default() -> Self {
        let codex = crate::paths::default_codex_dir();
        let claude = crate::paths::default_claude_dir();
        let gemini = crate::paths::default_gemini_dir();
        let backup = crate::paths::default_backup_dir();
        Self {
            codex_dir: codex.to_string_lossy().into_owned(),
            claude_dir: claude.to_string_lossy().into_owned(),
            gemini_dir: gemini.to_string_lossy().into_owned(),
            backup_dir: backup.to_string_lossy().into_owned(),
            open_command: "auto".into(),
            refresh_interval_ms: 5000,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DirValidation {
    pub valid: bool,
    pub has_state_db: bool,
    pub has_sessions: bool,
    pub threads_count: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub provider: String,
    pub id: String,
    pub rollout_path: String,
    pub cwd: String,
    pub cwd_display: String,
    pub title: String,
    pub first_user_message: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub source: Option<String>,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub tokens_used: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub archived: bool,
    pub git_branch: Option<String>,
    pub rollout_bytes: u64,
    pub logs_count: i64,
    pub has_backup: bool,
    pub resume_command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectGroup {
    pub cwd: String,
    pub cwd_display: String,
    pub sessions: Vec<SessionSummary>,
    pub latest_updated_at: i64,
    pub total_tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeleteResult {
    pub id: String,
    pub threads_rows_deleted: u32,
    pub logs_rows_deleted: u32,
    pub history_rows_deleted: u32,
    pub rollout_deleted: bool,
    pub rollout_missing: bool,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreviewEvent {
    pub index: usize,
    pub timestamp: String,
    pub role: String,
    pub kind: String,
    pub text_summary: String,
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionMetaBrief {
    pub id: Option<String>,
    pub timestamp: Option<String>,
    pub cwd: Option<String>,
    pub originator: Option<String>,
    pub cli_version: Option<String>,
    pub source: Option<String>,
    pub model_provider: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackupSummary {
    pub path: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub created_at: String,
    pub sessions_count: u32,
    pub total_bytes: u64,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub created_at: String,
    pub app_version: String,
    pub codex_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_dir: Option<String>,
    pub note: Option<String>,
    pub sessions: Vec<ManifestSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestSession {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub id: String,
    pub rollout_relpath: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_relpath: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar_relpath: Option<String>,
    pub title: String,
    pub cwd: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub tokens_used: i64,
    pub model: Option<String>,
    pub bytes_rollout: u64,
    pub logs_count: u32,
    #[serde(default)]
    pub history_rows: u32,
    pub sha256_rollout: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackupDetail {
    pub summary: BackupSummary,
    pub manifest: Manifest,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreResult {
    pub id: String,
    pub ok: bool,
    pub threads_inserted: bool,
    pub logs_inserted: u32,
    pub history_appended: u32,
    pub rollout_copied: bool,
    pub conflict: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyItem {
    pub id: String,
    pub ok: bool,
    pub expected_sha: String,
    pub actual_sha: Option<String>,
    pub missing: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub items: Vec<VerifyItem>,
    pub all_ok: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Kpi {
    pub sessions_total: u32,
    pub tokens_total: i64,
    pub active_projects: u32,
    pub avg_tokens_per_session: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimeseriesPoint {
    pub bucket_start: i64,
    pub sessions: u32,
    pub tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectStat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub cwd: String,
    pub cwd_display: String,
    pub sessions: u32,
    pub tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelStat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub sessions: u32,
    pub tokens: i64,
}

// ========================= 修复 / 诊断 =========================

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticReport {
    pub rollout_count: u32,
    pub archived_rollout_count: u32,
    pub index_count: u32,
    pub threads_count: u32,
    pub rollout_ids: Vec<String>,
    pub index_ids: Vec<String>,
    pub threads_ids: Vec<String>,
    /// 有 rollout 但不在 index 里
    pub missing_in_index: Vec<String>,
    /// 有 rollout 但不在 threads 里（Codex app 左侧看不到）
    pub missing_in_threads: Vec<String>,
    /// 在 index 但 rollout 已没了（孤儿 index 行）
    pub orphan_in_index: Vec<String>,
    /// 在 threads 但 rollout 已没了
    pub orphan_in_threads: Vec<String>,
    /// 当前 `config.toml` 读出的 model_provider
    pub current_provider: Option<String>,
    /// 每个 family 的 active 节点对应 provider 不是 current_provider
    pub provider_mismatched_families: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexRepairReport {
    pub scanned: u32,
    pub written: u32,
    pub salvaged: u32,
    pub dry_run: bool,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadsRebuildReport {
    pub scanned: u32,
    pub upserted: u32,
    pub skipped: u32,
    pub dry_run: bool,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncBranchReport {
    pub active_id: String,
    pub source_id: String,
    pub appended_lines: u32,
    pub total_lines: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct BranchSyncReport {
    pub source_id: String,
    pub target_id: String,
    pub appended_lines: u32,
    pub total_lines: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct BranchSyncState {
    pub branch_id: String,
    /// current / same / branch_ahead / active_ahead / diverged / missing
    pub relation: String,
    pub active_lines: Option<u64>,
    pub branch_lines: Option<u64>,
    pub appendable_lines_to_active: u32,
    pub appendable_lines_to_branch: u32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloneReport {
    pub source_id: String,
    pub new_id: Option<String>,
    pub new_rollout_path: Option<String>,
    pub new_provider: String,
    pub ok: bool,
    pub skipped_reason: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForkSessionReport {
    pub source_id: String,
    pub new_id: String,
    pub new_rollout_path: String,
    pub event_index: usize,
    pub included_lines: u64,
    pub cut_role: String,
    pub cut_kind: String,
    pub cut_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchStrategy {
    /// 活跃分支 + 归档旧节点（推荐）
    Continuous,
    /// 每个 provider 下独立副本，互不干扰
    Scatter,
    /// 直接改 rollout 的 provider 字段，不克隆
    Follow,
}

// ========================= 家族树 =========================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchStatus {
    Active,
    Archived,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FamilyBranch {
    pub id: String,
    pub provider: String,
    pub created_at: String,
    pub status: BranchStatus,
    pub rollout_relpath: String,
    /// 归档时固化的 rollout 校验（读取时比对；None 表示未固化）
    pub sha256: Option<String>,
    pub line_count: Option<u64>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Family {
    pub family_id: String,
    pub root_id: String,
    pub title: String,
    pub chain: Vec<FamilyBranch>,
    pub active_id: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FamilyStore {
    pub version: u32,
    pub families: std::collections::BTreeMap<String, Family>,
    /// session_id → family_id（反向索引，持久化便于前端快速查）
    pub index: std::collections::BTreeMap<String, String>,
}

impl Default for FamilyStore {
    fn default() -> Self {
        Self {
            version: 1,
            families: Default::default(),
            index: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FamilyIntegrityItem {
    pub family_id: String,
    pub branch_id: String,
    pub ok: bool,
    pub expected_sha: Option<String>,
    pub actual_sha: Option<String>,
    pub expected_lines: Option<u64>,
    pub actual_lines: Option<u64>,
    pub missing: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FamilyIntegrityReport {
    pub items: Vec<FamilyIntegrityItem>,
    pub all_ok: bool,
}

// ========================= Bundle 导出 / 导入 =========================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleManifest {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub session_id: String,
    pub rollout_relpath: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_relpath: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar_relpath: Option<String>,
    pub exported_at: String,
    pub updated_at: i64,
    pub thread_name: String,
    pub session_cwd: String,
    pub session_source: Option<String>,
    pub session_originator: Option<String>,
    pub model_provider: Option<String>,
    pub export_machine: String,
    pub export_group: String,
    pub sha256_rollout: String,
    pub rollout_line_count: u64,
    pub has_history: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportReport {
    pub session_id: String,
    pub ok: bool,
    pub bundle_path: Option<String>,
    pub error: Option<String>,
    pub skipped_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportMode {
    /// 本地若存在同 id 且 mtime 更新则保留本地
    KeepLocal,
    /// 本地若存在同 id 则覆盖
    Overwrite,
    /// 本地若存在同 id 则跳过（默认安全模式）
    Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectPathMapping {
    pub source_cwd: String,
    pub target_cwd: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportReport {
    pub session_id: String,
    pub ok: bool,
    pub rollout_written: bool,
    pub history_appended: u32,
    pub threads_upserted: bool,
    pub index_appended: bool,
    pub skipped_reason: Option<String>,
    pub error: Option<String>,
    pub verified: bool,
    /// true 表示本次导入时发现文件 sha256 与 manifest 不一致
    pub sha_mismatch: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ZipReport {
    pub path: String,
    pub files: u32,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BundleListItem {
    pub bundle_dir: String,
    pub manifest: BundleManifest,
    pub verified: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    /// 生效的 provider（未显式配置时回退到默认值 `openai`）
    pub current: Option<String>,
    /// 是否来自 config.toml 的显式配置（false 表示落在默认值）
    pub is_explicit: bool,
    pub config_path: String,
    pub exists: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectConfigIssue {
    pub project_cwd: String,
    pub config_path: String,
    pub session_count: u32,
    pub session_ids: Vec<String>,
    pub current_min_wait_timeout_ms: Option<u64>,
    pub current_default_wait_timeout_ms: Option<u64>,
    pub current_max_wait_timeout_ms: Option<u64>,
    pub suggested_default_wait_timeout_ms: Option<u64>,
    pub repairable: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectConfigReport {
    pub scanned_projects: u32,
    pub config_files: u32,
    pub issue_count: u32,
    pub repairable_count: u32,
    pub issues: Vec<ProjectConfigIssue>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectConfigRepairItem {
    pub project_cwd: String,
    pub config_path: String,
    pub changed: bool,
    pub dry_run: bool,
    pub old_default_wait_timeout_ms: Option<u64>,
    pub new_default_wait_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectConfigRepairReport {
    pub scanned_projects: u32,
    pub config_files: u32,
    pub issue_count: u32,
    pub repaired_count: u32,
    pub dry_run: bool,
    pub items: Vec<ProjectConfigRepairItem>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrphanPruneReport {
    pub index_removed: u32,
    pub threads_removed: u32,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryOrphanReport {
    pub provider: String,
    pub history_path: String,
    pub session_count: u32,
    pub history_rows: u32,
    pub linked_rows: u32,
    pub orphan_rows: u32,
    /// JSON 无效或没有可识别会话 id 的行不会被自动清理。
    pub untracked_rows: u32,
    pub orphan_session_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryPruneReport {
    pub provider: String,
    pub history_path: String,
    pub removed_rows: u32,
    pub dry_run: bool,
    pub orphan_session_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FamilyOverlay {
    pub session_id: String,
    /// threads 表中记录的 model_provider
    pub provider: Option<String>,
    pub family_id: Option<String>,
    pub branch_count: u32,
    pub is_active_branch: bool,
    /// "matches" / "resync" / "clonable" / "has_clone" / "unknown"
    pub clone_state: String,
}
