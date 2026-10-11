use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{AppError, AppResult};
use crate::models::{DirValidation, Settings, UpdateNetworkSettings};
use crate::paths;
use crate::state_db;

static SETTINGS_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "desktop")]
fn config_file(app: &tauri::AppHandle) -> AppResult<PathBuf> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| AppError::Path(e.to_string()))?;
    fs::create_dir_all(&dir)?;
    Ok(dir.join("settings.json"))
}

pub fn read_settings_file(file: &Path) -> AppResult<Settings> {
    if !file.exists() {
        return Ok(Settings::default());
    }
    let raw = fs::read_to_string(&file)?;
    let parsed: Settings = serde_json::from_str(&raw)?;
    Ok(parsed)
}

pub fn write_settings_file(file: &Path, settings: &Settings) -> AppResult<()> {
    validate_update_network(&settings.update_network)?;
    let parent = file
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let content = serde_json::to_vec_pretty(settings)?;
    let (temp_path, mut temp_file) = create_unique_settings_temp(file, parent)?;
    let write_result = (|| -> io::Result<()> {
        temp_file.write_all(&content)?;
        temp_file.sync_all()
    })();
    drop(temp_file);

    if let Err(error) = write_result {
        return Err(cleanup_temp_after_error(&temp_path, error).into());
    }
    if let Err(error) = atomic_replace(&temp_path, file) {
        return Err(cleanup_temp_after_error(&temp_path, error).into());
    }

    sync_settings_parent(parent)?;
    Ok(())
}

pub(crate) fn validate_update_network(network: &UpdateNetworkSettings) -> AppResult<()> {
    for (value, label, proxy) in [
        (network.proxy_url.trim(), "更新代理地址", true),
        (network.download_mirror.trim(), "下载加速地址", false),
    ] {
        if value.is_empty() {
            continue;
        }
        let url = url::Url::parse(value)
            .map_err(|_| AppError::Other(format!("{label}格式无效，请填写包含协议的完整地址")))?;
        let supported = matches!(url.scheme(), "http" | "https")
            || (proxy && matches!(url.scheme(), "socks5" | "socks5h"));
        if !supported || url.host_str().is_none() {
            return Err(AppError::Other(format!(
                "{label}需使用 {} 地址",
                if proxy {
                    "HTTP、HTTPS 或 SOCKS5"
                } else {
                    "HTTP 或 HTTPS"
                }
            )));
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(AppError::Other(format!(
                "{label}请勿包含账号密码、查询参数或片段"
            )));
        }
        if proxy && !matches!(url.path(), "" | "/") {
            return Err(AppError::Other(
                "更新代理地址只能包含协议、主机和端口".into(),
            ));
        }
    }
    Ok(())
}

