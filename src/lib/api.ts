import { invoke } from "@tauri-apps/api/core";
import { copyText } from "@/lib/clipboard";
import { isTauriRuntime, isWebRuntime, webuiApiToken } from "@/lib/runtime";
import { compareVersions, normalizeVersion } from "@/lib/version";

async function invokeCommand<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (isTauriRuntime()) {
    return invoke<T>(command, args);
  }
  const apiToken = webuiApiToken();
  if (!apiToken) {
    throw new Error("Web UI API token 未初始化，请刷新页面。");
  }

  const response = await fetch(`/api/invoke/${encodeURIComponent(command)}`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-CC-Sessions-Webui-Token": apiToken,
    },
    body: JSON.stringify(args ?? {}),
  });
  const text = await response.text();
  const data = text ? JSON.parse(text) : null;
  if (!response.ok) {
    throw new Error(String(data?.error ?? response.statusText));
  }
  return data as T;
}

function resumeCommandText(provider: SessionProvider, sessionId: string, cwd?: string) {
  switch (provider) {
    case "codex":
      return `codex resume ${sessionId}`;
    case "claude": {
      const projectDir = cwd?.trim();
      if (projectDir) {
        const quoted = projectDir.replaceAll("'", "''");
        return `Set-Location -LiteralPath '${quoted}'; claude --resume ${sessionId}`;
      }
      return `claude --resume ${sessionId}`;
    }
  }
}

export type SessionProvider = "codex" | "claude";
export type StatsProvider = "all" | SessionProvider;

export type Settings = {
  codex_dir: string;
  claude_dir: string;
  backup_dir: string;
  open_command: string;
  refresh_interval_ms: number;
  preview_only_messages: boolean;
  /** true = 过程消息默认全部收起；false = 默认全部展开。 */
  preview_collapse_process: boolean;
};

export type DirValidation = {
  valid: boolean;
  has_state_db: boolean;
  has_sessions: boolean;
  threads_count: number;
};

export type AppUpdateInfo = {
  current_version: string;
  latest_version: string;
  html_url: string;
  available: boolean;
  can_auto_install: boolean;
  install_mode: "portable" | "nsis" | "msi" | "webui" | "unsupported" | string;
  install_dir: string | null;
  asset_name: string | null;
  message: string | null;
};

export type UpdateCheckResult =
  | {
      state: "idle";
    }
  | {
      state: "checking";
    }
  | (AppUpdateInfo & {
      state: "current" | "available" | "installing";
    })
  | {
      state: "error";
      message: string;
    };

export type SessionConversionOrigin = {
  source_provider: SessionProvider;
  source_id: string;
  conversion_mode: SessionConversionMode | null;
  converted_at: string;
};

export type SessionSummary = {
  provider: SessionProvider;
  id: string;
  rollout_path: string;
  cwd: string;
  cwd_display: string;
  title: string;
  first_user_message: string;
  model: string | null;
  reasoning_effort: string | null;
  source: string | null;
  agent_nickname: string | null;
  agent_role: string | null;
  conversion_origin: SessionConversionOrigin | null;
  tokens_used: number;
  created_at: number;
  updated_at: number;
  archived: boolean;
  git_branch: string | null;
  rollout_bytes: number;
  logs_count: number;
  has_backup: boolean;
  resume_command: string;
};

export type ContentSearchMatch = {
  event_index: number;
  event_offset: number;
  timestamp: string;
  role: string;
  snippet: string;
};

export type ContentSearchResult = {
  session: SessionSummary;
  matches: ContentSearchMatch[];
};

export type ContentSearchStatus = {
  job_id: number;
  state: "running" | "completed" | "cancelled" | "failed";
  query: string;
  scanned_files: number;
  total_files: number;
  skipped_files: number;
  scanned_bytes: number;
  total_bytes: number;
  results: ContentSearchResult[];
  truncated: boolean;
  error: string | null;
};

export type ProjectGroup = {
  cwd: string;
  cwd_display: string;
  sessions: SessionSummary[];
  latest_updated_at: number;
  total_tokens: number;
};

export type PreviewEvent = {
  index: number;
  timestamp: string;
  role: "user" | "assistant" | "tool_call" | "tool_result" | "reasoning" | "meta" | "other" | string;
  kind: string;
  text_summary: string;
  raw: unknown;
};

export type TimelineMessageBrief = {
  /** Agent 回复所在的会话文件行号；仅用于该轮悬浮摘要，不作为独立时间线刻度。 */
  index: number;
  /** Agent 回复在预览分页计数中的事件序号。 */
  offset: number;
  timestamp: string;
  text: string;
};

export type UserPromptBrief = TimelineMessageBrief & {
  /** 同一轮最终 Agent 回复，只展示在左侧刻度的悬浮摘要中。 */
  response: TimelineMessageBrief | null;
};

export type UserPromptList = {
  prompts: UserPromptBrief[];
  total_events: number;
};

export type SessionMetaBrief = {
  id: string | null;
  timestamp: string | null;
  cwd: string | null;
  originator: string | null;
  cli_version: string | null;
  source: string | null;
  model_provider: string | null;
};

