use std::ffi::{c_void, OsString};
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

use super::InstallMode;
use crate::error::{AppError, AppResult};

const PORTABLE_MARKER: &str = "cc-session-manager.portable";
const INSTALL_REGISTRY_KEY: &str = r"Software\cc\CC Sessions";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub(super) fn detect_install_mode(current_exe: &Path, install_dir: &Path) -> InstallMode {
    let file_name = current_exe
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if install_dir.join(PORTABLE_MARKER).is_file() || file_name.contains("portable") {
        return InstallMode::Portable;
    }

    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey(INSTALL_REGISTRY_KEY);
    let Ok(key) = key else {
        return InstallMode::Portable;
    };
    let msi_dir = key.get_value::<String, _>("InstallDir").ok();
    let nsis_dir = key.get_value::<String, _>("").ok();
    classify_registry_install(install_dir, msi_dir.as_deref(), nsis_dir.as_deref())
}

pub(super) fn launch_update_helper(
    current_exe: &Path,
    update_dir: &Path,
    download_path: &Path,
    install_dir: &Path,
    mode: InstallMode,
    helper_flag: &str,
) -> AppResult<()> {
    let helper_path = update_dir.join("cc-sessions-update-helper.exe");
    fs::copy(current_exe, &helper_path).map_err(|error| {
        AppError::Other(format!(
            "创建更新助手失败 {}: {error}",
            helper_path.to_string_lossy()
        ))
    })?;
    Command::new(&helper_path)
        .arg(helper_flag)
        .arg(std::process::id().to_string())
        .arg(mode.as_str())
        .arg(download_path)
        .arg(current_exe)
        .arg(install_dir)
        .current_dir(update_dir)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|error| AppError::Other(format!("启动更新助手失败: {error}")))?;
    Ok(())
}

pub(super) fn run_update_helper(args: Vec<OsString>) -> AppResult<()> {
    if args.len() != 5 {
        return Err(AppError::Other(format!(
            "更新助手参数数量错误：预期 5，实际 {}",
            args.len()
        )));
    }
    let parent_pid = args[0]
        .to_string_lossy()
        .parse::<u32>()
        .map_err(|error| AppError::Other(format!("更新助手 PID 无效: {error}")))?;
    let mode = InstallMode::from_str(&args[1].to_string_lossy())
        .filter(|mode| *mode != InstallMode::Unsupported)
        .ok_or_else(|| AppError::Other("更新助手安装模式无效".to_string()))?;
    let download_path = PathBuf::from(&args[2]);
    let target_exe = PathBuf::from(&args[3]);
    let install_dir = PathBuf::from(&args[4]);

    wait_for_process_exit(parent_pid)?;
    let result = match mode {
        InstallMode::Portable => replace_portable(&download_path, &target_exe, &install_dir),
        InstallMode::Nsis => run_nsis_installer(&download_path, &install_dir),
        InstallMode::Msi => run_msi_installer(&download_path, &install_dir),
        InstallMode::Unsupported => unreachable!(),
    };
    if let Err(error) = result {
        let _ = restart_application(&target_exe, &install_dir);
        return Err(error);
    }
    restart_application(&target_exe, &install_dir)?;
    let _ = fs::remove_file(download_path);
    Ok(())
}

fn classify_registry_install(
    install_dir: &Path,
    msi_dir: Option<&str>,
    nsis_dir: Option<&str>,
) -> InstallMode {
    if msi_dir.is_some_and(|path| same_windows_path(install_dir, Path::new(path))) {
        InstallMode::Msi
    } else if nsis_dir.is_some_and(|path| same_windows_path(install_dir, Path::new(path))) {
        InstallMode::Nsis
    } else {
        InstallMode::Portable
    }
}

fn same_windows_path(left: &Path, right: &Path) -> bool {
    let normalize = |path: &Path| {
        path.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .replace('/', "\\")
            .to_ascii_lowercase()
    };
    normalize(left) == normalize(right)
}

