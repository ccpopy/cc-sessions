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

pub fn default_gemini_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".gemini"))
        .unwrap_or_else(|| PathBuf::from(".gemini"))
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

pub fn validate_gemini_dir(path: &Path) -> (bool, bool) {
    let exists = path.is_dir();
    let has_cli_chats = path.join("tmp").is_dir();
    let has_antigravity = ["antigravity-cli", "antigravity", "antigravity-ide"]
        .iter()
        .any(|surface| path.join(surface).join("conversations").is_dir());
    (exists, has_cli_chats || has_antigravity)
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

/// Codex App 的 Electron 全局状态文件：维护 workspace-roots / project-order。
/// 修复时若不同步该文件，官方 App 左侧项目列表不会显示新会话。
pub fn codex_global_state_json_path(codex: &Path) -> PathBuf {
    codex.join(".codex-global-state.json")
}

/// manager 自己维护的家族树元数据文件（Codex 原生不感知）。
pub fn family_store_path(codex: &Path) -> PathBuf {
    codex.join("session_family.json")
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
