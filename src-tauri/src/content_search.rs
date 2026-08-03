//! Explicit, in-process conversation content search.
//!
//! Data flow:
//! 1. The UI starts one job with an explicit provider and visibility scope.
//! 2. A single worker streams matching rollout files without creating an index.
//! 3. Existing preview classifiers decide which JSONL rows are real conversation messages.
//! 4. The UI polls a bounded status snapshot and may cancel the active job.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::models::{
    ContentSearchMatch, ContentSearchResult, ContentSearchStart, ContentSearchStatus,
    SessionSummary,
};
use crate::{rollout, sessions};

const MAX_MATCHING_SESSIONS: usize = 100;
const MAX_MATCHES_PER_SESSION: usize = 3;
const MAX_QUERY_CHARS: usize = 256;
const PROGRESS_STEP_BYTES: u64 = 4 * 1024 * 1024;
const SNIPPET_BEFORE_CHARS: usize = 72;
const SNIPPET_AFTER_CHARS: usize = 160;

#[derive(Clone)]
struct SearchJob {
    id: u64,
    cancel: Arc<AtomicBool>,
    status: Arc<Mutex<ContentSearchStatus>>,
}

struct SearchManager {
    next_id: AtomicU64,
    active: Mutex<Option<SearchJob>>,
}

struct SearchRequest {
    provider: String,
    codex_dir: String,
    claude_dir: String,
    query: String,
    rollout_paths: Vec<String>,
}

struct FileScanOutcome {
    matches: Vec<ContentSearchMatch>,
    bytes_read: u64,
    cancelled: bool,
    missing: bool,
}

impl SearchManager {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            active: Mutex::new(None),
        }
    }

    fn start(&self, request: SearchRequest) -> AppResult<ContentSearchStart> {
        validate_request(&request)?;

        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(job) = active.as_ref() {
            let status = job.status.lock().unwrap_or_else(|error| error.into_inner());
            if status.state == "running" {
                return Err(AppError::Other(
                    "已有全文搜索正在运行，请先停止当前搜索".to_string(),
                ));
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let job = SearchJob {
            id,
            cancel: Arc::new(AtomicBool::new(false)),
            status: Arc::new(Mutex::new(ContentSearchStatus {
                job_id: id,
                state: "running".to_string(),
                query: request.query.clone(),
                scanned_files: 0,
                total_files: 0,
                skipped_files: 0,
                scanned_bytes: 0,
                total_bytes: 0,
                results: Vec::new(),
                truncated: false,
                error: None,
            })),
        };
        let worker_job = job.clone();
        thread::Builder::new()
            .name("cc-sessions-content-search".to_string())
            .spawn(move || run_job(worker_job, request))
            .map_err(|error| AppError::Other(format!("无法启动全文搜索任务: {error}")))?;
        *active = Some(job);
        Ok(ContentSearchStart { job_id: id })
    }

    fn status(&self, job_id: u64) -> AppResult<ContentSearchStatus> {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let job = active
            .as_ref()
            .filter(|job| job.id == job_id)
            .ok_or_else(|| AppError::NotFound(format!("全文搜索任务 {job_id}")))?;
        let snapshot = job
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        Ok(snapshot)
    }

    fn active(&self) -> Option<ContentSearchStart> {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let job = active.as_ref()?;
        let status = job.status.lock().unwrap_or_else(|error| error.into_inner());
        (status.state == "running").then_some(ContentSearchStart { job_id: job.id })
    }

    fn cancel(&self, job_id: u64) -> AppResult<()> {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let job = active
            .as_ref()
            .filter(|job| job.id == job_id)
            .ok_or_else(|| AppError::NotFound(format!("全文搜索任务 {job_id}")))?;
        let status = job.status.lock().unwrap_or_else(|error| error.into_inner());
        if status.state == "running" {
            job.cancel.store(true, Ordering::Release);
        }
        Ok(())
    }
}

fn manager() -> &'static SearchManager {
    static MANAGER: OnceLock<SearchManager> = OnceLock::new();
    MANAGER.get_or_init(SearchManager::new)
}

