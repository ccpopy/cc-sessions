import { GitBranch, Loader2, ShieldCheck } from "lucide-react";
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
import type { SessionSummary } from "@/lib/api";
import { sessionDisplayTitle } from "@/lib/sessionText";

type Props = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  session: SessionSummary | null;
  running?: boolean;
  onConfirm: () => void | Promise<void>;
};

export function ForkRecommendationDialog({
  open,
  onOpenChange,
  session,
  running = false,
  onConfirm,
}: Props) {
  const title = session
    ? sessionDisplayTitle(session.title, session.first_user_message)
    : "当前会话";

  return (
    <AlertDialog open={open} onOpenChange={(value) => !running && onOpenChange(value)}>
      <AlertDialogContent className="sm:max-w-[520px]">
        <AlertDialogHeader>
          <div className="mb-1 flex h-10 w-10 items-center justify-center rounded-xl border border-primary/20 bg-primary/10 text-primary">
            <GitBranch className="h-5 w-5" />
          </div>
          <AlertDialogTitle>建议使用 Codex App 派生新任务</AlertDialogTitle>
          <AlertDialogDescription asChild>
            <div className="space-y-3 text-left leading-6">
              <p>
                Codex App 会按当前版本的会话格式创建新任务，能够完整处理历史记录、工具调用和后续恢复。
              </p>
              <div className="rounded-lg border border-primary/20 bg-primary/[0.06] px-3.5 py-3 text-foreground">
                <div className="flex items-center gap-2 font-medium">
                  <ShieldCheck className="h-4 w-4 text-primary" />
                  推荐操作
                </div>
                <p className="mt-1.5 text-sm text-muted-foreground">
                  返回 Codex App，打开该任务并选择“在新任务中继续”。
                </p>
              </div>
              <p className="text-sm text-muted-foreground">
                本地 Fork 会复制当前能够安全读取的会话数据。若 Codex 后续采用新的保存格式，可能出现历史不完整或新任务无法继续；检测到无法安全复制时，本次操作会自动停止。
              </p>
              <div className="rounded-md border bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
                即将 Fork：<span className="font-medium text-foreground">{title}</span>
              </div>
            </div>
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogAction
            disabled={running}
            className="gap-2 border border-input bg-background text-foreground shadow-sm hover:bg-accent hover:text-accent-foreground"
            onClick={(event) => {
              event.preventDefault();
              void onConfirm();
            }}
          >
            {running && <Loader2 className="h-4 w-4 animate-spin" />}
            {running ? "Fork 中…" : "仍然本地 Fork"}
          </AlertDialogAction>
          <AlertDialogCancel
            disabled={running}
            className="border-primary bg-primary text-primary-foreground hover:bg-primary/90 hover:text-primary-foreground"
          >
            取消，改用 Codex App
          </AlertDialogCancel>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
