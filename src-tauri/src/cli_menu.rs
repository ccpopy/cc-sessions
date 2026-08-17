use std::collections::{BTreeSet, HashMap};
use std::io::{self, Write};

use cc_session_manager_lib::models::{
    BackupSummary, BundleExportTarget, BundleListItem, ConvertReport, DeleteTarget, ExportReport,
    ImportMode, PreviewEvent, ProjectGroup, SessionSummary, SwitchStrategy,
};
use cc_session_manager_lib::{
    backup, bundle, convert, family, paths, repair, rollout, sessions, settings, stats,
};

type MenuResult<T> = Result<T, String>;

const PAGE_SIZE: usize = 15;
const DESKTOP_DELETE_RESTART_NOTICE: &str =
    "重启 Codex/ChatGPT 桌面应用后，已删除的会话才会从列表中消失；请勿继续在已删除的会话中对话。";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Back,
    Main,
    Exit,
}

enum SessionActionResult {
    Back,
    Refresh,
    Main,
    Exit,
}

#[derive(Debug, Clone, Copy)]
enum PreviewMode {
    Conversation,
    ConversationAndReasoning,
    All,
}

#[derive(Debug, Clone, Copy)]
enum PreviewFormat {
    Text,
    Summary,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionScope {
    Main,
    Subagent,
    All,
}

struct MenuContext {
    provider: Option<String>,
    codex_dir: String,
    claude_dir: String,
    opencode_dir: String,
    family_lock: family::FamilyLock,
}

pub fn run(
    provider: Option<String>,
    codex_dir: String,
    claude_dir: String,
    opencode_dir: String,
) -> MenuResult<()> {
    let mut ctx = MenuContext {
        provider,
        codex_dir,
        claude_dir,
        opencode_dir,
        family_lock: family::FamilyLock::default(),
    };

    loop {
        match main_menu(&mut ctx)? {
            Flow::Exit => return Ok(()),
            Flow::Back | Flow::Main => {}
        }
    }
}

fn main_menu(ctx: &mut MenuContext) -> MenuResult<Flow> {
    print_header(
        "CC Sessions",
        &[
            ("Codex 目录", ctx.codex_dir.as_str()),
            ("Claude 目录", ctx.claude_dir.as_str()),
            ("OpenCode 目录", ctx.opencode_dir.as_str()),
        ],
    );
    println!("1. Codex 会话");
    println!("2. Claude 会话");
    println!("3. OpenCode 会话");
    println!("4. 统计");
    println!("5. 备份");
    println!("6. 导入 / 导出 Bundle");
    println!("7. 修复 / 诊断");
    println!("8. 设置与路径检查");
    println!("9. 帮助");
    println!("0. 退出");

    match prompt("请选择: ")?.as_str() {
        "1" => run_child(|| sessions_menu(ctx, "codex")),
        "2" => run_child(|| sessions_menu(ctx, "claude")),
        "3" => run_child(|| sessions_menu(ctx, "opencode")),
        "4" => run_child(|| stats_menu(ctx)),
        "5" => run_child(|| backup_menu(ctx)),
        "6" => run_child(|| bundle_menu(ctx)),
        "7" => run_child(|| repair_menu(ctx)),
        "8" => run_child(|| settings_menu(ctx)),
        "9" => {
            show_interactive_help()?;
            Ok(Flow::Main)
        }
        "0" => Ok(Flow::Exit),
        _ => {
            println!("无效选择。");
            pause()
        }
    }
}

fn run_child(action: impl FnOnce() -> MenuResult<Flow>) -> MenuResult<Flow> {
    match action() {
        Ok(Flow::Exit) => Ok(Flow::Exit),
        Ok(Flow::Back | Flow::Main) => Ok(Flow::Main),
        Err(err) => {
            println!("操作失败: {err}");
            pause()
        }
    }
}

fn sessions_menu(ctx: &mut MenuContext, provider: &str) -> MenuResult<Flow> {
    loop {
        print_header(&format!("{} 会话", provider_label(provider)), &[]);
        println!("1. 查看会话列表");
        println!("2. 搜索会话");
        println!("3. 按项目查看");
        println!("4. 按大小查看（token 从小到大）");
        println!("5. 输入 session id 操作");
        println!("6. 返回上一层");
        println!("7. 返回主菜单");
        println!("0. 退出");

        match prompt("请选择: ")?.as_str() {
            "1" => {
                let include_archived = confirm_default_no("是否包含已归档会话？")?;
                let scope = prompt_session_scope(provider)?;
                let sessions = load_sessions(ctx, provider, include_archived, scope)?;
                match browse_sessions(ctx, provider, sessions, "会话列表")? {
                    Flow::Back => {}
                    other => return Ok(other),
                }
            }
            "2" => {
                let query = prompt_required("请输入搜索关键词: ")?;
                let include_archived = confirm_default_no("是否包含已归档会话？")?;
                let scope = prompt_session_scope(provider)?;
                let mut hits = sessions::search_sessions_with_opencode(
                    Some(provider.to_string()),
                    ctx.codex_dir.clone(),
                    Some(ctx.claude_dir.clone()),
                    Some(ctx.opencode_dir.clone()),
                    query,
                )
                .map_err(to_string)?;
                if !include_archived {
                    hits.retain(|session| !session.archived);
                }
                retain_session_scope(&mut hits, scope);
                match browse_sessions(ctx, provider, hits, "搜索结果")? {
                    Flow::Back => {}
                    other => return Ok(other),
                }
            }
            "3" => match projects_menu(ctx, provider)? {
                Flow::Back => {}
                other => return Ok(other),
            },
            "4" => {
                let include_archived = confirm_default_no("是否包含已归档会话？")?;
                let scope = prompt_session_scope(provider)?;
                let mut sessions = load_sessions(ctx, provider, include_archived, scope)?;
                sort_sessions_by_size(&mut sessions);
                match browse_sessions(ctx, provider, sessions, "按大小：token 从小到大")? {
                    Flow::Back => {}
                    other => return Ok(other),
                }
            }
            "5" => {
                let id = prompt_required("请输入 session id 或前缀: ")?;
                let sessions = load_sessions(ctx, provider, true, SessionScope::All)?;
                match find_session(&sessions, &id) {
                    Ok(session) => match session_action_menu(ctx, provider, session)? {
                        SessionActionResult::Back | SessionActionResult::Refresh => {}
                        SessionActionResult::Main => return Ok(Flow::Main),
                        SessionActionResult::Exit => return Ok(Flow::Exit),
                    },
                    Err(err) => {
                        println!("{err}");
                        pause()?;
                    }
                }
            }
            "6" => return Ok(Flow::Back),
            "7" => return Ok(Flow::Main),
            "0" => return Ok(Flow::Exit),
            _ => {
                println!("无效选择。");
                pause()?;
            }
        }
    }
}

fn projects_menu(ctx: &mut MenuContext, provider: &str) -> MenuResult<Flow> {
    let include_archived = confirm_default_no("是否包含已归档会话？")?;
    let scope = prompt_session_scope(provider)?;
    let sessions = load_sessions(ctx, provider, include_archived, scope)?;
    let projects = group_projects(sessions);
    if projects.is_empty() {
        println!("没有可显示的项目。");
        return pause();
    }

    loop {
        print_header(&format!("{} 项目", provider_label(provider)), &[]);
        for (index, project) in projects.iter().enumerate() {
            println!(
                "{:>2}. {:>4} 会话  {:>10} tokens  {}",
                index + 1,
                project.sessions.len(),
                project.total_tokens,
                project.cwd
            );
        }
        println!("b. 返回上一层");
        println!("m. 返回主菜单");
        println!("0. 退出");

        let input = prompt("请选择项目: ")?;
        match input.as_str() {
            "b" | "B" => return Ok(Flow::Back),
            "m" | "M" => return Ok(Flow::Main),
            "0" => return Ok(Flow::Exit),
            _ => match parse_index(&input, projects.len()) {
                Some(index) => {
                    let project = &projects[index];
                    match browse_sessions(
                        ctx,
                        provider,
                        project.sessions.clone(),
                        &format!("项目: {}", project.cwd_display),
                    )? {
                        Flow::Back => {}
                        other => return Ok(other),
                    }
                }
                None => {
                    println!("无效选择。");
                    pause()?;
                }
            },
        }
    }
}

fn session_selection_key(session: &SessionSummary) -> String {
    format!("{}\0{}", session.id, session.rollout_path)
}

fn refresh_browsed_sessions(
    ctx: &MenuContext,
    provider: &str,
    current: &[SessionSummary],
) -> MenuResult<Vec<SessionSummary>> {
    let refreshed = sessions::list_sessions_with_opencode(
        Some(provider.to_string()),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
    )
    .map_err(to_string)?;
    let mut refreshed_by_key = refreshed
        .into_iter()
        .map(|session| (session_selection_key(&session), session))
        .collect::<HashMap<_, _>>();
    Ok(current
        .iter()
        .filter_map(|session| refreshed_by_key.remove(&session_selection_key(session)))
        .collect())
}

fn browse_sessions(
    ctx: &mut MenuContext,
    provider: &str,
    mut sessions: Vec<SessionSummary>,
    title: &str,
) -> MenuResult<Flow> {
    if sessions.is_empty() {
        println!("没有匹配的会话。");
        return pause();
    }

    let mut page = 0usize;
    let mut selected_keys = BTreeSet::<String>::new();
    loop {
        if sessions.is_empty() {
            println!("当前列表已没有会话。");
            return pause();
        }
        let available_keys = sessions
            .iter()
            .map(session_selection_key)
            .collect::<BTreeSet<_>>();
        selected_keys.retain(|key| available_keys.contains(key));
        let total_pages = sessions.len().div_ceil(PAGE_SIZE);
        if page >= total_pages {
            page = total_pages.saturating_sub(1);
        }
        let start = page * PAGE_SIZE;
        let end = (start + PAGE_SIZE).min(sessions.len());
        print_header(
            title,
            &[
                (
                    "范围",
                    &format!("{}-{} / {}", start + 1, end, sessions.len()),
                ),
                ("已选", &selected_keys.len().to_string()),
            ],
        );
        for (offset, session) in sessions[start..end].iter().enumerate() {
            let selected = if selected_keys.contains(&session_selection_key(session)) {
                "*"
            } else {
                " "
            };
            println!(
                "{:>2}. [{}] {}  {}  {:>10} token  {}  {}",
                offset + 1,
                selected,
                short_timestamp(session.updated_at),
                if session.archived {
                    "archived"
                } else {
                    "active  "
                },
                session.tokens_used,
                compact(&session.id, 12),
                compact(&session.title, 64)
            );
            println!("    {}", compact(&session.cwd, 90));
        }
        println!("n. 下一页    p. 上一页    s. 选择    u. 取消选择    c. 清空选择");
        println!("d. 删除已选  b. 返回上一层    m. 返回主菜单    0. 退出");
        println!("当前页: {}/{}", page + 1, total_pages);

        let input = prompt("输入序号选择会话: ")?;
        match input.as_str() {
            "n" | "N" => {
                if page + 1 < total_pages {
                    page += 1;
                }
            }
            "p" | "P" => {
                page = page.saturating_sub(1);
            }
            "s" | "S" => {
                let indexes = prompt_page_indexes(end - start, "选择当前页序号")?;
                for index in indexes {
                    selected_keys.insert(session_selection_key(&sessions[start + index]));
                }
            }
            "u" | "U" => {
                let indexes = prompt_page_indexes(end - start, "取消选择当前页序号")?;
                for index in indexes {
                    selected_keys.remove(&session_selection_key(&sessions[start + index]));
                }
            }
            "c" | "C" => {
                selected_keys.clear();
            }
            "d" | "D" => {
                let selected = sessions
                    .iter()
                    .filter(|session| selected_keys.contains(&session_selection_key(session)))
                    .cloned()
                    .collect::<Vec<_>>();
                if delete_selected_sessions(ctx, provider, &selected)? {
                    sessions = refresh_browsed_sessions(ctx, provider, &sessions)?;
                }
            }
            "b" | "B" => return Ok(Flow::Back),
            "m" | "M" => return Ok(Flow::Main),
            "0" => return Ok(Flow::Exit),
            _ => match parse_index(&input, end - start) {
                Some(index) => {
                    let session = sessions[start + index].clone();
                    match session_action_menu(ctx, provider, session)? {
                        SessionActionResult::Back => {}
                        SessionActionResult::Refresh => {
                            sessions = refresh_browsed_sessions(ctx, provider, &sessions)?;
                        }
                        SessionActionResult::Main => return Ok(Flow::Main),
                        SessionActionResult::Exit => return Ok(Flow::Exit),
                    }
                }
                None => {
                    println!("无效选择。");
                    pause()?;
                }
            },
        }
    }
}

fn prompt_page_indexes(page_len: usize, label: &str) -> MenuResult<Vec<usize>> {
    let input = prompt(&format!("{label}，多个可用空格或逗号，支持 1-3: "))?;
    parse_index_list(&input, page_len)
}

fn parse_index_list(input: &str, len: usize) -> MenuResult<Vec<usize>> {
    let mut indexes = BTreeSet::new();
    for part in input.split(|ch: char| ch == ',' || ch.is_whitespace()) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start = parse_one_based_index(start.trim(), len)?;
            let end = parse_one_based_index(end.trim(), len)?;
            let (from, to) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            indexes.extend(from..=to);
        } else {
            indexes.insert(parse_one_based_index(part, len)?);
        }
    }
    if indexes.is_empty() {
        Err("至少需要输入一个当前页序号。".to_string())
    } else {
        Ok(indexes.into_iter().collect())
    }
}

