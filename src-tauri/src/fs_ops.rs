use std::fs;
use std::io::Read;
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

    const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(AppError::Other("预览图片不能超过 20 MiB".into()));
    }
    let mut file = fs::File::open(&path)?;
    let mut bytes = Vec::new();
    file.by_ref().take(12).read_to_end(&mut bytes)?;
    let mime = preview_image_mime(&bytes).ok_or_else(|| {
        AppError::Other(format!(
            "不支持的图片格式: {}（仅支持 PNG、JPEG、GIF、WebP）",
            path.display()
        ))
    })?;
    file.take(MAX_IMAGE_BYTES + 1 - bytes.len() as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(AppError::Other("预览图片不能超过 20 MiB".into()));
    }
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

pub fn claude_resume_command(session_id: &str, cwd: Option<&str>) -> String {
    if validate_resume_arguments(session_id, cwd).is_err() {
        return String::new();
    }
    let cwd = cwd
        .map(paths::strip_verbatim)
        .filter(|value| !value.trim().is_empty());
    let Some(cwd) = cwd else {
        return format!("claude --resume {session_id}");
    };

    #[cfg(target_os = "windows")]
    {
        let quoted = cwd.replace('\'', "''");
        format!("Set-Location -LiteralPath '{quoted}'; claude --resume {session_id}")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let quoted = cwd.replace('\'', "'\"'\"'");
        format!("cd -- '{quoted}' && claude --resume {session_id}")
    }
}

pub fn resume_command_text(
    provider: Option<String>,
    session_id: String,
    cwd: Option<String>,
) -> AppResult<String> {
    validate_resume_arguments(&session_id, cwd.as_deref())?;
    let text = match provider.as_deref().unwrap_or("codex") {
        "codex" => format!("codex resume {}", session_id),
        "claude" => claude_resume_command(&session_id, cwd.as_deref()),
        "opencode" => format!("opencode --session {}", session_id),
        // Cursor 的 IDE 会话没有命令行入口；只有 cursor-agent 的会话可以续聊，
        // 具体命令在 SessionSummary.resume_command 里按会话给出。
        "cursor" => String::new(),
        other => return Err(AppError::Other(format!("不支持的 provider: {other}"))),
    };
    Ok(text)
}

fn validate_resume_arguments(session_id: &str, cwd: Option<&str>) -> AppResult<()> {
    if !session_id
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        || !session_id
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_'))
        || cwd.is_some_and(|value| value.chars().any(char::is_control))
    {
        return Err(AppError::Other(
            "续聊参数无效：会话 ID 只能包含字母、数字、短横线和下划线，目录不能包含控制字符".into(),
        ));
    }
    Ok(())
}

pub(crate) fn session_resume_command(provider: &str, session_id: &str) -> String {
    if provider == "cursor-agent" {
        return if validate_resume_arguments(session_id, None).is_ok() {
            format!("cursor-agent --resume {session_id}")
        } else {
            String::new()
        };
    }
    resume_command_text(Some(provider.into()), session_id.into(), None).unwrap_or_default()
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn copy_resume_command(
    app: tauri::AppHandle,
    provider: Option<String>,
    session_id: String,
    cwd: Option<String>,
) -> AppResult<String> {
    let text = resume_command_text(provider, session_id, cwd)?;
    app.clipboard()
        .write_text(text.clone())
        .map_err(|e| AppError::Other(e.to_string()))?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    #[test]
    fn review_preview_image_rejects_oversized_file() {
        let root = TestDir::new();
        let path = root.path().join("oversized.png");
        fs::File::create(&path)
            .unwrap()
            .set_len(20 * 1024 * 1024 + 1)
            .unwrap();
        assert!(read_preview_image(path.to_string_lossy().into_owned())
            .unwrap_err()
            .to_string()
            .contains("20 MiB"));
    }
    #[test]
    fn review_resume_rejects_shell_syntax_and_controls() {
        for provider in ["codex", "claude", "opencode"] {
            for id in [
                "",
                "--help",
                "id; echo injected",
                "$(whoami)",
                "id\nnext",
                "a'b",
                "a\"b",
            ] {
                assert!(
                    resume_command_text(Some(provider.into()), id.into(), None).is_err(),
                    "{provider}: {id:?}"
                );
            }
        }
        assert!(resume_command_text(
            Some("claude".into()),
            "valid-id".into(),
            Some("project\nnext".into())
        )
        .is_err());
    }

    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{claude_resume_command, read_preview_image, resume_command_text};

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

    #[test]
    fn claude_resume_command_changes_to_the_session_project() {
        let command = claude_resume_command(
            "019f8e89-c687-7ce5-9e82-5434fcc9f133",
            Some("F:\\demo\\it's-project"),
        );

        #[cfg(target_os = "windows")]
        assert_eq!(
            command,
            "Set-Location -LiteralPath 'F:\\demo\\it''s-project'; claude --resume 019f8e89-c687-7ce5-9e82-5434fcc9f133"
        );
        #[cfg(not(target_os = "windows"))]
        assert_eq!(
            command,
            "cd -- 'F:\\demo\\it'\"'\"'s-project' && claude --resume 019f8e89-c687-7ce5-9e82-5434fcc9f133"
        );
    }

    #[test]
    fn opencode_resume_command_uses_session_flag() {
        assert_eq!(
            resume_command_text(Some("opencode".into()), "ses_test".into(), None)
                .expect("OpenCode resume command"),
            "opencode --session ses_test"
        );
    }
}