fn create_unique_settings_temp(file: &Path, parent: &Path) -> io::Result<(PathBuf, File)> {
    let file_name = file.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("设置文件路径缺少文件名: {}", file.to_string_lossy()),
        )
    })?;
    loop {
        let sequence = SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_name = format!(
            ".{}.{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            sequence
        );
        let temp_path = parent.join(temp_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(temp_file) => return Ok((temp_path, temp_file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

fn cleanup_temp_after_error(temp_path: &Path, primary: io::Error) -> io::Error {
    match fs::remove_file(temp_path) {
        Ok(()) => primary,
        Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => primary,
        Err(cleanup) => io::Error::new(
            primary.kind(),
            format!(
                "{}；清理临时设置文件 {} 失败: {}",
                primary,
                temp_path.to_string_lossy(),
                cleanup
            ),
        ),
    }
}

#[cfg(target_os = "windows")]
fn atomic_replace(temp_path: &Path, file: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let existing = temp_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replacement = file
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let moved = unsafe {
        MoveFileExW(
            existing.as_ptr(),
            replacement.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
fn atomic_replace(temp_path: &Path, file: &Path) -> io::Result<()> {
    fs::rename(temp_path, file)
}

fn sync_settings_parent(parent: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
    }
    Ok(())
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn get_settings(app: tauri::AppHandle) -> AppResult<Settings> {
    let file = config_file(&app)?;
    read_settings_file(&file)
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn save_settings(app: tauri::AppHandle, settings: Settings) -> AppResult<()> {
    let file = config_file(&app)?;
    write_settings_file(&file, &settings)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn default_codex_dir() -> String {
    paths::default_codex_dir().to_string_lossy().into_owned()
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn default_claude_dir() -> String {
    paths::default_claude_dir().to_string_lossy().into_owned()
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn default_opencode_dir() -> String {
    paths::default_opencode_dir().to_string_lossy().into_owned()
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn default_cursor_dir() -> String {
    paths::default_cursor_dir().to_string_lossy().into_owned()
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn validate_codex_dir(path: String) -> AppResult<DirValidation> {
    let p = PathBuf::from(&path);
    let (exists, has_state, has_sessions) = paths::validate_codex_dir(&p);
    let threads_count = if has_state {
        state_db::count_threads(&p).unwrap_or(0)
    } else {
        0
    };
    Ok(DirValidation {
        valid: exists && has_state && has_sessions,
        has_state_db: has_state,
        has_sessions,
        threads_count,
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn validate_claude_dir(path: String) -> AppResult<DirValidation> {
    let p = PathBuf::from(&path);
    let (exists, has_projects) = paths::validate_claude_dir(&p);
    let threads_count = if has_projects {
        crate::claude_sessions::scan_sessions(&p)?.len() as u32
    } else {
        0
    };
    Ok(DirValidation {
        valid: exists && has_projects,
        has_state_db: false,
        has_sessions: has_projects,
        threads_count,
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn validate_opencode_dir(path: String) -> AppResult<DirValidation> {
    let data_dir = PathBuf::from(&path);
    let database = crate::opencode_sessions::database_path(&data_dir);
    let count = crate::opencode_sessions::validate_data_dir(&data_dir)?;
    Ok(DirValidation {
        valid: data_dir.is_dir() && database.is_file(),
        has_state_db: database.is_file(),
        has_sessions: database.is_file(),
        threads_count: count,
    })
}

/// Cursor 的会话分散在 IDE 数据库与 cursor-agent 目录两处，任一处有会话即视为可用。
#[cfg_attr(feature = "desktop", tauri::command)]
pub fn validate_cursor_dir(path: String) -> AppResult<DirValidation> {
    let cursor_dir = PathBuf::from(&path);
    let agent_dir = paths::default_cursor_agent_dir();
    let database = crate::cursor_sessions::state_db_path(&cursor_dir);
    let count = crate::cursor_sessions::validate_data_dir(&cursor_dir, &agent_dir)?;
    Ok(DirValidation {
        valid: count > 0 || database.is_file(),
        has_state_db: database.is_file(),
        has_sessions: count > 0,
        threads_count: count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cc-sessions-settings-{name}-{}-{}",
            std::process::id(),
            SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn settings_write_atomically_replaces_existing_file() -> AppResult<()> {
        let root = temp_dir("replace");
        fs::create_dir_all(&root)?;
        let file = root.join("settings.json");
        let mut first = Settings::default();
        first.codex_dir = "first-codex".into();
        write_settings_file(&file, &first)?;

        let mut second = first.clone();
        second.codex_dir = "second-codex".into();
        second.claude_dir = "second-claude".into();
        second.opencode_dir = "second-opencode".into();
        write_settings_file(&file, &second)?;

        let restored = read_settings_file(&file)?;
        assert_eq!(restored.codex_dir, "second-codex");
        assert_eq!(restored.claude_dir, "second-claude");
        assert_eq!(restored.opencode_dir, "second-opencode");
        let entries = fs::read_dir(&root)?.collect::<Result<Vec<_>, _>>()?;
        let leftovers = entries
            .iter()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn invalid_settings_json_is_reported() -> AppResult<()> {
        let root = temp_dir("invalid-json");
        fs::create_dir_all(&root)?;
        let file = root.join("settings.json");
        fs::write(&file, "{\"codex_dir\":")?;
        assert!(read_settings_file(&file).is_err());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn legacy_settings_enable_preview_preferences_by_default() -> AppResult<()> {
        let root = temp_dir("legacy-preview-preferences");
        fs::create_dir_all(&root)?;
        let file = root.join("settings.json");
        fs::write(
            &file,
            serde_json::json!({
                "codex_dir": "legacy-codex",
                "backup_dir": "legacy-backups"
            })
            .to_string(),
        )?;

        let settings = read_settings_file(&file)?;

        assert!(settings.preview_only_messages);
        assert!(settings.preview_collapse_process);
        assert_eq!(
            settings.opencode_dir,
            paths::default_opencode_dir().to_string_lossy()
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn update_network_accepts_supported_protocols_and_rejects_ambiguous_addresses() {
        for proxy in [
            "",
            "http://127.0.0.1:7890",
            "https://proxy.example:443",
            "socks5://[::1]:1080",
            "socks5h://localhost:1080",
        ] {
            let network = UpdateNetworkSettings {
                proxy_url: proxy.into(),
                ..Default::default()
            };
            assert!(validate_update_network(&network).is_ok());
        }
        for proxy in [
            "127.0.0.1:7890",
            "file:///tmp/proxy",
            "http://localhost/proxy",
            "http://localhost:7890?token=secret",
        ] {
            let network = UpdateNetworkSettings {
                proxy_url: proxy.into(),
                ..Default::default()
            };
            let error = validate_update_network(&network).unwrap_err().to_string();
            assert!(error.contains("更新代理地址"));
            assert!(!error.contains("secret"));
        }
        for mirror in [
            "socks5://localhost:1080",
            "https://mirror.example/?url=",
            "https://user:secret@mirror.example/",
        ] {
            let network = UpdateNetworkSettings {
                download_mirror: mirror.into(),
                ..Default::default()
            };
            let error = validate_update_network(&network).unwrap_err().to_string();
            assert!(error.contains("下载加速地址"));
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn update_network_settings_roundtrip_and_legacy_defaults() -> AppResult<()> {
        let root = temp_dir("update-network-roundtrip");
        fs::create_dir_all(&root)?;
        let file = root.join("settings.json");
        assert_eq!(
            read_settings_file(&file)?.update_network.download_mirror,
            "https://ghproxy.net/"
        );
        let mut value = serde_json::to_value(Settings::default())?;
        for mirror in ["https://mirror.example/gh/", ""] {
            let network = serde_json::json!({
                "proxy_url": "http://127.0.0.1:7890",
                "download_mirror": mirror
            });
            value["update_network"] = network.clone();
            let settings: Settings = serde_json::from_value(value.clone())?;
            write_settings_file(&file, &settings)?;
            let restored = serde_json::to_value(read_settings_file(&file)?)?;
            assert_eq!(restored["update_network"], network);
        }

        value.as_object_mut().unwrap().remove("update_network");
        fs::write(&file, serde_json::to_vec(&value)?)?;
        let legacy = serde_json::to_value(read_settings_file(&file)?)?;
        assert_eq!(legacy["update_network"]["proxy_url"], "");
        assert_eq!(
            legacy["update_network"]["download_mirror"],
            "https://ghproxy.net/"
        );

        value["update_network"] = serde_json::json!({"proxy_url": "http://127.0.0.1:7890"});
        fs::write(&file, serde_json::to_vec(&value)?)?;
        let partial = read_settings_file(&file)?.update_network;
        assert_eq!(partial.proxy_url, "http://127.0.0.1:7890");
        assert_eq!(partial.download_mirror, "https://ghproxy.net/");
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn invalid_update_network_does_not_replace_saved_settings() -> AppResult<()> {
        let root = temp_dir("update-network-invalid");
        fs::create_dir_all(&root)?;
        let file = root.join("settings.json");
        write_settings_file(&file, &Settings::default())?;
        let before = fs::read(&file)?;
        let mut value = serde_json::to_value(Settings::default())?;
        value["update_network"] = serde_json::json!({
            "proxy_url": "file:///not-a-proxy",
            "download_mirror": ""
        });
        let invalid: Settings = serde_json::from_value(value)?;
        assert!(write_settings_file(&file, &invalid).is_err());
        assert_eq!(fs::read(&file)?, before);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn preview_preferences_are_persisted() -> AppResult<()> {
        let root = temp_dir("preview-preferences-roundtrip");
        fs::create_dir_all(&root)?;
        let file = root.join("settings.json");
        let mut settings = Settings::default();
        settings.preview_only_messages = false;
        settings.preview_collapse_process = false;

        write_settings_file(&file, &settings)?;
        let restored = read_settings_file(&file)?;

        assert!(!restored.preview_only_messages);
        assert!(!restored.preview_collapse_process);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