fn parse_one_based_index(input: &str, len: usize) -> MenuResult<usize> {
    let value = input
        .parse::<usize>()
        .map_err(|_| format!("无效序号: {input}"))?;
    if value == 0 || value > len {
        Err(format!("序号超出当前页范围: {value}"))
    } else {
        Ok(value - 1)
    }
}

fn session_action_menu(
    ctx: &mut MenuContext,
    provider: &str,
    session: SessionSummary,
) -> MenuResult<SessionActionResult> {
    if provider == "opencode" {
        return opencode_session_action_menu(ctx, session);
    }
    loop {
        let target_provider = conversion_target_provider(provider)?;
        let conversion_suffix = if sessions::session_is_subagent(&session) {
            "（子代理会话不支持）"
        } else {
            ""
        };
        print_header(
            "会话操作",
            &[
                ("Provider", provider),
                ("ID", session.id.as_str()),
                ("标题", session.title.as_str()),
                ("目录", session.cwd.as_str()),
            ],
        );
        println!("1. 预览会话内容");
        println!("2. 查看元信息");
        println!("3. 显示 resume 命令");
        println!("4. 重命名会话");
        println!("5. 移动会话目录");
        println!("6. 创建备份");
        println!("7. 导出 Bundle");
        println!(
            "8. 转换为 {} 会话{}",
            provider_label(target_provider),
            conversion_suffix
        );
        if provider == "codex" {
            println!("9. 归档 / 取消归档");
        } else {
            println!("9. 归档 / 取消归档（Claude 不支持）");
        }
        println!("10. 删除会话");
        println!("11. 返回上一层");
        println!("12. 返回主菜单");
        println!("0. 退出");

        match prompt("请选择: ")?.as_str() {
            "1" => preview_session(provider, &session)?,
            "2" => show_session_meta(provider, &session)?,
            "3" => show_resume_command(provider, &session)?,
            "4" => {
                rename_session(ctx, provider, &session)?;
                return Ok(SessionActionResult::Refresh);
            }
            "5" => {
                move_session_cwd(ctx, provider, &session)?;
                return Ok(SessionActionResult::Refresh);
            }
            "6" => create_backup_for_session(ctx, provider, &session)?,
            "7" => export_bundle_for_session(ctx, provider, &session)?,
            "8" => convert_session(ctx, provider, &session)?,
            "9" => toggle_archived(ctx, provider, &session)?,
            "10" => {
                delete_session(ctx, provider, &session)?;
                return Ok(SessionActionResult::Refresh);
            }
            "11" => return Ok(SessionActionResult::Back),
            "12" => return Ok(SessionActionResult::Main),
            "0" => return Ok(SessionActionResult::Exit),
            _ => {
                println!("无效选择。");
                pause()?;
            }
        }
    }
}

fn opencode_session_action_menu(
    ctx: &mut MenuContext,
    session: SessionSummary,
) -> MenuResult<SessionActionResult> {
    loop {
        print_header(
            "OpenCode 会话操作",
            &[
                ("ID", session.id.as_str()),
                ("标题", session.title.as_str()),
                ("目录", session.cwd.as_str()),
            ],
        );
        println!("1. 预览会话内容");
        println!("2. 查看元信息");
        println!("3. 显示 resume 命令");
        println!("4. 重命名会话");
        println!("5. 移动会话目录");
        println!("6. 创建备份");
        println!("7. 导出 Bundle");
        println!("8. 归档 / 取消归档");
        println!("9. 删除会话（包含全部子会话）");
        println!("10. 返回上一层");
        println!("11. 返回主菜单");
        println!("0. 退出");

        match prompt("请选择: ")?.as_str() {
            "1" => preview_session("opencode", &session)?,
            "2" => show_session_meta("opencode", &session)?,
            "3" => show_resume_command("opencode", &session)?,
            "4" => {
                rename_session(ctx, "opencode", &session)?;
                return Ok(SessionActionResult::Refresh);
            }
            "5" => {
                move_session_cwd(ctx, "opencode", &session)?;
                return Ok(SessionActionResult::Refresh);
            }
            "6" => create_backup_for_session(ctx, "opencode", &session)?,
            "7" => export_bundle_for_session(ctx, "opencode", &session)?,
            "8" => {
                toggle_archived(ctx, "opencode", &session)?;
                return Ok(SessionActionResult::Refresh);
            }
            "9" => {
                delete_session(ctx, "opencode", &session)?;
                return Ok(SessionActionResult::Refresh);
            }
            "10" => return Ok(SessionActionResult::Back),
            "11" => return Ok(SessionActionResult::Main),
            "0" => return Ok(SessionActionResult::Exit),
            _ => {
                println!("无效选择。");
                pause()?;
            }
        }
    }
}

