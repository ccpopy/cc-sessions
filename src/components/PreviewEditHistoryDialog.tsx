import { Undo2 } from "lucide-react";
import { useState } from "react";
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle } from "@/components/ui/alert-dialog";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import type { EditHistory } from "@/lib/api";
import { formatTimeString } from "@/lib/format";
import { editKindLabel } from "@/lib/previewEvent";

type Props = {
  open: boolean;
  history: EditHistory | null;
  mutating: boolean;
  onOpenChange: (open: boolean) => void;
  onUndo: () => void;
  onRestore: (snapshotName: string) => void;
  blockedReason: string | null;
  onReconcile: () => void;
};

export function PreviewEditHistoryDialog({
  open,
  history,
  mutating,
  onOpenChange,
  onUndo,
  onRestore,
  blockedReason,
  onReconcile,
}: Props) {
  const [restoreTarget, setRestoreTarget] = useState<string | null>(null);
  return (
    <>
    <Dialog open={open} onOpenChange={(nextOpen) => !mutating && onOpenChange(nextOpen)}>
      <DialogContent className="sm:max-w-[640px]">
        <DialogHeader>
          <DialogTitle>编辑历史</DialogTitle>
          <DialogDescription className="sr-only">
            查看、撤销或还原当前会话的编辑记录。
          </DialogDescription>
        </DialogHeader>
        {!history ? (
          <div className="py-6 text-center text-xs text-muted-foreground">加载中…</div>
        ) : (
          <div className="space-y-4">
            {history.pending_operation && (
              <div role="alert" className="rounded-md border border-amber-500/40 bg-amber-500/10 p-3 text-xs">
                <strong>提交状态待核对</strong>
                <div className="mt-1 break-all font-mono">{history.pending_operation.op_id}</div>
                <div>{history.pending_operation.description}</div>
                <div className="mt-1">{history.pending_operation.status === "conflict" ? "文件已有其他变化，保留操作清单和快照，请在独立副本中核对。" : "只补齐或清理操作记录，不会再次修改会话内容。"}</div>
                <Button className="mt-2 h-7 text-xs" disabled={mutating || !history.pending_operation.can_reconcile} onClick={onReconcile}>核对提交状态</Button>
              </div>
            )}
            {blockedReason && <div className="text-xs text-muted-foreground">{blockedReason}</div>}
            <div className="flex items-center justify-between">
              <div className="text-xs text-muted-foreground">
                {history.entries.length > 0
                  ? `共 ${history.entries.length} 次操作`
                  : "该会话还没有编辑记录"}
              </div>
              <Button
                size="sm"
                variant="outline"
                className="h-8 gap-1.5"
                disabled={mutating || !!blockedReason || !history.undo_available}
                title={history.undo_blocked_reason ?? undefined}
                onClick={onUndo}
              >
                <Undo2 className="h-3.5 w-3.5" />
                撤销最近一次
              </Button>
            </div>
            {history.undo_blocked_reason && (
              <div className="rounded-md border bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
                {history.undo_blocked_reason}
              </div>
            )}
            {history.entries.length > 0 && (
              <div className="max-h-48 space-y-1 overflow-auto rounded-md border bg-muted/30 p-2">
                {history.entries.map((entry) => (
                  <div key={entry.op_id} className="flex flex-wrap items-center gap-2 text-xs" title={entry.op_id}>
                    <span className="shrink-0 font-mono text-muted-foreground">
                      {formatTimeString(entry.ts)}
                    </span>
                    <Badge variant="outline" className="h-4 shrink-0 px-1 py-0 text-[10px] font-normal">
                      {editKindLabel(entry.kind)}
                    </Badge>
                    <span className="min-w-0 flex-1 truncate">{entry.description}</span>
                    <span className="text-muted-foreground">本地已提交 · 原生未验证</span>
                    <span className="w-full break-all font-mono text-[10px] text-muted-foreground">{entry.op_id}</span>
                  </div>
                ))}
              </div>
            )}
            <div>
              <div className="mb-1.5 text-xs font-medium">原始快照</div>
              {history.snapshots.length === 0 ? (
                <div className="text-xs text-muted-foreground">
                  暂无快照（首次改写或删除时会自动创建）
                </div>
              ) : (
                <div className="max-h-40 space-y-1 overflow-auto rounded-md border bg-muted/30 p-2">
                  {history.snapshots.map((snapshot) => (
                    <div key={snapshot.name} className="flex items-center gap-2 text-xs">
                      <span className="min-w-0 flex-1 truncate font-mono">{snapshot.name}</span>
                      <span className="shrink-0 text-muted-foreground">
                        {formatTimeString(snapshot.created_at)}
                      </span>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-6 shrink-0 px-2 text-xs"
                        disabled={mutating || !!blockedReason || !history.restore_available}
                        onClick={() => setRestoreTarget(snapshot.name)}
                      >
                        还原
                      </Button>
                    </div>
                  ))}
                </div>
              )}
            </div>
          </div>
        )}
      </DialogContent>
    </Dialog>
    <AlertDialog open={open && restoreTarget !== null} onOpenChange={(next) => !next && !mutating && setRestoreTarget(null)}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>确认还原快照</AlertDialogTitle>
          <AlertDialogDescription>将用所选快照覆盖当前会话内容，并保存还原前状态。如果检测到外部新增或修改，本次还原将被拒绝。</AlertDialogDescription>
        </AlertDialogHeader>
        <div className="break-all font-mono text-xs">{restoreTarget}</div>
        <AlertDialogFooter>
          <AlertDialogCancel disabled={mutating}>取消</AlertDialogCancel>
          <AlertDialogAction disabled={mutating} onClick={() => { if (restoreTarget) onRestore(restoreTarget); setRestoreTarget(null); }}>还原快照</AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
    </>
  );
}
