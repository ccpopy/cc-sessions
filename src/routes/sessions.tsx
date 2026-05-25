import { useCallback, useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { toast } from "sonner";
import { Loader2, MessageSquare, RotateCw } from "lucide-react";
import { TopBar } from "@/components/TopBar";
import { SessionList } from "@/components/SessionList";
import { PreviewDialog } from "@/components/PreviewDialog";
import { BackupCreateDialog } from "@/components/BackupCreateDialog";
import { DangerDialog } from "@/components/DangerDialog";
import { EmptyState } from "@/components/EmptyState";
import { FamilyHistorySheet } from "@/components/FamilyHistorySheet";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Button } from "@/components/ui/button";
import { useSessions } from "@/hooks/useSessions";
import { useBackupIndex } from "@/hooks/useBackups";
import { useSettings } from "@/stores/settings";
import { useSelection } from "@/stores/selection";
import { useView } from "@/stores/view";
import { useHotkeys } from "@/hooks/useHotkeys";
import { api, type FamilyOverlay, type SessionProvider, type SessionSummary } from "@/lib/api";
import { humanBytes, humanTokens } from "@/lib/format";
import { basename } from "@/lib/cwd";

export default function SessionsRoute({ provider = "codex" }: { provider?: SessionProvider }) {
  const navigate = useNavigate();
  const settings = useSettings((s) => s.settings);
  const query = useView((s) => s.query);
  const { sessions, loading, error, refresh } = useSessions(provider, query);
  const backupIndex = useBackupIndex(provider);
  const selected = useSelection((s) => s.selected);
  const setSelection = useSelection((s) => s.set);
  const clearSelection = useSelection((s) => s.clear);
  const prefillCwd = useView((s) => s.prefillCwd);
  const setPrefill = useView((s) => s.setPrefillCwd);

  const [preview, setPreview] = useState<SessionSummary | null>(null);
  const [backupTargets, setBackupTargets] = useState<SessionSummary[]>([]);
  const [deleteTargets, setDeleteTargets] = useState<SessionSummary[]>([]);
  const [overlay, setOverlay] = useState<Map<string, FamilyOverlay>>(new Map());
  const [currentProvider, setCurrentProvider] = useState<string | null>(null);
  const [familySheetId, setFamilySheetId] = useState<string | null>(null);
  const [cloning, setCloning] = useState(false);
  const showBranchRecords = useMemo(() => isExplicitBranchRecordQuery(query), [query]);
  const isCodex = provider === "codex";
  const providerLabel = labelForProvider(provider);

  const refreshOverlay = useCallback(async () => {
    if (!settings?.codex_dir || !isCodex) return;
    try {
      const [ov, info] = await Promise.all([
        api.getSessionFamilyOverlay(settings.codex_dir),
        api.getProviderInfo(settings.codex_dir),
      ]);
      setOverlay(new Map(ov.map((o) => [o.session_id, o])));
      setCurrentProvider(info.current);
    } catch (e) {
      // overlay 读不到时（例如没 state_5.sqlite）静默
      console.warn("overlay refresh failed", e);
    }
  }, [settings?.codex_dir, isCodex]);

  useEffect(() => {
    void refreshOverlay();
  }, [refreshOverlay, sessions.length]);

  const clonableCount = useMemo(() => {
    let n = 0;
    for (const o of overlay.values()) if (isProviderMaintenanceState(o.clone_state)) n++;
    return n;
  }, [overlay]);

  const providerMaintenanceLabel = useMemo(() => {
    let providerSync = 0;
    let indexRepair = 0;
    for (const o of overlay.values()) {
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
  }, [overlay]);

  const visibleSessions = useMemo(() => {
    if (!isCodex || showBranchRecords) return sessions;
    return sessions.filter((s) => !isHiddenFamilyBranch(s, overlay.get(s.id)));
  }, [sessions, overlay, showBranchRecords, isCodex]);

  useEffect(() => {
    if (selected.size === 0) return;
    const visibleIds = new Set(visibleSessions.map((s) => s.id));
    const next = Array.from(selected).filter((id) => visibleIds.has(id));
    if (next.length !== selected.size) setSelection(next);
  }, [selected, setSelection, visibleSessions]);

  const onCloneOne = useCallback(
    async (s: SessionSummary) => {
      if (!settings || !currentProvider) return;
      setCloning(true);
      try {
        const r = await api.cloneSessionForProvider({
          codex_dir: settings.codex_dir,
          session_id: s.id,
          target_provider: currentProvider,
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
        setCloning(false);
      }
    },
    [settings, currentProvider, refresh, refreshOverlay],
  );

  const onBatchClone = useCallback(async () => {
    if (!settings || !currentProvider || !isCodex) return;
    setCloning(true);
    try {
      const r = await api.batchCloneForCurrentProvider({
        codex_dir: settings.codex_dir,
        strategy: "scatter",
        dry_run: false,
      });
      const ok = r.filter((x) => x.ok).length;
      toast.success(`已处理 ${ok}/${r.length}`);
      await refresh();
      await refreshOverlay();
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setCloning(false);
    }
  }, [settings, currentProvider, isCodex, refresh, refreshOverlay]);

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
        const ids = Array.from(selected);
        setDeleteTargets(sessions.filter((s) => ids.includes(s.id)));
      },
    },
  ]);

  const selectedItems = useMemo(
    () => visibleSessions.filter((s) => selected.has(s.id)),
    [visibleSessions, selected],
  );

  const onCopyResume = async (s: SessionSummary) => {
    try {
      const text = s.resume_command || (await api.copyResumeCommand(s.provider, s.id));
      await navigator.clipboard.writeText(text);
      toast.success("已复制：" + text);
    } catch (e: any) {
      toast.error("复制失败：" + String(e?.message ?? e));
    }
  };

  const onReveal = async (s: SessionSummary) => {
    try {
      await api.revealCwd(s.cwd);
    } catch (e: any) {
      toast.error("打开失败：" + String(e?.message ?? e));
    }
  };

  const onArchiveToggle = async (s: SessionSummary) => {
    if (!settings) return;
    try {
      await api.setArchived(provider, settings.codex_dir, s.id, !s.archived);
      toast.success(s.archived ? "已取消归档" : "已归档");
      await refresh();
    } catch (e: any) {
      toast.error(String(e?.message ?? e));
    }
  };

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
        title={`${providerLabel} 会话`}
        stats={loading ? "加载中…" : `${visibleSessions.length} 条`}
        onRefresh={refresh}
        onBulkBackup={onBulkBackup}
        onBulkDelete={onBulkDelete}
        showListTools
      />

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
            一键处理
          </Button>
        </div>
      )}

      <ScrollArea className="flex-1">
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
            title={query ? "无匹配结果" : "尚无会话"}
            description={
              query
                ? "尝试清除搜索或换个关键字"
                : provider === "codex"
                  ? "打开 Codex 后会自动出现在此"
                  : provider === "claude"
                    ? "打开 Claude Code 后会自动出现在此"
                    : "打开 Gemini CLI 或 Antigravity 后会自动出现在此"
            }
            icon={<MessageSquare className="h-10 w-10" />}
          />
        ) : (
          <SessionList
            sessions={visibleSessions}
            backupIndex={backupIndex}
            overlay={overlay}
            currentProvider={currentProvider}
            onPreview={setPreview}
            onCopyResume={onCopyResume}
            onRevealCwd={onReveal}
            onArchiveToggle={isCodex ? onArchiveToggle : undefined}
            onBackup={(s) => setBackupTargets([s])}
            onDelete={(s) => setDeleteTargets([s])}
            onClone={isCodex ? onCloneOne : undefined}
            onOpenFamily={isCodex ? (s) => setFamilySheetId(s.id) : undefined}
          />
        )}
      </ScrollArea>

      <PreviewDialog
        open={!!preview}
        onOpenChange={(v) => !v && setPreview(null)}
        session={preview}
        codexDir={settings.codex_dir}
        onForked={async () => {
          setPreview(null);
          await refresh();
          await refreshOverlay();
        }}
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
            provider === "gemini" ? settings.gemini_dir : settings.claude_dir,
          );
          const okCount = r.filter((x) => x.ok).length;
          const failed = r.filter((x) => !x.ok);
          const rolloutMissing = r.filter((x) => x.ok && x.rollout_missing);
          const rolloutFailed = r.filter((x) => x.ok && !x.rollout_deleted && !x.rollout_missing);
          const cleanupFailed = r.filter((x) => x.ok && x.error);
          if (okCount > 0) toast.success(`已删除 ${okCount}/${r.length}`);
          if (rolloutMissing.length) {
            toast.info(`${rolloutMissing.length} 条 rollout 文件原本不存在，数据库记录已删除`);
          }
          if (cleanupFailed.length) {
            const title =
              rolloutFailed.length === cleanupFailed.length
                ? `${rolloutFailed.length} 条 rollout 文件删除失败，请手动处理`
                : `${cleanupFailed.length} 条删除完成，但部分附属清理失败`;
            toast.warning(title, {
              description: cleanupFailed
                .map((x) => x.error ?? x.id)
                .slice(0, 3)
                .join("\n"),
            });
          }
          if (failed.length) {
            const desc = failed
              .map((x) => x.error ?? x.id)
              .slice(0, 3)
              .join("\n");
            if (failed.length === r.length) {
              throw new Error(desc || "没有会话被删除");
            }
            toast.error(`${failed.length} 条会话删除失败`, { description: desc });
          }
          clearSelection();
          await refresh();
        }}
      >
        <DeleteSummary targets={deleteTargets} provider={provider} />
      </DangerDialog>
    </>
  );
}

