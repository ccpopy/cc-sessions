//! Detect whether the official Desktop process can overwrite its private global state.

#[cfg(any(target_os = "linux", test))]
use std::path::Path;

use crate::error::{AppError, AppResult};

const DESKTOP_RUNNING_ERROR: &str =
    "Codex/ChatGPT 桌面应用正在运行；为避免其内存中的旧项目状态覆盖本次修改，请完全退出桌面应用（包括后台进程）后重试";
#[cfg(any(windows, test))]
const WINDOWS_DESKTOP_EXECUTABLE: &str = "ChatGPT.exe";
#[cfg(any(windows, test))]
const WINDOWS_CODEX_PACKAGE_FAMILY: &str = "OpenAI.Codex_2p2nqsd0c76g0";

#[cfg(test)]
#[derive(Clone)]
pub(super) enum TestDesktopProbe {
    Running(bool),
    Error(&'static str),
}

#[cfg(test)]
thread_local! {
    static TEST_DESKTOP_PROBES: std::cell::RefCell<std::collections::VecDeque<TestDesktopProbe>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

/// Thread-local test injection. Production builds have no bypass or persisted setting.
#[cfg(test)]
pub(crate) struct DesktopTestProbeGuard(std::collections::VecDeque<TestDesktopProbe>);

#[cfg(test)]
impl DesktopTestProbeGuard {
    pub(crate) fn running() -> Self {
        Self::sequence([TestDesktopProbe::Running(true)])
    }

    pub(crate) fn running_after_not_running(count: usize) -> Self {
        Self::sequence(
            std::iter::repeat(TestDesktopProbe::Running(false))
                .take(count)
                .chain([TestDesktopProbe::Running(true)]),
        )
    }

    pub(super) fn sequence(probes: impl IntoIterator<Item = TestDesktopProbe>) -> Self {
        let probes = probes.into_iter().collect();
        let previous = TEST_DESKTOP_PROBES.replace(probes);
        Self(previous)
    }
}

#[cfg(test)]
impl Drop for DesktopTestProbeGuard {
    fn drop(&mut self) {
        TEST_DESKTOP_PROBES.set(std::mem::take(&mut self.0));
    }
}

pub(super) fn ensure_official_desktop_not_running() -> AppResult<()> {
    ensure_not_running_with(official_desktop_is_running)
}

fn ensure_not_running_with(detect: impl FnOnce() -> AppResult<bool>) -> AppResult<()> {
    if detect()? {
        Err(AppError::Other(DESKTOP_RUNNING_ERROR.to_string()))
    } else {
        Ok(())
    }
}

pub(super) fn official_desktop_is_running() -> AppResult<bool> {
    #[cfg(test)]
    {
        return match TEST_DESKTOP_PROBES.with_borrow_mut(|probes| probes.pop_front()) {
            Some(TestDesktopProbe::Running(running)) => Ok(running),
            Some(TestDesktopProbe::Error(message)) => Err(AppError::Other(message.to_string())),
            None => Ok(false),
        };
    }
    #[cfg(all(not(test), windows))]
    {
        return windows_desktop_is_running();
    }
    #[cfg(all(not(test), target_os = "linux"))]
    {
        return linux_desktop_is_running();
    }
    #[cfg(all(not(test), target_os = "macos"))]
    {
        return macos_desktop_is_running();
    }
    #[cfg(all(
        not(test),
        not(windows),
        not(target_os = "linux"),
        not(target_os = "macos")
    ))]
    {
        Err(AppError::Other(
            "当前平台无法安全确认 Codex App 是否运行，已拒绝修改项目状态".to_string(),
        ))
    }
}

