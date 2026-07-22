use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(target_os = "windows")]
mod windows;

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/ccpopy/cc-sessions/releases/latest";
const UPDATE_DIR_PREFIX: &str = "cc-sessions-update-";
const UPDATE_HELPER_FLAG: &str = "--cc-apply-update";

#[derive(Debug, Clone, Serialize)]
pub struct AppUpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub html_url: String,
    pub available: bool,
    pub can_auto_install: bool,
    pub install_mode: String,
    pub install_dir: Option<String>,
    pub asset_name: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallMode {
    Portable,
    Nsis,
    Msi,
    Unsupported,
}

impl InstallMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Portable => "portable",
            Self::Nsis => "nsis",
            Self::Msi => "msi",
            Self::Unsupported => "unsupported",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "portable" => Some(Self::Portable),
            "nsis" => Some(Self::Nsis),
            "msi" => Some(Self::Msi),
            "unsupported" => Some(Self::Unsupported),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct InstallContext {
    mode: InstallMode,
    current_exe: PathBuf,
    install_dir: PathBuf,
}

#[tauri::command]
pub async fn check_app_update() -> AppResult<AppUpdateInfo> {
    let release = fetch_latest_release().await?;
    build_update_info(&release, &install_context()?)
}

#[tauri::command]
pub async fn install_app_update(app: tauri::AppHandle) -> AppResult<()> {
    let release = fetch_latest_release().await?;
    let context = install_context()?;
    let current_version = env!("CARGO_PKG_VERSION");
    let latest_version = normalize_version(&release.tag_name);
    if compare_versions(&latest_version, current_version) <= 0 {
        return Err(AppError::Other(format!(
            "当前版本 {current_version} 已是最新版本"
        )));
    }
    if context.mode == InstallMode::Unsupported {
        return Err(AppError::Other(
            "当前平台暂不支持应用内自动安装，请打开 Release 页面下载".to_string(),
        ));
    }
    let asset =
        select_update_asset(&release.assets, context.mode, &latest_version).ok_or_else(|| {
            AppError::Other(format!(
                "Release {} 中没有适用于 {} 的更新包",
                release.tag_name,
                context.mode.as_str()
            ))
        })?;

    let update_dir = update_temp_dir(&latest_version);
    fs::create_dir_all(&update_dir)?;
    let download_path = download_asset(asset, &update_dir).await?;

    #[cfg(target_os = "windows")]
    windows::launch_update_helper(
        &context.current_exe,
        &update_dir,
        &download_path,
        &context.install_dir,
        context.mode,
        UPDATE_HELPER_FLAG,
    )?;

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (download_path, update_dir);
        return Err(AppError::Other(
            "当前平台暂不支持应用内自动安装".to_string(),
        ));
    }

    let exit_handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(350));
        exit_handle.exit(0);
    });
    Ok(())
}

pub fn run_update_helper_from_args() -> bool {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(UPDATE_HELPER_FLAG)) {
        return false;
    }

    #[cfg(target_os = "windows")]
    if let Err(error) = windows::run_update_helper(args.collect()) {
        write_helper_error(&error.to_string());
    }

    #[cfg(not(target_os = "windows"))]
    let _ = args;

    true
}

