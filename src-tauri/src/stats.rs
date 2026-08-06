use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use chrono::{Datelike, Duration, TimeZone, Timelike, Utc};

use crate::error::{AppError, AppResult};
use crate::models::{Kpi, ModelStat, ProjectStat, SessionSummary, TimeseriesPoint};
use crate::paths;

fn provider_or_codex(provider: Option<String>) -> String {
    provider.unwrap_or_else(|| "codex".to_string())
}

fn claude_root(claude_dir: Option<String>) -> PathBuf {
    PathBuf::from(
        claude_dir.unwrap_or_else(|| paths::default_claude_dir().to_string_lossy().into_owned()),
    )
}

fn opencode_root(opencode_dir: Option<String>) -> PathBuf {
    PathBuf::from(
        opencode_dir
            .unwrap_or_else(|| paths::default_opencode_dir().to_string_lossy().into_owned()),
    )
}

fn load_sessions(
    provider: &str,
    codex_dir: &str,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
) -> AppResult<Vec<SessionSummary>> {
    match provider {
        "codex" => {
            crate::sessions::list_sessions(Some("codex".into()), codex_dir.to_string(), claude_dir)
        }
        "claude" => crate::claude_sessions::scan_sessions(&claude_root(claude_dir)),
        "opencode" => crate::opencode_sessions::list_sessions(&opencode_root(opencode_dir)),
        "all" => {
            let mut out =
                crate::sessions::list_sessions(Some("codex".into()), codex_dir.to_string(), None)?;
            out.extend(crate::claude_sessions::scan_sessions(&claude_root(
                claude_dir,
            ))?);
            // 没装 / 没配 OpenCode 时按"零会话"处理，不让缺少 opencode.db 拖垮整块统计。
            let opencode = opencode_root(opencode_dir);
            if crate::opencode_sessions::database_path(&opencode).is_file() {
                out.extend(crate::opencode_sessions::list_sessions(&opencode)?);
            }
            Ok(out)
        }
        other => Err(AppError::Other(format!("不支持的 provider: {other}"))),
    }
}

fn filter_sessions(
    sessions: Vec<SessionSummary>,
    from_ts: Option<i64>,
    to_ts: Option<i64>,
    cwd_filter: &[String],
    include_archived: bool,
) -> Vec<SessionSummary> {
    let cwd_filter: HashSet<&str> = cwd_filter.iter().map(String::as_str).collect();
    sessions
        .into_iter()
        .filter(|s| include_archived || !s.archived)
        .filter(|s| from_ts.map(|from| s.updated_at >= from).unwrap_or(true))
        .filter(|s| to_ts.map(|to| s.updated_at <= to).unwrap_or(true))
        .filter(|s| cwd_filter.is_empty() || cwd_filter.contains(s.cwd.as_str()))
        .collect()
}

fn filter_family_active_sessions(
    sessions: Vec<SessionSummary>,
    codex_dir: &str,
) -> AppResult<Vec<SessionSummary>> {
    if !sessions.iter().any(|session| session.provider == "codex") {
        return Ok(sessions);
    }
    let store = crate::family::load(&PathBuf::from(codex_dir))?;
    let mut out = Vec::with_capacity(sessions.len());
    for session in sessions {
        if session.provider != "codex" {
            out.push(session);
            continue;
        }
        let Some(family_id) = store.index.get(&session.id) else {
            out.push(session);
            continue;
        };
        let family = store.families.get(family_id).ok_or_else(|| {
            AppError::Other(format!(
                "session_family 反向索引指向不存在的 family: session={} family={}",
                session.id, family_id
            ))
        })?;
        if family.active_id == session.id {
            out.push(session);
        }
    }
    Ok(out)
}

fn stat_sessions(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    from_ts: Option<i64>,
    to_ts: Option<i64>,
    cwd_filter: Vec<String>,
    include_archived: bool,
) -> AppResult<Vec<SessionSummary>> {
    let provider = provider_or_codex(provider);
    let sessions = load_sessions(&provider, &codex_dir, claude_dir, opencode_dir)?;
    let sessions = filter_family_active_sessions(sessions, &codex_dir)?;
    Ok(filter_sessions(
        sessions,
        from_ts,
        to_ts,
        &cwd_filter,
        include_archived,
    ))
}

pub fn stats_kpi(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    from_ts: Option<i64>,
    to_ts: Option<i64>,
    cwd_filter: Vec<String>,
    include_archived: bool,
) -> AppResult<Kpi> {
    let sessions = stat_sessions(
        provider,
        codex_dir,
        claude_dir,
        opencode_dir,
        from_ts,
        to_ts,
        cwd_filter,
        include_archived,
    )?;
    let count = sessions.len() as u32;
    let tokens = sessions.iter().map(|s| s.tokens_used).sum::<i64>();
    let projects = sessions
        .iter()
        .map(|s| s.cwd.as_str())
        .filter(|cwd| !cwd.is_empty())
        .collect::<HashSet<_>>()
        .len() as u32;
    Ok(Kpi {
        sessions_total: count,
        tokens_total: tokens,
        active_projects: projects,
        avg_tokens_per_session: if count == 0 {
            0.0
        } else {
            tokens as f64 / count as f64
        },
    })
}