fn wait_for_process_exit(pid: u32) -> AppResult<()> {
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 0x0000_0102;
    const WAIT_FAILED: u32 = 0xffff_ffff;

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return Ok(());
    }
    let wait = unsafe { WaitForSingleObject(handle, 5 * 60 * 1000) };
    unsafe {
        CloseHandle(handle);
    }
    match wait {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err(AppError::Other(
            "等待旧版本退出超时，已取消更新".to_string(),
        )),
        WAIT_FAILED => Err(AppError::Other(format!(
            "等待旧版本退出失败: {}",
            std::io::Error::last_os_error()
        ))),
        other => Err(AppError::Other(format!(
            "等待旧版本退出返回未知状态 {other}"
        ))),
    }
}

fn replace_portable(download_path: &Path, target_exe: &Path, install_dir: &Path) -> AppResult<()> {
    let file_name = target_exe
        .file_name()
        .ok_or_else(|| AppError::Other("当前便携版路径缺少文件名".to_string()))?;
    let mut staging_name = file_name.to_os_string();
    staging_name.push(format!(".{}.update", std::process::id()));
    let staging_path = install_dir.join(staging_name);
    fs::copy(download_path, &staging_path)?;
    if let Err(error) = replace_file(&staging_path, target_exe) {
        let _ = fs::remove_file(&staging_path);
        return Err(error.into());
    }
    fs::write(install_dir.join(PORTABLE_MARKER), b"portable\n")?;
    Ok(())
}

fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn run_nsis_installer(installer: &Path, install_dir: &Path) -> AppResult<()> {
    let status = Command::new(installer)
        .arg("/S")
        .arg(format!("/D={}", install_dir.to_string_lossy()))
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|error| AppError::Other(format!("启动 NSIS 更新安装器失败: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::Other(format!(
            "NSIS 更新安装失败，退出码 {:?}",
            status.code()
        )))
    }
}

fn run_msi_installer(installer: &Path, install_dir: &Path) -> AppResult<()> {
    let status = Command::new("msiexec.exe")
        .arg("/i")
        .arg(installer)
        .arg("/qn")
        .arg("/norestart")
        .arg(format!("INSTALLDIR={}", install_dir.to_string_lossy()))
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|error| AppError::Other(format!("启动 MSI 更新安装器失败: {error}")))?;
    match status.code() {
        Some(0 | 3010) => Ok(()),
        code => Err(AppError::Other(format!(
            "MSI 更新安装失败，退出码 {code:?}"
        ))),
    }
}

fn restart_application(target_exe: &Path, install_dir: &Path) -> AppResult<()> {
    Command::new(target_exe)
        .current_dir(install_dir)
        .spawn()
        .map_err(|error| AppError::Other(format!("更新后重新启动应用失败: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_values_distinguish_msi_nsis_and_portable() {
        let current = Path::new(r"D:\Apps\CC Sessions");
        assert_eq!(
            classify_registry_install(current, Some(r"D:\Apps\CC Sessions\"), None),
            InstallMode::Msi
        );
        assert_eq!(
            classify_registry_install(current, None, Some(r"d:/apps/cc sessions")),
            InstallMode::Nsis
        );
        assert_eq!(
            classify_registry_install(current, Some(r"C:\Program Files\CC Sessions"), None),
            InstallMode::Portable
        );
    }

    #[test]
    fn portable_replacement_preserves_the_original_target_path() -> AppResult<()> {
        let root = std::env::temp_dir().join(format!(
            "cc-sessions-portable-update-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&root)?;
        let download = root.join("download.exe");
        let target = root.join("custom-name.exe");
        fs::write(&download, b"new-version")?;
        fs::write(&target, b"old-version")?;

        replace_portable(&download, &target, &root)?;

        assert_eq!(fs::read(&target)?, b"new-version");
        assert!(root.join(PORTABLE_MARKER).is_file());
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn helper_flag_stays_in_sync_with_parent_module() {
        assert_eq!(super::super::UPDATE_HELPER_FLAG, "--cc-apply-update");
    }
}
