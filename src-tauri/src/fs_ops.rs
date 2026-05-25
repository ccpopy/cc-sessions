use std::path::PathBuf;
use std::process::Command;

#[cfg(feature = "desktop")]
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::error::{AppError, AppResult};
use crate::paths;

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn reveal_cwd(cwd: String) -> AppResult<()> {
    let cleaned = paths::strip_verbatim(&cwd);
    let path = PathBuf::from(&cleaned);
    if !path.exists() {
        return Err(AppError::NotFound(cleaned));
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("explorer")
            .arg(&path)
            .spawn()
            .map_err(AppError::Io)?;
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(&path)
            .spawn()
            .map_err(AppError::Io)?;
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open")
            .arg(&path)
            .spawn()
            .map_err(AppError::Io)?;
    }
    Ok(())
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn open_latest_release_page() -> AppResult<()> {
    open_external("https://github.com/ccpopy/cc-sessions/releases/latest")
}

fn open_external(url: &str) -> AppResult<()> {
    #[cfg(target_os = "windows")]
    {
        Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn()
            .map_err(AppError::Io)?;
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .spawn()
            .map_err(AppError::Io)?;
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map_err(AppError::Io)?;
    }
    Ok(())
}

pub fn resume_command_text(provider: Option<String>, session_id: String) -> AppResult<String> {
    let text = match provider.as_deref().unwrap_or("codex") {
        "codex" => format!("codex resume {}", session_id),
        "claude" => format!("claude --resume {}", session_id),
        "gemini" => format!("gemini --resume {}", session_id),
        other => return Err(AppError::Other(format!("不支持的 provider: {other}"))),
    };
    Ok(text)
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn copy_resume_command(
    app: tauri::AppHandle,
    provider: Option<String>,
    session_id: String,
) -> AppResult<String> {
    let text = resume_command_text(provider, session_id)?;
    app.clipboard()
        .write_text(text.clone())
        .map_err(|e| AppError::Other(e.to_string()))?;
    Ok(text)
}
