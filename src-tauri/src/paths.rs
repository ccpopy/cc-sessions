use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

/// 剥离 Windows 长路径前缀 `\\?\` 以及 UNC 变体 `\\?\UNC\`。
/// 实测 `threads.cwd` 中大量此类前缀需要清理。
pub fn strip_verbatim(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{}", rest);
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    s.to_string()
}

pub fn basename_display(s: &str) -> String {
    let stripped = strip_verbatim(s);
    let p = Path::new(&stripped);
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| stripped.clone())
}

pub fn is_wsl_unc_path(path: &Path) -> bool {
    wsl_unc_mapping(path).is_some()
}

/// Map Linux absolute paths stored by Codex inside WSL back to the selected
/// Windows-accessible WSL UNC root. Non-WSL and non-Linux paths are unchanged.
pub fn host_path_from_codex_record(codex_dir: &Path, raw: &str) -> PathBuf {
    let cleaned = strip_verbatim(raw.trim());
    if cleaned.starts_with('/') {
        if let Some(mapping) = wsl_unc_mapping(codex_dir) {
            return mapping.host_path_for_linux_path(&cleaned);
        }
    }
    PathBuf::from(cleaned)
}

pub fn host_path_string_from_codex_record(codex_dir: &Path, raw: &str) -> String {
    host_path_from_codex_record(codex_dir, raw)
        .to_string_lossy()
        .into_owned()
}

/// Convert a host-visible project path back to the path format stored by Codex.
///
/// A Codex directory selected through WSL is exposed to the Windows desktop app as a UNC path,
/// while the Codex process inside WSL expects Linux paths in `session_meta.cwd` and `threads.cwd`.
/// Refuse paths outside the selected distro instead of persisting an unusable Windows path.
pub fn codex_record_path_from_host(codex_dir: &Path, host_path: &Path) -> AppResult<String> {
    let cleaned = strip_verbatim(&host_path.to_string_lossy());
    if let Some(mapping) = wsl_unc_mapping(codex_dir) {
        return mapping.linux_path_for_host_path(&cleaned).ok_or_else(|| {
            AppError::Path(format!(
                "WSL Codex 的项目目录必须位于当前发行版 {} 中: {}",
                mapping.unc_root, cleaned
            ))
        });
    }
    Ok(cleaned)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WslUncMapping {
    unc_root: String,
    distro: String,
}

impl WslUncMapping {
    fn host_path_for_linux_path(&self, linux_path: &str) -> PathBuf {
        let mut out = self.unc_root.clone();
        for segment in linux_path.trim_start_matches('/').split('/') {
            if !segment.is_empty() {
                out.push('\\');
                out.push_str(segment);
            }
        }
        PathBuf::from(out)
    }

    fn linux_path_for_host_path(&self, host_path: &str) -> Option<String> {
        let (_, distro, segments) = parse_wsl_unc(host_path)?;
        if !distro.eq_ignore_ascii_case(&self.distro) {
            return None;
        }
        if segments.is_empty() {
            return Some("/".to_string());
        }
        Some(format!("/{}", segments.join("/")))
    }
}

fn parse_wsl_unc(raw: &str) -> Option<(String, String, Vec<String>)> {
    let normalized = strip_verbatim(raw).replace('/', "\\");
    let rest = normalized.strip_prefix(r"\\")?;
    let mut parts = rest.split('\\').filter(|part| !part.is_empty());
    let server = parts.next()?.to_string();
    if !server.eq_ignore_ascii_case("wsl.localhost") && !server.eq_ignore_ascii_case("wsl$") {
        return None;
    }
    let distro = parts.next()?.to_string();
    let segments = parts.map(str::to_string).collect::<Vec<_>>();
    if segments
        .iter()
        .any(|segment| segment == "." || segment == "..")
    {
        return None;
    }
    Some((server, distro, segments))
}

fn wsl_unc_mapping(path: &Path) -> Option<WslUncMapping> {
    let (server, distro, _) = parse_wsl_unc(&path.to_string_lossy())?;
    Some(WslUncMapping {
        unc_root: format!(r"\\{}\{}", server, distro),
        distro,
    })
}

pub fn default_codex_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".codex"))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

