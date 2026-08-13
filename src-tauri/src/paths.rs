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

pub fn default_backup_dir() -> PathBuf {
    let cc_root = default_codex_dir();
    cc_root
        .parent()
        .map(|p| p.join("cc-backups"))
        .unwrap_or_else(|| PathBuf::from("cc-backups"))
}

pub fn validate_codex_dir(path: &Path) -> (bool, bool, bool) {
    let exists = path.is_dir();
    let has_state = path.join("state_5.sqlite").is_file();
    let has_sessions = path.join("sessions").is_dir();
    (exists, has_state, has_sessions)
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

pub fn state_db_path(codex: &Path) -> PathBuf {
    codex.join("state_5.sqlite")
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
