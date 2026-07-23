use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "desktop")]
use crate::error::AppError;
use crate::error::AppResult;
use crate::models::{DirValidation, Settings};
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
        write_settings_file(&file, &second)?;

        let restored = read_settings_file(&file)?;
        assert_eq!(restored.codex_dir, "second-codex");
        assert_eq!(restored.claude_dir, "second-claude");
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
