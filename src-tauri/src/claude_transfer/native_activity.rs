//! Offline-move precondition, not an advisory lock. Native Claude can reopen files
//! between turns; require its processes to exit as well as checking open writers.
//! References: code.claude.com/docs/en/{setup,sessions}; lsof(8), fields p/f/a.
use super::*;
use std::collections::BTreeSet;
use std::os::fd::AsRawFd;
use std::process::{Command, Output};

fn unknown(reason: &str) -> AppError {
    AppError::Other(format!(
        "[MOVE_ACTIVITY_UNKNOWN] 无法确认 Claude 会话写入已停止（{reason}），未继续迁移；请确认 ps、lsof 可用且有权读取本用户进程后重试"
    ))
}

fn busy() -> AppError {
    AppError::Other(
        "[SESSION_BUSY] Claude Code 进程仍在运行或会话资产被写入进程占用；请退出 Claude Code（包括后台和 IDE 中的会话），迁移完成前不要重新打开".into(),
    )
}

fn run(command: &mut Command) -> AppResult<Output> {
    // Never include stdout/stderr or command arguments in errors: ps can contain prompts.
    command
        .env("LC_ALL", "C")
        .output()
        .map_err(|_| unknown("占用检查程序启动失败"))
}

pub(super) fn ensure_stopped() -> AppResult<()> {
    // Inspect comm separately: native installs may have a version-number executable,
    // and executable paths may contain spaces. args also covers the older Node CLI/SDK.
    for field in ["comm=", "args="] {
        let output = run(Command::new("/bin/ps").args(["-A", "-ww", "-o", "pid=", "-o", field]))?;
        check_processes(&output, field == "args=")?;
    }
    Ok(())
}

fn native_executable(path: &str) -> bool {
    let path = Path::new(path);
    matches!(
        path.file_name().and_then(|v| v.to_str()),
        Some("claude" | "claude-code" | "Claude")
    ) || path
        .parent()
        .is_some_and(|p| p.ends_with(".local/share/claude/versions"))
}

fn native_command(args: &str) -> bool {
    let mut words = args.split_whitespace();
    let Some(executable) = words.next() else {
        return false;
    };
    if native_executable(executable) {
        return true;
    }
    if !matches!(
        Path::new(executable).file_name().and_then(|v| v.to_str()),
        Some("node" | "nodejs" | "bun")
    ) {
        return false;
    }
    words.any(|word| {
        let path = Path::new(word);
        path.ends_with("@anthropic-ai/claude-code/cli.js")
            || path.ends_with("@anthropic-ai/claude-agent-sdk/cli.js")
    })
}

fn check_processes(output: &Output, args: bool) -> AppResult<()> {
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(unknown("进程枚举失败"));
    }
    let stdout = std::str::from_utf8(&output.stdout).map_err(|_| unknown("进程信息无法解析"))?;
    let mut saw_self = false;
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let line = line.trim_start();
        let (pid, command) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| unknown("进程信息不完整"))?;
        let pid: u32 = pid.parse().map_err(|_| unknown("进程身份无法解析"))?;
        saw_self |= pid == std::process::id();
        let command = command.trim();
        if (args && native_command(command)) || (!args && native_executable(command)) {
            return Err(busy());
        }
    }
    if !saw_self {
        return Err(unknown("进程枚举范围不完整"));
    }
    Ok(())
}