pub fn default_claude_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".claude"))
        .unwrap_or_else(|| PathBuf::from(".claude"))
}

pub fn default_opencode_dir() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".local").join("share").join("opencode"))
        .unwrap_or_else(|| PathBuf::from(".local/share/opencode"))
}

/// Cursor IDE 的用户数据目录，即 VS Code 系的 `<config>/Cursor/User`。
///
/// `dirs::config_dir()` 在三个平台上恰好给出 Cursor 使用的父目录：
/// macOS `~/Library/Application Support`、Windows `%APPDATA%`、Linux `~/.config`。
/// 会话主体存放在其下的 `globalStorage/state.vscdb`。
pub fn default_cursor_dir() -> PathBuf {
    dirs::config_dir()
        .map(|config| config.join("Cursor").join("User"))
        .unwrap_or_else(|| PathBuf::from("Cursor/User"))
}

/// cursor-agent CLI 的家目录 `~/.cursor`，与 IDE 数据目录相互独立。
///
/// 只有 `chats/` 子目录里的会话对本工具有意义；`projects/*/agent-transcripts/`
/// 是同一批会话的有损展示副本（无工具结果、无时间戳），刻意不读。
pub fn default_cursor_agent_dir() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".cursor"))
        .unwrap_or_else(|| PathBuf::from(".cursor"))
}

/// Cursor IDE 的会话数据库。
pub fn cursor_state_db_path(cursor_dir: &Path) -> PathBuf {
    cursor_dir.join("globalStorage").join("state.vscdb")
}

/// VS Code 系的工作区元数据目录，用于把 `workspaceId` 回查成项目路径。
pub fn cursor_workspace_storage_dir(cursor_dir: &Path) -> PathBuf {
    cursor_dir.join("workspaceStorage")
}

/// cursor-agent 的会话根目录，布局为 `chats/<md5(cwd)>/<agentId>/`。
pub fn cursor_agent_chats_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("chats")
}

pub fn default_backup_dir() -> PathBuf {
    let cc_root = default_codex_dir();
    cc_root
        .parent()
        .map(|p| p.join("cc-backups"))
        .unwrap_or_else(|| PathBuf::from("cc-backups"))
}

pub fn validate_codex_dir(path: &Path) -> AppResult<(bool, bool, bool)> {
    let exists = path.is_dir();
    let has_state = state_db_path(path)?.is_file();
    let has_sessions = path.join("sessions").is_dir();
    Ok((exists, has_state, has_sessions))
}

pub fn validate_claude_dir(path: &Path) -> (bool, bool) {
    let exists = path.is_dir();
    let has_projects = path.join("projects").is_dir();
    (exists, has_projects)
}

pub fn claude_projects_dir(claude: &Path) -> PathBuf {
    claude.join("projects")
}

/// 所有与 Codex 目录相关的关键子路径集中在此，方便其他模块引用。
pub fn sessions_dir(codex: &Path) -> PathBuf {
    codex.join("sessions")
}

pub fn archived_sessions_dir(codex: &Path) -> PathBuf {
    codex.join("archived_sessions")
}

pub fn session_index_path(codex: &Path) -> PathBuf {
    codex.join("session_index.jsonl")
}

pub fn history_path(codex: &Path) -> PathBuf {
    codex.join("history.jsonl")
}

/// SQLite files may live separately from CODEX_HOME. Never change the root used
/// for config, sessions, indexes, or manager metadata to locate a database.
pub fn state_db_path(codex: &Path) -> AppResult<PathBuf> {
    Ok(codex_sqlite_dir(codex)?.join("state_5.sqlite"))
}

pub fn thread_history_db_path(codex: &Path) -> AppResult<PathBuf> {
    Ok(codex_sqlite_dir(codex)?.join("thread_history_1.sqlite"))
}

pub fn logs_db_path(codex: &Path) -> AppResult<PathBuf> {
    let config = read_codex_path_config(codex)?;
    let dir = match configured_path(codex, &config, "log_dir")? {
        Some(dir) => dir,
        None => codex_sqlite_dir(codex)?,
    };
    Ok(dir.join("logs_2.sqlite"))
}