pub fn start_content_search(
    provider: String,
    codex_dir: String,
    claude_dir: String,
    query: String,
    rollout_paths: Vec<String>,
) -> AppResult<ContentSearchStart> {
    manager().start(SearchRequest {
        provider,
        codex_dir,
        claude_dir,
        query: query.trim().to_string(),
        rollout_paths,
    })
}

pub fn content_search_status(job_id: u64) -> AppResult<ContentSearchStatus> {
    manager().status(job_id)
}

pub fn active_content_search() -> AppResult<Option<ContentSearchStart>> {
    Ok(manager().active())
}

pub fn cancel_content_search(job_id: u64) -> AppResult<()> {
    manager().cancel(job_id)
}

fn validate_request(request: &SearchRequest) -> AppResult<()> {
    if !matches!(request.provider.as_str(), "codex" | "claude") {
        return Err(AppError::Other(format!(
            "不支持的 provider: {}",
            request.provider
        )));
    }
    if request.query.chars().count() < 2 {
        return Err(AppError::Other(
            "全文搜索关键词至少需要 2 个字符".to_string(),
        ));
    }
    if request.query.chars().count() > MAX_QUERY_CHARS {
        return Err(AppError::Other(format!(
            "全文搜索关键词不能超过 {MAX_QUERY_CHARS} 个字符"
        )));
    }
    if request.codex_dir.trim().is_empty() {
        return Err(AppError::Other("Codex 目录不能为空".to_string()));
    }
    if request.claude_dir.trim().is_empty() {
        return Err(AppError::Other("Claude 目录不能为空".to_string()));
    }
    if request.query.chars().any(char::is_control) {
        return Err(AppError::Other(
            "全文搜索关键词不能包含控制字符".to_string(),
        ));
    }
    Ok(())
}

fn run_job(job: SearchJob, request: SearchRequest) {
    let result = execute_search(&job, &request);
    let mut status = job.status.lock().unwrap_or_else(|error| error.into_inner());
    match result {
        Ok(()) if job.cancel.load(Ordering::Acquire) => {
            status.state = "cancelled".to_string();
        }
        Ok(()) => {
            status.state = "completed".to_string();
        }
        Err(AppError::Cancelled) => {
            status.state = "cancelled".to_string();
        }
        Err(error) => {
            status.state = "failed".to_string();
            status.error = Some(error.to_string());
        }
    }
}

fn execute_search(job: &SearchJob, request: &SearchRequest) -> AppResult<()> {
    let rollout_paths = request
        .rollout_paths
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let sessions = sessions::list_sessions_cancellable(
        Some(request.provider.clone()),
        request.codex_dir.clone(),
        Some(request.claude_dir.clone()),
        &job.cancel,
    )?
    .into_iter()
    .filter(|session| session_matches_scope(session, &rollout_paths))
    .collect::<Vec<_>>();

    if job.cancel.load(Ordering::Acquire) {
        return Ok(());
    }

    let total_bytes = sessions.iter().map(|session| session.rollout_bytes).sum();
    {
        let mut status = job.status.lock().unwrap_or_else(|error| error.into_inner());
        status.total_files = sessions.len();
        status.total_bytes = total_bytes;
    }

    let mut completed_bytes = 0u64;
    for session in sessions {
        if job.cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        let outcome = scan_session(job, &session, &request.query, completed_bytes)?;
        if outcome.cancelled {
            return Ok(());
        }
        completed_bytes = completed_bytes.saturating_add(if outcome.missing {
            session.rollout_bytes
        } else {
            outcome.bytes_read
        });

        let mut status = job.status.lock().unwrap_or_else(|error| error.into_inner());
        status.scanned_files += 1;
        status.scanned_bytes = completed_bytes.min(status.total_bytes);
        if outcome.missing {
            status.skipped_files += 1;
            continue;
        }
        if !outcome.matches.is_empty() {
            status.results.push(ContentSearchResult {
                session,
                matches: outcome.matches,
            });
            if status.results.len() >= MAX_MATCHING_SESSIONS {
                status.truncated = status.scanned_files < status.total_files;
                return Ok(());
            }
        }
    }
    Ok(())
}