pub(super) fn ensure_no_writers(files: &[(PathBuf, File)]) -> AppResult<()> {
    let lsof = ["/usr/bin/lsof", "/usr/sbin/lsof", "/bin/lsof", "/sbin/lsof"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .ok_or_else(|| unknown("未安装 lsof"))?;
    // Keep argv bounded for large sidecars. Our own read handles act as sentinels:
    // no output is an incomplete probe, never evidence that the files are unoccupied.
    for chunk in files.chunks(64) {
        let output = run(Command::new(lsof)
            .args(["-nP", "+w", "-S2", "-F0pfa", "--"])
            .args(chunk.iter().map(|(path, _)| path)))?;
        let expected = chunk.iter().map(|(_, file)| file.as_raw_fd()).collect();
        check_handles(&output, &expected)?;
    }
    Ok(())
}

fn check_handles(output: &Output, expected: &BTreeSet<i32>) -> AppResult<()> {
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(unknown("文件占用枚举失败或权限不足"));
    }
    let mut pid = None;
    let mut fd = None;
    let mut needs_mode = false;
    let mut observed = BTreeSet::new();
    for field in output
        .stdout
        .split(|b| *b == 0 || *b == b'\n')
        .filter(|v| !v.is_empty())
    {
        match field[0] {
            b'p' => {
                if needs_mode {
                    return Err(unknown("文件访问模式缺失"));
                }
                pid = std::str::from_utf8(&field[1..])
                    .ok()
                    .and_then(|v| v.parse::<u32>().ok());
                fd = None;
                if pid.is_none() {
                    return Err(unknown("文件占用进程身份不完整"));
                }
            }
            b'f' => {
                if needs_mode {
                    return Err(unknown("文件访问模式缺失"));
                }
                needs_mode = true;
                fd = std::str::from_utf8(&field[1..])
                    .ok()
                    .and_then(|v| v.parse::<i32>().ok());
            }
            b'a' => {
                if pid.is_none() || !needs_mode {
                    return Err(unknown("文件占用字段缺少进程身份"));
                }
                needs_mode = false;
                match &field[1..] {
                    b"w" | b"u" => return Err(busy()),
                    b"r" => {
                        if pid == Some(std::process::id()) {
                            if let Some(fd) = fd {
                                observed.insert(fd);
                            }
                        }
                    }
                    _ => return Err(unknown("文件访问模式无法确认")),
                }
            }
            _ => return Err(unknown("文件占用字段无法解析")),
        }
    }
    if needs_mode || !expected.is_subset(&observed) {
        return Err(unknown("未查到全部只读校验句柄"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn output(stdout: &str, code: i32, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn native_install_and_node_entrypoints_are_recognized() {
        assert!(native_executable(
            "/Users/a b/.local/share/claude/versions/2.1.283"
        ));
        assert!(native_executable("/opt/homebrew/bin/claude"));
        assert!(native_command("/usr/bin/node --no-warnings /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js --resume id"));
        assert!(native_command(
            "node /project/node_modules/@anthropic-ai/claude-agent-sdk/cli.js"
        ));
        assert!(!native_executable("/tmp/claude-transcript-viewer"));
        assert!(!native_command("/bin/echo claude"));
        assert!(!native_command("node /tmp/claude-code/cli.js"));
    }

    #[test]
    fn unavailable_or_partial_probe_is_not_idle() {
        assert!(check_processes(&output("", 0, ""), false).is_err());
        let self_row = format!("{} cc-sessions\n", std::process::id());
        assert!(check_processes(&output(&self_row, 0, "permission denied"), false).is_err());
        assert!(check_processes(&output(&self_row, 0, ""), false).is_ok());
        let expected = BTreeSet::from([7, 8]);
        assert!(check_handles(&output("", 1, ""), &expected).is_err());
        let handles = format!("p{}\0\nf7\0ar\0\nf8\0ar\0\n", std::process::id());
        assert!(check_handles(&output(&handles, 0, ""), &expected).is_ok());
        assert!(check_handles(&output(&handles.replace("f8", "f9"), 0, ""), &expected).is_err());
        assert!(check_handles(&output(&handles.replace("ar", "a "), 0, ""), &expected).is_err());
        assert!(check_handles(
            &output(&format!("{handles}p42\0\nf9\0\n"), 0, ""),
            &expected
        )
        .is_err());
        for mode in ["w", "u"] {
            let result = check_handles(
                &output(&format!("{handles}p42\0\nf9\0a{mode}\0\n"), 0, ""),
                &expected,
            );
            assert!(result.unwrap_err().to_string().contains("SESSION_BUSY"));
        }
    }
}