/// Resolve the same SQLite directory for reads, writes, and native app-server.
/// Explicit settings are authoritative even if the target database is missing.
/// For app-managed WSL homes, recognize sqlite/ only when it is unambiguous.
pub fn codex_sqlite_dir(codex: &Path) -> AppResult<PathBuf> {
    let env = std::env::var("CODEX_SQLITE_HOME").ok();
    resolve_codex_sqlite_dir(codex, env.as_deref())
}

fn resolve_codex_sqlite_dir(codex: &Path, env: Option<&str>) -> AppResult<PathBuf> {
    let config = read_codex_path_config(codex)?;
    if let Some(dir) = configured_path(codex, &config, "sqlite_home")? {
        return Ok(dir);
    }
    if let Some(value) = env.map(str::trim).filter(|value| !value.is_empty()) {
        return absolute_codex_path(codex, value, &std::env::current_dir()?);
    }
    let root = if codex.is_absolute() || looks_like_wsl_unc_path(codex) {
        normalize_database_path(codex)?
    } else {
        normalize_database_path(&std::env::current_dir()?.join(codex))?
    };
    let nested = root.join("sqlite");
    let root_db = root.join("state_5.sqlite");
    let nested_db = nested.join("state_5.sqlite");
    match (root_db.try_exists()?, nested_db.try_exists()?) {
        (true, true) => {
            Err(AppError::InvalidCodexDir(format!(
                "同时找到 {} 和 {}，无法确定当前 Codex 使用的数据库。请核实后在 config.toml 中设置 sqlite_home；不要移动数据库或把 --codex-dir 指向 sqlite 子目录。",
                root_db.display(), nested_db.display()
            )))
        }
        (false, true) => Ok(nested),
        _ => Ok(root),
    }
}

fn read_codex_path_config(codex: &Path) -> AppResult<toml::Table> {
    let path = config_toml_path(codex);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(toml::Table::new()),
        Err(error) => return Err(error.into()),
    };
    toml::from_str::<toml::Table>(&content).map_err(|error| {
        AppError::InvalidCodexDir(format!(
            "无法解析 {}，未选择或修改数据库: {}",
            path.display(),
            error.message()
        ))
    })
}

fn configured_path(codex: &Path, config: &toml::Table, key: &str) -> AppResult<Option<PathBuf>> {
    let Some(value) = config.get(key) else {
        return Ok(None);
    };
    let raw = value
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::InvalidCodexDir(format!("config.toml 的 {key} 必须是非空路径字符串"))
        })?;
    absolute_codex_path(codex, raw, codex).map(Some)
}

fn absolute_codex_path(codex: &Path, raw: &str, base: &Path) -> AppResult<PathBuf> {
    let expanded =
        if raw == "~" || raw.starts_with("~/") || (cfg!(windows) && raw.starts_with("~\\")) {
            // A WSL home selected through UNC belongs to a different OS user. Do not
            // expand its tilde against the Windows user's home directory.
            if is_wsl_unc_path(codex) {
                return Err(AppError::InvalidCodexDir(
                    "WSL 数据目录的路径配置请使用绝对 Linux 路径，不能在 Windows 中展开 ~".into(),
                ));
            }
            let home = dirs::home_dir().ok_or_else(|| AppError::Path("无法确定用户目录".into()))?;
            home.join(raw.get(2..).unwrap_or(""))
        } else if looks_like_wsl_unc_path(Path::new(raw)) {
            PathBuf::from(raw)
        } else {
            match wsl_unc_mapping(codex).filter(|_| raw.starts_with('/')) {
                Some(mapping) => mapping.host_path_for_linux_path(raw),
                None => PathBuf::from(raw),
            }
        };
    if expanded.is_absolute() || looks_like_wsl_unc_path(&expanded) {
        return normalize_database_path(&expanded);
    }
    let base = if base.is_absolute() || is_wsl_unc_path(base) {
        base.to_path_buf()
    } else {
        std::env::current_dir()?.join(base)
    };
    normalize_database_path(&base.join(expanded))
}