fn session_matches_scope(session: &SessionSummary, rollout_paths: &HashSet<&str>) -> bool {
    rollout_paths.contains(session.rollout_path.as_str())
}

fn scan_session(
    job: &SearchJob,
    session: &SessionSummary,
    query: &str,
    completed_bytes: u64,
) -> AppResult<FileScanOutcome> {
    let file = match File::open(&session.rollout_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileScanOutcome {
                matches: Vec::new(),
                bytes_read: 0,
                cancelled: false,
                missing: true,
            });
        }
        Err(error) => return Err(error.into()),
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut line_index = 0usize;
    let mut event_offset = 0usize;
    let mut bytes_read = 0u64;
    let mut reported_bytes = 0u64;
    let mut matches = Vec::new();
    let escaped_query = json_string_content(query);

    loop {
        if job.cancel.load(Ordering::Acquire) {
            return Ok(FileScanOutcome {
                matches,
                bytes_read,
                cancelled: true,
                missing: false,
            });
        }
        line.clear();
        let count = reader.read_line(&mut line)?;
        if count == 0 {
            break;
        }
        bytes_read = bytes_read.saturating_add(count as u64);
        if bytes_read.saturating_sub(reported_bytes) >= PROGRESS_STEP_BYTES {
            reported_bytes = bytes_read;
            let mut status = job.status.lock().unwrap_or_else(|error| error.into_inner());
            status.scanned_bytes = completed_bytes
                .saturating_add(bytes_read)
                .min(status.total_bytes);
        }

        let current_line_index = line_index;
        line_index += 1;
        if line.trim().is_empty() {
            continue;
        }
        let current_offset = event_offset;
        event_offset += 1;
        if matches.len() >= MAX_MATCHES_PER_SESSION
            || !line_might_contain_query(&line, query, &escaped_query)
        {
            continue;
        }

        let Ok(raw) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(event) = classify_event(&session.provider, current_line_index, raw) else {
            continue;
        };
        let mixed_assistant_text = rollout::preview_event_has_assistant_text_tool_use(&event);
        if !rollout::preview_event_is_conversation(&event) && !mixed_assistant_text {
            continue;
        }
        let text = rollout::preview_event_text(&event);
        if find_query(&text, query).is_none() {
            continue;
        }
        matches.push(ContentSearchMatch {
            event_index: event.index,
            event_offset: current_offset,
            timestamp: event.timestamp,
            role: if mixed_assistant_text {
                "assistant".to_string()
            } else {
                event.role
            },
            snippet: make_snippet(&text, query),
        });
    }

    Ok(FileScanOutcome {
        matches,
        bytes_read,
        cancelled: false,
        missing: false,
    })
}

fn classify_event(provider: &str, index: usize, raw: Value) -> Option<crate::models::PreviewEvent> {
    match provider {
        "codex" => Some(rollout::classify_preview(index, raw)),
        "claude" => crate::claude_sessions::classify_preview(index, raw),
        _ => None,
    }
}

fn find_query(text: &str, query: &str) -> Option<usize> {
    if query.is_ascii() {
        text.as_bytes()
            .windows(query.len())
            .position(|window| window.eq_ignore_ascii_case(query.as_bytes()))
    } else {
        text.find(query)
    }
}

fn json_string_content(text: &str) -> String {
    let encoded = serde_json::to_string(text).expect("serializing a string cannot fail");
    encoded[1..encoded.len() - 1].to_string()
}

