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
import type { DeletePlan, DeleteTurnSelection, PreviewEvent, SessionProvider, TextBlockEdit } from "@/lib/api";
import { deleteReasonLabel } from "@/lib/previewEvent";

type ActionState<T> = {
  target: T | null;
  running: boolean;
  onClose: () => void;
  onConfirm: () => void;
};

type Props = {
  provider: SessionProvider;
  sourceLabel: string;
  sessionId: string;
  rolloutPath: string;
  fork: ActionState<PreviewEvent>;
  edit: ActionState<PreviewEvent> & {
    text: string;
    onTextChange: (text: string) => void;
    blocks: TextBlockEdit[];
    onBlockChange: (index: number, text: string) => void;
  };
  deleteEvent: ActionState<PreviewEvent> & { plan: DeletePlan | null };
  deleteSelection: ActionState<{ start: number; end: number }> & {
    plan: DeletePlan | null;
  };
  onSelectTurn: (turn: DeleteTurnSelection) => void;
};

export function PreviewMutationDialogs({
  provider,
  sourceLabel,
  sessionId,
  rolloutPath,
  fork,
  edit,
  deleteEvent,
  deleteSelection,
  onSelectTurn,
}: Props) {
  return (
    <>
      <AlertDialog
        open={Boolean(fork.target)}
        onOpenChange={(open) => !open && !fork.running && fork.onClose()}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{provider === "codex" ? "从此处创建回溯分支" : "复制到此处"}</AlertDialogTitle>
            <AlertDialogDescription>
              {provider === "opencode"
                ? "按消息顺序复制从开头到所选内容所属的整条消息，包含这条消息的全部文字、思考和工具内容块，在同一项目下创建独立副本。原会话保持原样，副本不继承待办、分享、撤销或子会话状态。"
                : provider === "claude"
                ? "复制从会话开头到所选消息的主对话记录（包含这条消息），在同一项目下创建独立副本。原会话保持原样，副本不包含文件撤销历史。"
                : "系统会只复制当前节点之前的有效会话历史，生成一个新的 active 会话分支；原会话会归档到分支历史中，不会被删除。"}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <div className="rounded-md border bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
            <div className="font-mono">{provider === "opencode" ? "内容块" : "line"} {fork.target ? fork.target.index + 1 : ""}</div>
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
              {fork.running ? "创建中…" : provider === "codex" ? "创建分支" : "复制到此处"}
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
            <p className="text-xs text-muted-foreground">修改原会话，保存前自动备份，可在编辑历史中撤销。{edit.blocks.length > 1 && "请分别编辑文本块，图片及其他内容保持原位。"}</p>
            <div className="max-h-[50vh] space-y-3 overflow-auto">
            {edit.target && edit.blocks.length > 0 ? ((edit.target.raw as any).payload.item.content as any[]).map((block, index) => {
              const editable = edit.blocks.find((b) => b.content_index === index);
              return editable ? <label key={index} className="block space-y-1 text-xs text-muted-foreground">
                <span>文本块 {index + 1}</span>
                <Textarea value={editable.text} onChange={(event) => edit.onBlockChange(index, event.target.value)} rows={4} className="font-mono text-sm" />
              </label> : <div key={index} className="rounded border bg-muted/40 p-2 text-xs text-muted-foreground">{["local_image", "image"].includes(block.type) ? "图片" : "其他内容"} · 保留原位</div>;
            }) : <Textarea
              value={edit.text}
              onChange={(event) => edit.onTextChange(event.target.value)}
              rows={10}
              className="max-h-[50vh] font-mono text-sm"
              placeholder="消息文本"
            />}
            </div>
            <details className="text-xs text-muted-foreground"><summary className="cursor-pointer">操作详情</summary><div className="mt-2 break-all">{sourceLabel} · {sessionId}<br />{rolloutPath}<br />line {edit.target ? edit.target.index + 1 : ""}<br />本地保存不代表本次修改已通过原生客户端验证。外部更新后需刷新并重新选择。</div></details>
          </div>
          <div className="flex justify-end gap-2">
            <Button variant="outline" disabled={edit.running} onClick={edit.onClose}>取消</Button>
            <Button disabled={edit.running || (edit.blocks.length ? !edit.blocks.some((b) => b.text !== (edit.target?.raw as any)?.payload?.item?.content[b.content_index]?.text) : !edit.text.trim())} onClick={edit.onConfirm}>
              {edit.running ? "保存中…" : "保存改写"}
            </Button>
          </div>
        </DialogContent>
      </Dialog>

      <DeletePlanDialog
        provider={provider}
        identity={`${sourceLabel} · ${sessionId}\n${rolloutPath}`}
        open={Boolean(deleteEvent.target)}
        selectedRange={false}
        plan={deleteEvent.plan}
        running={deleteEvent.running}
        onClose={deleteEvent.onClose}
        onConfirm={deleteEvent.onConfirm}
        onSelectTurn={onSelectTurn}
      />
      <DeletePlanDialog
        provider={provider}
        identity={`${sourceLabel} · ${sessionId}\n${rolloutPath}`}
        open={Boolean(deleteSelection.target)}
        selectedRange
        plan={deleteSelection.plan}
        running={deleteSelection.running}
        onClose={deleteSelection.onClose}
        onConfirm={deleteSelection.onConfirm}
        onSelectTurn={onSelectTurn}
      />
    </>
  );
}