pub fn cleanup_stale_update_dirs() {
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let max_age = Duration::from_secs(30 * 24 * 60 * 60);
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(UPDATE_DIR_PREFIX) {
            continue;
        }
        let path = entry.path();
        let old_enough = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= max_age);
        if path.is_dir() && old_enough {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn install_context() -> AppResult<InstallContext> {
    let current_exe = std::env::current_exe()
        .map_err(|error| AppError::Other(format!("无法确定当前程序路径: {error}")))?;
    let install_dir = current_exe
        .parent()
        .ok_or_else(|| AppError::Other("无法确定当前程序目录".to_string()))?
        .to_path_buf();

    #[cfg(target_os = "windows")]
    let mode = windows::detect_install_mode(&current_exe, &install_dir);
    #[cfg(not(target_os = "windows"))]
    let mode = InstallMode::Unsupported;

    Ok(InstallContext {
        mode,
        current_exe,
        install_dir,
    })
}

fn build_update_info(
    release: &GithubRelease,
    context: &InstallContext,
) -> AppResult<AppUpdateInfo> {
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let latest_version = normalize_version(&release.tag_name);
    if latest_version.is_empty() {
        return Err(AppError::Other("GitHub Release 缺少有效版本号".to_string()));
    }
    let available = compare_versions(&latest_version, &current_version) > 0;
    let asset = select_update_asset(&release.assets, context.mode, &latest_version);
    let can_auto_install = available && context.mode != InstallMode::Unsupported && asset.is_some();
    let message = if !available {
        None
    } else if context.mode == InstallMode::Unsupported {
        Some("当前平台暂不支持自动安装，请打开 Release 页面下载".to_string())
    } else if asset.is_none() {
        Some(format!(
            "最新 Release 中没有适用于 {} 的更新包",
            context.mode.as_str()
        ))
    } else {
        None
    };

    Ok(AppUpdateInfo {
        current_version,
        latest_version,
        html_url: release.html_url.clone(),
        available,
        can_auto_install,
        install_mode: context.mode.as_str().to_string(),
        install_dir: Some(context.install_dir.to_string_lossy().into_owned()),
        asset_name: asset.map(|asset| asset.name.clone()),
        message,
    })
}

async fn fetch_latest_release() -> AppResult<GithubRelease> {
    let client = reqwest::Client::builder()
        .user_agent(format!("cc-sessions/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| AppError::Other(format!("创建更新请求失败: {error}")))?;
    let response = client
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| AppError::Other(format!("检查 GitHub Release 失败: {error}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| AppError::Other(format!("读取 GitHub Release 响应失败: {error}")))?;
    if !status.is_success() {
        return Err(AppError::Other(format!(
            "GitHub Release 返回 {status}: {}",
            text.trim()
        )));
    }
    serde_json::from_str(&text)
        .map_err(|error| AppError::Other(format!("解析 GitHub Release 响应失败: {error}")))
}

async fn download_asset(asset: &GithubAsset, update_dir: &Path) -> AppResult<PathBuf> {
    let expected_name = Path::new(&asset.name)
        .file_name()
        .filter(|name| *name == std::ffi::OsStr::new(&asset.name))
        .ok_or_else(|| AppError::Other("Release 更新包名称不安全".to_string()))?;
    let expected_digest = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64)
        .ok_or_else(|| AppError::Other("Release 更新包缺少 SHA-256 校验值".to_string()))?;

    let client = reqwest::Client::builder()
        .user_agent(format!("cc-sessions/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(5 * 60))
        .build()
        .map_err(|error| AppError::Other(format!("创建下载请求失败: {error}")))?;
    let response = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .map_err(|error| AppError::Other(format!("下载更新包失败: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(AppError::Other(format!("下载更新包返回 {status}")));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| AppError::Other(format!("读取更新包失败: {error}")))?;
    if bytes.len() as u64 != asset.size {
        return Err(AppError::Other(format!(
            "更新包大小不匹配：预期 {}，实际 {}",
            asset.size,
            bytes.len()
        )));
    }
    let actual_digest = hex::encode(Sha256::digest(&bytes));
    if !actual_digest.eq_ignore_ascii_case(expected_digest) {
        return Err(AppError::Other(format!(
            "更新包 SHA-256 校验失败：预期 {expected_digest}，实际 {actual_digest}"
        )));
    }

    let destination = update_dir.join(expected_name);
    let mut file = File::create(&destination)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(destination)
}

fn select_update_asset<'a>(
    assets: &'a [GithubAsset],
    mode: InstallMode,
    version: &str,
) -> Option<&'a GithubAsset> {
    let expected = expected_asset_name(mode, version, std::env::consts::ARCH)?;
    assets.iter().find(|asset| asset.name == expected)
}

fn expected_asset_name(mode: InstallMode, version: &str, arch: &str) -> Option<String> {
    if arch != "x86_64" {
        return None;
    }
    match mode {
        InstallMode::Portable => Some(format!(
            "cc-session-manager-portable-v{version}-windows.exe"
        )),
        InstallMode::Nsis => Some(format!("CC.Sessions_{version}_x64-setup.exe")),
        InstallMode::Msi => Some(format!("CC.Sessions_{version}_x64_en-US.msi")),
        InstallMode::Unsupported => None,
    }
}

fn update_temp_dir(version: &str) -> PathBuf {
    let safe_version: String = version
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
        .collect();
    std::env::temp_dir().join(format!(
        "{UPDATE_DIR_PREFIX}{safe_version}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    ))
}

fn normalize_version(raw: &str) -> String {
    raw.trim().trim_start_matches(['v', 'V']).to_string()
}

fn compare_versions(left: &str, right: &str) -> i8 {
    let left = parse_version(left);
    let right = parse_version(right);
    for index in 0..left.len().max(right.len()) {
        let left_part = *left.get(index).unwrap_or(&0);
        let right_part = *right.get(index).unwrap_or(&0);
        if left_part != right_part {
            return if left_part > right_part { 1 } else { -1 };
        }
    }
    0
}

fn parse_version(value: &str) -> Vec<u64> {
    normalize_version(value)
        .split(['.', '-'])
        .filter_map(|part| part.parse::<u64>().ok())
        .collect()
}

fn write_helper_error(message: &str) {
    let Ok(helper) = std::env::current_exe() else {
        return;
    };
    let Some(parent) = helper.parent() else {
        return;
    };
    let _ = fs::write(parent.join("update-error.log"), message);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> GithubAsset {
        GithubAsset {
            name: name.to_string(),
            browser_download_url: "https://example.invalid/asset".to_string(),
            size: 1,
            digest: Some(format!("sha256:{}", "0".repeat(64))),
        }
    }

    #[test]
    fn compares_numeric_release_versions() {
        assert_eq!(compare_versions("0.10.0", "0.9.9"), 1);
        assert_eq!(compare_versions("1.2", "1.2.0"), 0);
        assert_eq!(compare_versions("v1.1.9", "1.2.0"), -1);
    }

    #[test]
    fn selects_asset_matching_the_install_mode() {
        let assets = vec![
            asset("cc-session-manager-portable-v0.5.0-windows.exe"),
            asset("CC.Sessions_0.5.0_x64-setup.exe"),
            asset("CC.Sessions_0.5.0_x64_en-US.msi"),
        ];

        assert_eq!(
            select_update_asset(&assets, InstallMode::Portable, "0.5.0")
                .map(|asset| asset.name.as_str()),
            Some("cc-session-manager-portable-v0.5.0-windows.exe")
        );
        assert_eq!(
            select_update_asset(&assets, InstallMode::Nsis, "0.5.0")
                .map(|asset| asset.name.as_str()),
            Some("CC.Sessions_0.5.0_x64-setup.exe")
        );
        assert_eq!(
            select_update_asset(&assets, InstallMode::Msi, "0.5.0")
                .map(|asset| asset.name.as_str()),
            Some("CC.Sessions_0.5.0_x64_en-US.msi")
        );
    }
}