#[cfg(all(windows, not(test)))]
fn windows_desktop_is_running() -> AppResult<bool> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, APPMODEL_ERROR_NO_PACKAGE, ERROR_INSUFFICIENT_BUFFER,
        ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE, STILL_ACTIVE,
    };
    use windows_sys::Win32::Storage::Packaging::Appx::GetPackageFamilyName;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    struct OwnedHandle(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    unsafe fn package_family(
        process: windows_sys::Win32::Foundation::HANDLE,
    ) -> Result<Option<String>, u32> {
        let mut len = 0;
        let first = unsafe { GetPackageFamilyName(process, &mut len, std::ptr::null_mut()) };
        if first == APPMODEL_ERROR_NO_PACKAGE {
            return Ok(None);
        }
        if first != ERROR_INSUFFICIENT_BUFFER {
            return Err(first);
        }
        let mut buffer = vec![0u16; len as usize];
        let second = unsafe { GetPackageFamilyName(process, &mut len, buffer.as_mut_ptr()) };
        if second != 0 {
            return Err(second);
        }
        let nul = buffer
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(buffer.len());
        Ok(Some(String::from_utf16_lossy(&buffer[..nul])))
    }

    unsafe fn process_is_running(
        process: windows_sys::Win32::Foundation::HANDLE,
    ) -> Result<bool, u32> {
        let mut exit_code = 0;
        if unsafe { GetExitCodeProcess(process, &mut exit_code) } == 0 {
            return Err(unsafe { GetLastError() });
        }
        Ok(exit_code == STILL_ACTIVE as u32)
    }

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(AppError::Other(format!(
            "无法安全确认 Codex App 是否运行（进程快照失败，Windows 错误 {}），已拒绝修改项目状态",
            unsafe { GetLastError() }
        )));
    }
    let snapshot = OwnedHandle(snapshot);
    let mut entry = PROCESSENTRY32W::default();
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    if unsafe { Process32FirstW(snapshot.0, &mut entry) } == 0 {
        let error = unsafe { GetLastError() };
        if error == ERROR_NO_MORE_FILES {
            return Ok(false);
        }
        return Err(AppError::Other(format!(
            "无法安全确认 Codex App 是否运行（枚举进程失败，Windows 错误 {error}），已拒绝修改项目状态"
        )));
    }

    loop {
        if is_windows_desktop_candidate(&process_entry_name(&entry)) {
            let process =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID) };
            if process.is_null() {
                let error = unsafe { GetLastError() };
                if error != ERROR_INVALID_PARAMETER {
                    return Err(AppError::Other(format!(
                        "无法安全确认 Codex App 是否运行（打开候选进程 {} 失败，Windows 错误 {error}），已拒绝修改项目状态",
                        entry.th32ProcessID
                    )));
                }
            } else {
                let process = OwnedHandle(process);
                match unsafe { process_is_running(process.0) } {
                    Ok(false) => {}
                    Ok(true) => match unsafe { package_family(process.0) } {
                        Ok(Some(family))
                            if is_official_windows_desktop(
                                WINDOWS_DESKTOP_EXECUTABLE,
                                Some(&family),
                            ) =>
                        {
                            return Ok(true)
                        }
                        Ok(_) => {}
                        Err(error) => {
                            if error == ERROR_INVALID_PARAMETER
                                && matches!(unsafe { process_is_running(process.0) }, Ok(false))
                            {
                                // Exited candidates do not own Desktop state any more.
                            } else {
                                return Err(AppError::Other(format!(
                                    "无法安全确认 Codex App 是否运行（读取候选进程 {} 的包身份失败，Windows 错误 {error}），已拒绝修改项目状态",
                                    entry.th32ProcessID
                                )));
                            }
                        }
                    },
                    Err(error) => {
                        return Err(AppError::Other(format!(
                            "无法安全确认 Codex App 是否运行（读取候选进程 {} 的状态失败，Windows 错误 {error}），已拒绝修改项目状态",
                            entry.th32ProcessID
                        )))
                    }
                }
            }
        }
        if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
            let error = unsafe { GetLastError() };
            if error == ERROR_NO_MORE_FILES {
                break;
            }
            return Err(AppError::Other(format!(
                "无法安全确认 Codex App 是否运行（枚举进程失败，Windows 错误 {error}），已拒绝修改项目状态"
            )));
        }
    }
    Ok(false)
}