fn rename_session(ctx: &MenuContext, provider: &str, session: &SessionSummary) -> MenuResult<()> {
    let title = prompt_default("新标题", &session.title)?;
    if title.trim() == session.title.trim() {
        println!("标题未变化。");
        return pause().map(|_| ());
    }
    sessions::rename_session_with_provider_dirs(
        Some(provider.to_string()),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        session.id.clone(),
        Some(session.rollout_path.clone()),
        title,
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    println!("已重命名 {} 会话。", provider_label(provider));
    pause()?;
    Ok(())
}

fn move_session_cwd(ctx: &MenuContext, provider: &str, session: &SessionSummary) -> MenuResult<()> {
    let target_cwd = prompt_required("目标工作目录（必须已存在）: ")?;
    let report = sessions::move_session_cwd_with_provider_dirs(
        Some(provider.to_string()),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        session.id.clone(),
        Some(session.rollout_path.clone()),
        target_cwd,
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    println!("已移动: {} -> {}", report.old_cwd, report.new_cwd);
    if report.threads_updated > 1 {
        println!("同步更新 {} 条会话记录。", report.threads_updated);
    }
    if report.artifacts_moved > 0 {
        println!("移动 {} 项会话资产。", report.artifacts_moved);
    }
    if report.history_rows_updated > 0 {
        println!("更新 history.jsonl {} 行。", report.history_rows_updated);
    }
    if provider == "codex" && !report.desktop_project_synced {
        println!("警告：Core 已移动，但 Codex App 全局状态未初始化，项目归属未同步。");
    }
    if report.requires_project_open {
        println!("提示：首次在目标目录启动 OpenCode 后，会自动登记真实项目 ID。");
    }
    pause()?;
    Ok(())
}

fn preview_session(provider: &str, session: &SessionSummary) -> MenuResult<()> {
    let limit_value = prompt_usize("预览条数，0 表示全部", 40)?;
    let mode = choose_preview_mode()?;
    let format = choose_preview_format()?;
    let limit = if limit_value == 0 {
        None
    } else {
        Some(limit_value)
    };
    let events = collect_preview_events(provider, session, mode, limit)?;
    print_header(
        "会话预览",
        &[
            ("ID", session.id.as_str()),
            ("模式", preview_mode_label(mode)),
            ("显示", preview_format_label(format)),
        ],
    );
    if events.is_empty() {
        println!("没有匹配的预览事件。可选择“全部事件”查看工具调用、工具返回和元数据。");
        pause()?;
        return Ok(());
    }
    print_preview_events(&events, format);
    pause()?;
    Ok(())
}

fn choose_preview_mode() -> MenuResult<PreviewMode> {
    println!("1. 仅对话消息（默认，每轮只显示最终答复）");
    println!("2. 对话消息 + 推理过程");
    println!("3. 全部事件（包含工具调用、工具返回、元数据）");
    match prompt("请选择预览模式 [1]: ")?.as_str() {
        "" | "1" => Ok(PreviewMode::Conversation),
        "2" => Ok(PreviewMode::ConversationAndReasoning),
        "3" => Ok(PreviewMode::All),
        _ => Err("无效预览模式。".to_string()),
    }
}

fn choose_preview_format() -> MenuResult<PreviewFormat> {
    println!("1. 完整正文（默认）");
    println!("2. 一行摘要");
    println!("3. 原始 JSONL");
    match prompt("请选择显示方式 [1]: ")?.as_str() {
        "" | "1" => Ok(PreviewFormat::Text),
        "2" => Ok(PreviewFormat::Summary),
        "3" => Ok(PreviewFormat::Raw),
        _ => Err("无效显示方式。".to_string()),
    }
}

fn collect_preview_events(
    provider: &str,
    session: &SessionSummary,
    mode: PreviewMode,
    limit: Option<usize>,
) -> MenuResult<Vec<PreviewEvent>> {
    let mut offset = 0usize;
    let mut selected = Vec::new();
    let mut conversation_reducer = rollout::ConversationDisplayReducer::default();
    let batch = 100usize;
    loop {
        let next_limit = match limit {
            Some(max) => {
                let remaining = max.saturating_sub(selected.len());
                if remaining == 0 {
                    break;
                }
                remaining.min(batch)
            }
            None => batch,
        };
        let events = rollout::preview_session_range(
            Some(provider.to_string()),
            session.rollout_path.clone(),
            offset,
            next_limit,
        )
        .map_err(to_string)?;
        let fetched = events.len();
        if fetched == 0 {
            break;
        }
        for event in events {
            if matches!(mode, PreviewMode::All) || preview_event_visible(&event, mode) {
                if matches!(mode, PreviewMode::All) {
                    selected.push(event);
                } else {
                    conversation_reducer.push(event, &mut selected);
                }
                if limit.is_some_and(|max| selected.len() >= max) {
                    if let Some(max) = limit {
                        selected.truncate(max);
                    }
                    return Ok(selected);
                }
            }
        }
        offset += fetched;
        if fetched < next_limit {
            break;
        }
    }
    if !matches!(mode, PreviewMode::All) {
        conversation_reducer.finish(&mut selected);
    }
    if let Some(max) = limit {
        selected.truncate(max);
    }
    Ok(selected)
}

fn print_preview_events(events: &[PreviewEvent], format: PreviewFormat) {
    match format {
        PreviewFormat::Summary => {
            for event in events {
                println!(
                    "{:>4}  {:<12} {:<22} {}",
                    event.index,
                    event.role,
                    event.kind,
                    compact(&event.text_summary.replace('\n', " "), 110)
                );
            }
        }
        PreviewFormat::Raw => {
            for event in events {
                println!("{}", serde_json::to_string(&event.raw).unwrap_or_default());
            }
        }
        PreviewFormat::Text => {
            for (pos, event) in events.iter().enumerate() {
                if pos > 0 {
                    println!();
                }
                let timestamp = if event.timestamp.is_empty() {
                    "".to_string()
                } else {
                    format!(" {}", event.timestamp)
                };
                println!(
                    "----- event {} {} / {}{} -----",
                    event.index, event.role, event.kind, timestamp
                );
                let text = rollout::preview_event_text(event);
                if text.trim().is_empty() {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&event.raw).unwrap_or_default()
                    );
                } else {
                    println!("{text}");
                }
            }
        }
    }
}

fn preview_event_visible(event: &PreviewEvent, mode: PreviewMode) -> bool {
    match mode {
        PreviewMode::Conversation => rollout::preview_event_is_conversation(event),
        PreviewMode::ConversationAndReasoning => {
            rollout::preview_event_is_conversation_or_reasoning(event)
        }
        PreviewMode::All => true,
    }
}

fn preview_mode_label(mode: PreviewMode) -> &'static str {
    match mode {
        PreviewMode::Conversation => "仅对话消息",
        PreviewMode::ConversationAndReasoning => "对话消息 + 推理过程",
        PreviewMode::All => "全部事件",
    }
}

fn preview_format_label(format: PreviewFormat) -> &'static str {
    match format {
        PreviewFormat::Text => "完整正文",
        PreviewFormat::Summary => "一行摘要",
        PreviewFormat::Raw => "原始 JSONL",
    }
}

fn show_session_meta(provider: &str, session: &SessionSummary) -> MenuResult<()> {
    let meta =
        rollout::preview_session_meta(Some(provider.to_string()), session.rollout_path.clone())
            .map_err(to_string)?;
    print_header("会话元信息", &[("ID", session.id.as_str())]);
    println!("timestamp      {}", meta.timestamp.as_deref().unwrap_or(""));
    println!("cwd            {}", meta.cwd.as_deref().unwrap_or(""));
    println!(
        "originator     {}",
        meta.originator.as_deref().unwrap_or("")
    );
    println!(
        "cli_version    {}",
        meta.cli_version.as_deref().unwrap_or("")
    );
    println!("source         {}", meta.source.as_deref().unwrap_or(""));
    println!(
        "model_provider {}",
        meta.model_provider.as_deref().unwrap_or("")
    );
    pause()?;
    Ok(())
}

fn show_resume_command(_provider: &str, session: &SessionSummary) -> MenuResult<()> {
    println!("{}", session.resume_command);
    pause()?;
    Ok(())
}

fn create_backup_for_session(
    ctx: &MenuContext,
    provider: &str,
    session: &SessionSummary,
) -> MenuResult<()> {
    let backup_dir = prompt_default("备份目录", &paths::default_backup_dir().to_string_lossy())?;
    let name = prompt_optional("备份名称（留空自动生成）: ")?;
    let note = prompt_optional("备注（可留空）: ")?;
    let summary = backup::create_backup_with_opencode(
        Some(provider.to_string()),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        backup_dir,
        vec![session.id.clone()],
        Some(vec![BundleExportTarget {
            id: session.id.clone(),
            rollout_path: Some(session.rollout_path.clone()),
        }]),
        name,
        note,
    )
    .map_err(to_string)?;
    print_backup_summary(&summary);
    pause()?;
    Ok(())
}

fn export_bundle_for_session(
    ctx: &MenuContext,
    provider: &str,
    session: &SessionSummary,
) -> MenuResult<()> {
    let out_dir = prompt_required("导出目录: ")?;
    let reports = bundle::export_session_bundles_with_opencode(
        Some(provider.to_string()),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        out_dir,
        vec![session.id.clone()],
        Some(vec![BundleExportTarget {
            id: session.id.clone(),
            rollout_path: Some(session.rollout_path.clone()),
        }]),
        None,
        None,
    )
    .map_err(to_string)?;
    print_export_reports(&reports);
    pause()?;
    Ok(())
}

fn convert_session(ctx: &MenuContext, provider: &str, session: &SessionSummary) -> MenuResult<()> {
    if sessions::session_is_subagent(session) {
        println!("子代理会话暂不支持转换，请选择主会话。");
        return pause().map(|_| ());
    }

    let target_provider = conversion_target_provider(provider)?;
    let mode = choose_conversion_mode(provider)?;
    println!(
        "将新建一个 {} 会话，原会话不会修改。",
        provider_label(target_provider)
    );
    if !confirm_default_no("确认开始转换？")? {
        println!("已取消。");
        return pause().map(|_| ());
    }

    let report = convert::convert_session_with_lock(
        ctx.codex_dir.clone(),
        ctx.claude_dir.clone(),
        provider.to_string(),
        session.rollout_path.clone(),
        Some(mode.to_string()),
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    print_convert_report(&report);
    pause()?;
    Ok(())
}

fn choose_conversion_mode(provider: &str) -> MenuResult<&'static str> {
    let target_provider = conversion_target_provider(provider)?;
    println!("1. 简洁续聊（推荐）");
    if provider == "codex" {
        println!("   只保留真实用户消息和每轮最终答复。");
    } else {
        println!("   保留可见对话，工具调用和结果转为文本注记。");
    }
    println!("2. 原生{}（实验）", provider_label(target_provider));
    if provider == "codex" {
        println!("   保留过程回复，完整工具事件转换为 Claude 工具格式。");
    } else {
        println!("   保留过程回复和图片，完整工具事件转换为 Codex 工具格式。");
    }
    conversion_mode_from_choice(&prompt("请选择转换模式 [1]: ")?)
}

fn conversion_mode_from_choice(choice: &str) -> MenuResult<&'static str> {
    match choice {
        "" | "1" => Ok("simple"),
        "2" => Ok("native"),
        _ => Err("无效转换模式。".to_string()),
    }
}

fn conversion_target_provider(provider: &str) -> MenuResult<&'static str> {
    match provider {
        "codex" => Ok("claude"),
        "claude" => Ok("codex"),
        other => Err(format!("不支持的转换来源 provider: {other}")),
    }
}

fn print_convert_report(report: &ConvertReport) {
    println!(
        "已转换: {} -> {} ({})",
        provider_label(&report.source_provider),
        provider_label(&report.target_provider),
        report.conversion_mode.as_deref().unwrap_or("simple")
    );
    println!("新会话 ID: {}", report.new_id);
    println!("会话路径: {}", report.new_path);
    println!("消息数量: {}", report.imported_messages);
    println!("恢复命令: {}", report.resume_command);
    if report.dropped_reasoning > 0 {
        println!("丢弃推理: {} 段", report.dropped_reasoning);
    }
    if report.tool_notes > 0 {
        println!("处理工具事件: {} 条", report.tool_notes);
    }
    for warning in &report.warnings {
        println!("警告: {warning}");
    }
}

