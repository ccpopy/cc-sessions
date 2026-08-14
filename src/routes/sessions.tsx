import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { toast } from "sonner";
import {
  AlertTriangle,
  Archive,
  ChevronDown,
  ListChecks,
  ListFilter,
  Loader2,
  MessageSquare,
  Network,
  RotateCw,
} from "lucide-react";
import { TopBar } from "@/components/TopBar";
import { SessionList } from "@/components/SessionList";
import { PreviewDialog } from "@/components/PreviewDialog";
import type { PreviewJump } from "@/components/PreviewDialog";
import { ContentSearchDialog } from "@/components/ContentSearchDialog";
import { MarkdownExportDialog } from "@/components/MarkdownExportDialog";
import { BackupCreateDialog } from "@/components/BackupCreateDialog";
import { ConvertSessionDialog } from "@/components/ConvertSessionDialog";
import { DangerDialog } from "@/components/DangerDialog";
import { RenameSessionDialog } from "@/components/RenameSessionDialog";
import { MoveSessionCwdDialog } from "@/components/MoveSessionCwdDialog";
import { ForkRecommendationDialog } from "@/components/ForkRecommendationDialog";
import { EmptyState } from "@/components/EmptyState";
import { FamilyHistorySheet } from "@/components/FamilyHistorySheet";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useSessions } from "@/hooks/useSessions";
import { useBackupIndex } from "@/hooks/useBackups";
import { useSettings } from "@/stores/settings";
import { useSelection } from "@/stores/selection";
import { useView } from "@/stores/view";
import { useHotkeys } from "@/hooks/useHotkeys";
import {
  api,
  type ArchiveLedgerEntry,
  type ArchiveOrigin,
  type ContentSearchMatch,
  type DeleteResult,
  type FamilyOverlay,
  type ProviderSyncStatus,
  type SessionProvider,
  type SessionSummary,
} from "@/lib/api";
import { humanBytes, humanTokens } from "@/lib/format";
import { basename } from "@/lib/cwd";
import { sessionIdentity } from "@/lib/sessionIdentity";
import { providerLabel } from "@/lib/providerTheme";
import {
  ProviderSyncRegistry,
  type ProviderSyncSnapshot,
} from "@/lib/providerSyncRegistry";
import { SessionActionRegistry } from "@/lib/sessionActionRegistry";
import {
  selectSessionsForView,
  type ArchivedOriginGroupKey,
} from "@/lib/sessionVisibility";

// 归档视图来源筛选 chips：单选互斥，"all" 表示不过滤
const archiveOriginFilters: { key: ArchivedOriginGroupKey | "all"; label: string }[] = [
  { key: "all", label: "全部" },
  { key: "mine", label: "我的归档" },
  { key: "auto", label: "工具自动归档" },
  { key: "migrated", label: "迁移记录" },
  { key: "unknown", label: "未知来源" },
];

