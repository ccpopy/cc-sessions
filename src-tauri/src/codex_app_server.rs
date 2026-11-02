use std::ffi::{OsStr, OsString};
#[cfg(not(windows))]
use std::io::Read;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::time::SystemTime;

use serde_json::{json, Value};

use crate::error::{AppError, AppResult};

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(not(windows))]
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForkedThread {
    pub(crate) id: String,
    pub(crate) path: Option<PathBuf>,
    pub(crate) model_provider: Option<String>,
}

/// Minimal stdio client for the official Codex app-server methods needed by provider sync.
pub(crate) struct CodexAppServer {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_lines: Receiver<std::io::Result<String>>,
    next_request_id: u64,
}

impl CodexAppServer {
    pub(crate) fn start(codex_dir: &Path) -> AppResult<Self> {
        let inherited_path = std::env::var_os("PATH");
        // Finder starts GUI apps with a narrow PATH. Use the user's login shell for
        // both locating Codex and running a script-based CLI's interpreter (node).
        #[cfg(not(windows))]
        let search_path = merge_search_paths(
            inherited_path.as_deref(),
            login_shell_path(LOGIN_SHELL_TIMEOUT).as_deref(),
        );
        #[cfg(windows)]
        let search_path = inherited_path;
        let executable = resolve_codex_executable(search_path.as_deref());
        let mut command = Command::new(&executable);
        command
            .args(["app-server", "--stdio"])
            .env("CODEX_HOME", codex_dir);
        if let Some(path) = search_path {
            command.env("PATH", path);
        }
        Self::start_command(command)
    }

    fn start_command(mut command: Command) -> AppResult<Self> {
        let executable = command.get_program().to_string_lossy().into_owned();
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command.spawn().map_err(|error| {
            AppError::Other(format!(
                "无法启动 Codex app-server（{executable}）：{error}；请确认已安装当前 Codex App 或 CLI"
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::Other("Codex app-server 未提供 stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::Other("Codex app-server 未提供 stdout".into()))?;
        let (sender, stdout_lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });

        let mut server = Self {
            child,
            stdin: Some(stdin),
            stdout_lines,
            next_request_id: 1,
        };
        server.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "cc_sessions",
                    "title": "CC Sessions",
                    "version": env!("CARGO_PKG_VERSION")
                },
                // Some releases gate fork/resume.excludeTurns behind this opt-in.
                "capabilities": { "experimentalApi": true }
            }),
        )?;
        server.notify("initialized", json!({}))?;
        Ok(server)
    }

    pub(crate) fn fork_thread_for_provider(
        &mut self,
        thread_id: &str,
        model_provider: &str,
    ) -> AppResult<ForkedThread> {
        self.fork_thread(thread_id, model_provider, true)
    }

    /// `exclude_turns=false` 时官方会把源会话全部对话一并派生（对应“完整 Fork”）。
    pub(crate) fn fork_thread(
        &mut self,
        thread_id: &str,
        model_provider: &str,
        exclude_turns: bool,
    ) -> AppResult<ForkedThread> {
        let result = self.request(
            "thread/fork",
            json!({
                "threadId": thread_id,
                "modelProvider": model_provider,
                "excludeTurns": exclude_turns
            }),
        )?;
        parse_fork_result(&result)
    }

    pub(crate) fn delete_thread(&mut self, thread_id: &str) -> AppResult<()> {
        self.request("thread/delete", json!({"threadId": thread_id}))?;
        Ok(())
    }

    pub(crate) fn read_thread(&mut self, thread_id: &str) -> AppResult<ForkedThread> {
        let result = self.request("thread/read", json!({"threadId": thread_id}))?;
        parse_thread_result(&result, "thread/read")
    }

    pub(crate) fn resume_for_provider(
        &mut self,
        thread_id: &str,
        provider: &str,
    ) -> AppResult<ForkedThread> {
        // Let Core preserve the thread's cwd, permissions, history and logical identity.
        // No turn/start and no resident routing process are needed for this operation.
        let result = self.request(
            "thread/resume",
            json!({
                "threadId": thread_id, "modelProvider": provider, "excludeTurns": true
            }),
        )?;
        parse_thread_result(&result, "thread/resume")
    }

    pub(crate) fn set_thread_name(&mut self, thread_id: &str, name: &str) -> AppResult<()> {
        self.request(
            "thread/name/set",
            json!({"threadId": thread_id, "name": name}),
        )?;
        Ok(())
    }

    fn notify(&mut self, method: &str, params: Value) -> AppResult<()> {
        self.write_message(&json!({"method": method, "params": params}))
    }