export type DeleteResult = {
  id: string;
  rollout_path: string | null;
  threads_rows_deleted: number;
  logs_rows_deleted: number;
  history_rows_deleted: number;
  rollout_deleted: boolean;
  rollout_missing: boolean;
  sidecar_deleted: boolean;
  tasks_deleted: boolean;
  file_history_deleted: boolean;
  shared_data_preserved: boolean;
  ok: boolean;
  error: string | null;
};

export type DeleteTarget = {
  id: string;
  rollout_path?: string;
};

export type DeletePlanLine = {
  line_no: number;
  role: string;
  kind: string;
  summary: string;
  reason: "selected" | "tool_pair" | "mirror" | "reasoning_attached" | string;
};

export type DeletePlan = {
  rollout_path: string;
  lines: DeletePlanLine[];
  blocked: string[];
};

export type EditApplyReport = {
  op_id: string;
  kind: string;
  snapshot_created: string | null;
  changed_lines: number;
  deleted_lines: number;
  restored_lines: number;
};

export type EditHistoryEntry = {
  op_id: string;
  ts: string;
  kind: string;
  description: string;
  changes: number;
};

export type EditSnapshotInfo = {
  name: string;
  created_at: string;
  bytes: number;
};

export type EditHistory = {
  entries: EditHistoryEntry[];
  snapshots: EditSnapshotInfo[];
  undo_available: boolean;
  undo_blocked_reason: string | null;
};

export type MoveSessionCwdReport = {
  old_cwd: string;
  new_cwd: string;
  threads_updated: number;
  rollout_rewritten: boolean;
};

export type BackupSummary = {
  path: string;
  name: string;
  provider: SessionProvider | null;
  created_at: string;
  sessions_count: number;
  total_bytes: number;
  note: string | null;
};

export type ManifestSession = {
  provider: SessionProvider | null;
  id: string;
  rollout_relpath: string;
  source_relpath: string | null;
  sidecar_relpath: string | null;
  sidecar_files?: Array<{
    relpath: string;
    bytes: number;
    sha256: string;
  }>;
  title: string;
  cwd: string;
  created_at: number;
  updated_at: number;
  tokens_used: number;
  model: string | null;
  bytes_rollout: number;
  logs_count: number;
  history_rows: number;
  sha256_rollout: string;
};

export type Manifest = {
  version: number;
  provider: SessionProvider | null;
  created_at: string;
  app_version: string;
  codex_dir: string;
  claude_dir: string | null;
  note: string | null;
  artifacts?: Array<{
    relpath: string;
    bytes: number;
    sha256: string;
  }>;
  sessions: ManifestSession[];
};

export type BackupDetail = { summary: BackupSummary; manifest: Manifest };

export type RestoreResult = {
  id: string;
  ok: boolean;
  threads_inserted: boolean;
  logs_inserted: number;
  history_appended: number;
  rollout_copied: boolean;
  conflict: boolean;
  error: string | null;
};

export type VerifyItem = {
  id: string;
  ok: boolean;
  expected_sha: string;
  actual_sha: string | null;
  missing: boolean;
};

export type VerifyReport = { items: VerifyItem[]; all_ok: boolean };

export type Kpi = {
  sessions_total: number;
  tokens_total: number;
  active_projects: number;
  avg_tokens_per_session: number;
};

export type TimeseriesPoint = { bucket_start: number; sessions: number; tokens: number };
export type ProjectStat = {
  provider: SessionProvider | null;
  cwd: string;
  cwd_display: string;
  sessions: number;
  tokens: number;
};
export type ModelStat = {
  provider: SessionProvider | null;
  model: string;
  reasoning_effort: string | null;
  sessions: number;
  tokens: number;
};

// ========================= 修复 / 诊断 =========================

export type ProviderInfo = {
  current: string | null;
  is_explicit: boolean;
  config_path: string;
  exists: boolean;
};

export type ProjectConfigIssue = {
  project_cwd: string;
  config_path: string;
  session_count: number;
  session_ids: string[];
  current_min_wait_timeout_ms: number | null;
  current_default_wait_timeout_ms: number | null;
  current_max_wait_timeout_ms: number | null;
  suggested_default_wait_timeout_ms: number | null;
  repairable: boolean;
  message: string;
};

export type ProjectConfigReport = {
  scanned_projects: number;
  config_files: number;
  issue_count: number;
  repairable_count: number;
  issues: ProjectConfigIssue[];
};

export type ProjectConfigRepairItem = {
  project_cwd: string;
  config_path: string;
  changed: boolean;
  dry_run: boolean;
  old_default_wait_timeout_ms: number | null;
  new_default_wait_timeout_ms: number;
};

export type ProjectConfigRepairReport = {
  scanned_projects: number;
  config_files: number;
  issue_count: number;
  repaired_count: number;
  dry_run: boolean;
  items: ProjectConfigRepairItem[];
  errors: string[];
};