pub fn stats_timeseries(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    from_ts: Option<i64>,
    to_ts: Option<i64>,
    bucket: String,
    cwd_filter: Vec<String>,
    include_archived: bool,
) -> AppResult<Vec<TimeseriesPoint>> {
    let sessions = stat_sessions(
        provider,
        codex_dir,
        claude_dir,
        opencode_dir,
        from_ts,
        to_ts,
        cwd_filter,
        include_archived,
    )?;
    let mut map: BTreeMap<i64, (u32, i64)> = BTreeMap::new();
    for session in sessions {
        let bucket_start = bucket_start(session.updated_at, &bucket);
        let entry = map.entry(bucket_start).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += session.tokens_used;
    }
    Ok(map
        .into_iter()
        .map(|(bucket_start, (sessions, tokens))| TimeseriesPoint {
            bucket_start,
            sessions,
            tokens,
        })
        .collect())
}

pub fn stats_by_project(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    from_ts: Option<i64>,
    to_ts: Option<i64>,
    limit: usize,
    cwd_filter: Vec<String>,
    include_archived: bool,
) -> AppResult<Vec<ProjectStat>> {
    let sessions = stat_sessions(
        provider,
        codex_dir,
        claude_dir,
        opencode_dir,
        from_ts,
        to_ts,
        cwd_filter,
        include_archived,
    )?;
    let mut map: HashMap<(String, String), ProjectStat> = HashMap::new();
    for session in sessions {
        let key = (session.provider.clone(), session.cwd.clone());
        let entry = map.entry(key).or_insert_with(|| ProjectStat {
            provider: Some(session.provider.clone()),
            cwd: session.cwd.clone(),
            cwd_display: session.cwd_display.clone(),
            sessions: 0,
            tokens: 0,
        });
        entry.sessions += 1;
        entry.tokens += session.tokens_used;
    }
    let mut out = map.into_values().collect::<Vec<_>>();
    out.sort_by(|a, b| {
        b.sessions
            .cmp(&a.sessions)
            .then_with(|| b.tokens.cmp(&a.tokens))
    });
    out.truncate(limit);
    Ok(out)
}

pub fn stats_by_model(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    from_ts: Option<i64>,
    to_ts: Option<i64>,
    cwd_filter: Vec<String>,
    include_archived: bool,
) -> AppResult<Vec<ModelStat>> {
    let sessions = stat_sessions(
        provider,
        codex_dir,
        claude_dir,
        opencode_dir,
        from_ts,
        to_ts,
        cwd_filter,
        include_archived,
    )?;
    let mut map: HashMap<(String, String, Option<String>), ModelStat> = HashMap::new();
    for session in sessions {
        let model = session.model.clone().unwrap_or_default();
        let key = (
            session.provider.clone(),
            model.clone(),
            session.reasoning_effort.clone(),
        );
        let entry = map.entry(key).or_insert_with(|| ModelStat {
            provider: Some(session.provider.clone()),
            model,
            reasoning_effort: session.reasoning_effort.clone(),
            sessions: 0,
            tokens: 0,
        });
        entry.sessions += 1;
        entry.tokens += session.tokens_used;
    }
    let mut out = map.into_values().collect::<Vec<_>>();
    out.sort_by(|a, b| {
        b.sessions
            .cmp(&a.sessions)
            .then_with(|| b.tokens.cmp(&a.tokens))
    });
    Ok(out)
}

pub fn stats_heatmap(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: Option<String>,
    opencode_dir: Option<String>,
    from_ts: Option<i64>,
    to_ts: Option<i64>,
    cwd_filter: Vec<String>,
    include_archived: bool,
) -> AppResult<Vec<Vec<u32>>> {
    let sessions = stat_sessions(
        provider,
        codex_dir,
        claude_dir,
        opencode_dir,
        from_ts,
        to_ts,
        cwd_filter,
        include_archived,
    )?;
    let mut grid = vec![vec![0u32; 24]; 7];
    for session in sessions {
        if let Some(dt) = Utc.timestamp_opt(session.updated_at, 0).single() {
            let d = dt.weekday().num_days_from_sunday() as usize;
            let h = dt.hour() as usize;
            if d < 7 && h < 24 {
                grid[d][h] += 1;
            }
        }
    }
    Ok(grid)
}

