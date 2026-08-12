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
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Textarea } from "@/components/ui/textarea";
import type { DeletePlan, PreviewEvent, SessionProvider } from "@/lib/api";
import { deleteReasonLabel } from "@/lib/previewEvent";

type ActionState<T> = {
  target: T | null;
  running: boolean;
  onClose: () => void;
  onConfirm: () => void;
};

type Props = {
  provider: SessionProvider;
  fork: ActionState<PreviewEvent>;
  edit: ActionState<PreviewEvent> & {
    text: string;
    onTextChange: (text: string) => void;
  };
  deleteEvent: ActionState<PreviewEvent> & { plan: DeletePlan | null };
  deleteSelection: ActionState<{ start: number; end: number }> & {
    plan: DeletePlan | null;
  };
};

export function PreviewMutationDialogs({
  provider,
  fork,
  edit,
  deleteEvent,
  deleteSelection,
}: Props) {
  return (
    <>
      <AlertDialog
        open={Boolean(fork.target)}
        onOpenChange={(open) => !open && !fork.running && fork.onClose()}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>从此处创建回溯分支</AlertDialogTitle>
            <AlertDialogDescription>
              系统会只复制当前节点之前的有效会话历史，生成一个新的 active 会话分支；原会话会归档到分支历史中，不会被删除。
            </AlertDialogDescription>
          </AlertDialogHeader>
          <div className="rounded-md border bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
            <div className="font-mono">line {fork.target ? fork.target.index + 1 : ""}</div>
            {fork.target?.text_summary && (
              <div className="mt-1 line-clamp-2 text-foreground">{fork.target.text_summary}</div>
            )}
          </div>
          <AlertDialogFooter>
            <AlertDialogCancel disabled={fork.running}>取消</AlertDialogCancel>
            <AlertDialogAction
              disabled={fork.running}
              onClick={(event) => {
                event.preventDefault();
                fork.onConfirm();
              }}
            >
              创建分支
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <Dialog
        open={Boolean(edit.target)}
        onOpenChange={(open) => !open && !edit.running && edit.onClose()}
      >
        <DialogContent className="sm:max-w-[640px]">
          <DialogHeader>
            <DialogTitle>改写消息文本</DialogTitle>
            <DialogDescription className="sr-only">修改当前会话事件中的可编辑文本。</DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            <div className="rounded-md border bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
              <span className="font-mono">line {edit.target ? edit.target.index + 1 : ""}</span>
              <span className="mx-2 text-muted-foreground/50">·</span>
              {provider === "opencode" ? (
                <>只更新 opencode.db 中当前会话的 text 内容块（会话 ID 与时间戳不变，可直接续聊）；推理、工具调用及其他会话保持原样。编辑前会保存会话级快照，可在「编辑历史」中撤销或还原。</>
              ) : (
                <>会话文件会原地改写（会话 ID 不变，可直接 resume 续聊）；Codex 镜像行会同步更新，思考/推理与工具块保持原样。编辑前会自动保存原始快照，可在「编辑历史」中撤销或还原。</>
              )}
            </div>
            <Textarea
              value={edit.text}
              onChange={(event) => edit.onTextChange(event.target.value)}
              rows={10}
              className="max-h-[50vh] font-mono text-sm"
              placeholder="消息文本"
            />
          </div>
          <div className="flex justify-end gap-2">
            <Button variant="outline" disabled={edit.running} onClick={edit.onClose}>取消</Button>
            <Button disabled={edit.running || !edit.text.trim()} onClick={edit.onConfirm}>
              {edit.running ? "保存中…" : "保存改写"}
            </Button>
          </div>
        </DialogContent>
      </Dialog>

      <DeletePlanDialog
        provider={provider}
        open={Boolean(deleteEvent.target)}
        selectedRange={false}
        plan={deleteEvent.plan}
        running={deleteEvent.running}
        onClose={deleteEvent.onClose}
        onConfirm={deleteEvent.onConfirm}
      />
      <DeletePlanDialog
        provider={provider}
        open={Boolean(deleteSelection.target)}
        selectedRange
        plan={deleteSelection.plan}
        running={deleteSelection.running}
        onClose={deleteSelection.onClose}
        onConfirm={deleteSelection.onConfirm}
      />
    </>
  );
}

