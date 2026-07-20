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
import type { SessionSummary } from "@/lib/api";
import { sessionDisplayTitle } from "@/lib/sessionText";

type Props = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  session: SessionSummary | null;
  /** 成功时 resolve；抛错则保持弹窗打开（调用方负责错误提示）。 */
  onSubmit: (title: string) => Promise<void>;
};

export function RenameSessionDialog({ open, onOpenChange, session, onSubmit }: Props) {
  const [title, setTitle] = useState("");
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (open && session) {
      setTitle(sessionDisplayTitle(session.title, session.first_user_message));
    }
  }, [open, session]);

  const submit = async () => {
    const trimmed = title.trim();
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
          <DialogTitle>重命名会话</DialogTitle>
          <DialogDescription>
            名称写入 Codex 的 threads 数据库，官方 App 与本软件共用；该会话的全部
            provider 分支会一并改名，切换供应商后名称不再丢失。
          </DialogDescription>
        </DialogHeader>
        <Input
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.nativeEvent.isComposing) void submit();
          }}
          maxLength={120}
          placeholder="输入会话名称"
          autoFocus
        />
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={saving}>
            取消
          </Button>
          <Button onClick={() => void submit()} disabled={saving || !title.trim()}>
            {saving && <Loader2 className="h-4 w-4 animate-spin" />}
            保存
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
