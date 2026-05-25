import { useEffect, useState } from "react";
import { AlertTriangle, Loader2 } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { Badge } from "@/components/ui/badge";
import { api, type SessionProvider, type SessionSummary } from "@/lib/api";
import { humanBytes } from "@/lib/format";
import { useSettings } from "@/stores/settings";
import { toast } from "sonner";

type Props = {
  open: boolean;
  onOpenChange: (v: boolean) => void;
  provider: SessionProvider;
  sessions: SessionSummary[];
  onDone?: (backupPath: string) => void;
};

export function BackupCreateDialog({ open, onOpenChange, provider, sessions, onDone }: Props) {
  const settings = useSettings((s) => s.settings);
  const [name, setName] = useState("");
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    // 备份名中的时间戳用作文件夹名（Windows 文件名不允许冒号），
    // 因此这里采用 2026-04-20_14-30-00 这种文件系统友好的紧凑形式。
    const d = new Date();
    const pad = (n: number) => String(n).padStart(2, "0");
    const ts = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}_${pad(
      d.getHours(),
    )}-${pad(d.getMinutes())}-${pad(d.getSeconds())}`;
    setName(`backup-${ts}`);
    setNote("");
    setErr(null);
  }, [open]);

  const totalBytes = sessions.reduce((a, b) => a + b.rollout_bytes, 0);

  const submit = async () => {
    if (!settings) return;
    setBusy(true);
    setErr(null);
    try {
      const r = await api.createBackup({
        provider,
        codex_dir: settings.codex_dir,
        claude_dir: provider === "gemini" ? settings.gemini_dir : settings.claude_dir,
        backup_dir: settings.backup_dir,
        ids: sessions.map((s) => s.id),
        name,
        note: note.trim() || undefined,
      });
      toast.success(`备份完成：${r.name}`);
      onDone?.(r.path);
      onOpenChange(false);
    } catch (e: any) {
      setErr(String(e?.message ?? e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={(v) => !busy && onOpenChange(v)}>
      <DialogContent className="max-w-xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <AlertTriangle className="h-5 w-5 text-amber-500" />
            创建备份
          </DialogTitle>
          <DialogDescription>
            备份将包含 {sessions.length} 个会话；备份与删除是独立动作。
          </DialogDescription>
        </DialogHeader>

        <div className="min-w-0 space-y-4 overflow-auto pr-1">
          <div className="space-y-2">
            <Label>备份名</Label>
            <Input value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          <div className="space-y-2">
            <Label>备注（可选）</Label>
            <Textarea value={note} onChange={(e) => setNote(e.target.value)} rows={2} />
          </div>
          <div className="rounded-md border bg-muted/40 p-3">
            <div className="mb-1 flex items-center justify-between text-xs text-muted-foreground">
              <span>包含会话</span>
              <span>预估 {humanBytes(totalBytes)}</span>
            </div>
            <ul className="max-h-40 min-w-0 space-y-1 overflow-auto text-xs">
              {sessions.slice(0, 10).map((s) => (
                <li key={s.id} className="flex min-w-0 items-center gap-2">
                  <Badge variant="outline" className="h-4 shrink-0 px-1.5 font-mono text-[10px]">
                    {s.id.slice(0, 8)}
                  </Badge>
                  <span className="min-w-0 truncate">{s.title || "(无标题)"}</span>
                </li>
              ))}
              {sessions.length > 10 && (
                <li className="text-muted-foreground">…还有 {sessions.length - 10} 条</li>
              )}
            </ul>
          </div>
          {err && (
            <div className="max-h-24 overflow-auto rounded-md border border-destructive/40 bg-destructive/10 p-2 text-xs text-destructive [overflow-wrap:anywhere]">
              失败：{err}
            </div>
          )}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
            取消
          </Button>
          <Button onClick={submit} disabled={busy || sessions.length === 0 || !name.trim()}>
            {busy && <Loader2 className="h-4 w-4 animate-spin" />}
            备份
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
