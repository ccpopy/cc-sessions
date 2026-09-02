use std::ffi::OsString;
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
        let executable = resolve_codex_executable();
        let mut command = Command::new(&executable);
        command
            .args(["app-server", "--stdio"])
            .env("CODEX_HOME", codex_dir)
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
                "无法启动 Codex app-server（{}）：{error}；请确认已安装当前 Codex App 或 CLI",
                executable.to_string_lossy()
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
                "capabilities": {}
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
    let thread = result
        .get("thread")
        .and_then(Value::as_object)
        .ok_or_else(|| AppError::Other("Codex thread/fork 响应缺少 thread".into()))?;
    let id = thread
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| AppError::Other("Codex thread/fork 响应缺少新会话 ID".into()))?;
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

fn resolve_codex_executable() -> PathBuf {
    let executable_name = if cfg!(windows) {
        OsString::from("codex.exe")
    } else {
        OsString::from("codex")
    };
    if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
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