export type OrphanPruneReport = {
  index_removed: number;
  threads_removed: number;
  family_branches_removed: number;
  families_removed: number;
  families_skipped: string[];
  dry_run: boolean;
};

export type HistoryOrphanReport = {
  provider: SessionProvider;
  history_path: string;
  session_count: number;
  history_rows: number;
  linked_rows: number;
  orphan_rows: number;
  untracked_rows: number;
  orphan_session_ids: string[];
};

export type HistoryPruneReport = {
  provider: SessionProvider;
  history_path: string;
  removed_rows: number;
  dry_run: boolean;
  orphan_session_ids: string[];
};

export type GuiVisibilityIssue = {
  session_id: string;
  path: string;
  project_dir: string;
  cwd: string;
  proposed_title: string;
  updated_at: number;
  file_size: number;
};

export type GuiVisibilityReport = {
  provider: SessionProvider;
  projects_root: string;
  scanned_sessions: number;
  visible_sessions: number;
  sidechain_sessions: number;
  empty_sessions: number;
  unfixable_sessions: number;
  issues: GuiVisibilityIssue[];
};

export type GuiVisibilityFixReport = {
  provider: SessionProvider;
  fixed: number;
  skipped: number;
  dry_run: boolean;
  fixed_session_ids: string[];
  errors: string[];
};

export type DiagnosticReport = {
  rollout_count: number;
  archived_rollout_count: number;
  index_count: number;
  threads_count: number;
  threads_active_count: number;
  threads_archived_count: number;
  rollout_ids: string[];
  index_ids: string[];
  threads_ids: string[];
  missing_in_index: string[];
  missing_in_threads: string[];
  orphan_in_index: string[];
  orphan_in_threads: string[];
  current_provider: string | null;
  provider_mismatched_families: number;
};

export type IndexRepairReport = {
  scanned: number;
  written: number;
  salvaged: number;
  dry_run: boolean;
  errors: string[];
};

export type ThreadsRebuildReport = {
  scanned: number;
  upserted: number;
  skipped: number;
  dry_run: boolean;
  errors: string[];
};

export type CloneReport = {
  source_id: string;
  new_id: string | null;
  new_rollout_path: string | null;
  new_provider: string;
  ok: boolean;
  skipped_reason: string | null;
  error: string | null;
};

export type ConvertReport = {
  source_id: string;
  source_provider: SessionProvider;
  target_provider: SessionProvider;
  conversion_mode: SessionConversionMode | null;
  new_id: string;
  new_path: string;
  resume_command: string;
  imported_messages: number;
  dropped_reasoning: number;
  tool_notes: number;
  warnings: string[];
};

export type SessionConversionMode = "simple" | "native";

export type SyncBranchReport = {
  active_id: string;
  source_id: string;
  appended_lines: number;
  total_lines: number;
};

export type BranchSyncReport = {
  source_id: string;
  target_id: string;
  appended_lines: number;
  total_lines: number;
};

export type BranchSyncRelation =
  | "current"
  | "same"
  | "branch_ahead"
  | "active_ahead"
  | "diverged"
  | "missing";

export type BranchSyncState = {
  branch_id: string;
  relation: BranchSyncRelation;
  active_lines: number | null;
  branch_lines: number | null;
  appendable_lines_to_active: number;
  appendable_lines_to_branch: number;
  error: string | null;
};

export type ForkSessionReport = {
  source_id: string;
  new_id: string;
  new_rollout_path: string;
  event_index: number;
  included_lines: number;
  cut_role: string;
  cut_kind: string;
  cut_summary: string;
};

export type DuplicateSessionReport = {
  source_id: string;
  new_id: string;
  new_rollout_path: string;
  total_lines: number;
};

export type SwitchStrategy = "continuous" | "scatter" | "follow";

// ========================= 家族树 =========================

export type BranchStatus = "active" | "archived" | "deleted";

export type FamilyBranch = {
  id: string;
  provider: string;
  created_at: string;
  status: BranchStatus;
  rollout_relpath: string;
  sha256: string | null;
  line_count: number | null;
  note: string | null;
};

export type Family = {
  family_id: string;
  root_id: string;
  title: string;
  chain: FamilyBranch[];
  active_id: string;
  updated_at: string;
};

export type FamilyStore = {
  version: number;
  families: Record<string, Family>;
  index: Record<string, string>;
};

export type FamilyIntegrityItem = {
  family_id: string;
  branch_id: string;
  ok: boolean;
  expected_sha: string | null;
  actual_sha: string | null;
  expected_lines: number | null;
  actual_lines: number | null;
  missing: boolean;
};

export type FamilyIntegrityReport = { items: FamilyIntegrityItem[]; all_ok: boolean };

// ========================= Bundle 导出 / 导入 =========================

