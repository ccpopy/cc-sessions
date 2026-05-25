use std::fs;
use std::path::{Path, PathBuf};

#[cfg(feature = "desktop")]
use crate::error::AppError;
use crate::error::AppResult;
use crate::models::{DirValidation, Settings};
use crate::paths;
use crate::state_db;

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
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(&settings)?;
    fs::write(file, content)?;
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
pub fn default_gemini_dir() -> String {
    paths::default_gemini_dir().to_string_lossy().into_owned()
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
pub fn validate_gemini_dir(path: String) -> AppResult<DirValidation> {
    let p = PathBuf::from(&path);
    let (exists, has_sessions) = paths::validate_gemini_dir(&p);
    let threads_count = if has_sessions {
        crate::gemini_sessions::scan_sessions(&p)?.len() as u32
    } else {
        0
    };
    Ok(DirValidation {
        valid: exists && has_sessions,
        has_state_db: false,
        has_sessions,
        threads_count,
    })
}