fn looks_like_wsl_unc_path(path: &Path) -> bool {
    let raw = strip_verbatim(&path.to_string_lossy())
        .replace('/', "\\")
        .to_ascii_lowercase();
    raw.starts_with(r"\\wsl.localhost\") || raw.starts_with(r"\\wsl$\")
}

fn normalize_database_path(path: &Path) -> AppResult<PathBuf> {
    // Keep UNC classification reliable even on non-Windows hosts used by tests.
    let normalized = strip_verbatim(&path.to_string_lossy()).replace('/', "\\");
    if let Some(rest) = normalized.strip_prefix(r"\\") {
        let mut parts = rest.split('\\').filter(|part| !part.is_empty());
        if let (Some(server), Some(distro)) = (parts.next(), parts.next()) {
            if server.eq_ignore_ascii_case("wsl.localhost") || server.eq_ignore_ascii_case("wsl$") {
                let mut segments = Vec::new();
                for part in parts {
                    match part {
                        "." => {}
                        ".." => {
                            segments.pop();
                        }
                        value => segments.push(value),
                    }
                }
                let mut out = format!(r"\\{}\{}", server, distro);
                for part in segments {
                    out.push('\\');
                    out.push_str(part);
                }
                return Ok(PathBuf::from(out));
            }
        }
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            component => out.push(component.as_os_str()),
        }
    }
    Ok(out)
}

pub fn config_toml_path(codex: &Path) -> PathBuf {
    codex.join("config.toml")
}

/// Codex App 的 Electron 全局状态文件：维护当前的本地项目定义、会话项目归属和
/// project-order；写回时保留应用拥有的其他未知字段。只更新 rollout/SQLite cwd 时，
/// 官方 App 的左侧项目列表不会把会话移动到目标项目。
pub fn codex_global_state_json_path(codex: &Path) -> PathBuf {
    codex.join(".codex-global-state.json")
}

/// manager 自己维护的家族树元数据文件（Codex 原生不感知）。
pub fn family_store_path(codex: &Path) -> PathBuf {
    codex.join("session_family.json")
}

/// CC Sessions 自己维护的归档来源登记，Codex/Claude 原生均不读取。
/// 放在 codex_home 根目录（与 session_family.json 平级），不放 sessions/ 或
/// archived_sessions/——官方 `codex doctor` 的 scan_rollout_files 会扫描这两个根。
pub fn archive_ledger_path(codex: &Path) -> PathBuf {
    codex.join("archive_ledger.json")
}

/// CC Sessions 自己维护的转换来源登记，Codex/Claude 原生均不读取。
pub fn session_provenance_path(codex: &Path) -> PathBuf {
    codex.join("session_provenance.json")
}

/// 从 rollout 绝对路径推算相对于 codex_dir 的相对路径。
/// 若不是 codex 子路径则返回 `sessions/<basename>`（保底）。
#[allow(dead_code)]
pub fn rollout_relpath(abs: &str, codex: &Path) -> PathBuf {
    let abs_clean = strip_verbatim(abs);
    let codex_clean = strip_verbatim(&codex.to_string_lossy());
    let abs_p = PathBuf::from(&abs_clean);
    let cx_p = PathBuf::from(&codex_clean);
    match abs_p.strip_prefix(&cx_p) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => abs_p
            .file_name()
            .map(|n| PathBuf::from("sessions").join(n))
            .unwrap_or_else(|| PathBuf::from("sessions/unknown.jsonl")),
    }
}

/// 机器标识：优先取环境变量 `CSM_MACHINE_LABEL`，否则用 hostname/COMPUTERNAME。
pub fn machine_label() -> String {
    if let Ok(v) = std::env::var("CSM_MACHINE_LABEL") {
        if !v.trim().is_empty() {
            return sanitize_slug(v.trim());
        }
    }
    if let Ok(v) = std::env::var("COMPUTERNAME") {
        if !v.trim().is_empty() {
            return sanitize_slug(v.trim());
        }
    }
    if let Ok(v) = std::env::var("HOSTNAME") {
        if !v.trim().is_empty() {
            return sanitize_slug(v.trim());
        }
    }
    "unknown-machine".into()
}

/// 把任意字符串变成跨平台安全的文件/目录名片段。
pub fn sanitize_slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let ok = c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
        out.push(if ok { c } else { '_' });
    }
    if out.is_empty() {
        "_".into()
    } else {
        out
    }
}