fn line_might_contain_query(line: &str, query: &str, escaped_query: &str) -> bool {
    find_query(line, query).is_some()
        || (escaped_query != query && find_query(line, escaped_query).is_some())
        // A valid JSON string may encode any character as `\uXXXX`; parse such
        // lines before deciding so the raw-byte prefilter cannot hide content.
        || line.contains("\\u")
}

fn make_snippet(text: &str, query: &str) -> String {
    let Some(position) = find_query(text, query) else {
        return String::new();
    };
    let before = text[..position]
        .chars()
        .rev()
        .take(SNIPPET_BEFORE_CHARS)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    let match_and_after = text[position..]
        .chars()
        .take(query.chars().count() + SNIPPET_AFTER_CHARS)
        .collect::<String>();
    let mut snippet = compact_whitespace(&format!("{before}{match_and_after}"));
    if before.chars().count() == SNIPPET_BEFORE_CHARS {
        snippet.insert_str(0, "...");
    }
    if text[position..].chars().count() > query.chars().count() + SNIPPET_AFTER_CHARS {
        snippet.push_str("...");
    }
    snippet
}

fn compact_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::*;

    fn temp_file(name: &str, lines: &[Value]) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("cc-sessions-search-{name}-{unique}"));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("session.jsonl");
        let body = lines
            .iter()
            .map(|line| serde_json::to_string(line).expect("json"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, format!("{body}\n")).expect("fixture");
        path
    }

    fn session(provider: &str, path: &std::path::Path) -> SessionSummary {
        SessionSummary {
            provider: provider.to_string(),
            id: "search-session".to_string(),
            resume_command: String::new(),
            rollout_path: path.to_string_lossy().into_owned(),
            cwd: "/tmp/project".to_string(),
            cwd_display: "project".to_string(),
            title: "Search session".to_string(),
            first_user_message: "first".to_string(),
            model: None,
            reasoning_effort: None,
            source: None,
            agent_nickname: None,
            agent_role: None,
            conversion_origin: None,
            tokens_used: 0,
            created_at: 0,
            updated_at: 0,
            archived: false,
            git_branch: None,
            rollout_bytes: fs::metadata(path).expect("metadata").len(),
            logs_count: 0,
            has_backup: false,
        }
    }

    fn test_job() -> SearchJob {
        SearchJob {
            id: 1,
            cancel: Arc::new(AtomicBool::new(false)),
            status: Arc::new(Mutex::new(ContentSearchStatus {
                job_id: 1,
                state: "running".to_string(),
                query: "needle".to_string(),
                scanned_files: 0,
                total_files: 1,
                skipped_files: 0,
                scanned_bytes: 0,
                total_bytes: u64::MAX,
                results: Vec::new(),
                truncated: false,
                error: None,
            })),
        }
    }

    #[test]
    fn searches_later_codex_conversation_messages_only() {
        let path = temp_file(
            "codex",
            &[
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"text":"first prompt"}]}}),
                json!({"type":"event_msg","payload":{"type":"user_message","message":"later needle prompt"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"text":"later needle prompt"}]}}),
                json!({"type":"response_item","payload":{"type":"function_call_output","output":"needle in tool output"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"text":"later needle answer"}]}}),
            ],
        );
        let result =
            scan_session(&test_job(), &session("codex", &path), "needle", 0).expect("search");
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[0].role, "user");
        assert_eq!(result.matches[0].event_index, 2);
        assert_eq!(result.matches[1].role, "assistant");
        assert_eq!(result.matches[1].event_index, 4);
        fs::remove_dir_all(path.parent().expect("parent")).ok();
    }

    #[test]
    fn searches_later_claude_user_messages() {
        let path = temp_file(
            "claude",
            &[
                json!({"type":"user","message":{"role":"user","content":"first prompt"}}),
                json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"needle reasoning"}]}}),
                json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"needle_tool","input":{"query":"needle"}}]}}),
                json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"assistant needle answer"},{"type":"tool_use","name":"Search","input":{}}]}}),
                json!({"type":"user","message":{"role":"user","content":"later needle prompt"}}),
            ],
        );
        let result =
            scan_session(&test_job(), &session("claude", &path), "needle", 0).expect("search");
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[0].role, "assistant");
        assert_eq!(result.matches[0].event_index, 3);
        assert_eq!(result.matches[1].role, "user");
        assert_eq!(result.matches[1].event_index, 4);
        fs::remove_dir_all(path.parent().expect("parent")).ok();
    }

    #[test]
    fn searches_text_that_is_escaped_in_json() {
        let path = temp_file(
            "escaped",
            &[
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"text":"say \"needle\" here"}]}}),
            ],
        );
        let result =
            scan_session(&test_job(), &session("codex", &path), "\"needle\"", 0).expect("search");
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].snippet, "say \"needle\" here");
        fs::remove_dir_all(path.parent().expect("parent")).ok();
    }

    #[test]
    fn searches_unicode_escaped_json_content() {
        let path = temp_file("unicode-escaped", &[]);
        fs::write(
            &path,
            concat!(
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"text":"\u4f60\u597d"}]}}"#,
                "\n"
            ),
        )
        .expect("fixture");

        let result =
            scan_session(&test_job(), &session("codex", &path), "你好", 0).expect("search");
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].snippet, "你好");
        fs::remove_dir_all(path.parent().expect("parent")).ok();
    }

    #[test]
    fn requested_rollout_paths_are_the_authoritative_backend_scope() {
        let included_path = temp_file("scope-included", &[json!({"type":"session_meta"})]);
        let excluded_path = temp_file("scope-excluded", &[json!({"type":"session_meta"})]);
        let mut included = session("codex", &included_path);
        included.archived = true;
        included.agent_role = Some("worker".to_string());
        let excluded = session("codex", &excluded_path);
        let rollout_paths = HashSet::from([included.rollout_path.as_str()]);

        assert!(session_matches_scope(&included, &rollout_paths));
        assert!(!session_matches_scope(&excluded, &rollout_paths));
        fs::remove_dir_all(included_path.parent().expect("parent")).ok();
        fs::remove_dir_all(excluded_path.parent().expect("parent")).ok();
    }

    #[test]
    fn marks_missing_rollout_without_failing_the_search() {
        let path = temp_file("missing", &[json!({"type":"session_meta"})]);
        let missing_session = session("codex", &path);
        fs::remove_dir_all(path.parent().expect("parent")).expect("remove fixture");

        let outcome = scan_session(&test_job(), &missing_session, "needle", 0)
            .expect("missing rollout should be a counted search outcome");

        assert!(outcome.missing);
        assert!(!outcome.cancelled);
        assert_eq!(outcome.bytes_read, 0);
        assert!(outcome.matches.is_empty());
    }

    #[test]
    fn cancellation_stops_before_claude_discovery_reads_session_files() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("cc-sessions-cancel-{unique}"));
        let claude_dir = root.join("claude");
        let project_dir = claude_dir.join("projects").join("project");
        fs::create_dir_all(&project_dir).expect("project dir");
        fs::write(project_dir.join("invalid.jsonl"), [0xff]).expect("invalid fixture");

        let job = test_job();
        job.cancel.store(true, Ordering::Release);
        let request = SearchRequest {
            provider: "claude".to_string(),
            codex_dir: root.join("codex").to_string_lossy().into_owned(),
            claude_dir: claude_dir.to_string_lossy().into_owned(),
            query: "needle".to_string(),
            rollout_paths: Vec::new(),
        };

        let result = execute_search(&job, &request);
        fs::remove_dir_all(root).ok();

        assert!(matches!(result, Err(AppError::Cancelled)));
    }

    #[test]
    fn active_search_is_discoverable_only_while_running() {
        let manager = SearchManager::new();
        let job = test_job();
        *manager
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(job.clone());

        assert_eq!(manager.active().expect("running job").job_id, job.id);
        job.status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .state = "completed".to_string();
        assert!(manager.active().is_none());
    }
}
