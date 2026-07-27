import { useEffect, useState } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import type { SessionSummary } from "@/lib/api";

type Props = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  session: SessionSummary | null;
  /** 成功时 resolve；抛错则保持弹窗打开（调用方负责错误提示）。 */
  onSubmit: (targetCwd: string) => Promise<void>;
};

export function MoveSessionCwdDialog({ open, onOpenChange, session, onSubmit }: Props) {
  const [targetCwd, setTargetCwd] = useState("");
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (open && session) {
      setTargetCwd(session.cwd);
    }
  }, [open, session]);

  const submit = async () => {
    const trimmed = targetCwd.trim();
    if (!trimmed || saving) return;
    setSaving(true);
    try {
      await onSubmit(trimmed);
      onOpenChange(false);
    } catch {
      // 错误已由调用方 toast，弹窗保持打开供修改重试
    } finally {
      setSaving(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={(v) => !saving && onOpenChange(v)}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>移动会话到其他项目</DialogTitle>
          <DialogDescription>
            修改会话的工作目录路径。更改后会话会重新归入新项目的分组，同时更新 Codex 数据库和 rollout 记录。
          </DialogDescription>
        </DialogHeader>
        <div className="grid gap-2">
          <Label htmlFor="cwd">工作目录路径</Label>
          <Input
            id="cwd"
            value={targetCwd}
            onChange={(e) => setTargetCwd(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.nativeEvent.isComposing) void submit();
            }}
            maxLength={1024}
            placeholder="输入新的工作目录路径"
            autoFocus
          />
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={saving}>
            取消
          </Button>
          <Button onClick={() => void submit()} disabled={saving || !targetCwd.trim()}>
            {saving && <Loader2 className="h-4 w-4 animate-spin" />}
            保存
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