function DeletePlanDialog({
  provider,
  identity,
  open,
  selectedRange,
  plan,
  running,
  onClose,
  onConfirm,
  onSelectTurn,
}: {
  provider: SessionProvider;
  identity: string;
  open: boolean;
  selectedRange: boolean;
  plan: DeletePlan | null;
  running: boolean;
  onClose: () => void;
  onConfirm: () => void;
  onSelectTurn: (turn: DeleteTurnSelection) => void;
}) {
  const title = selectedRange ? "删除选中消息" : "删除这条消息？";
  const messages = plan?.messages.length ? plan.messages : plan?.lines.map((line) => ({ ...line, target: null })) ?? [];
  const unit = plan?.messages.length ? "条消息" : "项内容";
  const turns = new Set(messages.flatMap((m) => m.target ? [m.target.turn_id] : []));
  return (
    <AlertDialog open={open} onOpenChange={(nextOpen) => !nextOpen && !running && onClose()}>
      <AlertDialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-[640px]">
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription>
            {deleteDescription(provider, selectedRange)}
          </AlertDialogDescription>
        </AlertDialogHeader>
        {plan && plan.blocked.length === 0 && (
          <div className="rounded-md border bg-muted/40 p-3 text-xs">
            将删除 {messages.length} {unit}{turns.size > 0 && `，涉及 ${turns.size} 个回合`}。
            {selectedRange && <div className="mt-1">包含筛选或折叠后不可见的内容，请核对完整范围。</div>}
          </div>
        )}
        {!plan && <div className="py-2 text-center text-xs text-muted-foreground">正在生成删除计划…</div>}
        {plan && plan.blocked.length > 0 && (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
            {plan.blocked.map((reason, index) => <div key={index}>{reason}</div>)}
          </div>
        )}
        {plan?.required_turns.map((turn, index) => <div key={turn.turn_id} className="rounded-md border p-3 text-xs">
          <div>工具链所在回合 {index + 1}：{turn.messages.length} 条消息</div>
          <div className="my-2 space-y-1">{turn.messages.map((m) => <div key={m.target?.item_id} className="line-clamp-2">{m.role === "user" ? "你" : m.role === "assistant" ? "助手" : "工具 / 过程"} · {m.summary}</div>)}</div>
          <Button variant="outline" size="sm" disabled={running} onClick={() => onSelectTurn(turn)}>选择整轮</Button>
        </div>)}
        {plan && plan.blocked.length === 0 && (
          <ScrollArea className="rounded-md border" viewportClassName="max-h-52"><div className="space-y-3 p-3">
            {messages.map((m) => <div key={m.target?.item_id ?? m.line_no} className="text-xs"><span className="text-muted-foreground">{m.role === "user" ? "你" : m.role === "assistant" ? "助手" : "工具 / 过程"}{m.target && ` · 回合 ${[...turns].indexOf(m.target.turn_id) + 1}`}</span><div className="mt-1 whitespace-pre-wrap break-words">{m.summary || "（无文本）"}</div></div>)}
          </div></ScrollArea>
        )}
        {plan && <details className="min-w-0 text-xs text-muted-foreground"><summary className="cursor-pointer">底层记录与验证详情</summary>
          <div className="my-2 whitespace-pre-wrap break-all">{identity}<br />涉及 {plan.lines.length} 条底层记录；本地保存不代表本次操作已完成原生验证。</div>
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
        </details>}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={running}>取消</AlertDialogCancel>
          <AlertDialogAction
            disabled={running || !plan || plan.blocked.length > 0 || plan.lines.length === 0}
            className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
            onClick={(event) => {
              event.preventDefault();
              onConfirm();
            }}
          >
            {running
              ? "删除中…"
              : `删除 ${messages.length} ${unit}`}
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
  return "删除前自动备份，可在编辑历史中撤销。删除历史不会撤销已执行的代码修改、命令或其他外部操作，也不会重新执行工具。";
}