/**
 * 主会话列表只显示家族的"当前分支"（active）。
 * - active 分支：照常显示，并带"N 分支"徽标作为历史/恢复入口
 * - 非 active 分支（不论 Scatter 模式下未归档、还是 Continuous 模式下已归档）：默认隐藏
 *   通过显式查询 `id:` / `archived:`、或者点击徽标进入 FamilyHistorySheet 才能找到
 * - 没有 family_id（孤儿会话）：照常显示
 */
function isHiddenFamilyBranch(_s: SessionSummary, overlay?: FamilyOverlay): boolean {
  if (!overlay?.family_id) return false;
  return !overlay.is_active_branch;
}

function isExplicitBranchRecordQuery(query: string): boolean {
  const q = query.trim().toLowerCase();
  return q.startsWith("id:") || q.startsWith("archived:");
}

function isProviderMaintenanceState(cloneState: string): boolean {
  return cloneState === "clonable" || cloneState === "resync";
}

function DeleteSummary({
  targets,
  provider,
}: {
  targets: SessionSummary[];
  provider: SessionProvider;
}) {
  const totalLogs = targets.reduce((a, b) => a + b.logs_count, 0);
  const totalBytes = targets.reduce((a, b) => a + b.rollout_bytes, 0);
  const totalTokens = targets.reduce((a, b) => a + b.tokens_used, 0);
  return (
    <div className="min-w-0 max-w-full space-y-2 wrap-anywhere">
      <div className="min-w-0 whitespace-normal">
        {provider === "codex" ? (
          <>
            将删除 <b>{targets.length}</b> 条 threads 记录、
            <b>{totalLogs}</b> 条日志、
            <b>{targets.length}</b> 个 rollout 文件（共 <b>{humanBytes(totalBytes)}</b>，
            <b>{humanTokens(totalTokens)}</b> token）。
          </>
        ) : provider === "claude" ? (
          <>
            将删除 <b>{targets.length}</b> 个 jsonl 会话文件
            （共 <b>{humanBytes(totalBytes)}</b>，
            <b>{humanTokens(totalTokens)}</b> token），同名 sidecar 目录会一并清理。
          </>
        ) : (
          <>
            将删除 <b>{targets.length}</b> 个 Gemini / Antigravity 会话文件
            （共 <b>{humanBytes(totalBytes)}</b>），并清理同 ID 的 protobuf 摘要、标注和 CLI 日志行。
          </>
        )}
      </div>
      <div className="text-destructive">此操作不可撤销，也不会自动备份。</div>
      {targets.length > 1 && (
        <ul className="max-h-36 min-w-0 max-w-full space-y-0.5 overflow-y-auto overflow-x-hidden rounded-md border bg-muted/30 p-2 text-xs">
          {targets.slice(0, 5).map((t) => (
            <li key={t.id} className="grid min-w-0 grid-cols-[max-content_minmax(0,1fr)] items-center gap-2">
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

function labelForProvider(provider: SessionProvider): string {
  if (provider === "claude") return "Claude";
  if (provider === "gemini") return "Gemini";
  return "Codex";
}