fn bucket_start(ts: i64, bucket: &str) -> i64 {
    let Some(dt) = Utc.timestamp_opt(ts, 0).single() else {
        return 0;
    };
    let date = if bucket == "week" {
        dt.date_naive() - Duration::days(dt.weekday().num_days_from_monday() as i64)
    } else {
        dt.date_naive()
    };
    date.and_hms_opt(0, 0, 0)
        .map(|naive| Utc.from_utc_datetime(&naive).timestamp())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{BranchStatus, Family, FamilyBranch, FamilyStore};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ))
    }

    fn create_codex_session(codex: &Path) -> AppResult<()> {
        fs::create_dir_all(codex.join("sessions"))?;
        let rollout = codex.join("sessions").join("rollout-codex-1.jsonl");
        fs::write(&rollout, "{}\n")?;
        let conn = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        conn.execute(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT,
                cwd TEXT,
                title TEXT,
                first_user_message TEXT,
                model TEXT,
                reasoning_effort TEXT,
                tokens_used INTEGER,
                created_at INTEGER,
                updated_at INTEGER,
                archived INTEGER,
                git_branch TEXT,
                source TEXT,
                agent_nickname TEXT,
                agent_role TEXT
            )",
            [],
        )?;
        conn.execute(
            "INSERT INTO threads (
                id, rollout_path, cwd, title, first_user_message, model, reasoning_effort,
                tokens_used, created_at, updated_at, archived, git_branch, source,
                agent_nickname, agent_role
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 11, 1770000000, 1770000300, 0, NULL, NULL, NULL, NULL)",
            (
                "codex-1",
                rollout.to_string_lossy().into_owned(),
                "F:\\work\\codex-project",
                "Codex title",
                "hello codex",
                "gpt-5",
            ),
        )?;
        Ok(())
    }

    fn create_claude_session(claude: &Path) -> AppResult<()> {
        let dir = claude.join("projects").join("claude-project");
        fs::create_dir_all(&dir)?;
        let rows = [
            serde_json::json!({
                "sessionId": "claude-1",
                "cwd": "F:\\work\\claude-project",
                "timestamp": "2026-02-06T00:00:00Z",
                "type": "user",
                "message": {"role": "user", "content": "hello claude"}
            }),
            serde_json::json!({
                "sessionId": "claude-1",
                "cwd": "F:\\work\\claude-project",
                "timestamp": "2026-02-06T00:05:00Z",
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "model": "claude-3-5-sonnet",
                    "usage": {"input_tokens": 3, "output_tokens": 4},
                    "content": "answer"
                }
            }),
        ];
        let mut content = String::new();
        for row in rows {
            content.push_str(&serde_json::to_string(&row)?);
            content.push('\n');
        }
        fs::write(dir.join("claude-1.jsonl"), content)?;
        Ok(())
    }

    fn create_opencode_session(opencode: &Path) -> AppResult<()> {
        fs::create_dir_all(opencode)?;
        let conn = rusqlite::Connection::open(crate::opencode_sessions::database_path(opencode))?;
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
        )?;
        conn.execute(
            "INSERT INTO session VALUES ('ses_stats', 'global', NULL, 'slug', 'F:\\work\\opencode-project', 'OpenCode title', '1.0', 1770000000000, 1770000900000, NULL)",
            [],
        )?;
        conn.execute(
            "INSERT INTO message VALUES ('msg_stats', 'ses_stats', 1770000000000, 1770000000000, ?1)",
            [serde_json::json!({"role":"assistant","modelID":"opencode-model","tokens":{"total":23}})
                .to_string()],
        )?;
        Ok(())
    }

    #[test]
    fn aggregates_codex_claude_and_opencode_stats() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-stats-test");
        let codex = root.join("codex");
        let claude = root.join("claude");
        let opencode = root.join("opencode");
        create_codex_session(&codex)?;
        create_claude_session(&claude)?;
        create_opencode_session(&opencode)?;

        let kpi = stats_kpi(
            Some("all".to_string()),
            codex.to_string_lossy().into_owned(),
            Some(claude.to_string_lossy().into_owned()),
            Some(opencode.to_string_lossy().into_owned()),
            None,
            None,
            Vec::new(),
            false,
        )?;
        assert_eq!(kpi.sessions_total, 3);
        assert_eq!(kpi.tokens_total, 41);
        assert_eq!(kpi.active_projects, 3);

        let projects = stats_by_project(
            Some("all".to_string()),
            codex.to_string_lossy().into_owned(),
            Some(claude.to_string_lossy().into_owned()),
            Some(opencode.to_string_lossy().into_owned()),
            None,
            None,
            10,
            Vec::new(),
            false,
        )?;
        assert!(projects
            .iter()
            .any(|p| p.provider.as_deref() == Some("codex")));
        assert!(projects
            .iter()
            .any(|p| p.provider.as_deref() == Some("claude")));
        assert!(projects
            .iter()
            .any(|p| p.provider.as_deref() == Some("opencode")));

        let models = stats_by_model(
            Some("all".to_string()),
            codex.to_string_lossy().into_owned(),
            Some(claude.to_string_lossy().into_owned()),
            Some(opencode.to_string_lossy().into_owned()),
            None,
            None,
            Vec::new(),
            false,
        )?;
        assert!(models
            .iter()
            .any(|m| m.provider.as_deref() == Some("codex")));
        assert!(models
            .iter()
            .any(|m| m.provider.as_deref() == Some("claude")));
        assert!(models
            .iter()
            .any(|m| m.model == "opencode-model" && m.tokens == 23));

        // 只看 OpenCode 时不应混入其他 Agent 的会话。
        let opencode_only = stats_kpi(
            Some("opencode".to_string()),
            codex.to_string_lossy().into_owned(),
            Some(claude.to_string_lossy().into_owned()),
            Some(opencode.to_string_lossy().into_owned()),
            None,
            None,
            Vec::new(),
            false,
        )?;
        assert_eq!(opencode_only.sessions_total, 1);
        assert_eq!(opencode_only.tokens_total, 23);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn all_provider_stats_survive_missing_opencode_database() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-stats-no-opencode");
        let codex = root.join("codex");
        let claude = root.join("claude");
        create_codex_session(&codex)?;
        create_claude_session(&claude)?;

        let kpi = stats_kpi(
            Some("all".to_string()),
            codex.to_string_lossy().into_owned(),
            Some(claude.to_string_lossy().into_owned()),
            Some(root.join("opencode").to_string_lossy().into_owned()),
            None,
            None,
            Vec::new(),
            false,
        )?;
        assert_eq!(kpi.sessions_total, 2);
        assert_eq!(kpi.tokens_total, 18);

        fs::remove_dir_all(root).ok();
        Ok(())
    }

    #[test]
    fn stats_count_only_active_branch_of_scatter_family() -> AppResult<()> {
        let root = temp_dir("cc-session-manager-stats-family-test");
        let codex = root.join("codex");
        create_codex_session(&codex)?;
        let second_rollout = codex.join("sessions").join("rollout-codex-2.jsonl");
        fs::write(&second_rollout, "{}\n")?;
        let conn = rusqlite::Connection::open(codex.join("state_5.sqlite"))?;
        conn.execute(
            "INSERT INTO threads (
                id, rollout_path, cwd, title, first_user_message, model, reasoning_effort,
                tokens_used, created_at, updated_at, archived, git_branch, source,
                agent_nickname, agent_role
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 17, 1770000000, 1770000600, 0, NULL, 'cli', NULL, NULL)",
            (
                "codex-2",
                second_rollout.to_string_lossy().into_owned(),
                "F:\\work\\codex-project",
                "Codex clone",
                "hello codex",
                "gpt-5",
            ),
        )?;
        drop(conn);

        let family = Family {
            family_id: "codex-1".to_string(),
            root_id: "codex-1".to_string(),
            title: "Codex title".to_string(),
            chain: vec![
                FamilyBranch {
                    id: "codex-1".to_string(),
                    provider: "custom".to_string(),
                    created_at: "2026-02-06T00:00:00Z".to_string(),
                    status: BranchStatus::Archived,
                    rollout_relpath: "sessions/rollout-codex-1.jsonl".to_string(),
                    sha256: None,
                    line_count: None,
                    note: None,
                },
                FamilyBranch {
                    id: "codex-2".to_string(),
                    provider: "openai".to_string(),
                    created_at: "2026-02-06T00:00:00Z".to_string(),
                    status: BranchStatus::Active,
                    rollout_relpath: "sessions/rollout-codex-2.jsonl".to_string(),
                    sha256: None,
                    line_count: None,
                    note: None,
                },
            ],
            active_id: "codex-2".to_string(),
            updated_at: "2026-02-06T00:10:00Z".to_string(),
        };
        let mut families = BTreeMap::new();
        families.insert("codex-1".to_string(), family);
        let mut index = BTreeMap::new();
        index.insert("codex-1".to_string(), "codex-1".to_string());
        index.insert("codex-2".to_string(), "codex-1".to_string());
        crate::family::save(
            &codex,
            &FamilyStore {
                version: 1,
                families,
                index,
            },
        )?;

        for include_archived in [false, true] {
            let kpi = stats_kpi(
                Some("codex".to_string()),
                codex.to_string_lossy().into_owned(),
                None,
                None,
                None,
                None,
                Vec::new(),
                include_archived,
            )?;
            assert_eq!(kpi.sessions_total, 1);
            assert_eq!(kpi.tokens_total, 17);
        }

        fs::remove_dir_all(root).ok();
        Ok(())
    }
}