export type BundleManifest = {
  version: number;
  provider: SessionProvider | null;
  session_id: string;
  rollout_relpath: string;
  source_relpath: string | null;
  sidecar_relpath: string | null;
  exported_at: string;
  updated_at: number;
  thread_name: string;
  session_cwd: string;
  session_source: string | null;
  session_originator: string | null;
  model_provider: string | null;
  export_machine: string;
  export_group: string;
  sha256_rollout: string;
  rollout_line_count: number;
  has_history: boolean;
  artifacts?: Array<{
    relpath: string;
    bytes: number;
    sha256: string;
  }>;
};

export type BundleExportTarget = {
  id: string;
  rollout_path?: string;
};

export type BundleListItem = {
  bundle_dir: string;
  manifest: BundleManifest;
  verified: boolean | null;
};

export type ExportReport = {
  session_id: string;
  ok: boolean;
  bundle_path: string | null;
  error: string | null;
  skipped_reason: string | null;
};

export type ImportMode = "keep_local" | "overwrite" | "skip";

export type ProjectPathMapping = {
  source_cwd: string;
  target_cwd: string;
};

export type ImportReport = {
  session_id: string;
  ok: boolean;
  rollout_written: boolean;
  history_appended: number;
  threads_upserted: boolean;
  index_appended: boolean;
  skipped_reason: string | null;
  error: string | null;
  verified: boolean;
  sha_mismatch: boolean;
};

export type CloneState = "matches" | "resync" | "clonable" | "has_clone" | "subagent" | "unknown";

export type FamilyOverlay = {
  session_id: string;
  provider: string | null;
  family_id: string | null;
  branch_count: number;
  is_active_branch: boolean;
  clone_state: CloneState;
};

export type ZipReport = { path: string; files: number; bytes: number };

// ========================= Markdown 导出 =========================

export type MarkdownExportOptions = {
  include_front_matter: boolean;
  include_reasoning: boolean;
  include_tools: boolean;
  ai_handoff_preamble: boolean;
  selected_indices?: number[] | null;
};

export type MarkdownExportHeader = {
  title: string;
  session_id: string;
  provider: SessionProvider;
  model?: string | null;
  reasoning_effort?: string | null;
  cwd?: string;
  created_at?: number;
  updated_at?: number;
  tokens_used?: number;
  resume_command?: string;
};

export type MarkdownExportReport = {
  ok: boolean;
  out_path: string | null;
  markdown: string;
  message_count: number;
  bytes: number;
};

export type PreviewImageData = {
  data_url: string;
  mime: string;
};

