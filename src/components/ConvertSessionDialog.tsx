import { useEffect, useState } from "react";
import { ArrowLeftRight, Loader2 } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import {
  api,
  type ConvertReport,
  type CoreSessionProvider,
  type SessionConversionMode,
  type SessionSummary,
} from "@/lib/api";
import { sessionDisplayTitle } from "@/lib/sessionText";
import { useSettings } from "@/stores/settings";
import { toast } from "sonner";

type Props = {
  target: SessionSummary | null;
  onOpenChange: (v: boolean) => void;
  onDone?: (report: ConvertReport) => void;
};

export function ConvertSessionDialog({ target, onOpenChange, onDone }: Props) {
  const settings = useSettings((s) => s.settings);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [conversionMode, setConversionMode] = useState<SessionConversionMode>("simple");
  // Cursor 只出不进，两个目标都可选；Codex 与 Claude 各自只有一个可能的目标。
  const isCursorSource = target?.provider === "cursor";
  const [targetProvider, setTargetProvider] = useState<CoreSessionProvider>("claude");
  const effectiveTarget: CoreSessionProvider = isCursorSource
    ? targetProvider
    : target?.provider === "codex"
      ? "claude"
      : "codex";

  const providerLabels: Record<string, string> = {
    codex: "Codex",
    claude: "Claude Code",
    cursor: "Cursor",
  };
  const targetProviderLabel = providerLabels[effectiveTarget];
  const sourceProviderLabel = providerLabels[target?.provider ?? ""] ?? "会话";
  const nativeModeLabel =
    effectiveTarget === "claude" ? "原生Claude（实验）" : "原生Codex（实验）";
  const simpleModeDescription =
    effectiveTarget === "claude"
      ? "只保留真实用户消息和每轮最终答复，不依赖 Claude 的工具事件格式。"
      : "保留可见对话，工具调用和结果转为文本注记，不依赖 Codex 的工具事件格式。";
  const nativeModeDescription =
    effectiveTarget === "claude"
      ? "保留过程回复，配对完整的工具事件转为 Claude tool_use/tool_result。"
      : "保留过程回复和图片，配对完整的工具事件转为 Codex function_call/function_call_output。";

  useEffect(() => {
    if (!target) return;
    setConversionMode("simple");
    setTargetProvider("claude");
    setErr(null);
  }, [target?.rollout_path]);

  const submit = async () => {
    if (!settings || !target) return;
    setBusy(true);
    setErr(null);
    try {
      const report = await api.convertSessionProvider({
        codex_dir: settings.codex_dir,
        claude_dir: settings.claude_dir,
        cursor_dir: settings.cursor_dir,
        source_provider: target.provider,
        target_provider: effectiveTarget,
        rollout_path: target.rollout_path,
        conversion_mode: conversionMode,
      });
      const dropped = [
        report.dropped_reasoning > 0 ? `丢弃推理 ${report.dropped_reasoning} 段` : null,
        report.tool_notes > 0
          ? report.conversion_mode === "simple"
            ? report.source_provider === "codex"
              ? `跳过工具事件 ${report.tool_notes} 条`
              : `工具事件转为文本 ${report.tool_notes} 条`
            : `转换工具事件 ${report.tool_notes} 条`
          : null,
      ]
        .filter(Boolean)
        .join("，");
      toast.success(`已转换为 ${targetProviderLabel} 会话（${report.imported_messages} 条消息）`, {
        description: [dropped, `恢复命令：${report.resume_command}`]
          .filter(Boolean)
          .join("\n"),
      });
      for (const warning of report.warnings) {
        toast.warning(warning);
      }
      onDone?.(report);
      onOpenChange(false);
    } catch (e: any) {
      setErr(String(e?.message ?? e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={!!target} onOpenChange={(v) => !busy && onOpenChange(v)}>
      <DialogContent className="max-w-xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <ArrowLeftRight className="h-5 w-5 text-emerald-500" />
            转换为 {targetProviderLabel} 会话
          </DialogTitle>
          <DialogDescription>
            将 {sourceProviderLabel} 会话「
            {target ? sessionDisplayTitle(target.title, target.first_user_message) : ""}
            」转换为新的 {targetProviderLabel} 会话。
          </DialogDescription>
        </DialogHeader>

        {isCursorSource && (
          <div className="space-y-2">
            <span className="text-sm font-medium text-foreground">转换目标</span>
            <RadioGroup
              value={targetProvider}
              onValueChange={(value) => setTargetProvider(value as CoreSessionProvider)}
              className="flex gap-2"
            >
              {(["claude", "codex"] as const).map((value) => (
                <Label
                  key={value}
                  htmlFor={`convert-to-${value}`}
                  className="flex flex-1 cursor-pointer items-center gap-2 rounded-md border bg-muted/30 p-3"
                >
                  <RadioGroupItem id={`convert-to-${value}`} value={value} />
                  <span className="text-sm font-medium text-foreground">
                    {providerLabels[value]}
                  </span>
                </Label>
              ))}
            </RadioGroup>
          </div>
        )}

        <RadioGroup
          value={conversionMode}
          onValueChange={(value) => setConversionMode(value as SessionConversionMode)}
          className="gap-2"
        >
          <Label
            htmlFor="convert-simple"
            className="flex cursor-pointer items-start gap-3 rounded-md border bg-muted/30 p-3"
          >
            <RadioGroupItem id="convert-simple" value="simple" className="mt-0.5" />
            <span className="space-y-1">
              <span className="block text-sm font-medium text-foreground">简洁续聊（推荐）</span>
              <span className="block text-xs font-normal leading-relaxed text-muted-foreground">
                {simpleModeDescription}
              </span>
            </span>
          </Label>
          <Label
            htmlFor="convert-native"
            className="flex cursor-pointer items-start gap-3 rounded-md border bg-muted/30 p-3"
          >
            <RadioGroupItem id="convert-native" value="native" className="mt-0.5" />
            <span className="space-y-1">
              <span className="block text-sm font-medium text-foreground">{nativeModeLabel}</span>
              <span className="block text-xs font-normal leading-relaxed text-muted-foreground">
                {nativeModeDescription}
              </span>
            </span>
          </Label>
        </RadioGroup>

        <div className="space-y-1.5 rounded-md border bg-muted/40 p-3 text-xs text-muted-foreground">
          <p>· 转换会新建本地会话，不修改原会话。</p>
          <p>· 推理状态不会迁移，历史工具不会重新执行。</p>
        </div>

        {err && (
          <div className="max-h-24 overflow-y-auto rounded-md border border-destructive/40 bg-destructive/10 p-2 text-xs text-destructive wrap-anywhere">
            失败：{err}
          </div>
        )}

        <DialogFooter>
          <Button variant="outline" disabled={busy} onClick={() => onOpenChange(false)}>
            取消
          </Button>
          <Button disabled={busy || !settings} onClick={submit}>
            {busy && <Loader2 className="h-4 w-4 animate-spin" />}
            开始转换
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
