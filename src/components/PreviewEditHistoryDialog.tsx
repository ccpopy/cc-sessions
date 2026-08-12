import { Undo2 } from "lucide-react";

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
};

export function PreviewEditHistoryDialog({
  open,
  history,
  mutating,
  onOpenChange,
  onUndo,
  onRestore,
}: Props) {
  return (
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
                disabled={mutating || !history.undo_available}
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
                  <div key={entry.op_id} className="flex items-center gap-2 text-xs">
                    <span className="shrink-0 font-mono text-muted-foreground">
                      {formatTimeString(entry.ts)}
                    </span>
                    <Badge variant="outline" className="h-4 shrink-0 px-1 py-0 text-[10px] font-normal">
                      {editKindLabel(entry.kind)}
                    </Badge>
                    <span className="min-w-0 flex-1 truncate">{entry.description}</span>
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
                        disabled={mutating}
                        onClick={() => onRestore(snapshot.name)}
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
  );
}
