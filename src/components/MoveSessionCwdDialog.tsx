import { useEffect, useState } from "react";
import { FolderOpen, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
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
import { pickDirectoryPath } from "@/lib/dialog";

type Props = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  session: SessionSummary | null;
  /** 成功时 resolve；抛错则保持弹窗打开（调用方负责错误提示）。 */
  onSubmit: (targetCwd: string, preserveClaudePathCase: boolean) => Promise<void>;
};

export function MoveSessionCwdDialog({
  open,
  onOpenChange,
  session,
  onSubmit,
}: Props) {
  const [targetCwd, setTargetCwd] = useState("");
  const [saving, setSaving] = useState(false);
  const [picking, setPicking] = useState(false);
  const [preserveClaudePathCase, setPreserveClaudePathCase] = useState(false);

  useEffect(() => {
    if (open && session) {
      setTargetCwd(session.cwd);
      setPreserveClaudePathCase(false);
    }
  }, [open, session]);

  const submit = async () => {
    const trimmed = targetCwd.trim();
    if (!trimmed || saving || picking) return;
    setSaving(true);
    try {
      await onSubmit(trimmed, session?.provider === "claude" && preserveClaudePathCase);
      onOpenChange(false);
    } catch {
      // 错误已由调用方 toast，弹窗保持打开供修改重试
    } finally {
      setSaving(false);
    }
  };

  const pickTarget = async () => {
    if (saving || picking) return;
    setPicking(true);
    try {
      const picked = await pickDirectoryPath({
        defaultPath: targetCwd.trim() || session?.cwd || undefined,
        title: "选择会话的新项目目录",
        webPrompt: "请输入运行 cc-sessions webui 的环境可访问的新项目目录路径。",
      });
      if (picked) setTargetCwd(picked);
    } finally {
      setPicking(false);
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(value) => !saving && !picking && onOpenChange(value)}
    >
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>移动会话到其他项目</DialogTitle>
          <DialogDescription>
            {session?.provider === "claude"
              ? "移动主 transcript、同名 sidecar 与 companion 文件，并同步改写 JSONL cwd 和 history 项目路径。"
              : session?.provider === "opencode"
                ? "在同一 SQLite 事务中更新会话及其子会话的项目标识、目录和兼容 path 字段。"
                : "已添加到 Codex Desktop 的项目，推荐优先使用官方的移动功能。本功能主要用于 CLI 会话，或移动到尚未添加的新目录；操作前请完全退出 Codex Desktop。"}
          </DialogDescription>
        </DialogHeader>
        <div className="grid gap-2">
          <Label htmlFor="cwd">工作目录路径</Label>
          <div className="flex gap-2">
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
            <Button
              type="button"
              variant="outline"
              size="icon"
              onClick={() => void pickTarget()}
              disabled={saving || picking}
              title="选择目录"
              aria-label="选择目录"
            >
              {picking ? (
                <Loader2 className="h-4 w-4 animate-spin" />
              ) : (
                <FolderOpen className="h-4 w-4" />
              )}
            </Button>
          </div>
          {session?.provider === "claude" && (
            <div className="mt-1 flex items-start gap-2 rounded-md border bg-muted/30 p-3">
              <Checkbox
                id="preserve-claude-path-case"
                checked={preserveClaudePathCase}
                onCheckedChange={(checked) => setPreserveClaudePathCase(checked === true)}
                disabled={saving || picking}
                className="mt-0.5"
              />
              <div className="grid gap-1">
                <Label htmlFor="preserve-claude-path-case" className="cursor-pointer leading-4">
                  保留输入路径大小写（高级）
                </Label>
                <p className="text-xs leading-5 text-muted-foreground">
                  默认使用系统解析后的标准路径。仅在需要精确匹配 Claude 已有的 Windows
                  路径变体时开启。
                </p>
                {preserveClaudePathCase && (
                  <p className="text-xs leading-5 text-amber-600 dark:text-amber-400">
                    这可能继续保留同一目录的多个 Claude 项目变体。
                  </p>
                )}
              </div>
            </div>
          )}
        </div>
        <DialogFooter>
          <Button
            variant="outline"
            onClick={() => onOpenChange(false)}
            disabled={saving || picking}
          >
            取消
          </Button>
          <Button
            onClick={() => void submit()}
            disabled={saving || picking || !targetCwd.trim()}
          >
            {saving && <Loader2 className="h-4 w-4 animate-spin" />}
            保存
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
