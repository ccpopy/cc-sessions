import { useCallback, useEffect, useRef, useState } from "react";
import {
  CheckCircle2,
  GitBranch,
  Loader2,
  Radio,
  ShieldAlert,
  Trash2,
} from "lucide-react";
import { toast } from "sonner";

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

import { api, type BranchSyncState, type Family, type FamilyBranch } from "@/lib/api";
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
  const [loading, setLoading] = useState(false);
  const [rollbackTarget, setRollbackTarget] = useState<FamilyBranch | null>(null);
  const [syncTarget, setSyncTarget] = useState<SyncTarget | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<FamilyBranch | null>(null);
  const [running, setRunning] = useState(false);
  const requestSeq = useRef(0);

  const load = useCallback(async () => {
    const requestId = ++requestSeq.current;
    setFamily(null);
    setSyncStates({});
    setRollbackTarget(null);
    setSyncTarget(null);
    setDeleteTarget(null);
    if (!sessionId || !codexDir) {
      setLoading(false);
      return;
    }
    setLoading(true);
    try {
      const store = await api.getFamilyStore(codexDir);
      if (requestSeq.current !== requestId) return;
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
          setSyncStates(Object.fromEntries(states.map((s) => [s.branch_id, s])));
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
      setRollbackTarget(null);
      setSyncTarget(null);
      setDeleteTarget(null);
      setLoading(false);
    }
  }, [open, load]);

  const doRollback = async () => {
    if (!family || !rollbackTarget) return;
    setRunning(true);
    try {
      const syncState = syncStates[rollbackTarget.id];
      let synchronizedLines = 0;
      if (syncState?.relation === "active_ahead") {
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
      await load();
      onChanged?.();
      setRollbackTarget(null);
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setRunning(false);
    }
  };

  const doDelete = async () => {
    if (!family || !deleteTarget) return;
    setRunning(true);
    try {
      const result = await api.deleteFamilyBranch(codexDir, family.family_id, deleteTarget.id);
      await load();
      await onChanged?.();
      if (!result.ok) {
        throw new Error(result.error ?? `分支 ${deleteTarget.id} 未删除干净`);
      }
      toast.success(`已删除 ${deleteTarget.provider} 分支`);
      setDeleteTarget(null);
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setRunning(false);
    }
  };

  const doSync = async () => {
    if (!family || !syncTarget) return;
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
      await load();
      onChanged?.();
      setSyncTarget(null);
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
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
          <SheetDescription className="break-words">
            同一条会话在不同 provider 下会形成多个分支。任意时刻只有一个“当前分支”
            对 Codex App 和主会话列表可见，历史分支只在这里查看和切换。
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
              此会话尚未进入分支记录。
              <br />
              （首次克隆或修复后会自动建档）
            </div>
          ) : (
            <div className="min-w-0 max-w-full space-y-3 overflow-hidden">
              <div className="min-w-0 max-w-full overflow-hidden rounded-md border bg-muted/20 p-3 text-xs">
                <div className="flex min-w-0 items-center gap-2">
                  <span className="shrink-0 text-muted-foreground">分组 ID</span>
                  <code className="min-w-0 wrap-anywhere font-mono text-[11px]">
                    {family.family_id}
                  </code>
                </div>
                <div className="mt-1 min-w-0 wrap-anywhere text-muted-foreground">
                  {family.title || "（无标题）"}
                </div>
              </div>

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
                              {b.provider || "(未知 provider)"}
                            </Badge>
                            {isCurrent && (
                              <Badge variant="outline" className="h-5 px-1.5 font-normal text-emerald-600">
                                当前 provider
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
                            {!isActive && <ArchiveOriginBadge origin={b.archive_origin ?? null} />}
                            <span className="text-muted-foreground">
                              {safeDate(b.created_at)}
                            </span>
                          </div>
                          <div className="min-w-0 wrap-anywhere font-mono text-[11px] text-muted-foreground">
                            {b.id}
                          </div>
                          <div
                            className="min-w-0 wrap-anywhere font-mono text-[11px] leading-5 text-muted-foreground"
                            title={b.rollout_relpath}
                          >
                            {b.rollout_relpath}
                          </div>
                          {(b.sha256 || b.line_count != null) && (
                            <div className="flex min-w-0 flex-wrap items-center gap-1.5 text-[11px] text-muted-foreground">
                              {b.sha256 ? (
                                <>
                                  <CheckCircle2 className="h-3 w-3 shrink-0 text-emerald-500" />
                                  <span className="shrink-0">校验：</span>
                                  <code className="wrap-anywhere font-mono">
                                    {b.sha256.slice(0, 10)}…
                                  </code>
                                  {b.line_count != null && (
                                    <span>· {b.line_count} 行</span>
                                  )}
                                </>
                              ) : (
                                <>
                                  <ShieldAlert className="h-3 w-3 shrink-0 text-amber-500" />
                                  <span>尚未固化校验信息（归档后自动记录）</span>
                                </>
                              )}
                            </div>
                          )}
                          {!isActive && syncState && (
                            <div className="flex min-w-0 flex-wrap items-center gap-1.5 text-[11px] text-muted-foreground">
                              <BranchSyncBadge state={syncState} />
                            </div>
                          )}
                        </div>
                      </div>
                      {!isActive && (
                        <div className="mt-2 flex flex-wrap justify-end gap-2">
                          {syncState?.relation === "branch_ahead" && (
                            <Button
                              variant="ghost"
                              size="sm"
                              onClick={() => setSyncTarget({ branch: b, direction: "into_active" })}
                              className="shrink-0 gap-1.5"
                              title="将此分支相对当前分支的新增内容追加到当前分支（不切换 provider）"
                            >
                              合并到当前
                            </Button>
                          )}
                          {syncState?.relation === "active_ahead" && (
                            <Button
                              variant="ghost"
                              size="sm"
                              onClick={() => setSyncTarget({ branch: b, direction: "into_branch" })}
                              className="shrink-0 gap-1.5"
                              title="将当前分支的新增内容追加到此历史分支（不切换 provider）"
                            >
                              同步到此分支
                            </Button>
                          )}
                          <Button
                            variant="outline"
                            size="sm"
                            onClick={() => setRollbackTarget(b)}
                            className="shrink-0 gap-1.5"
                          >
                            设为当前
                          </Button>
                          <Button
                            variant="ghost"
                            size="sm"
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
          open={!!rollbackTarget}
          onOpenChange={(v) => !v && setRollbackTarget(null)}
        >
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>切换当前分支</AlertDialogTitle>
              <AlertDialogDescription className="break-words">
                {rollbackTarget && syncStates[rollbackTarget.id]?.relation === "active_ahead" ? (
                  <>
                    目标分支落后当前分支 {syncStates[rollbackTarget.id].appendable_lines_to_branch} 行。
                    确认后会先同步这些增量，再归档当前分支并恢复 <b>{rollbackTarget.provider}</b>
                    分支，避免从旧基线继续对话造成真实分叉。
                  </>
                ) : (
                  <>
                    将当前分支（provider=
                    {family?.chain.find((x) => x.id === family.active_id)?.provider ?? "-"}
                    ）归档至 <code>archived_sessions/</code>
                    ，并从归档恢复 <b>{rollbackTarget?.provider}</b> 下的目标分支。
                  </>
                )}
                {rollbackTarget && syncStates[rollbackTarget.id]?.relation === "diverged" && (
                  <>
                    <br />
                    两个分支都已有不同的新记录，无法自动合并；切换只会改变当前可见分支，不会消除“已分叉”状态。
                  </>
                )}
                <br />
                此操作不会删除任何文件，随时可再切回。
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>取消</AlertDialogCancel>
              <AlertDialogAction disabled={running} onClick={doRollback}>
                {running
                  ? "执行中…"
                  : rollbackTarget && syncStates[rollbackTarget.id]?.relation === "active_ahead"
                    ? "同步并切换"
                    : rollbackTarget && syncStates[rollbackTarget.id]?.relation === "diverged"
                      ? "仍要切换"
                      : "确认切换"}
              </AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>

        <AlertDialog
          open={!!deleteTarget}
          onOpenChange={(v) => !v && setDeleteTarget(null)}
        >
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>删除历史分支</AlertDialogTitle>
              <AlertDialogDescription className="break-words">
                将彻底删除 <b>{deleteTarget?.provider}</b> 分支：
                包括其 rollout 文件、threads 记录、logs 与 session_index
                条目；如果此分支已在归档目录中，归档副本也会一并删除。
                <br />
                此操作不可撤销，且不会创建备份。
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>取消</AlertDialogCancel>
              <AlertDialogAction
                disabled={running}
                onClick={doDelete}
                className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
              >
                {running ? "执行中…" : "确认删除"}
              </AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>

        <AlertDialog
          open={!!syncTarget}
          onOpenChange={(v) => !v && setSyncTarget(null)}
        >
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>
                {syncTarget?.direction === "into_branch" ? "同步当前分支新增内容" : "合并分支新增内容"}
              </AlertDialogTitle>
              <AlertDialogDescription className="break-words text-justify">
                {syncTarget?.direction === "into_branch" ? (
                  <>
                    把当前分支相对 <b>{syncTarget.branch.provider}</b> 历史分支的新增对话，
                    追加到该历史分支的 rollout 文件。当前 provider 不会切换，Codex App
                    仍继续显示当前分支；之后切回该历史分支时，它会带上这些新增上下文。
                  </>
                ) : (
                  <>
                    把 <b>{syncTarget?.branch.provider}</b> 分支相对当前分支的新增对话，
                    追加到当前分支的 rollout 文件。provider 保持不变，Codex App
                    会继续以当前分支显示。
                  </>
                )}
                <br />
                只有一边完整包含另一边时才允许同步；如果两边都各自产生了不同新对话，操作会直接失败。
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>取消</AlertDialogCancel>
              <AlertDialogAction disabled={running} onClick={doSync}>
                {running
                  ? "执行中…"
                  : syncTarget?.direction === "into_branch"
                    ? "确认同步"
                    : "确认合并"}
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
          此分支领先 {state.appendable_lines_to_active} 行
        </Badge>
      );
    case "active_ahead":
      return (
        <Badge variant="outline" className="h-5 px-1.5 font-normal text-blue-600">
          此分支落后 {state.appendable_lines_to_branch} 行
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
