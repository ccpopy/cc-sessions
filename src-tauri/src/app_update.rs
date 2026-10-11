use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::{AppError, AppResult};
use crate::models::UpdateNetworkSettings;
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
pub async fn check_app_update(
    app: tauri::AppHandle,
    network: Option<UpdateNetworkSettings>,
) -> AppResult<AppUpdateInfo> {
    let network = configured_update_network(&app, network)?;
    let release = fetch_latest_release(&network).await?;
    build_update_info(&release, &install_context()?)
}

#[tauri::command]
pub async fn install_app_update(
    app: tauri::AppHandle,
    network: Option<UpdateNetworkSettings>,
) -> AppResult<()> {
    let network = configured_update_network(&app, network)?;
    let release = fetch_latest_release(&network).await?;
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
    let download_path = download_asset(asset, &update_dir, &network).await?;

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

fn configured_update_network(
    app: &tauri::AppHandle,
    network: Option<UpdateNetworkSettings>,
) -> AppResult<UpdateNetworkSettings> {
    match network {
        Some(network) => Ok(network),
        None => Ok(crate::settings::get_settings(app.clone())?.update_network),
    }
}

fn update_client(network: &UpdateNetworkSettings, timeout: Duration) -> AppResult<reqwest::Client> {
    crate::settings::validate_update_network(network)?;
    let mut builder = reqwest::Client::builder()
        .user_agent(format!("cc-sessions/{}", env!("CARGO_PKG_VERSION")))
        .timeout(timeout);
    if !network.proxy_url.trim().is_empty() {
        let proxy = reqwest::Proxy::all(network.proxy_url.trim()).map_err(|error| {
            AppError::Other(format!("创建更新代理失败: {}", error.without_url()))
        })?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|error| AppError::Other(format!("创建更新请求失败: {}", error.without_url())))
}

async fn fetch_latest_release(network: &UpdateNetworkSettings) -> AppResult<GithubRelease> {
    let client = update_client(network, Duration::from_secs(30))?;
    fetch_release(&client, LATEST_RELEASE_API).await
}

async fn fetch_release(client: &reqwest::Client, api_url: &str) -> AppResult<GithubRelease> {
    let response = client
        .get(api_url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| {
            AppError::Other(format!(
                "检查 GitHub Release 失败，请检查网络或更新代理设置: {}",
                error.without_url()
            ))
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(AppError::Other(format!("GitHub Release 返回 {status}")));
    }
    let text = response.text().await.map_err(|error| {
        AppError::Other(format!(
            "读取 GitHub Release 响应失败: {}",
            error.without_url()
        ))
    })?;
    serde_json::from_str(&text)
        .map_err(|error| AppError::Other(format!("解析 GitHub Release 响应失败: {error}")))
}

fn update_download_url(original: &str, network: &UpdateNetworkSettings) -> AppResult<String> {
    crate::settings::validate_update_network(network)?;
    let mirror = network.download_mirror.trim().trim_end_matches('/');
    if mirror.is_empty() {
        Ok(original.to_string())
    } else {
        // Prefix-style GitHub mirrors expect the complete original URL, including its scheme.
        Ok(format!("{mirror}/{original}"))
    }
}

async fn download_asset(
    asset: &GithubAsset,
    update_dir: &Path,
    network: &UpdateNetworkSettings,
) -> AppResult<PathBuf> {
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

    let client = update_client(network, Duration::from_secs(5 * 60))?;
    let download_url = update_download_url(&asset.browser_download_url, network)?;
    let response = client.get(download_url).send().await.map_err(|error| {
        AppError::Other(format!(
            "下载更新包失败，请检查下载加速地址或更新代理设置: {}",
            error.without_url()
        ))
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(AppError::Other(format!("下载更新包返回 {status}")));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| AppError::Other(format!("读取更新包失败: {}", error.without_url())))?;
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

    fn mock_update_server(
        responses: Vec<(u16, Vec<u8>)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = format!("http://{}", server.server_addr());
        let worker = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, bytes) in responses {
                let request = server
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .expect("update request must reach the configured local endpoint");
                requests.push(format!("{} {}", request.method(), request.url()));
                request
                    .respond(tiny_http::Response::from_data(bytes).with_status_code(status))
                    .unwrap();
            }
            requests
        });
        (address, worker)
    }

    #[test]
    fn update_network_mirror_preserves_the_complete_download_url() {
        let original =
            "https://github.com/ccpopy/cc-sessions/releases/download/v9.0.0/test%20x64.exe";
        assert_eq!(
            update_download_url(original, &UpdateNetworkSettings::default()).unwrap(),
            format!("https://ghproxy.net/{original}")
        );
        let direct = UpdateNetworkSettings {
            download_mirror: String::new(),
            ..Default::default()
        };
        assert_eq!(update_download_url(original, &direct).unwrap(), original);
        for mirror in ["https://mirror.example/gh", " https://mirror.example/gh/ "] {
            let network = UpdateNetworkSettings {
                download_mirror: mirror.into(),
                ..Default::default()
            };
            assert_eq!(
                update_download_url(original, &network).unwrap(),
                format!("https://mirror.example/gh/{original}")
            );
        }
    }

    #[test]
    fn update_network_proxy_carries_metadata_and_verified_mirror_download() {
        let payload = b"verified synthetic update".to_vec();
        let original = "https://github.com/ccpopy/cc-sessions/releases/download/v9.0.0/test.exe";
        let metadata = serde_json::json!({
            "tag_name": "v9.0.0",
            "html_url": "https://github.com/ccpopy/cc-sessions/releases/tag/v9.0.0",
            "assets": [{
                "name": "test.exe", "browser_download_url": original,
                "size": payload.len(), "digest": format!("sha256:{}", hex::encode(Sha256::digest(&payload)))
            }]
        });
        let (proxy, worker) = mock_update_server(vec![
            (200, serde_json::to_vec(&metadata).unwrap()),
            (200, payload.clone()),
        ]);
        let network = UpdateNetworkSettings {
            proxy_url: proxy,
            download_mirror: "http://mirror.invalid/gh/".into(),
        };
        let root = update_temp_dir("network-proxy");
        fs::create_dir_all(&root).unwrap();
        tauri::async_runtime::block_on(async {
            let client = update_client(&network, Duration::from_secs(5)).unwrap();
            let release = fetch_release(&client, "http://api.invalid/releases/latest")
                .await
                .unwrap();
            assert_eq!(release.tag_name, "v9.0.0");
            let path = download_asset(&release.assets[0], &root, &network)
                .await
                .unwrap();
            assert_eq!(fs::read(path).unwrap(), payload);
        });
        assert_eq!(
            worker.join().unwrap(),
            vec![
                "GET http://api.invalid/releases/latest".to_string(),
                format!("GET http://mirror.invalid/gh/{original}"),
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn update_network_direct_and_mirror_only_downloads_succeed() {
        let payload = b"verified update without an explicit proxy".to_vec();
        for mirror_only in [false, true] {
            let (server, worker) = mock_update_server(vec![(200, payload.clone())]);
            let mut update = asset("test.exe");
            update.size = payload.len() as u64;
            update.digest = Some(format!("sha256:{}", hex::encode(Sha256::digest(&payload))));
            let mut network = UpdateNetworkSettings {
                download_mirror: String::new(),
                ..Default::default()
            };
            let expected_path;
            if mirror_only {
                network.download_mirror = format!("{server}/gh/");
                expected_path = format!("GET /gh/{}", update.browser_download_url);
            } else {
                update.browser_download_url = format!("{server}/test.exe");
                expected_path = "GET /test.exe".into();
            }
            let root = update_temp_dir(if mirror_only {
                "network-mirror-only"
            } else {
                "network-direct"
            });
            fs::create_dir_all(&root).unwrap();
            let result = tauri::async_runtime::block_on(download_asset(&update, &root, &network));
            assert_eq!(fs::read(result.unwrap()).unwrap(), payload);
            assert_eq!(worker.join().unwrap(), vec![expected_path]);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn update_network_mirror_failures_do_not_replace_a_download() {
        let valid = b"trusted-update";
        for (name, status, bytes, reason) in [
            ("digest", 200, b"changed-update".to_vec(), "SHA-256"),
            ("size", 200, b"short".to_vec(), "大小不匹配"),
            ("status", 503, b"private response body".to_vec(), "503"),
        ] {
            let (proxy, worker) = mock_update_server(vec![(status, bytes)]);
            let network = UpdateNetworkSettings {
                proxy_url: proxy,
                download_mirror: "http://mirror.invalid/".into(),
            };
            let mut update = asset("test.exe");
            update.size = valid.len() as u64;
            update.digest = Some(format!("sha256:{}", hex::encode(Sha256::digest(valid))));
            let root = update_temp_dir(&format!("network-{name}"));
            fs::create_dir_all(&root).unwrap();
            let destination = root.join("test.exe");
            fs::write(&destination, b"keep-existing-file").unwrap();
            let error = tauri::async_runtime::block_on(download_asset(&update, &root, &network))
                .unwrap_err()
                .to_string();
            assert!(error.contains(reason), "{error}");
            assert!(!error.contains("private response body"));
            assert_eq!(fs::read(destination).unwrap(), b"keep-existing-file");
            assert_eq!(worker.join().unwrap().len(), 1);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn update_network_proxy_failure_on_github_https_is_reported_without_fallback() {
        let (proxy, worker) = mock_update_server(vec![(502, Vec::new())]);
        let network = UpdateNetworkSettings {
            proxy_url: proxy,
            ..Default::default()
        };
        let error = tauri::async_runtime::block_on(fetch_latest_release(&network))
            .unwrap_err()
            .to_string();
        assert!(error.contains("更新代理设置"), "{error}");
        assert_eq!(worker.join().unwrap(), vec!["CONNECT api.github.com:443"]);
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