export const api = {
  appVersion: () => invokeCommand<string>("app_version"),
  checkAppUpdate: async (): Promise<AppUpdateInfo> => {
    if (isTauriRuntime()) {
      return invokeCommand<AppUpdateInfo>("check_app_update");
    }

    const [currentVersion, response] = await Promise.all([
      invokeCommand<string>("app_version"),
      fetch("https://api.github.com/repos/ccpopy/cc-sessions/releases/latest", {
        headers: { Accept: "application/vnd.github+json" },
      }),
    ]);
    if (!response.ok) {
      throw new Error(`GitHub 返回 ${response.status}`);
    }
    const payload = (await response.json()) as { tag_name?: unknown; html_url?: unknown };
    const latestTag = typeof payload.tag_name === "string" ? payload.tag_name : "";
    const htmlUrl = typeof payload.html_url === "string" ? payload.html_url : "";
    if (!latestTag || !htmlUrl) {
      throw new Error("GitHub Release 响应缺少 tag_name 或 html_url");
    }
    const latestVersion = normalizeVersion(latestTag);
    const available = compareVersions(latestVersion, currentVersion) > 0;
    return {
      current_version: currentVersion,
      latest_version: latestVersion,
      html_url: htmlUrl,
      available,
      can_auto_install: false,
      install_mode: "webui",
      install_dir: null,
      asset_name: null,
      message: available
        ? "CLI WebUI 无法自动替换桌面程序，请打开下载页面手动更新。"
        : null,
    };
  },
  installAppUpdate: async () => {
    if (!isTauriRuntime()) {
      throw new Error("CLI WebUI 无法自动安装桌面更新，请打开下载页面。");
    }
    return invokeCommand<void>("install_app_update");
  },
  getSettings: () => invokeCommand<Settings>("get_settings"),
  saveSettings: (settings: Settings) => invokeCommand<void>("save_settings", { settings }),
  openLatestReleasePage: async () => {
    if (isWebRuntime()) {
      window.open("https://github.com/ccpopy/cc-sessions/releases/latest", "_blank", "noopener,noreferrer");
      return;
    }
    return invokeCommand<void>("open_latest_release_page");
  },
  defaultCodexDir: () => invokeCommand<string>("default_codex_dir"),
  defaultClaudeDir: () => invokeCommand<string>("default_claude_dir"),
  validateCodexDir: (path: string) => invokeCommand<DirValidation>("validate_codex_dir", { path }),
  validateClaudeDir: (path: string) => invokeCommand<DirValidation>("validate_claude_dir", { path }),

  listSessions: (provider: SessionProvider, codexDir: string, claudeDir?: string) =>
    invokeCommand<SessionSummary[]>("list_sessions", { provider, codexDir, claudeDir }),
  groupByProject: (provider: SessionProvider, codexDir: string, claudeDir?: string) =>
    invokeCommand<ProjectGroup[]>("group_sessions_by_project", { provider, codexDir, claudeDir }),
  searchSessions: (provider: SessionProvider, codexDir: string, claudeDir: string | undefined, query: string) =>
    invokeCommand<SessionSummary[]>("search_sessions", { provider, codexDir, claudeDir, query }),
  startContentSearch: (p: {
    provider: SessionProvider;
    codexDir: string;
    claudeDir: string;
    query: string;
    rolloutPaths: readonly string[];
  }) => invokeCommand<{ job_id: number }>("start_content_search", p),
  contentSearchStatus: (jobId: number) =>
    invokeCommand<ContentSearchStatus>("content_search_status", { jobId }),
  activeContentSearch: () =>
    invokeCommand<{ job_id: number } | null>("active_content_search"),
  cancelContentSearch: (jobId: number) =>
    invokeCommand<void>("cancel_content_search", { jobId }),
  setArchived: (provider: SessionProvider, codexDir: string, id: string, v: boolean) =>
    invokeCommand<void>("set_archived", { provider, codexDir, id, v }),
  renameSession: (provider: SessionProvider, codexDir: string, id: string, title: string) =>
    invokeCommand<number>("rename_session", { provider, codexDir, id, title }),
  moveSessionCwd: (provider: SessionProvider, codexDir: string, id: string, targetCwd: string) =>
    invokeCommand<MoveSessionCwdReport>("move_session_cwd", { provider, codexDir, id, targetCwd }),
  deleteSession: (
    provider: SessionProvider,
    codexDir: string,
    id: string,
    claudeDir?: string,
    target?: DeleteTarget,
  ) => invokeCommand<DeleteResult>("delete_session", { provider, codexDir, claudeDir, id, target }),
  deleteSessions: (
    provider: SessionProvider,
    codexDir: string,
    ids: string[],
    claudeDir?: string,
    targets?: DeleteTarget[],
  ) =>
    invokeCommand<DeleteResult[]>("delete_sessions", {
      provider,
      codexDir,
      claudeDir,
      ids,
      targets,
    }),

  previewHead: (provider: SessionProvider, rolloutPath: string, limit: number) =>
    invokeCommand<PreviewEvent[]>("preview_session_head", { provider, rolloutPath, limit }),
  previewRange: (provider: SessionProvider, rolloutPath: string, offset: number, limit: number) =>
    invokeCommand<PreviewEvent[]>("preview_session_range", { provider, rolloutPath, offset, limit }),
  previewUserPrompts: (provider: SessionProvider, rolloutPath: string) =>
    invokeCommand<UserPromptList>("preview_session_user_prompts", { provider, rolloutPath }),
  previewMeta: (provider: SessionProvider, rolloutPath: string) =>
    invokeCommand<SessionMetaBrief>("preview_session_meta", { provider, rolloutPath }),
  readPreviewImage: (path: string) =>
    invokeCommand<PreviewImageData>("read_preview_image", { path }),

  exportSessionMarkdown: (p: {
    provider: SessionProvider;
    rollout_path: string;
    out_path?: string | null;
    header: MarkdownExportHeader;
    options: MarkdownExportOptions;
  }) =>
    invokeCommand<MarkdownExportReport>("export_session_markdown", {
      provider: p.provider,
      rolloutPath: p.rollout_path,
      outPath: p.out_path ?? null,
      header: p.header,
      options: p.options,
    }),

  createBackup: (p: {
    provider: SessionProvider;
    codex_dir: string;
    claude_dir?: string;
    backup_dir: string;
    ids: string[];
    targets?: BundleExportTarget[];
    name?: string;
    note?: string;
  }) =>
    invokeCommand<BackupSummary>("create_backup", {
      provider: p.provider,
      codexDir: p.codex_dir,
      claudeDir: p.claude_dir,
      backupDir: p.backup_dir,
      ids: p.ids,
      targets: p.targets,
      name: p.name,
      note: p.note,
    }),
  listBackups: (backupDir: string, provider?: SessionProvider) =>
    invokeCommand<BackupSummary[]>("list_backups", { backupDir, provider }),
  openBackup: (backupDir: string, backupPath: string) =>
    invokeCommand<BackupDetail>("open_backup", { backupDir, backupPath }),
  restoreSession: (p: {
    provider: SessionProvider;
    backup_dir: string;
    backup_path: string;
    codex_dir: string;
    claude_dir?: string;
    id: string;
    backup_rollout_relpath?: string;
    overwrite: boolean;
  }) =>
    invokeCommand<RestoreResult>("restore_session", {
      provider: p.provider,
      backupDir: p.backup_dir,
      backupPath: p.backup_path,
      codexDir: p.codex_dir,
      claudeDir: p.claude_dir,
      id: p.id,
      backupRolloutRelpath: p.backup_rollout_relpath,
      overwrite: p.overwrite,
    }),
  restoreAll: (p: {
    provider: SessionProvider;
    backup_dir: string;
    backup_path: string;
    codex_dir: string;
    claude_dir?: string;
    overwrite: boolean;
  }) =>
    invokeCommand<RestoreResult[]>("restore_all", {
      provider: p.provider,
      backupDir: p.backup_dir,
      backupPath: p.backup_path,
      codexDir: p.codex_dir,
      claudeDir: p.claude_dir,
      overwrite: p.overwrite,
    }),
  deleteBackup: (backupDir: string, backupPath: string) =>
    invokeCommand<void>("delete_backup", { backupDir, backupPath }),
  verifyBackup: (backupDir: string, backupPath: string) =>
    invokeCommand<VerifyReport>("verify_backup", { backupDir, backupPath }),

  statsKpi: (p: {
    provider: StatsProvider;
    codex_dir: string;
    claude_dir?: string;
    from_ts: number | null;
    to_ts: number | null;
    cwd_filter: string[];
    include_archived: boolean;
  }) =>
    invokeCommand<Kpi>("stats_kpi", {
      provider: p.provider,
      codexDir: p.codex_dir,
      claudeDir: p.claude_dir,
      fromTs: p.from_ts,
      toTs: p.to_ts,
      cwdFilter: p.cwd_filter,
      includeArchived: p.include_archived,
    }),
  statsTimeseries: (p: {
    provider: StatsProvider;
    codex_dir: string;
    claude_dir?: string;
    from_ts: number | null;
    to_ts: number | null;
    bucket: "day" | "week";
    cwd_filter: string[];
    include_archived: boolean;
  }) =>
    invokeCommand<TimeseriesPoint[]>("stats_timeseries", {
      provider: p.provider,
      codexDir: p.codex_dir,
      claudeDir: p.claude_dir,
      fromTs: p.from_ts,
      toTs: p.to_ts,
      bucket: p.bucket,
      cwdFilter: p.cwd_filter,
      includeArchived: p.include_archived,
    }),
  statsByProject: (p: {
    provider: StatsProvider;
    codex_dir: string;
    claude_dir?: string;
    from_ts: number | null;
    to_ts: number | null;
    limit: number;
    cwd_filter: string[];
    include_archived: boolean;
  }) =>
    invokeCommand<ProjectStat[]>("stats_by_project", {
      provider: p.provider,
      codexDir: p.codex_dir,
      claudeDir: p.claude_dir,
      fromTs: p.from_ts,
      toTs: p.to_ts,
      limit: p.limit,
      cwdFilter: p.cwd_filter,
      includeArchived: p.include_archived,
    }),
  statsByModel: (p: {
    provider: StatsProvider;
    codex_dir: string;
    claude_dir?: string;
    from_ts: number | null;
    to_ts: number | null;
    cwd_filter: string[];
    include_archived: boolean;
  }) =>
    invokeCommand<ModelStat[]>("stats_by_model", {
      provider: p.provider,
      codexDir: p.codex_dir,
      claudeDir: p.claude_dir,
      fromTs: p.from_ts,
      toTs: p.to_ts,
      cwdFilter: p.cwd_filter,
      includeArchived: p.include_archived,
    }),
  statsHeatmap: (p: {
    provider: StatsProvider;
    codex_dir: string;
    claude_dir?: string;
    from_ts: number | null;
    to_ts: number | null;
    cwd_filter: string[];
    include_archived: boolean;
  }) =>
    invokeCommand<number[][]>("stats_heatmap", {
      provider: p.provider,
      codexDir: p.codex_dir,
      claudeDir: p.claude_dir,
      fromTs: p.from_ts,
      toTs: p.to_ts,
      cwdFilter: p.cwd_filter,
      includeArchived: p.include_archived,
    }),

  revealCwd: (cwd: string) => invokeCommand<void>("reveal_cwd", { cwd }),
  copyResumeCommand: async (provider: SessionProvider, sessionId: string, cwd?: string) => {
    if (isWebRuntime()) {
      const text = resumeCommandText(provider, sessionId, cwd);
      await copyText(text);
      return text;
    }
    return invokeCommand<string>("copy_resume_command", { provider, sessionId, cwd });
  },

  // ========================= 修复 =========================
  getProviderInfo: (codexDir: string) => invokeCommand<ProviderInfo>("get_provider_info", { codexDir }),
  diagnoseProjectConfigs: (codexDir: string) =>
    invokeCommand<ProjectConfigReport>("diagnose_project_configs", { codexDir }),
  repairProjectConfigs: (codexDir: string, dryRun: boolean) =>
    invokeCommand<ProjectConfigRepairReport>("repair_project_configs", { codexDir, dryRun }),
  diagnoseCodexState: (codexDir: string) =>
    invokeCommand<DiagnosticReport>("diagnose_codex_state", { codexDir }),
  repairSessionIndex: (codexDir: string, dryRun: boolean) =>
    invokeCommand<IndexRepairReport>("repair_session_index", { codexDir, dryRun }),
  rebuildThreadsTable: (codexDir: string, dryRun: boolean) =>
    invokeCommand<ThreadsRebuildReport>("rebuild_threads_table", { codexDir, dryRun }),
  pruneOrphanEntries: (p: {
    codex_dir: string;
    prune_index: boolean;
    prune_threads: boolean;
    prune_family: boolean;
    dry_run: boolean;
  }) =>
    invokeCommand<OrphanPruneReport>("prune_orphan_entries", {
      codexDir: p.codex_dir,
      pruneIndex: p.prune_index,
      pruneThreads: p.prune_threads,
      pruneFamily: p.prune_family,
      dryRun: p.dry_run,
    }),
  diagnoseClaudeHistoryOrphans: (claudeDir: string) =>
    invokeCommand<HistoryOrphanReport>("diagnose_claude_history_orphans", { claudeDir }),
  pruneClaudeHistoryOrphans: (claudeDir: string, dryRun: boolean) =>
    invokeCommand<HistoryPruneReport>("prune_claude_history_orphans", { claudeDir, dryRun }),
  diagnoseClaudeGuiVisibility: (claudeDir: string) =>
    invokeCommand<GuiVisibilityReport>("diagnose_claude_gui_visibility", { claudeDir }),
  repairClaudeGuiVisibility: (claudeDir: string, dryRun: boolean, sessionIds?: string[]) =>
    invokeCommand<GuiVisibilityFixReport>("repair_claude_gui_visibility", {
      claudeDir,
      dryRun,
      sessionIds: sessionIds ?? null,
    }),
  convertSessionProvider: (p: {
    codex_dir: string;
    claude_dir: string;
    source_provider: SessionProvider;
    rollout_path: string;
    conversion_mode?: SessionConversionMode;
  }) =>
    invokeCommand<ConvertReport>("convert_session_provider", {
      codexDir: p.codex_dir,
      claudeDir: p.claude_dir,
      sourceProvider: p.source_provider,
      rolloutPath: p.rollout_path,
      conversionMode: p.conversion_mode ?? null,
    }),
  cloneSessionForProvider: (p: {
    codex_dir: string;
    session_id: string;
    target_provider?: string;
    strategy: SwitchStrategy;
    dry_run: boolean;
  }) =>
    invokeCommand<CloneReport>("clone_session_for_provider", {
      codexDir: p.codex_dir,
      sessionId: p.session_id,
      targetProvider: p.target_provider,
      strategy: p.strategy,
      dryRun: p.dry_run,
    }),
  forkSessionAtEvent: (p: {
    codex_dir: string;
    session_id: string;
    rollout_path: string;
    event_index: number;
  }) =>
    invokeCommand<ForkSessionReport>("fork_session_at_event", {
      codexDir: p.codex_dir,
      sessionId: p.session_id,
      rolloutPath: p.rollout_path,
      eventIndex: p.event_index,
    }),
  duplicateSession: (p: {
    codex_dir: string;
    session_id: string;
    rollout_path: string;
  }) =>
    invokeCommand<DuplicateSessionReport>("duplicate_session", {
      codexDir: p.codex_dir,
      sessionId: p.session_id,
      rolloutPath: p.rollout_path,
    }),
  planSessionEventDeletion: (provider: string, rolloutPath: string, lineNos: number[]) =>
    invokeCommand<DeletePlan>("plan_session_event_deletion", {
      provider,
      rolloutPath,
      lineNos,
    }),
  editSessionEventText: (p: {
    provider: string;
    rollout_path: string;
    session_id: string;
    backup_dir: string;
    line_no: number;
    new_text: string;
  }) =>
    invokeCommand<EditApplyReport>("edit_session_event_text", {
      provider: p.provider,
      rolloutPath: p.rollout_path,
      sessionId: p.session_id,
      backupDir: p.backup_dir,
      lineNo: p.line_no,
      newText: p.new_text,
    }),
  deleteSessionEvents: (p: {
    provider: string;
    rollout_path: string;
    session_id: string;
    backup_dir: string;
    line_nos: number[];
  }) =>
    invokeCommand<EditApplyReport>("delete_session_events", {
      provider: p.provider,
      rolloutPath: p.rollout_path,
      sessionId: p.session_id,
      backupDir: p.backup_dir,
      lineNos: p.line_nos,
    }),
  undoLastSessionEdit: (p: {
    provider: string;
    rollout_path: string;
    session_id: string;
    backup_dir: string;
  }) =>
    invokeCommand<EditApplyReport>("undo_last_session_edit", {
      provider: p.provider,
      rolloutPath: p.rollout_path,
      sessionId: p.session_id,
      backupDir: p.backup_dir,
    }),
  restoreSessionEditSnapshot: (p: {
    provider: string;
    rollout_path: string;
    session_id: string;
    backup_dir: string;
    snapshot_name: string;
  }) =>
    invokeCommand<EditApplyReport>("restore_session_edit_snapshot", {
      provider: p.provider,
      rolloutPath: p.rollout_path,
      sessionId: p.session_id,
      backupDir: p.backup_dir,
      snapshotName: p.snapshot_name,
    }),
  sessionEditHistory: (p: {
    provider: string;
    rollout_path: string;
    session_id: string;
    backup_dir: string;
  }) =>
    invokeCommand<EditHistory>("session_edit_history", {
      provider: p.provider,
      rolloutPath: p.rollout_path,
      sessionId: p.session_id,
      backupDir: p.backup_dir,
    }),
  getProviderSyncPlan: (codexDir: string) =>
    invokeCommand<string[]>("get_provider_sync_plan", { codexDir }),
  batchCloneForCurrentProvider: (p: {
    codex_dir: string;
    strategy: SwitchStrategy;
    dry_run: boolean;
  }) =>
    invokeCommand<CloneReport[]>("batch_clone_for_current_provider", {
      codexDir: p.codex_dir,
      strategy: p.strategy,
      dryRun: p.dry_run,
    }),
  rollbackFamilyActive: (codexDir: string, familyId: string, targetBranchId: string) =>
    invokeCommand<void>("rollback_family_active", { codexDir, familyId, targetBranchId }),
  deleteFamilyBranch: (codexDir: string, familyId: string, branchId: string) =>
    invokeCommand<DeleteResult>("delete_family_branch", { codexDir, familyId, branchId }),
  getFamilyBranchSyncStates: (codexDir: string, familyId: string) =>
    invokeCommand<BranchSyncState[]>("get_family_branch_sync_states", { codexDir, familyId }),
  syncBranchIntoActive: (codexDir: string, familyId: string, sourceBranchId: string) =>
    invokeCommand<SyncBranchReport>("sync_branch_into_active", {
      codexDir,
      familyId,
      sourceBranchId,
    }),
  syncActiveIntoBranch: (codexDir: string, familyId: string, targetBranchId: string) =>
    invokeCommand<BranchSyncReport>("sync_active_into_branch", {
      codexDir,
      familyId,
      targetBranchId,
    }),

  // ========================= 家族 =========================
  getFamilyStore: (codexDir: string) => invokeCommand<FamilyStore>("get_family_store", { codexDir }),
  verifyFamilyIntegrity: (codexDir: string) =>
    invokeCommand<FamilyIntegrityReport>("verify_family_integrity", { codexDir }),
  getSessionFamilyOverlay: (codexDir: string) =>
    invokeCommand<FamilyOverlay[]>("get_session_family_overlay", { codexDir }),

  // ========================= Bundle 导出 / 导入 =========================
  exportSessionBundles: (p: {
    provider: SessionProvider;
    codex_dir: string;
    claude_dir?: string;
    out_dir: string;
    ids: string[];
    targets?: BundleExportTarget[];
    machine_label?: string;
    export_group?: string;
  }) =>
    invokeCommand<ExportReport[]>("export_session_bundles", {
      provider: p.provider,
      codexDir: p.codex_dir,
      claudeDir: p.claude_dir,
      outDir: p.out_dir,
      ids: p.ids,
      targets: p.targets,
      machineLabel: p.machine_label,
      exportGroup: p.export_group,
    }),
  exportAllBundles: (p: {
    provider: SessionProvider;
    codex_dir: string;
    claude_dir?: string;
    out_dir: string;
    machine_label?: string;
    export_group?: string;
    active_only: boolean;
  }) =>
    invokeCommand<ExportReport[]>("export_all_bundles", {
      provider: p.provider,
      codexDir: p.codex_dir,
      claudeDir: p.claude_dir,
      outDir: p.out_dir,
      machineLabel: p.machine_label,
      exportGroup: p.export_group,
      activeOnly: p.active_only,
    }),
  listBundles: (srcDir: string, provider?: SessionProvider) =>
    invokeCommand<BundleListItem[]>("list_bundles", { srcDir, provider }),
  verifyBundlesCmd: (srcDir: string, provider?: SessionProvider) =>
    invokeCommand<BundleListItem[]>("verify_bundles", { srcDir, provider }),
  importSessionBundles: (p: {
    provider: SessionProvider;
    src_dir: string;
    codex_dir: string;
    claude_dir?: string;
    mode: ImportMode;
    make_visible: boolean;
    strict: boolean;
    project_mappings: ProjectPathMapping[];
  }) =>
    invokeCommand<ImportReport[]>("import_session_bundles", {
      provider: p.provider,
      srcDir: p.src_dir,
      codexDir: p.codex_dir,
      claudeDir: p.claude_dir,
      mode: p.mode,
      makeVisible: p.make_visible,
      strict: p.strict,
      projectMappings: p.project_mappings,
    }),
  packBundlesZip: (srcDir: string, zipPath: string) =>
    invokeCommand<ZipReport>("pack_bundles_zip", { srcDir, zipPath }),
  unpackZip: (zipPath: string, dstDir: string) =>
    invokeCommand<ZipReport>("unpack_zip", { zipPath, dstDir }),
  unpackZipToTemp: (zipPath: string) =>
    invokeCommand<ZipReport>("unpack_zip_to_temp", { zipPath }),
};