export default function SessionsRoute({ provider = "codex" }: { provider?: SessionProvider }) {
  const navigate = useNavigate();
  const settings = useSettings((s) => s.settings);
  const query = useView((s) => s.query);
  const { sessions, allSessions, loading, error, refresh } = useSessions(provider, query);
  const isCodex = provider === "codex";
  const isOpenCode = provider === "opencode";
  const supportsArchive = isCodex || isOpenCode;
  const { index: backupIndex, error: backupIndexError } = useBackupIndex(provider);
  const selected = useSelection((s) => s.selected);
  const setSelection = useSelection((s) => s.set);
  const clearSelection = useSelection((s) => s.clear);
  const prefillCwd = useView((s) => s.prefillCwd);
  const setPrefill = useView((s) => s.setPrefillCwd);
  const showSubagentSessions = useView((s) => s.showSubagentSessions);
  const setShowSubagentSessions = useView((s) => s.setShowSubagentSessions);
  const showArchivedSessions = useView((s) => s.showArchivedSessions);
  const setShowArchivedSessions = useView((s) => s.setShowArchivedSessions);

  const setSubagentView = useCallback(
    (enabled: boolean) => {
      if (enabled) setShowArchivedSessions(false);
      setShowSubagentSessions(enabled);
    },
    [setShowArchivedSessions, setShowSubagentSessions],
  );
  const setArchivedView = useCallback(
    (enabled: boolean) => {
      if (enabled) setShowSubagentSessions(false);
      setShowArchivedSessions(enabled);
    },
    [setShowArchivedSessions, setShowSubagentSessions],
  );

  const [preview, setPreview] = useState<SessionSummary | null>(null);
  const [previewJump, setPreviewJump] = useState<PreviewJump | null>(null);
  const [contentSearchOpen, setContentSearchOpen] = useState(false);
  const [exportTarget, setExportTarget] = useState<SessionSummary | null>(null);
  const [renameTarget, setRenameTarget] = useState<SessionSummary | null>(null);
  const [moveTarget, setMoveTarget] = useState<SessionSummary | null>(null);
  const [duplicateTarget, setDuplicateTarget] = useState<SessionSummary | null>(null);
  const [convertTarget, setConvertTarget] = useState<SessionSummary | null>(null);
  const [backupTargets, setBackupTargets] = useState<SessionSummary[]>([]);
  const [deleteTargets, setDeleteTargets] = useState<SessionSummary[]>([]);
  const [overlay, setOverlay] = useState<Map<string, FamilyOverlay>>(new Map());
  const [providerSyncPlan, setProviderSyncPlan] = useState<string[]>([]);
  const [currentProvider, setCurrentProvider] = useState<string | null>(null);
  const [maintenanceError, setMaintenanceError] = useState<string | null>(null);
  const [providerSyncProgress, setProviderSyncProgress] = useState<ProviderSyncStatus | null>(null);
  const [familySheetId, setFamilySheetId] = useState<string | null>(null);
  // 归档来源 ledger：归档视图按来源分组的前置数据；读取失败时回退为全部"未知来源"
  const [ledgerBySession, setLedgerBySession] = useState<Map<string, ArchiveOrigin>>(new Map());
  const [archiveLedgerError, setArchiveLedgerError] = useState<string | null>(null);
  // 归档视图来源筛选 chips：单选互斥，"all" 表示不过滤
  const [originFilter, setOriginFilter] = useState<ArchivedOriginGroupKey | "all">("all");
  const providerSyncRegistry = useRef(new ProviderSyncRegistry());
  const duplicateRegistry = useRef(new SessionActionRegistry());
  const [providerSyncState, setProviderSyncState] = useState<ProviderSyncSnapshot>(() =>
    providerSyncRegistry.current.snapshot(),
  );
  const [duplicatingSessionIds, setDuplicatingSessionIds] = useState<ReadonlySet<string>>(
    () => duplicateRegistry.current.snapshot(),
  );
  const sessionScrollRef = useRef<HTMLDivElement>(null);
  const publishProviderSyncState = useCallback(() => {
    setProviderSyncState(providerSyncRegistry.current.snapshot());
  }, []);
  const publishDuplicateState = useCallback(() => {
    setDuplicatingSessionIds(duplicateRegistry.current.snapshot());
  }, []);
  const cloning =
    providerSyncState.batchActive || providerSyncState.sessionIds.size > 0;
  const showHiddenRecords = useMemo(() => isExplicitHiddenRecordQuery(query), [query]);
  const hasActiveVisibilityFilter = showSubagentSessions || (supportsArchive && showArchivedSessions);
  const overlayScope = JSON.stringify([provider, settings?.codex_dir ?? ""]);
  const refreshScope = JSON.stringify([
    provider,
    settings?.codex_dir ?? "",
    settings?.claude_dir ?? "",
    settings?.opencode_dir ?? "",
    query,
  ]);
  const overlayScopeRef = useRef(overlayScope);
  const refreshScopeRef = useRef(refreshScope);
  const overlayRequestSeq = useRef(0);

  // 两种视图互斥；同时为 true 只可能来自热更新前的旧状态。
  useEffect(() => {
    if (showSubagentSessions && showArchivedSessions) {
      setShowArchivedSessions(false);
    }
  }, [setShowArchivedSessions, showArchivedSessions, showSubagentSessions]);
  const refreshAllRequestSeq = useRef(0);
  const overlayFlight = useRef<{
    scope: string;
    requestId: number;
    promise: Promise<void>;
  } | null>(null);
  const refreshAllFlight = useRef<{
    scope: string;
    requestId: number;
    promise: Promise<void>;
  } | null>(null);
  overlayScopeRef.current = overlayScope;
  refreshScopeRef.current = refreshScope;

  useEffect(() => {
    clearSelection();
    setPreview(null);
    setPreviewJump(null);
    setContentSearchOpen(false);
    setExportTarget(null);
    setRenameTarget(null);
    setMoveTarget(null);
    setBackupTargets([]);
    setDeleteTargets([]);
    setFamilySheetId(null);
  }, [provider, clearSelection]);

  const refreshOverlay = useCallback((): Promise<void> => {
    const codexDir = settings?.codex_dir;
    if (!codexDir || !isCodex) return Promise.resolve();

    const active = overlayFlight.current;
    if (active?.scope === overlayScope) return active.promise;

    const requestId = ++overlayRequestSeq.current;
    // ledger 单独捕获失败：来源分组是展示层增强，读取失败只降级为"全部未知来源"，
    // 不能连带 overlay/provider 状态一起失败（计划 §F3 失败降级要求）。
    const ledgerPromise = api
      .getArchiveLedger(codexDir)
      .then((ledger) => {
        if (overlayScopeRef.current !== overlayScope) return;
        setLedgerBySession(new Map(ledger.map((entry) => [entry.session_id, entry.origin])));
        setArchiveLedgerError(null);
      })
      .catch((error) => {
        if (overlayScopeRef.current !== overlayScope) return;
        setLedgerBySession(new Map());
        setArchiveLedgerError(String((error as Error)?.message ?? error));
      });
    const promise: Promise<void> = Promise.all([
      api.getSessionFamilyOverlay(codexDir),
      api.getProviderInfo(codexDir),
      api.getProviderSyncPlan(codexDir),
      ledgerPromise,
    ])
      .then(([ov, info, syncPlan]) => {
        if (overlayScopeRef.current !== overlayScope) return;
        setOverlay(new Map(ov.map((o) => [o.session_id, o])));
        setCurrentProvider(info.current);
        setProviderSyncPlan(syncPlan);
        setMaintenanceError(null);
      })
      .catch((error) => {
        if (overlayScopeRef.current !== overlayScope) return;
        setOverlay(new Map());
        setCurrentProvider(null);
        setProviderSyncPlan([]);
        setMaintenanceError(String((error as Error)?.message ?? error));
      })
      .finally(() => {
        const current = overlayFlight.current;
        if (current?.scope === overlayScope && current.requestId === requestId) {
          overlayFlight.current = null;
        }
      });
    overlayFlight.current = { scope: overlayScope, requestId, promise };
    return promise;
  }, [isCodex, overlayScope, settings?.codex_dir]);

  useEffect(() => {
    void refreshOverlay();
  }, [refreshOverlay]);

  const refreshAll = useCallback((): Promise<void> => {
    const active = refreshAllFlight.current;
    if (active?.scope === refreshScope) return active.promise;

    const requestId = ++refreshAllRequestSeq.current;
    const promise: Promise<void> = Promise.resolve()
      .then(async () => {
        await refresh();
        if (refreshScopeRef.current !== refreshScope) return;
        await refreshOverlay();
      })
      .finally(() => {
        const current = refreshAllFlight.current;
        if (current?.scope === refreshScope && current.requestId === requestId) {
          refreshAllFlight.current = null;
        }
      });
    refreshAllFlight.current = { scope: refreshScope, requestId, promise };
    return promise;
  }, [refresh, refreshOverlay, refreshScope]);

  useEffect(() => {
    // Codex 通常在另一个窗口持续写入会话；用户切回列表时单次刷新。
    // 不轮询，避免再次引入大目录重复扫描和高资源占用。
    const refreshOnWindowFocus = () => {
      void refreshAll();
    };
    window.addEventListener("focus", refreshOnWindowFocus);
    return () => window.removeEventListener("focus", refreshOnWindowFocus);
  }, [refreshAll]);

  const providerMaintenanceItems = useMemo(() => {
    const items = new Map<string, FamilyOverlay>();
    for (const item of overlay.values()) {
      if (!isProviderMaintenanceState(item.clone_state)) continue;
      if (item.family_id && !item.is_active_branch) continue;
      const key = item.family_id ? `family:${item.family_id}` : `session:${item.session_id}`;
      items.set(key, item);
    }
    return Array.from(items.values());
  }, [overlay]);
  const clonableCount = providerSyncPlan.length;

  const providerMaintenanceLabel = useMemo(() => {
    let providerSync = 0;
    let indexRepair = 0;
    for (const o of providerMaintenanceItems) {
      if (o.clone_state === "clonable") providerSync++;
      if (o.clone_state === "resync") indexRepair++;
    }
    if (providerSync > 0 && indexRepair > 0) {
      return "需要同步到当前 provider 或修复本地索引";
    }
    if (indexRepair > 0) {
      return "需要修复本地索引可见性";
    }
    return "需要同步到当前 provider";
  }, [providerMaintenanceItems]);

  const visibleSessions = useMemo(() => {
    return selectSessionsForView(sessions, overlay, currentProvider, {
      provider,
      showSubagentSessions,
      showArchivedSessions,
      includeHiddenRecords: showHiddenRecords,
    });
  }, [
    sessions,
    overlay,
    showHiddenRecords,
    isCodex,
    showSubagentSessions,
    showArchivedSessions,
    currentProvider,
  ]);

  const searchResultIds = useMemo(
    () => visibleSessions.map(sessionIdentity),
    [visibleSessions],
  );
  const selectedSearchResultCount = useMemo(
    () => searchResultIds.filter((id) => selected.has(id)).length,
    [searchResultIds, selected],
  );
  const canSelectSearchResults =
    query.trim().length > 0 && !loading && searchResultIds.length > 0;
  const allSearchResultsSelected =
    canSelectSearchResults && selectedSearchResultCount === searchResultIds.length;
  const toggleSearchResultSelection = useCallback(() => {
    if (allSearchResultsSelected) clearSelection();
    else setSelection(searchResultIds);
  }, [allSearchResultsSelected, clearSelection, searchResultIds, setSelection]);

  const contentSearchRolloutPaths = useMemo(() => {
    return selectSessionsForView(allSessions, overlay, currentProvider, {
      provider,
      showSubagentSessions,
      showArchivedSessions,
    })
      .map((session) => session.rollout_path)
      .sort((left, right) => left.localeCompare(right));
  }, [
    allSessions,
    currentProvider,
    overlay,
    provider,
    showArchivedSessions,
    showSubagentSessions,
  ]);

  useEffect(() => {
    if (selected.size === 0) return;
    const visibleIds = new Set(visibleSessions.map(sessionIdentity));
    const next = Array.from(selected).filter((id) => visibleIds.has(id));
    if (next.length !== selected.size) setSelection(next);
  }, [selected, setSelection, visibleSessions]);

  const onDuplicate = useCallback(
    async (s: SessionSummary) => {
      if (!settings) return;
      if (!duplicateRegistry.current.tryBegin(s.id)) return;
      publishDuplicateState();
      try {
        const report = await api.duplicateSession({
          codex_dir: settings.codex_dir,
          session_id: s.id,
          rollout_path: s.rollout_path,
        });
        toast.success("已完整 Fork 会话", {
          description: `新会话 ${report.new_id.slice(0, 8)}，共 ${report.total_lines} 行`,
        });
        await refresh();
        await refreshOverlay();
      } catch (e: any) {
        toast.error("Fork 会话失败", { description: String(e?.message ?? e) });
      } finally {
        duplicateRegistry.current.finish(s.id);
        publishDuplicateState();
      }
    },
    [settings, publishDuplicateState, refresh, refreshOverlay],
  );

  const confirmDuplicate = useCallback(async () => {
    if (!duplicateTarget) return;
    await onDuplicate(duplicateTarget);
    setDuplicateTarget(null);
  }, [duplicateTarget, onDuplicate]);

  const onCloneOne = useCallback(
    async (s: SessionSummary) => {
      if (!settings || !currentProvider) return;
      if (!providerSyncRegistry.current.tryBeginSession(s.id)) return;
      publishProviderSyncState();
      try {
        const r = await api.cloneSessionForProvider({
          codex_dir: settings.codex_dir,
          session_id: s.id,
          strategy: "scatter",
          dry_run: false,
        });
        if (r.ok) {
          toast.success(
            r.skipped_reason
              ? `已跳过：${r.skipped_reason}`
              : `已克隆到 ${r.new_provider}`,
          );
          await refresh();
          await refreshOverlay();
        } else {
          toast.error(r.error ?? "克隆失败");
        }
      } catch (e) {
        toast.error(String((e as Error)?.message ?? e));
      } finally {
        providerSyncRegistry.current.finishSession(s.id);
        publishProviderSyncState();
      }
    },
    [settings, currentProvider, publishProviderSyncState, refresh, refreshOverlay],
  );

  const onBatchClone = useCallback(async () => {
    if (!settings || !currentProvider || !isCodex) return;
    if (!providerSyncRegistry.current.tryBeginBatch()) return;
    publishProviderSyncState();
    try {
      setProviderSyncProgress(null);
      const r = await api.runProviderSync({
        codex_dir: settings.codex_dir,
        strategy: "scatter",
        dry_run: false,
      }, setProviderSyncProgress);
      const ok = r.filter((x) => x.ok).length;
      const failed = r.filter((x) => !x.ok);
      await refresh();
      await refreshOverlay();
      if (r.length === 0) {
        toast.info("当前没有需要同步的会话");
      } else if (ok > 0) {
        toast.success(`已处理 ${ok}/${r.length}`);
      }
      if (failed.length > 0) {
        const description = failed
          .map((item) => item.error ?? `${item.source_id} 未处理`)
          .slice(0, 3)
          .join("\n");
        if (failed.length === r.length) {
          throw new Error(description || "所有会话同步均失败");
        }
        toast.error(`${failed.length} 条会话同步失败`, { description });
      }
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setProviderSyncProgress(null);
      providerSyncRegistry.current.finishBatch();
      publishProviderSyncState();
    }
  }, [settings, currentProvider, isCodex, publishProviderSyncState, refresh, refreshOverlay]);

  useHotkeys([
    {
      combo: "mod+k",
      handler: (e) => {
        e.preventDefault();
        (document.querySelector('input[placeholder*="搜索"]') as HTMLInputElement | null)?.focus();
      },
    },
    {
      combo: "delete",
      handler: () => {
        if (selected.size === 0) return;
        setDeleteTargets(sessions.filter((s) => selected.has(sessionIdentity(s))));
      },
    },
  ]);

  const selectedItems = useMemo(
    () => visibleSessions.filter((s) => selected.has(sessionIdentity(s))),
    [visibleSessions, selected],
  );

  const onPreview = useCallback((session: SessionSummary) => {
    setPreviewJump(null);
    setPreview(session);
  }, []);

  const onBackup = useCallback((session: SessionSummary) => {
    setBackupTargets([session]);
  }, []);

  const onDelete = useCallback((session: SessionSummary) => {
    setDeleteTargets([session]);
  }, []);

  const onOpenFamily = useCallback((session: SessionSummary) => {
    setFamilySheetId(session.id);
  }, []);

  const onCopyResume = useCallback(async (s: SessionSummary) => {
    try {
      const text = await api.copyResumeCommand(s.provider, s.id, s.cwd);
      toast.success("已复制：" + text);
    } catch (e: any) {
      toast.error("复制失败：" + String(e?.message ?? e));
    }
  }, []);

  const onReveal = useCallback(async (s: SessionSummary) => {
    try {
      await api.revealCwd(s.cwd);
    } catch (e: any) {
      toast.error("打开失败：" + String(e?.message ?? e));
    }
  }, []);

  const onArchiveToggle = useCallback(async (s: SessionSummary) => {
    if (!settings) return;
    try {
      await api.setArchived(
        provider,
        settings.codex_dir,
        s.id,
        !s.archived,
        settings.opencode_dir,
      );
      toast.success(s.archived ? "已取消归档" : "已归档");
      await refreshAll();
    } catch (e: any) {
      toast.error(String(e?.message ?? e));
    }
  }, [provider, refreshAll, settings]);

  const onBulkBackup = () => {
    if (selectedItems.length === 0) return;
    setBackupTargets(selectedItems);
  };
  const onBulkDelete = () => {
    if (selectedItems.length === 0) return;
    setDeleteTargets(selectedItems);
  };

  useEffect(() => {
    return () => clearSelection();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  if (!settings) {
    return (
      <div className="flex h-full items-center justify-center">
        <Loader2 className="h-6 w-6 animate-spin text-muted-foreground" />
      </div>
    );
  }

  return (
    <>
      <TopBar
        title={`${providerLabel(provider)} 会话`}
        stats={loading ? "加载中…" : `${visibleSessions.length} 条`}
        onRefresh={refreshAll}
        refreshing={loading}
        onBulkBackup={onBulkBackup}
        onBulkDelete={onBulkDelete}
        onContentSearch={() => setContentSearchOpen(true)}
        showListTools
      >
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              variant="outline"
              size="sm"
              className="h-8 shrink-0 gap-1.5 border-border/70 bg-muted/30 px-2 text-xs font-normal text-muted-foreground shadow-none data-[state=open]:bg-accent data-[state=open]:text-accent-foreground xl:px-2.5"
              aria-label={
                hasActiveVisibilityFilter ? "显示更多，当前已启用筛选" : "显示更多"
              }
              title="显示更多"
            >
              <ListFilter className="h-3.5 w-3.5" />
              <span className="hidden xl:inline">显示更多</span>
              <ChevronDown className="hidden h-3.5 w-3.5 opacity-60 xl:block" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent
            align="start"
            style={{
              width: canSelectSearchResults
                ? "max(var(--radix-dropdown-menu-trigger-width), 10rem)"
                : "var(--radix-dropdown-menu-trigger-width)",
              minWidth: "var(--radix-dropdown-menu-trigger-width)",
            }}
          >
            {canSelectSearchResults && (
              <>
                <DropdownMenuCheckboxItem
                  checked={allSearchResultsSelected}
                  onCheckedChange={toggleSearchResultSelection}
                  className="gap-1.5 px-2 [&>span:first-child]:hidden"
                >
                  <ListChecks className="h-3.5 w-3.5 text-muted-foreground" />
                  <span className="whitespace-nowrap">
                    {allSearchResultsSelected ? "取消全选" : "全选搜索结果"}
                  </span>
                  <span className="ml-auto text-[10px] tabular-nums text-muted-foreground">
                    {searchResultIds.length}
                  </span>
                </DropdownMenuCheckboxItem>
                <DropdownMenuSeparator />
              </>
            )}
            <DropdownMenuCheckboxItem
              checked={showSubagentSessions}
              onCheckedChange={(checked) => setSubagentView(checked === true)}
              className="gap-1.5 px-2 [&>span:first-child]:hidden"
            >
              <Network className="h-3.5 w-3.5 text-muted-foreground" />
              <span className="whitespace-nowrap">{isOpenCode ? "子会话" : "子代理"}</span>
              <span
                aria-hidden="true"
                data-state={showSubagentSessions ? "checked" : "unchecked"}
                className="ml-auto inline-flex h-3.5 w-6 shrink-0 items-center rounded-full bg-input p-0.5 transition-colors data-[state=checked]:bg-primary"
              >
                <span
                  className={`block h-2.5 w-2.5 rounded-full bg-background shadow-sm transition-transform ${
                    showSubagentSessions ? "translate-x-2.5" : "translate-x-0"
                  }`}
                />
              </span>
            </DropdownMenuCheckboxItem>
            {supportsArchive && (
              <DropdownMenuCheckboxItem
                checked={showArchivedSessions}
                onCheckedChange={(checked) => setArchivedView(checked === true)}
                className="gap-1.5 px-2 [&>span:first-child]:hidden"
              >
                <Archive className="h-3.5 w-3.5 text-muted-foreground" />
                <span className="whitespace-nowrap">已归档</span>
                <span
                  aria-hidden="true"
                  data-state={showArchivedSessions ? "checked" : "unchecked"}
                  className="ml-auto inline-flex h-3.5 w-6 shrink-0 items-center rounded-full bg-input p-0.5 transition-colors data-[state=checked]:bg-primary"
                >
                  <span
                    className={`block h-2.5 w-2.5 rounded-full bg-background shadow-sm transition-transform ${
                      showArchivedSessions ? "translate-x-2.5" : "translate-x-0"
                    }`}
                  />
                </span>
              </DropdownMenuCheckboxItem>
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      </TopBar>

      {prefillCwd && (
        <div className="flex shrink-0 items-center gap-2 border-b bg-muted/40 px-6 py-2 text-xs">
          <span className="text-muted-foreground">已过滤项目：</span>
          <code className="rounded bg-background px-1.5 py-0.5 font-mono">{prefillCwd}</code>
          <button
            className="ml-auto text-muted-foreground hover:text-foreground"
            onClick={() => setPrefill(null)}
          >
            清除过滤
          </button>
        </div>
      )}

      {isCodex && maintenanceError && (
        <div className="flex shrink-0 items-start gap-2 border-b border-amber-500/30 bg-amber-500/10 px-6 py-2 text-xs text-amber-800 dark:text-amber-300">
          <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span className="min-w-0 break-words">
            Provider 与分支状态读取失败，相关同步操作已停用：{maintenanceError}
          </span>
        </div>
      )}

      {backupIndexError && (
        <div className="flex shrink-0 items-start gap-2 border-b border-amber-500/30 bg-amber-500/10 px-6 py-2 text-xs text-amber-800 dark:text-amber-300">
          <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span className="min-w-0 break-words">部分备份索引读取失败：{backupIndexError}</span>
        </div>
      )}

      {isCodex && clonableCount > 0 && currentProvider && (
        <div className="flex shrink-0 items-center gap-2 border-b bg-blue-500/5 px-6 py-2 text-xs">
          <RotateCw className="h-3.5 w-3.5 text-blue-600" />
          <span className="text-foreground">
            有 <b>{clonableCount}</b> 条会话{providerMaintenanceLabel}
            {providerMaintenanceLabel !== "需要修复本地索引可见性" && (
              <>
                {" "}
                <code className="rounded bg-background px-1.5 py-0.5 font-mono">{currentProvider}</code>
              </>
            )}
          </span>
          <Button
            size="sm"
            variant="outline"
            disabled={cloning}
            onClick={onBatchClone}
            className="ml-auto h-7 gap-1.5"
          >
            <RotateCw className={cloning ? "h-3.5 w-3.5 animate-spin" : "h-3.5 w-3.5"} />
            {providerSyncProgress
              ? providerSyncProgress.total > 0
                ? `处理中 ${providerSyncProgress.completed}/${providerSyncProgress.total}`
                : "正在扫描…"
              : providerSyncState.batchActive
                ? "正在准备…"
              : "一键处理"}
          </Button>
        </div>
      )}

      {isCodex && showArchivedSessions && (
        <>
          {archiveLedgerError && (
            <div className="flex shrink-0 items-start gap-2 border-b border-amber-500/30 bg-amber-500/10 px-6 py-2 text-xs text-amber-800 dark:text-amber-300">
              <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
              <span className="min-w-0 break-words">
                归档来源读取失败，已按"未知来源"分组显示：{archiveLedgerError}
              </span>
            </div>
          )}
          <div className="flex shrink-0 flex-wrap items-center gap-1.5 border-b bg-muted/20 px-6 py-2 text-xs">
            <span className="text-muted-foreground">归档来源：</span>
            {archiveOriginFilters.map((filter) => (
              <button
                key={filter.key}
                type="button"
                onClick={() => setOriginFilter(filter.key)}
                className={`rounded-full border px-2.5 py-0.5 text-[11px] transition-colors ${
                  originFilter === filter.key
                    ? "border-foreground/30 bg-foreground/10 text-foreground"
                    : "border-border/60 text-muted-foreground hover:text-foreground"
                }`}
              >
                {filter.label}
              </button>
            ))}
          </div>
        </>
      )}

      <ScrollArea className="flex-1" viewportRef={sessionScrollRef}>
        {error ? (
          <EmptyState
            title="读取失败"
            description={error}
            icon={<MessageSquare className="h-10 w-10" />}
          />
        ) : loading && visibleSessions.length === 0 ? (
          <div className="flex h-[60vh] items-center justify-center">
            <Loader2 className="h-6 w-6 animate-spin text-muted-foreground" />
          </div>
        ) : visibleSessions.length === 0 ? (
          <EmptyState
            title={
              query
                ? "无匹配结果"
                : showSubagentSessions
                  ? isOpenCode
                    ? "尚无子会话"
                    : "尚无子代理会话"
                  : "尚无会话"
            }
            description={
              query
                ? "尝试清除搜索或换个关键字"
                : showSubagentSessions
                  ? isOpenCode
                    ? "OpenCode 产生子会话后会自动出现在此"
                    : "产生子代理后会自动出现在此"
                : provider === "codex"
                  ? "打开 Codex 后会自动出现在此"
                  : provider === "claude"
                    ? "打开 Claude Code 后会自动出现在此"
                    : "在 OpenCode 中创建会话后会自动出现在此"
            }
            icon={<MessageSquare className="h-10 w-10" />}
          />
        ) : (
          <SessionList
            sessions={visibleSessions}
            scrollElementRef={sessionScrollRef}
            backupIndex={backupIndex}
            overlay={overlay}
            currentProvider={currentProvider}
            syncingSessionIds={providerSyncState.sessionIds}
            syncActionsDisabled={providerSyncState.batchActive}
            duplicatingSessionIds={duplicatingSessionIds}
            archivedGrouping={
              isCodex && showArchivedSessions ? { ledgerBySession, originFilter } : undefined
            }
            onPreview={onPreview}
            onCopyResume={onCopyResume}
            onRevealCwd={onReveal}
            onArchiveToggle={supportsArchive ? onArchiveToggle : undefined}
            onBackup={onBackup}
            onDelete={onDelete}
            onClone={isCodex ? onCloneOne : undefined}
            onDuplicate={isCodex ? setDuplicateTarget : undefined}
            onOpenFamily={isCodex ? onOpenFamily : undefined}
            onExportMarkdown={setExportTarget}
            onConvert={isOpenCode ? undefined : setConvertTarget}
            onRename={setRenameTarget}
            onMoveCwd={setMoveTarget}
          />
        )}
      </ScrollArea>

      <ContentSearchDialog
        open={contentSearchOpen}
        onOpenChange={setContentSearchOpen}
        provider={provider}
        codexDir={settings.codex_dir}
        claudeDir={settings.claude_dir}
        opencodeDir={settings.opencode_dir}
        showSubagentSessions={showSubagentSessions}
        showArchivedSessions={showArchivedSessions}
        rolloutPaths={contentSearchRolloutPaths}
        onOpenResult={(session, match: ContentSearchMatch, searchQuery) => {
          setPreviewJump({
            eventIndex: match.event_index,
            eventOffset: match.event_offset,
            query: searchQuery,
          });
          setContentSearchOpen(false);
          setPreview(session);
        }}
      />

      <PreviewDialog
        open={!!preview}
        onOpenChange={(v) => {
          if (!v) {
            setPreview(null);
            setPreviewJump(null);
          }
        }}
        session={preview}
        initialJump={previewJump}
        allSessions={allSessions}
        codexDir={settings.codex_dir}
        backupDir={settings.backup_dir}
        onForked={async () => {
          setPreview(null);
          await refresh();
          await refreshOverlay();
        }}
        onEdited={async () => {
          await refresh();
          await refreshOverlay();
        }}
      />

      <MarkdownExportDialog
        open={!!exportTarget}
        onOpenChange={(v) => !v && setExportTarget(null)}
        session={exportTarget}
      />

      <RenameSessionDialog
        open={!!renameTarget}
        onOpenChange={(v) => !v && setRenameTarget(null)}
        session={renameTarget}
        onSubmit={async (title) => {
          if (!settings || !renameTarget) return;
          try {
            const renamed = await api.renameSession({
              provider,
              codex_dir: settings.codex_dir,
              claude_dir: settings.claude_dir,
              opencode_dir: settings.opencode_dir,
              id: renameTarget.id,
              rollout_path: renameTarget.rollout_path,
              title,
            });
            toast.success(renamed > 1 ? `已重命名（同步 ${renamed} 个分支）` : "已重命名");
            await refresh();
          } catch (e: any) {
            toast.error("重命名失败：" + String(e?.message ?? e));
            throw e;
          }
        }}
      />

      <MoveSessionCwdDialog
        open={!!moveTarget}
        onOpenChange={(v) => !v && setMoveTarget(null)}
        session={moveTarget}
        onSubmit={async (targetCwd) => {
          if (!settings || !moveTarget) return;
          try {
            const r = await api.moveSessionCwd({
              provider,
              codex_dir: settings.codex_dir,
              claude_dir: settings.claude_dir,
              opencode_dir: settings.opencode_dir,
              id: moveTarget.id,
              rollout_path: moveTarget.rollout_path,
              target_cwd: targetCwd,
            });
            toast.success(
              `已移动到新项目：${basename(r.new_cwd)}` +
              (provider === "codex" && r.threads_updated > 1
                ? `（同步 ${r.threads_updated} 个分支）`
                : provider === "opencode" && r.threads_updated > 1
                  ? `（包含 ${r.threads_updated - 1} 个子会话）`
                  : provider === "claude" && r.artifacts_moved > 1
                    ? `（移动 ${r.artifacts_moved} 项会话资产）`
                    : ""),
            );
            if (provider === "codex" && !r.desktop_project_synced) {
              toast.warning("Core 已移动，但 Codex App 全局状态未初始化，项目归属未同步");
            }
            if (r.requires_project_open) {
              toast.info("首次在目标目录启动 OpenCode 时，会由 OpenCode 自动登记真实项目 ID");
            }
            await refresh();
          } catch (e: any) {
            toast.error("移动失败：" + String(e?.message ?? e));
            throw e;
          }
        }}
      />

      <ForkRecommendationDialog
        open={!!duplicateTarget}
        onOpenChange={(value) => !value && setDuplicateTarget(null)}
        session={duplicateTarget}
        running={!!duplicateTarget && duplicatingSessionIds.has(duplicateTarget.id)}
        onConfirm={confirmDuplicate}
      />

      <FamilyHistorySheet
        open={!!familySheetId}
        onOpenChange={(v) => !v && setFamilySheetId(null)}
        sessionId={familySheetId}
        codexDir={settings.codex_dir}
        currentProvider={currentProvider}
        onChanged={async () => {
          await refresh();
          await refreshOverlay();
        }}
      />

      <ConvertSessionDialog
        target={convertTarget}
        onOpenChange={(v) => !v && setConvertTarget(null)}
        onDone={() => {
          void refresh();
        }}
      />

      <BackupCreateDialog
        open={backupTargets.length > 0}
        onOpenChange={(v) => !v && setBackupTargets([])}
        provider={provider}
        sessions={backupTargets}
        onDone={(backupPath) => {
          clearSelection();
          void refresh();
          const backupName = basename(backupPath);
          navigate(`/${provider}/backups/${encodeURIComponent(backupName)}`, {
            state: { path: backupPath },
          });
        }}
      />

      <DangerDialog
        open={deleteTargets.length > 0}
        onOpenChange={(v) => !v && setDeleteTargets([])}
        title={deleteTargets.length === 1 ? "删除会话" : `删除 ${deleteTargets.length} 条会话`}
        confirmText={deleteTargets.length === 1 ? "删除" : "全部删除"}
        onConfirm={async () => {
          if (!settings) return;
          const ids = deleteTargets.map((s) => s.id);
          const r = await api.deleteSessions(
            provider,
            settings.codex_dir,
            ids,
            settings.claude_dir,
            deleteTargets.map((session) => ({
              id: session.id,
              rollout_path: session.rollout_path,
            })),
            settings.opencode_dir,
          );
          const okCount = r.filter((x) => x.ok).length;
          const failed = r.filter((x) => !x.ok);
          const partiallyDeleted = failed.filter(deleteResultHasMutation);
          const rolloutMissing = r.filter((x) => x.ok && x.rollout_missing);
          const sharedPreserved = rolloutMissing.filter((x) => x.shared_data_preserved);
          const missingCleaned = rolloutMissing.filter((x) => !x.shared_data_preserved);
          clearSelection();
          await refreshAll();
          if (okCount > 0) toast.success(`已删除 ${okCount}/${r.length}`);
          if (sharedPreserved.length) {
            toast.info(
              `${sharedPreserved.length} 个会话文件原本不存在，但同 ID 副本仍在，共享历史与附属数据已保留`,
            );
          }
          if (missingCleaned.length) {
            toast.info(
              provider === "codex"
                ? `${missingCleaned.length} 条 rollout 文件原本不存在，数据库和索引残留已清理`
                : `${missingCleaned.length} 个会话文件原本不存在，会话级历史与附属残留已清理`,
            );
          }
          if (failed.length) {
            const details = failed
              .map((x) => x.error ?? x.id)
              .slice(0, 3)
              .join("\n");
            const partialNotice = partiallyDeleted.length > 0
              ? `${partiallyDeleted.length} 条已经删除了部分文件或索引；列表已按当前磁盘状态刷新。`
              : "";
            const desc = [partialNotice, details].filter(Boolean).join("\n");
            if (failed.length === r.length) {
              throw new Error(
                ["没有会话被完整删除。", desc || "列表已刷新，未发现已完成的删除。"]
                  .filter(Boolean)
                  .join("\n"),
              );
            }
            toast.error(`${failed.length} 条会话未删除干净`, { description: desc });
          }
        }}
      >
        <DeleteSummary targets={deleteTargets} provider={provider} overlay={overlay} />
      </DangerDialog>
    </>
  );
}

function deleteResultHasMutation(result: DeleteResult): boolean {
  return result.rollout_deleted
    || result.sidecar_deleted
    || result.tasks_deleted
    || result.file_history_deleted
    || result.threads_rows_deleted > 0
    || result.logs_rows_deleted > 0
    || result.history_rows_deleted > 0;
}

function isExplicitHiddenRecordQuery(query: string): boolean {
  const q = query.trim().toLowerCase();
  return q.startsWith("id:") || q.startsWith("archived:");
}

function isProviderMaintenanceState(cloneState: string): boolean {
  return cloneState === "clonable" || cloneState === "resync";
}

function DeleteSummary({
  targets,
  provider,
  overlay,
}: {
  targets: SessionSummary[];
  provider: SessionProvider;
  overlay: Map<string, FamilyOverlay>;
}) {
  const totalBytes = targets.reduce((a, b) => a + b.rollout_bytes, 0);
  const totalTokens = targets.reduce((a, b) => a + b.tokens_used, 0);
  const familiesToDelete = new Map<string, number>();
  for (const target of targets) {
    const item = overlay.get(target.id);
    if (!item?.family_id || !item.is_active_branch) continue;
    familiesToDelete.set(
      item.family_id,
      Math.max(familiesToDelete.get(item.family_id) ?? 0, item.branch_count || 1),
    );
  }
  const totalFamilyBranches = Array.from(familiesToDelete.values()).reduce(
    (sum, count) => sum + count,
    0,
  );
  const scopedTargets = targets.filter((target) => {
    const familyId = overlay.get(target.id)?.family_id;
    return !familyId || !familiesToDelete.has(familyId);
  }).length;
  const deletesFamily = provider === "codex" && familiesToDelete.size > 0;
  return (
    <div className="min-w-0 max-w-full space-y-2 wrap-anywhere">
      <div className="min-w-0 whitespace-normal">
        {deletesFamily ? (
          <>
            将整组删除 <b>{familiesToDelete.size}</b> 条 active 会话及其全部{" "}
            <b>{totalFamilyBranches}</b> 个 provider/历史分支
            {scopedTargets > 0 && <>，并单独删除另外 <b>{scopedTargets}</b> 个所选会话或历史分支</>}。
            只有选中 family 的 active 分支才会整组删除；单独选中的非 active 历史分支不会影响当前 active 会话。
          </>
        ) : provider === "codex" ? (
          <>
            将删除 <b>{targets.length}</b> 个所选会话或历史分支对应的 threads、日志、rollout 与索引。
            非 active 历史分支只删除自身，不会影响同一 family 的当前 active 会话。
          </>
        ) : provider === "claude" ? (
          <>
            将删除 <b>{targets.length}</b> 个 jsonl 会话文件
            （共 <b>{humanBytes(totalBytes)}</b>，
            <b>{humanTokens(totalTokens)}</b> token）及同名 sidecar。删除最后一个同 ID 副本时，还会清理
            history.jsonl 匹配行、tasks/&lt;id&gt; 与 file-history/&lt;id&gt;；仍有同 ID 副本时这些共享数据会保留。
            项目 memory、.claude.json、session-env 和 shell-snapshots 不会被删除。
          </>
        ) : (
          <>
            将从 <code>opencode.db</code> 删除 <b>{targets.length}</b> 个会话及其消息、片段和关联记录。
            OpenCode 使用 SQLite 外键级联清理，不会触碰项目工作区文件。
          </>
        )}
      </div>
      <div className="text-destructive">此操作不可撤销，也不会自动备份。</div>
      {targets.length > 1 && (
        <ul className="max-h-36 min-w-0 max-w-full space-y-0.5 overflow-y-auto overflow-x-hidden rounded-md border bg-muted/30 p-2 text-xs">
          {targets.slice(0, 5).map((t) => (
            <li key={sessionIdentity(t)} className="grid min-w-0 grid-cols-[max-content_minmax(0,1fr)] items-center gap-2">
              <code className="shrink-0 font-mono text-[10px]">{t.id.slice(0, 8)}</code>
              <span className="min-w-0 truncate" title={t.title || "(无标题)"}>
                {t.title || "(无标题)"}
              </span>
            </li>
          ))}
          {targets.length > 5 && (
            <li className="text-muted-foreground">…还有 {targets.length - 5} 条</li>
          )}
        </ul>
      )}
    </div>
  );
}