function DeletePlanDialog({
  provider,
  open,
  selectedRange,
  plan,
  running,
  onClose,
  onConfirm,
}: {
  provider: SessionProvider;
  open: boolean;
  selectedRange: boolean;
  plan: DeletePlan | null;
  running: boolean;
  onClose: () => void;
  onConfirm: () => void;
}) {
  const title = selectedRange ? "删除选中事件" : "删除会话事件";
  return (
    <AlertDialog open={open} onOpenChange={(nextOpen) => !nextOpen && !running && onClose()}>
      <AlertDialogContent className="sm:max-w-[640px]">
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription>
            {deleteDescription(provider, selectedRange)}
          </AlertDialogDescription>
        </AlertDialogHeader>
        {!plan && <div className="py-2 text-center text-xs text-muted-foreground">正在生成删除计划…</div>}
        {plan && plan.blocked.length > 0 && (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
            {plan.blocked.map((reason, index) => <div key={index}>{reason}</div>)}
          </div>
        )}
        {plan && plan.blocked.length === 0 && (
          <ScrollArea className="rounded-md border bg-muted/40" viewportClassName="max-h-72">
            <div className="space-y-1.5 p-2 pr-3">
              {selectedRange && (
                <div className="mb-2 text-[11px] font-medium text-muted-foreground">
                  共 {plan.lines.length} 个事件将被删除
                </div>
              )}
              {plan.lines.map((line) => (
                <div key={line.line_no} className="flex items-start gap-2 text-xs leading-[1.45]">
                  <span className="w-16 shrink-0 select-none text-right font-mono text-[11px] tabular-nums text-muted-foreground">
                    line {line.line_no + 1}
                  </span>
                  <Badge
                    variant={line.reason === "selected" ? "default" : "outline"}
                    className="mt-px h-4 shrink-0 px-1 py-0 text-[10px] font-normal"
                  >
                    {deleteReasonLabel(line.reason)}
                  </Badge>
                  <span className="shrink-0 text-muted-foreground">{line.role}</span>
                  <span className="min-w-0 flex-1 wrap-anywhere">{line.summary}</span>
                </div>
              ))}
            </div>
          </ScrollArea>
        )}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={running}>取消</AlertDialogCancel>
          <AlertDialogAction
            disabled={running || !plan || plan.blocked.length > 0}
            className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
            onClick={(event) => {
              event.preventDefault();
              onConfirm();
            }}
          >
            {running
              ? "删除中…"
              : `${selectedRange ? "删除选中" : "删除"} ${plan?.lines.length ?? 0} 个事件`}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

function deleteDescription(provider: SessionProvider, selectedRange: boolean) {
  if (provider === "opencode") {
    return selectedRange
      ? "将删除选取范围内的事件（含首尾），并按同轮消息补全安全删除范围；用户消息会连同本轮响应删除，assistant 过程或回答会删除本轮完整响应链。删除前会保存当前会话快照，不影响数据库中的其他会话。"
      : "OpenCode 会按同轮消息安全删除：选择用户消息会同时删除本轮完整响应；选择推理、工具或回答时，会删除该轮完整 assistant 响应链并保留用户提问。只快照当前会话，不会覆盖整个数据库。";
  }
  return selectedRange
    ? "将删除选取范围内的事件（含首尾）。为保证续聊不报错，配对的工具调用/返回、镜像行与关联推理会一起删除。删除前会自动保存原始快照，可在「编辑历史」中撤销或还原。"
    : "为保证续聊不报错，配对的工具调用/返回、镜像行与关联推理会一起删除。删除前会自动保存原始快照，可在「编辑历史」中撤销或还原。";
}