    fn request(&mut self, method: &str, params: Value) -> AppResult<Value> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        self.write_message(&json!({
            "method": method,
            "id": request_id,
            "params": params
        }))?;

        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AppError::Other(format!(
                    "Codex app-server {method} 请求超时"
                )));
            }
            let line = match self.stdout_lines.recv_timeout(remaining) {
                Ok(Ok(line)) => line,
                Ok(Err(error)) => return Err(error.into()),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(AppError::Other(format!(
                        "Codex app-server {method} 请求超时"
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let status = self.child.try_wait().ok().flatten();
                    return Err(AppError::Other(format!(
                        "Codex app-server 在 {method} 响应前退出（状态 {status:?}）"
                    )));
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let response: Value = serde_json::from_str(&line).map_err(|error| {
                AppError::Other(format!("Codex app-server 返回了无效 JSON：{error}"))
            })?;
            if response.get("id").and_then(Value::as_u64) != Some(request_id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误");
                return Err(AppError::Other(format!(
                    "Codex app-server {method} 失败：{message}"
                )));
            }
            return response.get("result").cloned().ok_or_else(|| {
                AppError::Other(format!("Codex app-server {method} 响应缺少 result"))
            });
        }
    }

    fn write_message(&mut self, message: &Value) -> AppResult<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| AppError::Other("Codex app-server stdin 已关闭".into()))?;
        serde_json::to_writer(&mut *stdin, message)?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    }
}

impl Drop for CodexAppServer {
    fn drop(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_fork_result(result: &Value) -> AppResult<ForkedThread> {
    parse_thread_result(result, "thread/fork")
}

fn parse_thread_result(result: &Value, method: &str) -> AppResult<ForkedThread> {
    let thread = result
        .get("thread")
        .and_then(Value::as_object)
        .ok_or_else(|| AppError::Other(format!("Codex {method} 响应缺少 thread")))?;
    let id = thread
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| AppError::Other(format!("Codex {method} 响应缺少会话 ID")))?;
    let path = thread
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let model_provider = result
        .get("modelProvider")
        .or_else(|| thread.get("modelProvider"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
        .map(str::to_string);
    Ok(ForkedThread {
        id: id.to_string(),
        path,
        model_provider,
    })
}

fn resolve_codex_executable(search_path: Option<&OsStr>) -> PathBuf {
    let executable_name = if cfg!(windows) {
        OsString::from("codex.exe")
    } else {
        OsString::from("codex")
    };
    if let Some(path) = search_path.and_then(|paths| {
        std::env::split_paths(paths)
            .map(|directory| directory.join(&executable_name))
            .find(|candidate| candidate.is_file())
    }) {
        return path;
    }
    #[cfg(windows)]
    if let Some(path) = windows_desktop_codex_executable() {
        return path;
    }
    PathBuf::from(executable_name)
}

#[cfg(not(windows))]
fn merge_search_paths(inherited: Option<&OsStr>, login: Option<&OsStr>) -> Option<OsString> {
    let Some(login) = login.filter(|path| !path.is_empty()) else {
        return inherited.map(OsStr::to_os_string);
    };
    // Keep an explicitly supplied PATH ahead of the shell's extra directories.
    let directories = inherited
        .into_iter()
        .chain(std::iter::once(login))
        .flat_map(std::env::split_paths);
    std::env::join_paths(directories)
        .ok()
        .or_else(|| inherited.map(OsStr::to_os_string))
}

#[cfg(not(windows))]
fn login_shell_path(timeout: Duration) -> Option<OsString> {
    let shell = std::env::var_os("SHELL")
        .filter(|shell| Path::new(shell).is_absolute())
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                OsString::from("/bin/zsh")
            } else {
                OsString::from("/bin/sh")
            }
        });
    login_shell_path_with(&shell, timeout)
}

