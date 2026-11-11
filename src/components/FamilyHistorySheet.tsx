import { useCallback, useEffect, useRef, useState } from "react";
import {
  CheckCircle2,
  GitBranch,
  Loader2,
  Radio,
  Trash2,
} from "lucide-react";
import { toast } from "sonner";
import { DESKTOP_DELETE_RESTART_NOTICE } from "@/lib/desktopRestart";

import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";

import { api, type ArchiveOrigin, type BranchSyncState, type Family, type FamilyBranch } from "@/lib/api";
import { ArchiveOriginBadge } from "@/components/ArchiveOriginBadge";

type SyncTarget = {
  branch: FamilyBranch;
  direction: "into_active" | "into_branch";
};

type Props = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  sessionId: string | null;
  codexDir: string;
  currentProvider: string | null;
  onChanged?: () => void;
};

export function FamilyHistorySheet({
  open,
  onOpenChange,
  sessionId,
  codexDir,
  currentProvider,
  onChanged,
}: Props) {
  const [family, setFamily] = useState<Family | null>(null);
  const [syncStates, setSyncStates] = useState<Record<string, BranchSyncState>>({});
  const [ledgerOriginBySession, setLedgerOriginBySession] = useState<ReadonlyMap<string, ArchiveOrigin>>(
    new Map(),
  );
  const [loading, setLoading] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<FamilyBranch | null>(null);
  const [running, setRunning] = useState(false);
  const requestSeq = useRef(0);
  const mutationInFlight = useRef(false);
  const scope = JSON.stringify([open, sessionId, codexDir]);
  const scopeRef = useRef(scope);
  scopeRef.current = scope;

  const load = useCallback(async () => {
    const requestId = ++requestSeq.current;
    setFamily(null);
    setSyncStates({});
    setDeleteTarget(null);
    if (!sessionId || !codexDir) {
      setLoading(false);
      return;
    }
    setLoading(true);
    try {
      const [store, ledgerEntries] = await Promise.all([
        api.getFamilyStore(codexDir),
        api.getArchiveLedger(codexDir),
      ]);
      if (requestSeq.current !== requestId) return;
      // 徽标显示与来源筛选共用同一数据源：ledger 优先（与列表 SessionCard 一致）。
      // 历史版本的存量会话可能只有 ledger 记录，family 分支字段为空。
      setLedgerOriginBySession(new Map(ledgerEntries.map((e) => [e.session_id, e.origin])));
      const fid = store.index[sessionId];
      if (!fid) {
        setFamily(null);
        setSyncStates({});
      } else {
        const nextFamily = store.families[fid] ?? null;
        setFamily(nextFamily);
        if (nextFamily) {
          const states = await api.getFamilyBranchSyncStates(codexDir, nextFamily.family_id);
          if (requestSeq.current !== requestId) return;
          const byId = Object.fromEntries(states.map((s) => [s.branch_id, s]));
          setSyncStates(byId);
          setFamily({
            ...nextFamily,
            chain: nextFamily.chain.map((branch) => {
              const path = byId[branch.id]?.rollout_relpath ?? branch.rollout_relpath;
              return {
                ...branch,
                provider: byId[branch.id]?.provider ?? branch.provider,
                rollout_relpath: path,
                // 登记的校验值属于原文件，不能移用到续写文件。
                ...(path !== branch.rollout_relpath ? { sha256: null, line_count: null } : {}),
              };
            }),
          });
        } else {
          setSyncStates({});
        }
      }
    } catch (e) {
      if (requestSeq.current === requestId) {
        setFamily(null);
        setSyncStates({});
        toast.error(String((e as Error)?.message ?? e));
      }
    } finally {
      if (requestSeq.current === requestId) setLoading(false);
    }
  }, [sessionId, codexDir]);

  useEffect(() => {
    if (open) {
      void load();
    } else {
      requestSeq.current += 1;
      setFamily(null);
      setSyncStates({});
      setDeleteTarget(null);
      setLoading(false);
    }
  }, [open, load]);

  const doRollback = async (rollbackTarget: FamilyBranch) => {
    if (!family || mutationInFlight.current) return;
    mutationInFlight.current = true;
    setRunning(true);
    try {
      const syncState = syncStates[rollbackTarget.id];
      let synchronizedLines = 0;
      if (canSyncIntoBranch(syncState)) {
        const result = await api.syncActiveIntoBranch(
          codexDir,
          family.family_id,
          rollbackTarget.id,
        );
        synchronizedLines = result.appended_lines;
      }
      await api.rollbackFamilyActive(codexDir, family.family_id, rollbackTarget.id);
      toast.success(
        synchronizedLines > 0
          ? `已同步 ${synchronizedLines} 行并切换到 ${rollbackTarget.provider} 分支`
          : `已切换到 ${rollbackTarget.provider} 分支`,
      );
      if (scopeRef.current === scope) await load();
      onChanged?.();
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      mutationInFlight.current = false;
      setRunning(false);
    }
  };

  const doDelete = async () => {
    if (!family || !deleteTarget || mutationInFlight.current) return;
    mutationInFlight.current = true;
    setRunning(true);
    try {
      const result = await api.deleteFamilyBranch(codexDir, family.family_id, deleteTarget.id);
      if (scopeRef.current === scope) await load();
      await onChanged?.();
      if (!result.ok) {
        throw new Error(result.error ?? `分支 ${deleteTarget.id} 未删除干净`);
      }
      if (result.desktop_restart_required) {
        toast.warning(`已删除 ${deleteTarget.provider} 分支，但 Desktop 列表尚未刷新`, {
          description: DESKTOP_DELETE_RESTART_NOTICE,
        });
      } else {
        toast.success(`已删除 ${deleteTarget.provider} 分支`);
      }
      setDeleteTarget(null);
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      mutationInFlight.current = false;
      setRunning(false);
    }
  };

  const doSync = async (syncTarget: SyncTarget) => {
    if (!family || mutationInFlight.current) return;
    mutationInFlight.current = true;
    setRunning(true);
    try {
      if (syncTarget.direction === "into_active") {
        const r = await api.syncBranchIntoActive(codexDir, family.family_id, syncTarget.branch.id);
        toast.success(
          `已把 ${syncTarget.branch.provider} 分支的 ${r.appended_lines} 行增量合并到当前分支（共 ${r.total_lines} 行）`,
        );
      } else {
        const r = await api.syncActiveIntoBranch(codexDir, family.family_id, syncTarget.branch.id);
        toast.success(
          `已把当前分支的 ${r.appended_lines} 行增量同步到 ${syncTarget.branch.provider} 分支（共 ${r.total_lines} 行）`,
        );
      }
      if (scopeRef.current === scope) await load();
      onChanged?.();
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      mutationInFlight.current = false;
      setRunning(false);
    }
  };

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent
        side="right"
        className="flex min-w-0 w-[min(560px,92vw)] max-w-none flex-col overflow-hidden sm:max-w-none"
      >
        <SheetHeader className="min-w-0 pr-8">
          <SheetTitle className="flex min-w-0 items-center gap-2 text-base">
            <GitBranch className="h-4 w-4" />
            会话分支
            {family && (
              <Badge variant="outline" className="h-5 shrink-0 px-1.5 font-normal">
                {family.chain.length} 分支
              </Badge>
            )}
          </SheetTitle>
          <SheetDescription className="sr-only">
            查看、切换和同步会话分支。
          </SheetDescription>
        </SheetHeader>

        <Separator className="my-3" />

        <ScrollArea
          className="min-w-0 max-w-full flex-1 overflow-hidden pr-2"
          viewportClassName="overflow-x-hidden [&>div]:!block [&>div]:min-w-0 [&>div]:max-w-full"
        >
          {loading ? (
            <div className="flex items-center gap-1.5 py-6 text-xs text-muted-foreground">
              <Loader2 className="h-3.5 w-3.5 animate-spin" /> 加载中…
            </div>
          ) : !family ? (
            <div className="py-8 text-center text-xs text-muted-foreground">
              暂无分支
            </div>
          ) : (
            <div className="min-w-0 max-w-full space-y-3 overflow-hidden">
              <h3 className="min-w-0 break-words text-sm font-medium" title={family.family_id}>
                {family.title || "未命名会话"}
              </h3>

              <div className="min-w-0 max-w-full space-y-2 overflow-hidden">
                {family.chain.map((b, idx) => {
                  const isActive = b.id === family.active_id;
                  const isCurrent =
                    currentProvider != null && b.provider === currentProvider;
                  const syncState = syncStates[b.id];
                  return (
                    <div
                      key={b.id}
                      className={
                        isActive
                          ? "min-w-0 rounded-md border-2 border-emerald-500/40 bg-emerald-500/5 p-3"
                          : "min-w-0 rounded-md border bg-card p-3"
                      }
                    >
                      <div className="flex min-w-0 items-start gap-2">
                        <div
                          className={
                            isActive
                              ? "mt-0.5 flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-emerald-500/20 text-emerald-600"
                              : "mt-0.5 flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-muted text-muted-foreground"
                          }
                        >
                          {isActive ? (
                            <Radio className="h-3.5 w-3.5" />
                          ) : (
                            <span className="text-[10px] font-semibold">
                              {idx + 1}
                            </span>
                          )}
                        </div>
                        <div className="min-w-0 flex-1 space-y-1">
                          <div className="flex min-w-0 flex-wrap items-center gap-1.5 text-xs">
                            <Badge
                              variant="outline"
                              className={
                                isCurrent
                                  ? "h-5 border-emerald-500/30 px-1.5 font-normal text-emerald-600"
                                  : "h-5 px-1.5 font-normal"
                              }
                            >
                              {b.provider || "未知服务商"}
                            </Badge>
                            {isCurrent && (
                              <Badge variant="outline" className="h-5 px-1.5 font-normal text-emerald-600">
                                当前服务
                              </Badge>
                            )}
                            {isActive ? (
                              <Badge className="h-5 bg-emerald-500 px-1.5 font-normal text-white">
                                当前分支
                              </Badge>
                            ) : (
                              <Badge variant="outline" className="h-5 px-1.5 font-normal text-muted-foreground">
                                {inactiveBranchStatusLabel(b.status)}
                              </Badge>
                            )}
                            {!isActive && b.status === "archived" && (
                              <ArchiveOriginBadge
                                origin={ledgerOriginBySession.get(b.id) ?? b.archive_origin ?? null}
                                onSetOrigin={async (origin) => {
                                  await api.setArchiveOrigin(codexDir, b.id, origin);
                                  await load();
                                  await onChanged?.();
                                }}
                              />
                            )}
                            <span className="text-muted-foreground">
                              {safeDate(b.created_at)}
                            </span>
                          </div>
                          <details className="text-[11px] text-muted-foreground">
                            <summary className="w-fit cursor-pointer py-1 hover:text-foreground">文件信息</summary>
                            <div className="space-y-1 pt-1 font-mono wrap-anywhere">
                              <div>{b.id}</div>
                              <div>{b.rollout_relpath}</div>
                              {(b.sha256 || b.line_count != null) && (
                                <div className="flex items-center gap-1.5">
                                  {b.sha256 && <><CheckCircle2 className="h-3 w-3 text-emerald-500" /><span>{b.sha256.slice(0, 10)}…</span></>}
                                  {b.line_count != null && <span>{b.line_count} 行</span>}
                                </div>
                              )}
                            </div>
                          </details>
                          {!isActive && syncState && (
                            <div className="flex min-w-0 flex-wrap items-center gap-1.5 text-[11px] text-muted-foreground">
                              <BranchSyncBadge state={syncState} />
                            </div>
                          )}
                        </div>
                      </div>
                      {!isActive && (
                        <div className="mt-2 flex flex-wrap justify-end gap-2">
                          {syncState?.relation === "branch_ahead" && syncState.appendable_lines_to_active > 0 && (
                            <Button
                              variant="ghost"
                              size="sm"
                              disabled={running}
                              onClick={() => void doSync({ branch: b, direction: "into_active" })}
                              className="shrink-0 gap-1.5"
                              title="将此分支相对当前分支的新增内容追加到当前分支（不切换 provider）"
                            >
                              合并到当前
                            </Button>
                          )}
                          {canSyncIntoBranch(syncState) && (
                            <Button
                              variant="ghost"
                              size="sm"
                              disabled={running}
                              onClick={() => void doSync({ branch: b, direction: "into_branch" })}
                              className="shrink-0 gap-1.5"
                              title="将当前分支的新增内容追加到此历史分支（不切换 provider）"
                            >
                              同步到此分支
                            </Button>
                          )}
                          <Button
                            variant="outline"
                            size="sm"
                            disabled={running || syncState?.relation === "missing"}
                            onClick={() => void doRollback(b)}
                            className="shrink-0 gap-1.5"
                          >
                            {canSyncIntoBranch(syncState) ? "同步并设为当前" : "设为当前"}
                          </Button>
                          <Button
                            variant="ghost"
                            size="sm"
                            disabled={running}
                            onClick={() => setDeleteTarget(b)}
                            className="shrink-0 gap-1.5 text-destructive hover:bg-destructive/10 hover:text-destructive"
                            title="从家族中删除该历史分支（不可恢复）"
                          >
                            <Trash2 className="h-3.5 w-3.5" />
                            删除
                          </Button>
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            </div>
          )}
        </ScrollArea>

        <AlertDialog
          open={!!deleteTarget}
          onOpenChange={(v) => !v && !running && setDeleteTarget(null)}
        >
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>删除历史分支</AlertDialogTitle>
              <AlertDialogDescription className="break-words">
                删除 <b>{deleteTarget?.provider}</b> 分支及其关联记录。不可撤销，不会自动备份。
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel disabled={running}>取消</AlertDialogCancel>
              <AlertDialogAction
                disabled={running}
                onClick={(event) => { event.preventDefault(); void doDelete(); }}
                className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
              >
                {running ? "删除中…" : "删除分支"}
              </AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>

      </SheetContent>
    </Sheet>
  );
}

function safeDate(iso: string): string {
  try {
    return new Date(iso).toLocaleString();
  } catch {
    return iso;
  }
}

function canSyncIntoBranch(state?: BranchSyncState): boolean {
  return state?.relation === "active_ahead" && state.appendable_lines_to_branch > 0;
}

function BranchSyncBadge({ state }: { state: BranchSyncState }) {
  switch (state.relation) {
    case "same":
      return (
        <Badge variant="outline" className="h-5 px-1.5 font-normal text-emerald-600">
          已同步
        </Badge>
      );
    case "branch_ahead":
      return (
        <Badge variant="outline" className="h-5 px-1.5 font-normal text-blue-600">
          {state.appendable_lines_to_active > 0 ? `此分支领先 ${state.appendable_lines_to_active} 行` : "此分支有新增历史"}
        </Badge>
      );
    case "active_ahead":
      return (
        <Badge variant="outline" className="h-5 px-1.5 font-normal text-blue-600">
          {state.appendable_lines_to_branch > 0 ? `此分支落后 ${state.appendable_lines_to_branch} 行` : "当前分支有新增历史"}
        </Badge>
      );
    case "diverged":
      return (
        <Badge
          variant="outline"
          className="h-5 px-1.5 font-normal text-amber-600"
          title={state.error ?? undefined}
        >
          已分叉
        </Badge>
      );
    case "missing":
      return (
        <Badge variant="outline" className="h-5 px-1.5 font-normal text-destructive" title={state.error ?? undefined}>
          文件缺失
        </Badge>
      );
    case "unavailable":
      return (
        <Badge variant="outline" className="h-5 px-1.5 font-normal text-muted-foreground" title={state.error ?? undefined}>
          暂无法比较
        </Badge>
      );
    default:
      return null;
  }
}

function inactiveBranchStatusLabel(status: FamilyBranch["status"]): string {
  switch (status) {
    case "archived":
    case "active":
      return "历史分支";
    case "deleted":
      return "已删除";
    default:
      return status;
  }
}