/// 校验外部 manifest / zip 中声明的相对路径，拒绝绝对路径和目录穿越。
pub fn checked_relative_path(raw: &str) -> AppResult<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AppError::Path("相对路径不能为空".into()));
    }
    if trimmed.contains('\0') {
        return Err(AppError::Path(format!("路径包含 NUL 字符: {raw}")));
    }
    if trimmed.starts_with('/') || trimmed.starts_with('\\') {
        return Err(AppError::Path(format!("拒绝绝对路径: {raw}")));
    }

    let normalized = trimmed.replace('\\', "/");
    let bytes = normalized.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Err(AppError::Path(format!("拒绝 Windows 盘符路径: {raw}")));
    }

    let mut out = PathBuf::new();
    for segment in normalized.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return Err(AppError::Path(format!("拒绝目录穿越路径: {raw}")));
        }
        if segment.contains(':') {
            return Err(AppError::Path(format!("路径片段包含冒号: {raw}")));
        }
        if segment.chars().any(|c| c.is_control()) {
            return Err(AppError::Path(format!("路径包含控制字符: {raw}")));
        }
        out.push(segment);
    }

    if out.as_os_str().is_empty() {
        return Err(AppError::Path(format!("相对路径无有效片段: {raw}")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SqliteFixture(PathBuf);

    impl SqliteFixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cc-sqlite-layout-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(path.join("sqlite")).unwrap();
            Self(path)
        }
        fn touch(&self, path: &str) {
            std::fs::write(self.0.join(path), []).unwrap();
        }
        fn config(&self, value: &str) {
            std::fs::write(self.0.join("config.toml"), value).unwrap();
        }
    }
    impl Drop for SqliteFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolves_root_and_nested_sqlite_without_rebasing_codex_home() -> AppResult<()> {
        let f = SqliteFixture::new();
        assert_eq!(resolve_codex_sqlite_dir(&f.0, None)?, f.0);
        f.touch("state_5.sqlite");
        assert_eq!(resolve_codex_sqlite_dir(&f.0, None)?, f.0);
        std::fs::remove_file(f.0.join("state_5.sqlite"))?;
        f.touch("sqlite/state_5.sqlite");
        assert_eq!(resolve_codex_sqlite_dir(&f.0, None)?, f.0.join("sqlite"));
        assert_eq!(sessions_dir(&f.0), f.0.join("sessions"));
        assert_eq!(session_index_path(&f.0), f.0.join("session_index.jsonl"));
        assert_eq!(config_toml_path(&f.0), f.0.join("config.toml"));
        Ok(())
    }

    #[test]
    fn rejects_ambiguous_databases_until_explicitly_configured() -> AppResult<()> {
        let f = SqliteFixture::new();
        f.touch("state_5.sqlite");
        f.touch("sqlite/state_5.sqlite");
        let error = resolve_codex_sqlite_dir(&f.0, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("同时找到") && error.contains("sqlite_home"));
        f.config("sqlite_home = 'sqlite'");
        assert_eq!(resolve_codex_sqlite_dir(&f.0, None)?, f.0.join("sqlite"));
        Ok(())
    }

    #[test]
    fn sqlite_config_precedes_environment_and_missing_paths_do_not_fall_back() -> AppResult<()> {
        let f = SqliteFixture::new();
        f.touch("state_5.sqlite");
        f.config("sqlite_home = 'missing'");
        assert_eq!(
            resolve_codex_sqlite_dir(&f.0, Some("other"))?,
            f.0.join("missing")
        );
        assert!(!f.0.join("missing").exists());
        Ok(())
    }

    #[test]
    fn sqlite_environment_is_trimmed_and_relative_to_process_cwd() -> AppResult<()> {
        let f = SqliteFixture::new();
        assert_eq!(
            resolve_codex_sqlite_dir(&f.0, Some("  custom-db  "))?,
            std::env::current_dir()?.join("custom-db")
        );
        assert_eq!(resolve_codex_sqlite_dir(&f.0, Some("  "))?, f.0);
        Ok(())
    }

    #[test]
    fn sqlite_config_preserves_path_whitespace_and_resolves_against_home() -> AppResult<()> {
        let f = SqliteFixture::new();
        f.config("sqlite_home = ' database with spaces '");
        assert_eq!(
            resolve_codex_sqlite_dir(&f.0, None)?,
            f.0.join(" database with spaces ")
        );
        Ok(())
    }

    #[test]
    fn sqlite_resolution_rejects_invalid_config_without_fallback() {
        let f = SqliteFixture::new();
        f.touch("state_5.sqlite");
        for config in ["sqlite_home = 123", "sqlite_home = ''", "broken = ["] {
            f.config(config);
            assert!(resolve_codex_sqlite_dir(&f.0, None).is_err());
        }
    }

    #[test]
    fn sqlite_config_maps_linux_absolute_paths_to_selected_wsl_distro() -> AppResult<()> {
        let codex = Path::new(r"\\wsl.localhost\Ubuntu\home\alice\.codex");
        assert_eq!(
            absolute_codex_path(codex, "/home/alice/.codex/sqlite", codex)?,
            PathBuf::from(r"\\wsl.localhost\Ubuntu\home\alice\.codex\sqlite")
        );
        assert!(absolute_codex_path(codex, "~/db", codex).is_err());
        let relative = absolute_codex_path(codex, "../sqlite/./db/..", codex)?;
        assert_eq!(
            relative,
            PathBuf::from(r"\\wsl.localhost\Ubuntu\home\alice\sqlite")
        );
        assert!(is_wsl_unc_path(&relative));
        Ok(())
    }

    #[test]
    fn maps_linux_paths_under_wsl_unc_root() {
        let codex = Path::new(r"\\wsl.localhost\Ubuntu\home\alice\.codex");
        let mapped =
            host_path_string_from_codex_record(codex, "/home/alice/.codex/sessions/a.jsonl");

        assert_eq!(
            mapped,
            r"\\wsl.localhost\Ubuntu\home\alice\.codex\sessions\a.jsonl"
        );
    }

    #[test]
    fn maps_linux_paths_under_wsl_dollar_unc_root() {
        let codex = Path::new(r"\\wsl$\Ubuntu\home\alice\.codex");
        let mapped = host_path_string_from_codex_record(codex, "/home/alice/project");

        assert_eq!(mapped, r"\\wsl$\Ubuntu\home\alice\project");
    }

    #[test]
    fn leaves_non_wsl_linux_paths_unchanged() {
        let codex = Path::new(r"C:\Users\alice\.codex");
        let mapped = host_path_string_from_codex_record(codex, "/home/alice/.codex/a.jsonl");

        assert_eq!(mapped, r"/home/alice/.codex/a.jsonl");
    }

    #[test]
    fn maps_wsl_host_paths_back_to_linux_records() -> AppResult<()> {
        let codex = Path::new(r"\\wsl.localhost\Ubuntu\home\alice\.codex");
        let host = Path::new(r"\\wsl.localhost\Ubuntu\home\alice\project");

        assert_eq!(
            codex_record_path_from_host(codex, host)?,
            "/home/alice/project"
        );
        Ok(())
    }

    #[test]
    fn maps_wsl_host_paths_across_unc_aliases_and_ascii_case() -> AppResult<()> {
        let codex = Path::new(r"\\wsl$\Ubuntu\home\alice\.codex");
        let host = Path::new(r"\\WSL.LOCALHOST\ubuntu\home\alice\project");

        assert_eq!(
            codex_record_path_from_host(codex, host)?,
            "/home/alice/project"
        );
        Ok(())
    }

    #[test]
    fn rejects_host_paths_outside_the_selected_wsl_distro() {
        let codex = Path::new(r"\\wsl$\Ubuntu\home\alice\.codex");
        let host = Path::new(r"C:\work\project");

        assert!(codex_record_path_from_host(codex, host).is_err());

        let other_distro = Path::new(r"\\wsl.localhost\Ubuntu-Preview\home\alice\project");
        assert!(codex_record_path_from_host(codex, other_distro).is_err());

        let unicode_distro = Path::new(r"\\wsl.localhost\发行版\home\alice\project");
        assert!(codex_record_path_from_host(codex, unicode_distro).is_err());

        let traversal = Path::new(r"\\wsl.localhost\Ubuntu\home\alice\..\bob\project");
        assert!(codex_record_path_from_host(codex, traversal).is_err());
    }
}