fn toggle_archived(ctx: &MenuContext, provider: &str, session: &SessionSummary) -> MenuResult<()> {
    if provider == "claude" {
        println!("Claude 会话不支持归档。");
        return pause().map(|_| ());
    }
    let target = !session.archived;
    let label = if target { "归档" } else { "取消归档" };
    if !confirm_yes(&format!("确认要{label}会话 {} 吗？", session.id))? {
        println!("已取消。");
        return pause().map(|_| ());
    }
    sessions::set_archived_with_provider_dir(
        Some(provider.to_string()),
        ctx.codex_dir.clone(),
        Some(ctx.opencode_dir.clone()),
        session.id.clone(),
        target,
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    println!("已完成: {label}");
    pause()?;
    Ok(())
}

fn delete_session(ctx: &MenuContext, provider: &str, session: &SessionSummary) -> MenuResult<()> {
    println!("将删除会话及相关索引记录: {}", session.id);
    if provider == "codex" {
        println!(
            "若该会话属于 provider/历史分支组，将删除整个会话组。单分支删除请使用家族分支菜单。"
        );
    } else if provider == "claude" {
        println!("将删除当前选中的 Claude 会话文件及其同名 sidecar。");
        println!(
            "若这是最后一个同 ID 副本，还会清理 history.jsonl、tasks/<id> 与 file-history/<id>；否则保留这些共享数据。"
        );
    } else {
        println!("将从 opencode.db 删除该会话、全部子会话及其消息、片段和关联事件。");
    }
    if !confirm_yes("这是破坏性操作。请输入 yes 确认删除。")? {
        println!("已取消。");
        pause()?;
        return Ok(());
    }
    let result = sessions::delete_session_with_provider_dir(
        Some(provider.to_string()),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        session.id.clone(),
        Some(DeleteTarget {
            id: session.id.clone(),
            rollout_path: Some(session.rollout_path.clone()),
        }),
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    println!("ok={}", result.ok);
    if let Some(error) = result.error {
        println!("error={error}");
    }
    pause()?;
    Ok(())
}

fn delete_selected_sessions(
    ctx: &MenuContext,
    provider: &str,
    selected: &[SessionSummary],
) -> MenuResult<bool> {
    if selected.is_empty() {
        println!("尚未选择会话。");
        pause()?;
        return Ok(false);
    }

    let total_tokens = selected
        .iter()
        .map(|session| session.tokens_used)
        .sum::<i64>();
    let total_bytes = selected
        .iter()
        .map(|session| session.rollout_bytes)
        .sum::<u64>();
    println!(
        "将删除 {} 条会话，共 {} token，{} bytes。",
        selected.len(),
        total_tokens,
        total_bytes
    );
    if provider == "codex" {
        println!("所选 Codex 会话若属于分支组，将连同组内全部 provider/历史分支一起删除。");
    } else if provider == "claude" {
        println!("每条 Claude 会话将按当前选中的文件路径删除，并清理其同名 sidecar。");
        println!(
            "仅当同 ID 的最后一个副本被删除时，才会清理 history.jsonl、tasks/<id> 与 file-history/<id>。"
        );
    } else {
        println!("每条 OpenCode 会话会连同其全部子会话和数据库关联记录一起删除。");
    }
    for session in selected.iter().take(10) {
        println!(
            "  {}  {:>10} token  {}",
            session.id,
            session.tokens_used,
            compact(&session.title, 70)
        );
    }
    if selected.len() > 10 {
        println!("  ... 还有 {} 条", selected.len() - 10);
    }

    if !confirm_yes("这是破坏性操作。请输入 yes 确认删除已选会话。")? {
        println!("已取消。");
        pause()?;
        return Ok(false);
    }

    let ids = selected
        .iter()
        .map(|session| session.id.clone())
        .collect::<Vec<_>>();
    let targets = selected
        .iter()
        .map(|session| DeleteTarget {
            id: session.id.clone(),
            rollout_path: Some(session.rollout_path.clone()),
        })
        .collect::<Vec<_>>();
    let results = sessions::delete_sessions_with_provider_dir(
        Some(provider.to_string()),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        ids,
        Some(targets),
        &ctx.family_lock,
    )
    .map_err(to_string)?;

    let deleted = results
        .iter()
        .filter(|result| result.ok)
        .map(|result| result.id.clone())
        .collect::<Vec<_>>();
    println!("已删除 {}/{} 条。", deleted.len(), results.len());
    if results
        .iter()
        .any(|result| result.ok && result.desktop_restart_required)
    {
        println!("提示：{DESKTOP_DELETE_RESTART_NOTICE}");
    }
    for result in results
        .iter()
        .filter(|result| !result.ok || result.error.is_some())
    {
        println!(
            "{} ok={} error={}",
            result.id,
            result.ok,
            result.error.as_deref().unwrap_or("")
        );
    }
    pause()?;
    Ok(true)
}

fn stats_menu(ctx: &mut MenuContext) -> MenuResult<Flow> {
    loop {
        print_header("统计", &[]);
        println!("1. KPI");
        println!("2. 按项目统计");
        println!("3. 按模型统计");
        println!("4. 时间序列");
        println!("5. 热力图");
        println!("6. 返回上一层");
        println!("7. 返回主菜单");
        println!("0. 退出");

        match prompt("请选择: ")?.as_str() {
            "1" => stats_kpi(ctx)?,
            "2" => stats_projects(ctx)?,
            "3" => stats_models(ctx)?,
            "4" => stats_timeseries(ctx)?,
            "5" => stats_heatmap(ctx)?,
            "6" => return Ok(Flow::Back),
            "7" => return Ok(Flow::Main),
            "0" => return Ok(Flow::Exit),
            _ => {
                println!("无效选择。");
                pause()?;
            }
        }
    }
}

fn stats_provider(ctx: &MenuContext) -> MenuResult<String> {
    if let Some(provider) = ctx.provider.as_deref() {
        return Ok(provider.to_string());
    }
    print_header("选择统计范围", &[]);
    println!("1. 全部");
    println!("2. Codex");
    println!("3. Claude");
    println!("4. OpenCode");
    match prompt("请选择: ")?.as_str() {
        "1" => Ok("all".to_string()),
        "2" => Ok("codex".to_string()),
        "3" => Ok("claude".to_string()),
        "4" => Ok("opencode".to_string()),
        _ => Err("无效统计范围。".to_string()),
    }
}

fn stats_kpi(ctx: &MenuContext) -> MenuResult<()> {
    let provider = stats_provider(ctx)?;
    let data = stats::stats_kpi(
        Some(provider),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        None,
        None,
        Vec::new(),
        false,
    )
    .map_err(to_string)?;
    println!("sessions_total           {}", data.sessions_total);
    println!("tokens_total             {}", data.tokens_total);
    println!("active_projects          {}", data.active_projects);
    println!(
        "avg_tokens_per_session   {:.2}",
        data.avg_tokens_per_session
    );
    pause()?;
    Ok(())
}

fn stats_projects(ctx: &MenuContext) -> MenuResult<()> {
    let provider = stats_provider(ctx)?;
    let limit = prompt_usize("显示数量", 20)?;
    let data = stats::stats_by_project(
        Some(provider),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        None,
        None,
        limit,
        Vec::new(),
        false,
    )
    .map_err(to_string)?;
    println!("provider  sessions  tokens  cwd");
    for item in data {
        println!(
            "{:<8} {:>8} {:>8}  {}",
            item.provider.as_deref().unwrap_or(""),
            item.sessions,
            item.tokens,
            item.cwd
        );
    }
    pause()?;
    Ok(())
}

fn stats_models(ctx: &MenuContext) -> MenuResult<()> {
    let provider = stats_provider(ctx)?;
    let data = stats::stats_by_model(
        Some(provider),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        None,
        None,
        Vec::new(),
        false,
    )
    .map_err(to_string)?;
    println!("provider  sessions  tokens  model");
    for item in data {
        println!(
            "{:<8} {:>8} {:>8}  {} {}",
            item.provider.as_deref().unwrap_or(""),
            item.sessions,
            item.tokens,
            item.model,
            item.reasoning_effort.as_deref().unwrap_or("")
        );
    }
    pause()?;
    Ok(())
}

fn stats_timeseries(ctx: &MenuContext) -> MenuResult<()> {
    let provider = stats_provider(ctx)?;
    let bucket = prompt_default("时间粒度 day/week", "day")?;
    let data = stats::stats_timeseries(
        Some(provider),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        None,
        None,
        bucket,
        Vec::new(),
        false,
    )
    .map_err(to_string)?;
    println!("bucket_start  sessions  tokens");
    for item in data {
        println!(
            "{}  {:>8}  {:>8}",
            short_timestamp(item.bucket_start),
            item.sessions,
            item.tokens
        );
    }
    pause()?;
    Ok(())
}

fn stats_heatmap(ctx: &MenuContext) -> MenuResult<()> {
    let provider = stats_provider(ctx)?;
    let data = stats::stats_heatmap(
        Some(provider),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        None,
        None,
        Vec::new(),
        false,
    )
    .map_err(to_string)?;
    println!("每行从周日到周六，每列为 0-23 点。");
    for row in data {
        println!(
            "{}",
            row.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join("\t")
        );
    }
    pause()?;
    Ok(())
}

fn backup_menu(ctx: &mut MenuContext) -> MenuResult<Flow> {
    loop {
        print_header("备份", &[]);
        println!("1. 创建备份（输入 session id）");
        println!("2. 列出备份");
        println!("3. 打开备份详情");
        println!("4. 校验备份");
        println!("5. 恢复单个会话");
        println!("6. 恢复全部会话");
        println!("7. 删除备份");
        println!("8. 返回上一层");
        println!("9. 返回主菜单");
        println!("0. 退出");

        match prompt("请选择: ")?.as_str() {
            "1" => backup_create_by_id(ctx)?,
            "2" => backup_list()?,
            "3" => backup_open()?,
            "4" => backup_verify()?,
            "5" => backup_restore_one(ctx)?,
            "6" => backup_restore_all(ctx)?,
            "7" => backup_delete()?,
            "8" => return Ok(Flow::Back),
            "9" => return Ok(Flow::Main),
            "0" => return Ok(Flow::Exit),
            _ => {
                println!("无效选择。");
                pause()?;
            }
        }
    }
}

fn choose_concrete_provider() -> MenuResult<String> {
    println!("1. Codex");
    println!("2. Claude");
    println!("3. OpenCode");
    match prompt("请选择 provider: ")?.as_str() {
        "1" => Ok("codex".to_string()),
        "2" => Ok("claude".to_string()),
        "3" => Ok("opencode".to_string()),
        _ => Err("无效 provider。".to_string()),
    }
}

fn backup_create_by_id(ctx: &MenuContext) -> MenuResult<()> {
    let provider = choose_concrete_provider()?;
    let backup_dir = prompt_default("备份目录", &paths::default_backup_dir().to_string_lossy())?;
    let ids = prompt_ids()?;
    let name = prompt_optional("备份名称（留空自动生成）: ")?;
    let note = prompt_optional("备注（可留空）: ")?;
    let summary = backup::create_backup_with_opencode(
        Some(provider),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        backup_dir,
        ids,
        None,
        name,
        note,
    )
    .map_err(to_string)?;
    print_backup_summary(&summary);
    pause()?;
    Ok(())
}

fn backup_list() -> MenuResult<()> {
    let provider = choose_concrete_provider()?;
    let backup_dir = prompt_default("备份目录", &paths::default_backup_dir().to_string_lossy())?;
    let items = backup::list_backups(backup_dir, Some(provider)).map_err(to_string)?;
    print_backup_summaries(&items);
    pause()?;
    Ok(())
}

fn backup_open() -> MenuResult<()> {
    let path = prompt_required("备份路径: ")?;
    let root = explicit_backup_root(&path)?;
    let detail = backup::open_backup(root, path).map_err(to_string)?;
    print_backup_summary(&detail.summary);
    println!("sessions:");
    for session in detail.manifest.sessions {
        println!(
            "{}  {}  {}",
            session.provider.as_deref().unwrap_or(""),
            session.id,
            compact(&session.title, 80)
        );
    }
    pause()?;
    Ok(())
}

fn backup_verify() -> MenuResult<()> {
    let path = prompt_required("备份路径: ")?;
    let root = explicit_backup_root(&path)?;
    let report = backup::verify_backup(root, path).map_err(to_string)?;
    println!("all_ok={}", report.all_ok);
    for item in report.items {
        println!(
            "{}  {}  missing={}",
            item.id,
            if item.ok { "ok" } else { "bad" },
            item.missing
        );
    }
    pause()?;
    Ok(())
}

fn backup_restore_one(ctx: &MenuContext) -> MenuResult<()> {
    let provider = choose_concrete_provider()?;
    let path = prompt_required("备份路径: ")?;
    let id = prompt_required("session id: ")?;
    let overwrite = confirm_yes("如目标已存在，是否允许覆盖？输入 yes 才会覆盖。")?;
    let backup_root = explicit_backup_root(&path)?;
    let result = backup::restore_session_with_lock(
        Some(provider),
        backup_root,
        path,
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        id,
        None,
        overwrite,
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    println!("{} ok={}", result.id, result.ok);
    if let Some(error) = result.error {
        println!("error={error}");
    }
    pause()?;
    Ok(())
}

fn backup_restore_all(ctx: &MenuContext) -> MenuResult<()> {
    let provider = choose_concrete_provider()?;
    let path = prompt_required("备份路径: ")?;
    if !confirm_yes("将恢复备份中的所有会话。请输入 yes 确认。")? {
        println!("已取消。");
        return pause().map(|_| ());
    }
    let overwrite = confirm_yes("如目标已存在，是否允许覆盖？输入 yes 才会覆盖。")?;
    let backup_root = explicit_backup_root(&path)?;
    let results = backup::restore_all_with_lock(
        Some(provider),
        backup_root,
        path,
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        overwrite,
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    for result in results {
        println!("{} ok={}", result.id, result.ok);
        if let Some(error) = result.error {
            println!("{} error={}", result.id, error);
        }
    }
    pause()?;
    Ok(())
}

fn backup_delete() -> MenuResult<()> {
    let path = prompt_required("备份路径: ")?;
    if !confirm_yes("将删除整个备份目录。请输入 yes 确认。")? {
        println!("已取消。");
        return pause().map(|_| ());
    }
    let root = explicit_backup_root(&path)?;
    backup::delete_backup(root, path.clone()).map_err(to_string)?;
    println!("已删除: {path}");
    pause()?;
    Ok(())
}

fn explicit_backup_root(path: &str) -> MenuResult<String> {
    std::path::Path::new(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.to_string_lossy().into_owned())
        .ok_or_else(|| format!("备份路径缺少父目录: {path}"))
}

fn bundle_menu(ctx: &mut MenuContext) -> MenuResult<Flow> {
    loop {
        print_header("Bundle 导入 / 导出", &[]);
        println!("1. 导出指定会话");
        println!("2. 导出全部会话");
        println!("3. 列出 Bundle");
        println!("4. 校验 Bundle");
        println!("5. 导入 Bundle");
        println!("6. 打包 Bundle ZIP");
        println!("7. 解包 ZIP");
        println!("8. 返回上一层");
        println!("9. 返回主菜单");
        println!("0. 退出");

        match prompt("请选择: ")?.as_str() {
            "1" => bundle_export(ctx)?,
            "2" => bundle_export_all(ctx)?,
            "3" => bundle_list(false)?,
            "4" => bundle_list(true)?,
            "5" => bundle_import(ctx)?,
            "6" => bundle_pack()?,
            "7" => bundle_unpack()?,
            "8" => return Ok(Flow::Back),
            "9" => return Ok(Flow::Main),
            "0" => return Ok(Flow::Exit),
            _ => {
                println!("无效选择。");
                pause()?;
            }
        }
    }
}

fn bundle_export(ctx: &MenuContext) -> MenuResult<()> {
    let provider = choose_concrete_provider()?;
    let out_dir = prompt_required("导出目录: ")?;
    let ids = prompt_ids()?;
    let targets = resolve_bundle_export_targets(ctx, &provider, &ids)?;
    let reports = bundle::export_session_bundles_with_opencode(
        Some(provider),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        out_dir,
        ids,
        targets,
        None,
        None,
    )
    .map_err(to_string)?;
    print_export_reports(&reports);
    pause()?;
    Ok(())
}

fn bundle_export_all(ctx: &MenuContext) -> MenuResult<()> {
    let provider = choose_concrete_provider()?;
    let out_dir = prompt_required("导出目录: ")?;
    let active_only = if provider == "claude" {
        false
    } else if provider == "opencode" {
        confirm_default_no("是否只导出未归档会话？")?
    } else {
        confirm_default_no("是否只导出 active 会话？")?
    };
    let reports = bundle::export_all_bundles_with_opencode(
        Some(provider),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        out_dir,
        None,
        None,
        active_only,
    )
    .map_err(to_string)?;
    print_export_reports(&reports);
    pause()?;
    Ok(())
}

fn bundle_list(verify: bool) -> MenuResult<()> {
    let provider = choose_concrete_provider()?;
    let src_dir = prompt_required("Bundle 目录: ")?;
    let items = if verify {
        bundle::verify_bundles(src_dir, Some(provider)).map_err(to_string)?
    } else {
        bundle::list_bundles(src_dir, Some(provider)).map_err(to_string)?
    };
    print_bundle_items(&items);
    pause()?;
    Ok(())
}

fn bundle_import(ctx: &MenuContext) -> MenuResult<()> {
    let provider = choose_concrete_provider()?;
    let src_dir = prompt_required("Bundle 目录: ")?;
    let mode = choose_import_mode()?;
    let make_visible = if provider == "codex" {
        confirm_default_no("是否导入后写入本地可见索引？")?
    } else {
        false
    };
    let strict = confirm_default_no("是否启用严格校验？")?;
    let reports = bundle::import_session_bundles_with_lock(
        Some(provider),
        src_dir,
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
        mode,
        make_visible,
        strict,
        Vec::new(),
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    for report in reports {
        println!(
            "{} ok={} verified={} {}",
            report.session_id,
            report.ok,
            report.verified,
            report.skipped_reason.as_deref().unwrap_or("")
        );
        if let Some(error) = report.error {
            println!("{} error={}", report.session_id, error);
        }
    }
    pause()?;
    Ok(())
}

fn bundle_pack() -> MenuResult<()> {
    let src_dir = prompt_required("Bundle 目录: ")?;
    let zip_path = prompt_required("ZIP 输出路径: ")?;
    let report = bundle::pack_bundles_zip(src_dir, zip_path).map_err(to_string)?;
    println!(
        "{} files={} bytes={}",
        report.path, report.files, report.bytes
    );
    pause()?;
    Ok(())
}

fn bundle_unpack() -> MenuResult<()> {
    let zip_path = prompt_required("ZIP 路径: ")?;
    let dst_dir = prompt_required("解包目标目录: ")?;
    let report = bundle::unpack_zip(zip_path, dst_dir).map_err(to_string)?;
    println!(
        "{} files={} bytes={}",
        report.path, report.files, report.bytes
    );
    pause()?;
    Ok(())
}

fn repair_menu(ctx: &mut MenuContext) -> MenuResult<Flow> {
    loop {
        print_header("修复 / 诊断", &[]);
        println!("1. 查看 provider 信息（Codex）");
        println!("2. 诊断 Codex 状态");
        println!("3. 修复 session_index.jsonl");
        println!("4. 重建 threads 表");
        println!("5. 清理 orphan 记录");
        println!("6. 克隆会话到指定 provider");
        println!("7. 批量克隆到当前 provider");
        println!("8. 从事件创建回溯分支");
        println!("9. 家族分支管理");
        println!("10. Claude：GUI 会话列表修复");
        println!("11. Claude：history.jsonl 残留清理");
        println!("12. 返回上一层");
        println!("13. 返回主菜单");
        println!("0. 退出");

        match prompt("请选择: ")?.as_str() {
            "1" => repair_provider_info(ctx)?,
            "2" => repair_diagnose(ctx)?,
            "3" => repair_index(ctx)?,
            "4" => repair_threads(ctx)?,
            "5" => repair_prune(ctx)?,
            "6" => repair_clone(ctx)?,
            "7" => repair_batch_clone(ctx)?,
            "8" => repair_fork(ctx)?,
            "9" => match family_menu(ctx)? {
                Flow::Back => {}
                other => return Ok(other),
            },
            "10" => repair_claude_gui(ctx)?,
            "11" => repair_claude_history(ctx)?,
            "12" => return Ok(Flow::Back),
            "13" => return Ok(Flow::Main),
            "0" => return Ok(Flow::Exit),
            _ => {
                println!("无效选择。");
                pause()?;
            }
        }
    }
}

/// Claude GUI（VS Code 插件等）会话列表可见性诊断与修复。
fn repair_claude_gui(ctx: &MenuContext) -> MenuResult<()> {
    let report =
        repair::diagnose_claude_gui_visibility(ctx.claude_dir.clone()).map_err(to_string)?;
    println!("projects_root      {}", report.projects_root);
    println!("scanned_sessions   {}", report.scanned_sessions);
    println!("visible_sessions   {}", report.visible_sessions);
    println!("sidechain_sessions {}", report.sidechain_sessions);
    println!("empty_sessions     {}", report.empty_sessions);
    println!("unfixable_sessions {}", report.unfixable_sessions);
    println!("invisible_fixable  {}", report.issues.len());
    if report.issues.is_empty() {
        println!("所有会话都能在 GUI 历史列表中正常显示。");
        return pause().map(|_| ());
    }
    println!("以下会话在 GUI 历史列表中不可见，将补写的标题来自会话内容：");
    for issue in &report.issues {
        println!(
            "  {}  [{}]  {}",
            issue.session_id, issue.project_dir, issue.proposed_title
        );
    }
    if !confirm_default_no("是否为以上会话补写 custom-title 记录（恢复 GUI 显示）？")?
    {
        return pause().map(|_| ());
    }
    let dry_run = choose_dry_run("补写 custom-title 记录")?;
    let result = repair::repair_claude_gui_visibility(ctx.claude_dir.clone(), dry_run, None)
        .map_err(to_string)?;
    println!(
        "fixed={} skipped={} dry_run={}",
        result.fixed, result.skipped, result.dry_run
    );
    for error in result.errors {
        println!("error {error}");
    }
    pause()?;
    Ok(())
}

/// Claude history.jsonl 残留诊断与清理。
fn repair_claude_history(ctx: &MenuContext) -> MenuResult<()> {
    let report =
        repair::diagnose_claude_history_orphans(ctx.claude_dir.clone()).map_err(to_string)?;
    println!("history_path   {}", report.history_path);
    println!("session_count  {}", report.session_count);
    println!("history_rows   {}", report.history_rows);
    println!("linked_rows    {}", report.linked_rows);
    println!("orphan_rows    {}", report.orphan_rows);
    println!("untracked_rows {}", report.untracked_rows);
    if report.orphan_rows == 0 {
        println!("暂未发现 Claude history 残留。");
        return pause().map(|_| ());
    }
    for id in &report.orphan_session_ids {
        println!("  orphan {id}");
    }
    if !confirm_default_no("是否清理这些残留历史行？")? {
        return pause().map(|_| ());
    }
    let dry_run = choose_dry_run("清理 Claude history 残留")?;
    let result =
        repair::prune_claude_history_orphans(ctx.claude_dir.clone(), dry_run).map_err(to_string)?;
    println!(
        "removed_rows={} dry_run={}",
        result.removed_rows, result.dry_run
    );
    pause()?;
    Ok(())
}

fn repair_provider_info(ctx: &MenuContext) -> MenuResult<()> {
    let info = repair::get_provider_info(ctx.codex_dir.clone()).map_err(to_string)?;
    println!("current      {}", info.current.as_deref().unwrap_or(""));
    println!("is_explicit  {}", info.is_explicit);
    println!("config_path  {}", info.config_path);
    println!("exists       {}", info.exists);
    pause()?;
    Ok(())
}

fn repair_diagnose(ctx: &MenuContext) -> MenuResult<()> {
    let report = repair::diagnose_codex_state(ctx.codex_dir.clone()).map_err(to_string)?;
    println!("rollout_count                  {}", report.rollout_count);
    println!(
        "archived_rollout_count         {}",
        report.archived_rollout_count
    );
    println!("index_count                    {}", report.index_count);
    println!("threads_count                  {}", report.threads_count);
    println!(
        "threads_active_count           {}",
        report.threads_active_count
    );
    println!(
        "threads_archived_count         {}",
        report.threads_archived_count
    );
    println!(
        "missing_in_index               {}",
        report.missing_in_index.len()
    );
    println!(
        "missing_in_threads             {}",
        report.missing_in_threads.len()
    );
    println!(
        "orphan_in_index                {}",
        report.orphan_in_index.len()
    );
    println!(
        "orphan_in_threads              {}",
        report.orphan_in_threads.len()
    );
    println!(
        "orphan_subagent_count          {}",
        report.orphan_subagent_count
    );
    println!(
        "provider_mismatched_families   {}",
        report.provider_mismatched_families
    );
    pause()?;
    Ok(())
}

fn repair_index(ctx: &MenuContext) -> MenuResult<()> {
    let dry_run = choose_dry_run("写入 session_index.jsonl")?;
    let report = repair::repair_session_index(ctx.codex_dir.clone(), dry_run).map_err(to_string)?;
    println!(
        "scanned={} written={} salvaged={} dry_run={}",
        report.scanned, report.written, report.salvaged, report.dry_run
    );
    for error in report.errors {
        println!("error {error}");
    }
    pause()?;
    Ok(())
}

fn repair_threads(ctx: &MenuContext) -> MenuResult<()> {
    let dry_run = choose_dry_run("重建 threads 表")?;
    let report =
        repair::rebuild_threads_table(ctx.codex_dir.clone(), dry_run).map_err(to_string)?;
    println!(
        "scanned={} upserted={} skipped={} dry_run={}",
        report.scanned, report.upserted, report.skipped, report.dry_run
    );
    for error in report.errors {
        println!("error {error}");
    }
    pause()?;
    Ok(())
}

fn repair_prune(ctx: &MenuContext) -> MenuResult<()> {
    let prune_index = confirm_default_no("是否清理 session_index.jsonl 里的 orphan？")?;
    let prune_threads = confirm_default_no("是否清理 threads 表里的 orphan？")?;
    let prune_family = confirm_default_no("是否清理 session_family.json 里可安全确认的 orphan？")?;
    let prune_subagents = confirm_default_no("是否清理父会话已消失的孤儿子代理会话？")?;
    if !prune_index && !prune_threads && !prune_family && !prune_subagents {
        println!("未选择任何清理目标。");
        return pause().map(|_| ());
    }
    let dry_run = choose_dry_run("清理 orphan 记录")?;
    let report = repair::prune_orphan_entries_with_lock(
        ctx.codex_dir.clone(),
        prune_index,
        prune_threads,
        prune_family,
        prune_subagents,
        dry_run,
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    println!(
        "index_removed={} threads_removed={} subagents_removed={} family_branches_removed={} families_removed={} families_recovered={} families_normalized={} families_skipped={} dry_run={}",
        report.index_removed,
        report.threads_removed,
        report.subagents_removed,
        report.family_branches_removed,
        report.families_removed,
        report.families_recovered,
        report.families_normalized,
        report.families_skipped.len(),
        report.dry_run
    );
    if report.desktop_restart_required {
        println!("提示：{DESKTOP_DELETE_RESTART_NOTICE}");
    }
    for family_id in report.families_skipped {
        println!("family_skipped {family_id}");
    }
    pause()?;
    Ok(())
}

fn repair_clone(ctx: &MenuContext) -> MenuResult<()> {
    let id = prompt_required("session id: ")?;
    let target_provider = prompt_optional("目标 provider（留空使用当前 provider）: ")?;
    let strategy = choose_switch_strategy()?;
    let dry_run = choose_dry_run("克隆会话")?;
    let report = repair::clone_session_for_provider_with_lock(
        ctx.codex_dir.clone(),
        id,
        target_provider,
        strategy,
        dry_run,
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    println!(
        "{} ok={} new_id={} {}",
        report.source_id,
        report.ok,
        report.new_id.as_deref().unwrap_or(""),
        report.skipped_reason.as_deref().unwrap_or("")
    );
    if let Some(error) = report.error {
        println!("error={error}");
    }
    pause()?;
    Ok(())
}

fn repair_batch_clone(ctx: &MenuContext) -> MenuResult<()> {
    let strategy = choose_switch_strategy()?;
    let dry_run = choose_dry_run("批量克隆会话")?;
    let reports = repair::batch_clone_for_current_provider_with_lock(
        ctx.codex_dir.clone(),
        strategy,
        dry_run,
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    for report in reports {
        println!(
            "{} ok={} new_id={} {}",
            report.source_id,
            report.ok,
            report.new_id.as_deref().unwrap_or(""),
            report.skipped_reason.as_deref().unwrap_or("")
        );
        if let Some(error) = report.error {
            println!("{} error={}", report.source_id, error);
        }
    }
    pause()?;
    Ok(())
}

fn repair_fork(ctx: &MenuContext) -> MenuResult<()> {
    let id = prompt_required("session id: ")?;
    let rollout_path = prompt_required("active rollout 路径: ")?;
    let event_index = prompt_usize("事件序号", 0)?;
    if !confirm_yes("创建回溯分支会归档当前 active 分支。请输入 yes 确认。")? {
        println!("已取消。");
        return pause().map(|_| ());
    }
    let report = repair::fork_session_at_event_with_lock(
        ctx.codex_dir.clone(),
        id,
        rollout_path,
        event_index,
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    println!(
        "{} -> {} line={} {}",
        report.source_id, report.new_id, report.event_index, report.cut_summary
    );
    pause()?;
    Ok(())
}

fn family_menu(ctx: &mut MenuContext) -> MenuResult<Flow> {
    loop {
        print_header("家族分支管理", &[]);
        println!("1. 查看 family store 摘要");
        println!("2. 校验家族完整性");
        println!("3. 查看 session family overlay");
        println!("4. 回滚 active 分支");
        println!("5. 删除非 active 分支");
        println!("6. 查看分支同步状态");
        println!("7. 同步分支到 active");
        println!("8. 同步 active 到分支");
        println!("9. 返回上一层");
        println!("10. 返回主菜单");
        println!("0. 退出");

        match prompt("请选择: ")?.as_str() {
            "1" => family_store(ctx)?,
            "2" => family_verify(ctx)?,
            "3" => family_overlay(ctx)?,
            "4" => family_rollback(ctx)?,
            "5" => family_delete_branch(ctx)?,
            "6" => family_sync_states(ctx)?,
            "7" => family_sync_into_active(ctx)?,
            "8" => family_sync_active_into(ctx)?,
            "9" => return Ok(Flow::Back),
            "10" => return Ok(Flow::Main),
            "0" => return Ok(Flow::Exit),
            _ => {
                println!("无效选择。");
                pause()?;
            }
        }
    }
}

fn family_store(ctx: &MenuContext) -> MenuResult<()> {
    let store = family::get_family_store_with_lock(ctx.codex_dir.clone(), &ctx.family_lock)
        .map_err(to_string)?;
    println!("families={}", store.families.len());
    println!("branches={}", store.index.len());
    pause()?;
    Ok(())
}

fn family_verify(ctx: &MenuContext) -> MenuResult<()> {
    let report = family::verify_family_integrity_with_lock(ctx.codex_dir.clone(), &ctx.family_lock)
        .map_err(to_string)?;
    println!("all_ok={}", report.all_ok);
    for item in report.items {
        println!(
            "{} {} {}",
            item.family_id,
            item.branch_id,
            if item.ok { "ok" } else { "bad" }
        );
    }
    pause()?;
    Ok(())
}

fn family_overlay(ctx: &MenuContext) -> MenuResult<()> {
    let overlay =
        family::get_session_family_overlay_with_lock(ctx.codex_dir.clone(), &ctx.family_lock)
            .map_err(to_string)?;
    println!("session_id  provider  family_id  branches  active  clone_state");
    for item in overlay {
        println!(
            "{}  {}  {}  {}  {}  {}",
            item.session_id,
            item.provider.as_deref().unwrap_or(""),
            item.family_id.as_deref().unwrap_or(""),
            item.branch_count,
            item.is_active_branch,
            item.clone_state
        );
    }
    pause()?;
    Ok(())
}

fn family_rollback(ctx: &MenuContext) -> MenuResult<()> {
    let family_id = prompt_required("family id: ")?;
    let branch_id = prompt_required("目标 branch id: ")?;
    if !confirm_yes("回滚会归档当前 active 并恢复目标分支。请输入 yes 确认。")?
    {
        println!("已取消。");
        return pause().map(|_| ());
    }
    repair::rollback_family_active_with_lock(
        ctx.codex_dir.clone(),
        family_id,
        branch_id,
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    println!("已完成。");
    pause()?;
    Ok(())
}

fn family_delete_branch(ctx: &MenuContext) -> MenuResult<()> {
    let family_id = prompt_required("family id: ")?;
    let branch_id = prompt_required("branch id: ")?;
    if !confirm_yes("将删除非 active 分支及相关文件。请输入 yes 确认。")? {
        println!("已取消。");
        return pause().map(|_| ());
    }
    let result = repair::delete_family_branch_with_lock(
        ctx.codex_dir.clone(),
        family_id,
        branch_id,
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    println!("{} ok={}", result.id, result.ok);
    if let Some(error) = result.error {
        println!("error={error}");
    }
    pause()?;
    Ok(())
}

fn family_sync_states(ctx: &MenuContext) -> MenuResult<()> {
    let family_id = prompt_required("family id: ")?;
    let states = repair::get_family_branch_sync_states_with_lock(
        ctx.codex_dir.clone(),
        family_id,
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    println!("branch_id  relation  to_active  to_branch  error");
    for state in states {
        println!(
            "{}  {}  {}  {}  {}",
            state.branch_id,
            state.relation,
            state.appendable_lines_to_active,
            state.appendable_lines_to_branch,
            state.error.as_deref().unwrap_or("")
        );
    }
    pause()?;
    Ok(())
}

fn family_sync_into_active(ctx: &MenuContext) -> MenuResult<()> {
    let family_id = prompt_required("family id: ")?;
    let source_branch_id = prompt_required("源 branch id: ")?;
    if !confirm_yes("将源分支增量同步到 active。请输入 yes 确认。")? {
        println!("已取消。");
        return pause().map(|_| ());
    }
    let report = repair::sync_branch_into_active_with_lock(
        ctx.codex_dir.clone(),
        family_id,
        source_branch_id,
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    println!(
        "active={} source={} appended={} total={}",
        report.active_id, report.source_id, report.appended_lines, report.total_lines
    );
    pause()?;
    Ok(())
}

fn family_sync_active_into(ctx: &MenuContext) -> MenuResult<()> {
    let family_id = prompt_required("family id: ")?;
    let target_branch_id = prompt_required("目标 branch id: ")?;
    if !confirm_yes("将 active 增量同步到目标分支。请输入 yes 确认。")? {
        println!("已取消。");
        return pause().map(|_| ());
    }
    let report = repair::sync_active_into_branch_with_lock(
        ctx.codex_dir.clone(),
        family_id,
        target_branch_id,
        &ctx.family_lock,
    )
    .map_err(to_string)?;
    println!(
        "source={} target={} appended={} total={}",
        report.source_id, report.target_id, report.appended_lines, report.total_lines
    );
    pause()?;
    Ok(())
}

fn settings_menu(ctx: &mut MenuContext) -> MenuResult<Flow> {
    loop {
        print_header("设置与路径检查", &[]);
        println!("1. 查看默认路径");
        println!("2. 校验当前路径");
        println!("3. 修改本次运行的 Codex 目录");
        println!("4. 修改本次运行的 Claude 目录");
        println!("5. 修改本次运行的 OpenCode 目录");
        println!("6. 返回上一层");
        println!("7. 返回主菜单");
        println!("0. 退出");

        match prompt("请选择: ")?.as_str() {
            "1" => settings_defaults()?,
            "2" => settings_validate(ctx)?,
            "3" => {
                ctx.codex_dir = prompt_default("Codex 目录", &ctx.codex_dir)?;
            }
            "4" => {
                ctx.claude_dir = prompt_default("Claude 目录", &ctx.claude_dir)?;
            }
            "5" => {
                ctx.opencode_dir = prompt_default("OpenCode 目录", &ctx.opencode_dir)?;
            }
            "6" => return Ok(Flow::Back),
            "7" => return Ok(Flow::Main),
            "0" => return Ok(Flow::Exit),
            _ => {
                println!("无效选择。");
                pause()?;
            }
        }
    }
}

fn settings_defaults() -> MenuResult<()> {
    let defaults = cc_session_manager_lib::models::Settings::default();
    println!("codex_dir              {}", defaults.codex_dir);
    println!("claude_dir             {}", defaults.claude_dir);
    println!("opencode_dir           {}", defaults.opencode_dir);
    println!("backup_dir             {}", defaults.backup_dir);
    println!("refresh_interval_ms    {}", defaults.refresh_interval_ms);
    pause()?;
    Ok(())
}

fn settings_validate(ctx: &MenuContext) -> MenuResult<()> {
    let codex = settings::validate_codex_dir(ctx.codex_dir.clone()).map_err(to_string)?;
    let claude = settings::validate_claude_dir(ctx.claude_dir.clone()).map_err(to_string)?;
    let opencode = settings::validate_opencode_dir(ctx.opencode_dir.clone()).map_err(to_string)?;
    println!(
        "codex   valid={} state_db={} sessions={} threads={}",
        codex.valid, codex.has_state_db, codex.has_sessions, codex.threads_count
    );
    println!(
        "claude  valid={} projects={} sessions={}",
        claude.valid, claude.has_sessions, claude.threads_count
    );
    println!(
        "opencode valid={} db={} sessions={}",
        opencode.valid, opencode.has_state_db, opencode.threads_count
    );
    pause()?;
    Ok(())
}

fn show_interactive_help() -> MenuResult<()> {
    print_header("帮助", &[]);
    println!("直接运行 cc-sessions 会进入交互菜单。");
    println!("输入菜单序号即可进入下一层，例如 1 进入 Codex 会话。");
    println!("列表页支持 n 下一页、p 上一页、b 返回上一层、m 返回主菜单、0 退出。");
    println!("列表页支持 s 多选当前页序号、u 取消选择、c 清空选择、d 删除已选会话。");
    println!("会话列表默认显示主会话，选择子代理范围后只显示子代理会话。");
    println!("会话预览默认只显示用户和助手消息；选择“全部事件”才会显示工具调用。");
    println!("进入主会话详情后，可选择“转换为 Claude/Codex”并指定转换模式。");
    println!("删除、覆盖恢复、清理和分支切换等危险操作需要输入 yes 确认。");
    println!("脚本用法仍然保留，例如 cc-sessions list --limit 20。");
    pause().map(|_| ())
}

fn load_sessions(
    ctx: &MenuContext,
    provider: &str,
    include_archived: bool,
    scope: SessionScope,
) -> MenuResult<Vec<SessionSummary>> {
    let mut list = sessions::list_sessions_with_opencode(
        Some(provider.to_string()),
        ctx.codex_dir.clone(),
        Some(ctx.claude_dir.clone()),
        Some(ctx.opencode_dir.clone()),
    )
    .map_err(to_string)?;
    if !include_archived {
        list.retain(|session| !session.archived);
    }
    retain_session_scope(&mut list, scope);
    Ok(list)
}

fn prompt_session_scope(provider: &str) -> MenuResult<SessionScope> {
    let label = if provider == "opencode" {
        "是否只查看子会话？"
    } else {
        "是否只查看子代理会话？"
    };
    if confirm_default_no(label)? {
        Ok(SessionScope::Subagent)
    } else {
        Ok(SessionScope::Main)
    }
}

fn retain_session_scope(list: &mut Vec<SessionSummary>, scope: SessionScope) {
    match scope {
        SessionScope::Main => list.retain(|session| !sessions::session_is_subagent(session)),
        SessionScope::Subagent => list.retain(sessions::session_is_subagent),
        SessionScope::All => {}
    }
}

fn group_projects(sessions: Vec<SessionSummary>) -> Vec<ProjectGroup> {
    let mut groups: HashMap<String, ProjectGroup> = HashMap::new();
    for session in sessions {
        let entry = groups.entry(session.cwd.clone()).or_insert(ProjectGroup {
            cwd: session.cwd.clone(),
            cwd_display: session.cwd_display.clone(),
            sessions: Vec::new(),
            latest_updated_at: 0,
            total_tokens: 0,
        });
        entry.latest_updated_at = entry.latest_updated_at.max(session.updated_at);
        entry.total_tokens += session.tokens_used;
        entry.sessions.push(session);
    }
    let mut out = groups.into_values().collect::<Vec<_>>();
    out.sort_by_key(|group| std::cmp::Reverse(group.latest_updated_at));
    out
}

fn sort_sessions_by_size(sessions: &mut [SessionSummary]) {
    sessions.sort_by(|a, b| {
        a.tokens_used
            .cmp(&b.tokens_used)
            .then_with(|| a.rollout_bytes.cmp(&b.rollout_bytes))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.id.cmp(&b.id))
    });
}

fn find_session(sessions: &[SessionSummary], input: &str) -> MenuResult<SessionSummary> {
    let exact = sessions.iter().find(|session| session.id == input);
    if let Some(session) = exact {
        return Ok(session.clone());
    }
    let matches = sessions
        .iter()
        .filter(|session| session.id.starts_with(input))
        .cloned()
        .collect::<Vec<_>>();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => Err(format!("未找到会话: {input}")),
        _ => Err(format!("前缀不唯一，匹配到 {} 个会话。", matches.len())),
    }
}

fn choose_dry_run(action: &str) -> MenuResult<bool> {
    println!("{action}");
    println!("1. 只预览，不写入磁盘（dry-run）");
    println!("2. 直接执行写入");
    match prompt("请选择: ")?.as_str() {
        "1" => Ok(true),
        "2" => {
            if confirm_yes("该操作会写入本地数据。请输入 yes 确认执行。")? {
                Ok(false)
            } else {
                Err("已取消执行。".to_string())
            }
        }
        _ => Err("无效选择。".to_string()),
    }
}

fn choose_switch_strategy() -> MenuResult<SwitchStrategy> {
    println!("1. continuous：活跃分支 + 归档旧节点");
    println!("2. scatter：每个 provider 独立副本");
    println!("3. follow：就地改写 provider");
    match prompt("请选择策略: ")?.as_str() {
        "1" => Ok(SwitchStrategy::Continuous),
        "2" => Ok(SwitchStrategy::Scatter),
        "3" => Ok(SwitchStrategy::Follow),
        _ => Err("无效策略。".to_string()),
    }
}

fn choose_import_mode() -> MenuResult<ImportMode> {
    println!("1. skip：本地存在同 id 则跳过");
    println!("2. overwrite：本地存在同 id 则覆盖");
    println!("3. keep-local：本地更新则保留本地");
    match prompt("请选择导入模式: ")?.as_str() {
        "1" => Ok(ImportMode::Skip),
        "2" => Ok(ImportMode::Overwrite),
        "3" => Ok(ImportMode::KeepLocal),
        _ => Err("无效导入模式。".to_string()),
    }
}

fn prompt_ids() -> MenuResult<Vec<String>> {
    let raw = prompt_required("session id（多个可用空格或逗号分隔）: ")?;
    let ids = raw
        .split(|ch: char| ch == ',' || ch.is_whitespace())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if ids.is_empty() {
        Err("至少需要一个 session id。".to_string())
    } else {
        Ok(ids)
    }
}

fn resolve_bundle_export_targets(
    ctx: &MenuContext,
    provider: &str,
    ids: &[String],
) -> MenuResult<Option<Vec<BundleExportTarget>>> {
    if provider != "claude" {
        return Ok(None);
    }
    let available = load_sessions(ctx, provider, true, SessionScope::All)?;
    let mut targets = Vec::with_capacity(ids.len());
    for id in ids {
        let matches = available
            .iter()
            .filter(|session| session.id == *id)
            .collect::<Vec<_>>();
        let rollout_path = match matches.as_slice() {
            [session] => Some(session.rollout_path.clone()),
            [] => None,
            duplicates => {
                println!(
                    "检测到 {} 个同 ID Claude 会话，请选择精确 transcript：",
                    duplicates.len()
                );
                for (index, session) in duplicates.iter().enumerate() {
                    println!("{}. {}", index + 1, session.rollout_path);
                }
                let selected = prompt_required("请输入序号: ")?
                    .parse::<usize>()
                    .map_err(|_| "序号必须是正整数。".to_string())?;
                let session = duplicates
                    .get(selected.saturating_sub(1))
                    .ok_or_else(|| "序号超出范围。".to_string())?;
                Some(session.rollout_path.clone())
            }
        };
        targets.push(BundleExportTarget {
            id: id.clone(),
            rollout_path,
        });
    }
    Ok(Some(targets))
}

fn print_header(title: &str, fields: &[(&str, &str)]) {
    println!();
    println!("============================================================");
    println!("{title}");
    for (name, value) in fields {
        println!("{name}: {value}");
    }
    println!("============================================================");
}

fn print_backup_summary(summary: &BackupSummary) {
    println!("name              {}", summary.name);
    println!("path              {}", summary.path);
    println!(
        "provider          {}",
        summary.provider.as_deref().unwrap_or("")
    );
    println!("created_at        {}", summary.created_at);
    println!("sessions_count    {}", summary.sessions_count);
    println!("total_bytes       {}", summary.total_bytes);
}

fn print_backup_summaries(items: &[BackupSummary]) {
    println!("created_at  provider  sessions  bytes  name");
    for item in items {
        println!(
            "{}  {:<8} {:>8} {:>8}  {}",
            item.created_at,
            item.provider.as_deref().unwrap_or(""),
            item.sessions_count,
            item.total_bytes,
            item.name
        );
        println!("    {}", item.path);
    }
}

fn print_export_reports(reports: &[ExportReport]) {
    for report in reports {
        println!(
            "{} ok={} {}",
            report.session_id,
            report.ok,
            report.bundle_path.as_deref().unwrap_or("")
        );
        if let Some(reason) = &report.skipped_reason {
            println!("{} skipped={}", report.session_id, reason);
        }
        if let Some(error) = &report.error {
            println!("{} error={}", report.session_id, error);
        }
    }
}

fn print_bundle_items(items: &[BundleListItem]) {
    println!("verified  provider  session_id  updated_at  bundle_dir");
    for item in items {
        println!(
            "{}  {:<8} {}  {}  {}",
            item.verified
                .map(|value| value.to_string())
                .unwrap_or_else(|| "".to_string()),
            item.manifest.provider.as_deref().unwrap_or(""),
            item.manifest.session_id,
            item.manifest.updated_at,
            item.bundle_dir
        );
    }
}

fn provider_label(provider: &str) -> &'static str {
    match provider {
        "codex" => "Codex",
        "claude" => "Claude",
        "opencode" => "OpenCode",
        _ => "未知",
    }
}

fn prompt(label: &str) -> MenuResult<String> {
    print!("{label}");
    io::stdout().flush().map_err(to_string)?;
    let mut input = String::new();
    io::stdin().read_line(&mut input).map_err(to_string)?;
    Ok(input.trim().to_string())
}

fn prompt_required(label: &str) -> MenuResult<String> {
    let value = prompt(label)?;
    if value.trim().is_empty() {
        Err("输入不能为空。".to_string())
    } else {
        Ok(value)
    }
}

fn prompt_default(label: &str, default: &str) -> MenuResult<String> {
    let value = prompt(&format!("{label} [{default}]: "))?;
    if value.trim().is_empty() {
        Ok(default.to_string())
    } else {
        Ok(value)
    }
}

fn prompt_optional(label: &str) -> MenuResult<Option<String>> {
    let value = prompt(label)?;
    if value.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn prompt_usize(label: &str, default: usize) -> MenuResult<usize> {
    let value = prompt(&format!("{label} [{default}]: "))?;
    if value.trim().is_empty() {
        return Ok(default);
    }
    value
        .parse::<usize>()
        .map_err(|_| format!("{label} 需要非负整数。"))
}

fn confirm_default_no(label: &str) -> MenuResult<bool> {
    let value = prompt(&format!("{label} [y/N]: "))?;
    Ok(matches!(value.as_str(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

fn confirm_yes(label: &str) -> MenuResult<bool> {
    let value = prompt(&format!("{label} "))?;
    Ok(value == "yes")
}

fn pause() -> MenuResult<Flow> {
    prompt("按回车继续...")?;
    Ok(Flow::Back)
}

fn parse_index(input: &str, len: usize) -> Option<usize> {
    let value = input.parse::<usize>().ok()?;
    if value == 0 || value > len {
        None
    } else {
        Some(value - 1)
    }
}

fn short_timestamp(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn compact(value: &str, max_chars: usize) -> String {
    let flat = value.replace(['\r', '\n'], " ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let mut out = flat.chars().take(max_chars).collect::<String>();
    out.push_str("...");
    out
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_target_switches_provider() {
        assert_eq!(conversion_target_provider("codex").unwrap(), "claude");
        assert_eq!(conversion_target_provider("claude").unwrap(), "codex");
        assert!(conversion_target_provider("all").is_err());
    }

    #[test]
    fn conversion_mode_choice_defaults_to_simple() {
        assert_eq!(conversion_mode_from_choice("").unwrap(), "simple");
        assert_eq!(conversion_mode_from_choice("1").unwrap(), "simple");
        assert_eq!(conversion_mode_from_choice("2").unwrap(), "native");
        assert!(conversion_mode_from_choice("3").is_err());
    }
}
