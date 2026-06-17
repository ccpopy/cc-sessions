import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Copy, Download, FileText, Loader2 } from "lucide-react";
import { toast } from "sonner";

import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { Label } from "@/components/ui/label";
import { Checkbox } from "@/components/ui/checkbox";
import { Badge } from "@/components/ui/badge";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import {
  api,
  type MarkdownExportOptions,
  type MarkdownExportReport,
  type SessionSummary,
} from "@/lib/api";
import { extractConversationMessages, type ConversationMessage } from "@/lib/markdown";
import { copyText } from "@/lib/clipboard";
import { saveFilePath } from "@/lib/dialog";
import { humanBytes, shortId } from "@/lib/format";
import { cn } from "@/lib/utils";

type Props = {
  open: boolean;
  onOpenChange: (v: boolean) => void;
  session: SessionSummary | null;
};

const EVENT_LIMIT = 100_000;

export function MarkdownExportDialog({ open, onOpenChange, session }: Props) {
  const [includeFrontMatter, setIncludeFrontMatter] = useState(true);
  const [includeReasoning, setIncludeReasoning] = useState(false);
  const [includeTools, setIncludeTools] = useState(false);
  const [handoff, setHandoff] = useState(false);
  const [selectionMode, setSelectionMode] = useState(false);

  const [messages, setMessages] = useState<ConversationMessage[]>([]);
  const [checked, setChecked] = useState<Set<number>>(new Set());
  const [loadingMessages, setLoadingMessages] = useState(false);

  const [report, setReport] = useState<MarkdownExportReport | null>(null);
  const [generating, setGenerating] = useState(false);
  const [saving, setSaving] = useState(false);

  const provider = session?.provider ?? "codex";
  const rolloutPath = session?.rollout_path ?? "";

  const header = useMemo(() => {
    if (!session) return null;
    return {
      title: session.title || "(无标题)",
      session_id: session.id,
      provider: session.provider,
      model: session.model,
      reasoning_effort: session.reasoning_effort,
      cwd: session.cwd,
      created_at: session.created_at,
      updated_at: session.updated_at,
      tokens_used: session.tokens_used,
      resume_command: session.resume_command,
    };
  }, [session]);

  // 打开时载入会话事件，构建可选择的对话列表
  useEffect(() => {
    if (!open || !rolloutPath) return;
    setIncludeFrontMatter(true);
    setIncludeReasoning(false);
    setIncludeTools(false);
    setHandoff(false);
    setSelectionMode(false);
    setReport(null);
    setLoadingMessages(true);
    let cancelled = false;
    void (async () => {
      try {
        const events = await api.previewRange(provider, rolloutPath, 0, EVENT_LIMIT);
        if (cancelled) return;
        const msgs = extractConversationMessages(events);
        setMessages(msgs);
        setChecked(new Set(msgs.map((m) => m.index)));
      } catch (e: any) {
        if (!cancelled) toast.error("读取会话失败：" + String(e?.message ?? e));
      } finally {
        if (!cancelled) setLoadingMessages(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [open, rolloutPath, provider]);

  const buildOptions = useCallback((): MarkdownExportOptions => {
    const selected =
      selectionMode && messages.length > 0
        ? messages.map((m) => m.index).filter((i) => checked.has(i))
        : null;
    return {
      include_front_matter: includeFrontMatter,
      include_reasoning: includeReasoning,
      include_tools: includeTools,
      ai_handoff_preamble: handoff,
      selected_indices: selected,
    };
  }, [selectionMode, messages, checked, includeFrontMatter, includeReasoning, includeTools, handoff]);

  // 防抖地生成预览（out_path=null，仅取返回文本）
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    if (!open || !header || !rolloutPath || loadingMessages) return;
    if (debounceRef.current) clearTimeout(debounceRef.current);
    debounceRef.current = setTimeout(() => {
      setGenerating(true);
      void api
        .exportSessionMarkdown({ provider, rollout_path: rolloutPath, header, options: buildOptions() })
        .then((r) => setReport(r))
        .catch((e: any) => toast.error("生成预览失败：" + String(e?.message ?? e)))
        .finally(() => setGenerating(false));
    }, 250);
    return () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    };
  }, [open, header, rolloutPath, provider, loadingMessages, buildOptions]);

  const selectedCount = useMemo(
    () => (selectionMode ? messages.filter((m) => checked.has(m.index)).length : messages.length),
    [selectionMode, messages, checked],
  );

  const toggleOne = (index: number) => {
    setChecked((prev) => {
      const next = new Set(prev);
      if (next.has(index)) next.delete(index);
      else next.add(index);
      return next;
    });
  };
  const checkAll = () => setChecked(new Set(messages.map((m) => m.index)));
  const checkNone = () => setChecked(new Set());

  const onCopy = async () => {
    if (!report) return;
    try {
      await copyText(report.markdown);
      toast.success(`已复制 Markdown（${report.message_count} 条对话）`);
    } catch (e: any) {
      toast.error("复制失败：" + String(e?.message ?? e));
    }
  };

  const onExportFile = async () => {
    if (!header || !rolloutPath) return;
    const path = await saveFilePath({
      title: "导出会话为 Markdown",
      defaultPath: `${slugify(header.title)}-${shortId(header.session_id)}.md`,
      filters: [{ name: "Markdown", extensions: ["md"] }],
    });
    if (!path) return;
    setSaving(true);
    try {
      const r = await api.exportSessionMarkdown({
        provider,
        rollout_path: rolloutPath,
        out_path: path,
        header,
        options: buildOptions(),
      });
      toast.success("已导出 Markdown", {
        description: `${r.message_count} 条对话 · ${humanBytes(r.bytes)} · ${r.out_path}`,
      });
    } catch (e: any) {
      toast.error("导出失败：" + String(e?.message ?? e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex h-[85vh] max-w-[96vw] min-w-0 flex-col gap-0 overflow-hidden p-0 sm:max-w-[960px]">
        <DialogHeader className="border-b border-border/60 px-6 pb-3.5 pt-4">
          <DialogTitle className="flex items-center gap-2 text-[15px] font-semibold tracking-tight">
            <FileText className="h-[18px] w-[18px] text-muted-foreground" />
            导出为 Markdown
          </DialogTitle>
          {session && (
            <p className="mt-1 line-clamp-1 text-xs text-muted-foreground">{session.title || "(无标题)"}</p>
          )}
        </DialogHeader>

        <div className="grid min-h-0 flex-1 grid-cols-1 overflow-hidden md:grid-cols-[300px_minmax(0,1fr)]">
          {/* 左侧：选项 + 选择 */}
          <div className="flex min-h-0 flex-col border-b border-border/60 md:border-b-0 md:border-r">
            <ScrollArea className="min-h-0 flex-1">
              <div className="space-y-4 px-5 py-4">
                <div className="space-y-3">
                  <h3 className="text-xs font-semibold text-muted-foreground">内容</h3>
                  <SwitchRow
                    id="md-handoff"
                    label="AI 交接前言"
                    hint="在开头加入给另一个 AI 的引导说明与原始诉求"
                    checked={handoff}
                    onChange={setHandoff}
                  />
                  <SwitchRow
                    id="md-front"
                    label="YAML 头信息"
                    hint="标题 / 模型 / 时间 / token / resume 等元数据"
                    checked={includeFrontMatter}
                    onChange={setIncludeFrontMatter}
                  />
                  <SwitchRow
                    id="md-reasoning"
                    label="包含推理过程"
                    hint="模型 thinking，可能不可读或加密"
                    checked={includeReasoning}
                    onChange={setIncludeReasoning}
                  />
                  <SwitchRow
                    id="md-tools"
                    label="包含工具调用"
                    hint="工具调用与返回，通常是执行噪音"
                    checked={includeTools}
                    onChange={setIncludeTools}
                  />
                </div>

                <Separator />

                <div className="space-y-3">
                  <SwitchRow
                    id="md-selection"
                    label="选择消息片段"
                    hint="只导出勾选的对话；用于摘录某一段"
                    checked={selectionMode}
                    onChange={setSelectionMode}
                  />
                  {selectionMode && (
                    <div className="flex items-center gap-2 text-xs">
                      <Button variant="outline" size="sm" className="h-7" onClick={checkAll}>
                        全选
                      </Button>
                      <Button variant="outline" size="sm" className="h-7" onClick={checkNone}>
                        清空
                      </Button>
                      <span className="text-muted-foreground">
                        {checked.size}/{messages.length}
                      </span>
                    </div>
                  )}
                </div>

                {selectionMode && (
                  <div className="space-y-1">
                    {loadingMessages ? (
                      <div className="flex justify-center py-6">
                        <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />
                      </div>
                    ) : messages.length === 0 ? (
                      <p className="py-4 text-center text-xs text-muted-foreground">无对话消息</p>
                    ) : (
                      messages.map((m) => (
                        <div
                          key={m.index}
                          role="button"
                          tabIndex={0}
                          onClick={() => toggleOne(m.index)}
                          onKeyDown={(e) => {
                            if (e.key === "Enter" || e.key === " ") {
                              e.preventDefault();
                              toggleOne(m.index);
                            }
                          }}
                          className={cn(
                            "flex w-full cursor-pointer items-start gap-2 rounded-md border border-transparent px-2 py-1.5 text-left text-xs hover:bg-muted/50",
                            checked.has(m.index) && "border-border/60 bg-muted/40",
                          )}
                        >
                          <Checkbox
                            checked={checked.has(m.index)}
                            className="pointer-events-none mt-0.5 shrink-0"
                          />
                          <span className="min-w-0 flex-1">
                            <span
                              className={cn(
                                "mr-1.5 font-medium",
                                m.role === "user" ? "text-primary" : "text-emerald-600 dark:text-emerald-400",
                              )}
                            >
                              {m.role === "user" ? "你" : "AI"}
                            </span>
                            <span className="text-muted-foreground">{singleLine(m.text)}</span>
                          </span>
                        </div>
                      ))
                    )}
                  </div>
                )}
              </div>
            </ScrollArea>
          </div>

          {/* 右侧：预览 */}
          <div className="flex min-h-0 flex-col bg-muted/20">
            <div className="flex items-center gap-2 border-b border-border/60 px-4 py-2 text-xs text-muted-foreground">
              <span>预览</span>
              {generating && <Loader2 className="h-3 w-3 animate-spin" />}
              <span className="ml-auto flex items-center gap-2">
                <Badge variant="outline" className="h-5 px-1.5 font-normal">
                  {selectedCount} 条对话
                </Badge>
                {report && (
                  <span className="tabular-nums text-muted-foreground/70">{humanBytes(report.bytes)}</span>
                )}
              </span>
            </div>
            <ScrollArea className="min-h-0 flex-1">
              <pre className="whitespace-pre-wrap break-words px-4 py-3 font-mono text-xs leading-relaxed text-foreground/90">
                {report?.markdown ?? ""}
              </pre>
            </ScrollArea>
          </div>
        </div>

        <div className="flex items-center gap-2 border-t border-border/60 px-6 py-3">
          <p className="text-xs text-muted-foreground">
            默认仅导出对话；工具调用与推理需手动开启。
          </p>
          <div className="ml-auto flex items-center gap-2">
            <Button variant="outline" size="sm" onClick={onCopy} disabled={!report || generating} className="gap-1.5">
              <Copy className="h-3.5 w-3.5" />
              复制 Markdown
            </Button>
            <Button size="sm" onClick={onExportFile} disabled={saving || !header} className="gap-1.5">
              {saving ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Download className="h-3.5 w-3.5" />}
              导出为文件
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function SwitchRow({
  id,
  label,
  hint,
  checked,
  onChange,
}: {
  id: string;
  label: string;
  hint: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <div className="flex items-start justify-between gap-3">
      <div className="min-w-0">
        <Label htmlFor={id} className="cursor-pointer text-sm">
          {label}
        </Label>
        <p className="mt-0.5 text-[11px] leading-snug text-muted-foreground">{hint}</p>
      </div>
      <Switch id={id} checked={checked} onCheckedChange={onChange} className="mt-0.5 shrink-0" />
    </div>
  );
}

function singleLine(text: string): string {
  return text.replace(/\s+/g, " ").trim().slice(0, 80);
}

function slugify(title: string): string {
  const cleaned = title
    .replace(/[\\/:*?"<>|]+/g, " ")
    .replace(/\s+/g, "-")
    .replace(/^-+|-+$/g, "");
  return (cleaned || "session").slice(0, 60);
}
