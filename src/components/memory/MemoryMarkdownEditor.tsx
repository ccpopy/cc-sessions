import { Columns2, Eye, FileCode2, PencilLine } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";

export type MemoryEditorMode = "preview" | "split" | "source";

type Props = {
  value: string;
  onChange: (value: string) => void;
  mode: MemoryEditorMode;
  onModeChange: (mode: MemoryEditorMode) => void;
};

const MODES: Array<{
  value: MemoryEditorMode;
  label: string;
  title: string;
  icon: typeof Eye;
}> = [
  { value: "preview", label: "阅读", title: "阅读渲染后的文档", icon: Eye },
  { value: "split", label: "双栏", title: "编辑 Markdown 并实时预览", icon: Columns2 },
  { value: "source", label: "源码", title: "只编辑 Markdown 源码", icon: FileCode2 },
];

export function MemoryMarkdownEditor({ value, onChange, mode, onModeChange }: Props) {
  return (
    <div className="flex min-h-0 flex-1 flex-col bg-background/75">
      <div className="flex h-10 shrink-0 items-center gap-2 border-b border-border/60 bg-muted/15 px-3">
        <div
          role="group"
          aria-label="Memory 编辑器显示模式"
          className="inline-flex items-center rounded-md border border-border/70 bg-background p-0.5 shadow-sm"
        >
          {MODES.map((item) => {
            const Icon = item.icon;
            const active = mode === item.value;
            return (
              <button
                key={item.value}
                type="button"
                aria-pressed={active}
                title={item.title}
                onClick={() => onModeChange(item.value)}
                className={cn(
                  "flex h-6 items-center gap-1 rounded px-2 text-[11px] transition-colors",
                  "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
                  active
                    ? "bg-provider-claude/12 font-medium text-provider-claude-fg shadow-sm"
                    : "text-muted-foreground hover:bg-accent hover:text-foreground",
                )}
              >
                <Icon className="h-3 w-3" />
                <span>{item.label}</span>
              </button>
            );
          })}
        </div>

        <span className="hidden text-[10px] text-muted-foreground sm:inline">
          {mode === "preview"
            ? "以文档样式阅读，不会改变原始 Markdown"
            : mode === "split"
              ? "左侧修改会实时呈现在右侧"
              : "直接编辑 Markdown 原文"}
        </span>

        {mode === "preview" && (
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="ml-auto h-7 gap-1.5 text-xs"
            onClick={() => onModeChange("split")}
          >
            <PencilLine className="h-3.5 w-3.5" />
            编辑内容
          </Button>
        )}
      </div>

      {mode === "preview" ? (
        <MarkdownPreview value={value} className="flex-1" />
      ) : mode === "source" ? (
        <MarkdownSource value={value} onChange={onChange} className="flex-1" />
      ) : (
        <div className="grid min-h-0 flex-1 grid-cols-1 grid-rows-2 xl:grid-cols-2 xl:grid-rows-1">
          <EditorPane label="MARKDOWN 源码" className="border-b border-border/60 xl:border-b-0 xl:border-r">
            <MarkdownSource value={value} onChange={onChange} className="flex-1" />
          </EditorPane>
          <EditorPane label="实时预览">
            <MarkdownPreview value={value} className="flex-1" />
          </EditorPane>
        </div>
      )}
    </div>
  );
}

function EditorPane({
  label,
  className,
  children,
}: {
  label: string;
  className?: string;
  children: React.ReactNode;
}) {
  return (
    <div className={cn("flex min-h-0 min-w-0 flex-col", className)}>
      <div className="flex h-7 shrink-0 items-center border-b border-border/45 bg-muted/10 px-3 text-[9px] font-semibold tracking-[0.12em] text-muted-foreground">
        {label}
      </div>
      {children}
    </div>
  );
}

function MarkdownSource({
  value,
  onChange,
  className,
}: {
  value: string;
  onChange: (value: string) => void;
  className?: string;
}) {
  return (
    <Textarea
      value={value}
      onChange={(event) => onChange(event.target.value)}
      spellCheck={false}
      className={cn(
        "memory-source-scrollbar min-h-0 resize-none rounded-none border-0 bg-transparent p-5 font-mono text-[13px] leading-6 shadow-none focus-visible:ring-0",
        className,
      )}
      placeholder="记录需要 Claude 长期记住的项目约束、偏好与经验，支持 Markdown。"
    />
  );
}

function MarkdownPreview({ value, className }: { value: string; className?: string }) {
  return (
    <ScrollArea className={cn("min-h-0", className)}>
      {value.trim() ? (
        <article className="memory-md mx-auto w-full max-w-4xl px-7 py-6 sm:px-9 sm:py-8">
          <ReactMarkdown remarkPlugins={[remarkGfm]} skipHtml>
            {value}
          </ReactMarkdown>
        </article>
      ) : (
        <div className="flex min-h-full items-center justify-center px-6 py-12 text-center text-xs leading-relaxed text-muted-foreground">
          这里会显示 Markdown 的渲染结果。切换到“双栏”或“源码”即可开始编辑。
        </div>
      )}
    </ScrollArea>
  );
}