#[cfg(any(windows, test))]
pub(super) fn is_windows_desktop_candidate(process_name: &str) -> bool {
    process_name.eq_ignore_ascii_case(WINDOWS_DESKTOP_EXECUTABLE)
}

#[cfg(any(windows, test))]
pub(super) fn is_official_windows_desktop(
    process_name: &str,
    package_family: Option<&str>,
) -> bool {
    is_windows_desktop_candidate(process_name)
        && package_family == Some(WINDOWS_CODEX_PACKAGE_FAMILY)
}

#[cfg(all(windows, not(test)))]
fn process_entry_name(
    entry: &windows_sys::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W,
) -> String {
    let end = entry
        .szExeFile
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(entry.szExeFile.len());
    String::from_utf16_lossy(&entry.szExeFile[..end])
}

#[cfg(all(target_os = "linux", not(test)))]
fn linux_desktop_is_running() -> AppResult<bool> {
    const LINUX_DESKTOP_EXECUTABLE: &str = "/usr/lib/chatgpt/ChatGPT";
    procfs_desktop_is_running(Path::new("/proc"), LINUX_DESKTOP_EXECUTABLE)
}

#[cfg(all(target_os = "linux", not(test)))]
fn procfs_desktop_is_running(proc_root: &Path, expected_executable: &str) -> AppResult<bool> {
    use std::os::unix::fs::MetadataExt;

    let own_uid = std::fs::metadata(proc_root.join("self"))
        .map_err(|error| {
            AppError::Other(format!(
                "无法安全确认 Codex App 是否运行（读取当前进程身份失败: {error}），已拒绝修改项目状态"
            ))
        })?
        .uid();
    let entries = std::fs::read_dir(proc_root).map_err(|error| {
        AppError::Other(format!(
            "无法安全确认 Codex App 是否运行（读取进程目录失败: {error}），已拒绝修改项目状态"
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            AppError::Other(format!(
                "无法安全确认 Codex App 是否运行（枚举进程失败: {error}），已拒绝修改项目状态"
            ))
        })?;
        if !entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|value| value.is_ascii_digit())
        {
            continue;
        }
        let process_path = entry.path();
        match entry.metadata() {
            Ok(metadata) if metadata.uid() != own_uid => continue,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(AppError::Other(format!(
                    "无法安全确认 Codex App 是否运行（读取进程所有者失败: {error}），已拒绝修改项目状态"
                )))
            }
        }
        let process_name = match std::fs::read_to_string(process_path.join("comm")) {
            Ok(process_name) => process_name,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(AppError::Other(format!(
                "无法安全确认 Codex App 是否运行（读取进程名称失败: {error}），已拒绝修改项目状态"
            )))
            }
        };
        if !is_linux_desktop_candidate(&process_name) {
            continue;
        }
        match std::fs::read_link(process_path.join("exe")) {
            Ok(executable) if is_linux_desktop_executable(&executable, expected_executable) => {
                return Ok(true)
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(AppError::Other(format!(
                    "无法安全确认 Codex App 是否运行（读取候选进程身份失败: {error}），已拒绝修改项目状态"
                )))
            }
        }
    }
    Ok(false)
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn is_linux_desktop_candidate(process_name: &str) -> bool {
    process_name.trim_end_matches(['\r', '\n']) == "ChatGPT"
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn is_linux_desktop_executable(executable: &Path, expected_executable: &str) -> bool {
    executable == Path::new(expected_executable)
        || executable
            .to_str()
            .is_some_and(|path| path == format!("{expected_executable} (deleted)"))
}

#[cfg(all(target_os = "macos", not(test)))]
fn macos_desktop_is_running() -> AppResult<bool> {
    Err(AppError::Other(
        "当前版本尚无法安全确认 macOS Codex App 是否运行，已拒绝修改项目状态".to_string(),
    ))
}