#[cfg(not(windows))]
fn login_shell_path_with(shell: &OsStr, timeout: Duration) -> Option<OsString> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;

    let mut child = Command::new(shell)
        .args([
            "-lic",
            "/usr/bin/printenv PATH | /usr/bin/sed 's/^/CC_SESSIONS_LOGIN_PATH=/'",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut stdout = child.stdout.take()?;
                // A background process started by shell initialization may keep
                // stdout open after the shell exits. Read available bytes only.
                let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
                if flags < 0
                    || unsafe {
                        libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK)
                    } < 0
                {
                    return None;
                }
                let mut output = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    match stdout.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(size) if output.len() + size <= 65_536 => {
                            output.extend_from_slice(&chunk[..size]);
                        }
                        Ok(_) => return None,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => return None,
                    }
                }
                return output
                    .split(|byte| *byte == b'\n')
                    .filter_map(|line| line.strip_prefix(b"CC_SESSIONS_LOGIN_PATH="))
                    .filter(|path| !path.is_empty())
                    .last()
                    .map(|path| OsString::from_vec(path.to_vec()));
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(windows)]
fn windows_desktop_codex_executable() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("OpenAI")
        .join("Codex")
        .join("bin");
    let mut candidates = Vec::new();
    let direct = root.join("codex.exe");
    if direct.is_file() {
        candidates.push(direct);
    }
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let candidate = entry.path().join("codex.exe");
        if candidate.is_file() {
            candidates.push(candidate);
        }
    }
    candidates.into_iter().max_by_key(|candidate| {
        std::fs::metadata(candidate)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    use std::fs;

    #[cfg(not(windows))]
    #[test]
    fn gui_path_finds_codex_and_its_node_interpreter() -> AppResult<()> {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "cc-sessions-codex-path-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let codex_bin = root.join("codex-bin");
        let node_bin = root.join("node-bin");
        let narrow_bin = root.join("narrow-bin");
        for directory in [&codex_bin, &node_bin, &narrow_bin] {
            fs::create_dir_all(directory)?;
        }
        let codex = codex_bin.join("codex");
        let node = node_bin.join("node");
        let shell = root.join("login-shell");
        fs::write(&codex, "#!/usr/bin/env node\n")?;
        fs::write(&node, "#!/bin/sh\nprintf 'app-server started\\n'\n")?;
        fs::write(
            &shell,
            "#!/bin/sh\nroot=$(cd \"$(dirname \"$0\")\" && pwd)\nprintf 'shell greeting\\nCC_SESSIONS_LOGIN_PATH=%s\\nshell goodbye\\n' \"$root/codex-bin:$root/node-bin\"\n",
        )?;
        for executable in [&codex, &node, &shell] {
            fs::set_permissions(executable, fs::Permissions::from_mode(0o755))?;
        }

        let login_path = login_shell_path_with(shell.as_os_str(), Duration::from_secs(1))
            .expect("login shell PATH");
        assert_eq!(
            resolve_codex_executable(Some(narrow_bin.as_os_str())),
            PathBuf::from("codex")
        );
        for inherited in [narrow_bin.as_os_str(), codex_bin.as_os_str()] {
            let search_path =
                merge_search_paths(Some(inherited), Some(&login_path)).expect("combined PATH");
            assert_eq!(resolve_codex_executable(Some(&search_path)), codex);
            let output = Command::new(&codex)
                .args(["app-server", "--stdio"])
                .env("PATH", search_path)
                .output()?;
            assert!(output.status.success());
            assert_eq!(output.stdout, b"app-server started\n");
        }
        assert_eq!(
            merge_search_paths(Some(narrow_bin.as_os_str()), None),
            Some(narrow_bin.as_os_str().to_os_string())
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn negotiates_experimental_capability_before_fork_and_resume() -> AppResult<()> {
        // Exercise the actual stdio initialization and requests against a capability-gated peer.
        // Node is also required by the frontend build and is installed in the CI test jobs.
        let mut command = Command::new("node");
        command.args([
            "--input-type=commonjs",
            "-e",
            r#"
const readline = require('node:readline');
let experimentalApi = false, initialized = false;
readline.createInterface({ input: process.stdin }).on('line', line => {
    const { id, method, params } = JSON.parse(line);
    const reply = value => process.stdout.write(JSON.stringify({ id, ...value }) + '\n');
    if (method === 'initialize') {
        experimentalApi = params.capabilities?.experimentalApi === true;
        reply({ result: { userAgent: 'capability-gated-fixture' } });
    } else if (method === 'initialized') {
        initialized = true;
    } else if (!initialized) {
        reply({ error: { code: -32002, message: 'Not initialized' } });
    } else if (['thread/fork', 'thread/resume'].includes(method)) {
        if (!experimentalApi && Object.hasOwn(params, 'excludeTurns')) {
            reply({ error: { code: -32600, message: method + '.excludeTurns requires experimentalApi capability' } });
        } else {
            const threadId = method === 'thread/resume' ? params.threadId : (params.excludeTurns ? 'fork-metadata' : 'fork-full');
            reply({ result: { thread: { id: threadId, modelProvider: params.modelProvider } } });
        }
    } else {
        reply({ error: { code: -32601, message: 'Unknown method' } });
    }
});
"#,
        ]);
        let mut server = CodexAppServer::start_command(command)?;
        let fork = server.fork_thread_for_provider("source-thread", "openai")?;
        assert_eq!(fork.id, "fork-metadata");
        assert_eq!(fork.model_provider.as_deref(), Some("openai"));
        assert_eq!(
            server.fork_thread("source-thread", "custom", false)?.id,
            "fork-full"
        );
        let resumed = server.resume_for_provider("source-thread", "custom")?;
        assert_eq!(resumed.id, "source-thread");
        assert_eq!(resumed.model_provider.as_deref(), Some("custom"));
        Ok(())
    }

    #[test]
    fn parses_official_fork_response() -> AppResult<()> {
        let result = json!({
            "thread": {
                "id": "new-thread",
                "path": r"C:\Users\me\.codex\sessions\rollout-new-thread.jsonl",
                "modelProvider": "custom"
            },
            "modelProvider": "custom"
        });
        let fork = parse_fork_result(&result)?;
        assert_eq!(fork.id, "new-thread");
        assert_eq!(
            fork.path.as_deref(),
            Some(Path::new(
                r"C:\Users\me\.codex\sessions\rollout-new-thread.jsonl"
            ))
        );
        assert_eq!(fork.model_provider.as_deref(), Some("custom"));
        Ok(())
    }
}
