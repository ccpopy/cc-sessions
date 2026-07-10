use std::fs;
use std::path::PathBuf;
use std::process::Command;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde::Serialize;

#[cfg(feature = "desktop")]
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::error::{AppError, AppResult};
use crate::paths;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewImage {
    pub data_url: String,
    pub mime: String,
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub fn read_preview_image(path: String) -> AppResult<PreviewImage> {
    let raw = path.trim();
    reject_non_local_preview_path(raw)?;
    let cleaned = paths::strip_verbatim(raw);
    let path = PathBuf::from(&cleaned);
    if !path.is_absolute() {
        return Err(AppError::Path(format!(
            "预览图片必须使用绝对路径: {cleaned}"
        )));
    }

    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(AppError::NotFound(cleaned));
        }
        Err(err) => return Err(AppError::Io(err)),
    };
    if !metadata.is_file() {
        return Err(AppError::Path(format!(
            "预览图片路径不是文件: {}",
            path.display()
        )));
    }

    let bytes = fs::read(&path)?;
    let mime = preview_image_mime(&bytes).ok_or_else(|| {
        AppError::Other(format!(
            "不支持的图片格式: {}（仅支持 PNG、JPEG、GIF、WebP）",
            path.display()
        ))
    })?;
    let encoded = BASE64_STANDARD.encode(bytes);
    Ok(PreviewImage {
        data_url: format!("data:{mime};base64,{encoded}"),
        mime: mime.to_string(),
    })
}

fn reject_non_local_preview_path(path: &str) -> AppResult<()> {
    let lower = path.to_ascii_lowercase();
    if has_url_scheme(path) {
        return Err(AppError::Path(format!(
            "预览图片路径必须是本地文件，不能使用 URL 或 data URL: {path}"
        )));
    }
    if path.starts_with(r"\\")
        || path.starts_with("//")
        || lower.starts_with(r"\??\")
        || lower.starts_with(r"\device\")
    {
        return Err(AppError::Path(format!(
            "预览图片不支持 UNC 或设备路径: {path}"
        )));
    }
    Ok(())
}

fn has_url_scheme(path: &str) -> bool {
    let Some(colon) = path.find(':') else {
        return false;
    };
    if colon == 1 && path.as_bytes()[0].is_ascii_alphabetic() {
        return false;
    }
    colon > 0
        && path[..colon].bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic()
                || (index > 0 && (byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')))
        })
}

fn preview_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::read_preview_image;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "cc-sessions-preview-image-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_supported_images_by_magic_bytes() {
        let dir = TestDir::new();
        let cases: [(&str, &[u8], &str, &str); 4] = [
            (
                "image.txt",
                b"\x89PNG\r\n\x1a\ncontent",
                "image/png",
                "iVBORw0KGgpjb250ZW50",
            ),
            (
                "image.bin",
                b"\xff\xd8\xff\xe0content",
                "image/jpeg",
                "/9j/4GNvbnRlbnQ=",
            ),
            (
                "image.data",
                b"GIF89acontent",
                "image/gif",
                "R0lGODlhY29udGVudA==",
            ),
            (
                "image.unknown",
                b"RIFF\x07\x00\x00\x00WEBPcontent",
                "image/webp",
                "UklGRgcAAABXRUJQY29udGVudA==",
            ),
        ];

        for (name, bytes, mime, encoded) in cases {
            let path = dir.path().join(name);
            fs::write(&path, bytes).expect("write fixture");

            let image = read_preview_image(path.to_string_lossy().into_owned())
                .expect("supported image should load");

            assert_eq!(image.mime, mime);
            assert_eq!(image.data_url, format!("data:{mime};base64,{encoded}"));
        }
    }

    #[test]
    fn rejects_relative_missing_directory_and_unsupported_paths() {
        let relative = read_preview_image("relative.png".to_string())
            .expect_err("relative paths must be rejected");
        assert!(relative.to_string().contains("绝对路径"));

        for unsafe_path in [
            "https://example.com/image.png",
            "ftp://example.com/image.png",
            "file:///C:/Temp/image.png",
            "data:image/png;base64,AAAA",
            r"\\server\share\image.png",
            r"\\?\C:\Temp\image.png",
            r"\\.\PhysicalDrive0",
        ] {
            let error = read_preview_image(unsafe_path.to_string())
                .expect_err("URLs, UNC paths, and device paths must be rejected");
            assert!(
                error.to_string().contains("不能使用 URL")
                    || error.to_string().contains("UNC 或设备路径")
            );
        }

        let dir = TestDir::new();
        let missing = read_preview_image(
            dir.path()
                .join("missing.png")
                .to_string_lossy()
                .into_owned(),
        )
        .expect_err("missing paths must be rejected");
        assert!(missing.to_string().contains("not found"));

        let directory = read_preview_image(dir.path().to_string_lossy().into_owned())
            .expect_err("directories must be rejected");
        assert!(directory.to_string().contains("不是文件"));

        let svg_path = dir.path().join("unsafe.svg");
        fs::write(
            &svg_path,
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
        )
        .expect("write SVG fixture");
        let unsupported = read_preview_image(svg_path.to_string_lossy().into_owned())
            .expect_err("SVG must be rejected");
        assert!(unsupported.to_string().contains("不支持的图片格式"));
    }
}
