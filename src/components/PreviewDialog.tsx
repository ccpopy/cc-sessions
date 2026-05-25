import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import {
  Bot,
  ChevronDown,
  Copy,
  FileJson,
  FolderOpen,
  GitBranch,
  MessageSquare,
  Sparkles,
  Terminal,
  User,
  Wrench,
} from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { JsonView, defaultStyles } from "react-json-view-lite";
import "react-json-view-lite/dist/index.css";

import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
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
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Label } from "@/components/ui/label";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import { api, type PreviewEvent, type SessionSummary } from "@/lib/api";
import { formatTimeString, humanTokens } from "@/lib/format";
import { shouldIgnoreTextEditingHotkey } from "@/lib/keyboard";
import { parseEmbeddedTranscriptPrompt, type EmbeddedTranscriptPrompt } from "@/lib/sessionText";
import { cn } from "@/lib/utils";
import { toast } from "sonner";

type Props = {
  open: boolean;
  onOpenChange: (v: boolean) => void;
  session: SessionSummary | null;
  customRolloutPath?: string;
  codexDir?: string;
  onForked?: () => void | Promise<void>;
};

type DiffCommentPrompt = {
  comments: DiffComment[];
  request: string;
};

type DiffComment = {
  number: number;
  context: string;
  body: string;
};

type ForkAction = {
  enabled: boolean;
  pending: boolean;
  onSelect: (event: PreviewEvent) => void;
};

const PAGE = 200;

export function PreviewDialog({
  open,
  onOpenChange,
  session,
  customRolloutPath,
  codexDir,
  onForked,
}: Props) {
  const rolloutPath = customRolloutPath ?? session?.rollout_path ?? "";
  const provider = session?.provider ?? "codex";
  const [events, setEvents] = useState<PreviewEvent[]>([]);
  const [loading, setLoading] = useState(false);
  const [done, setDone] = useState(false);
  const [filter, setFilter] = useState("");
  const [onlyMsg, setOnlyMsg] = useState(true);
  const [forkTarget, setForkTarget] = useState<PreviewEvent | null>(null);
  const [forking, setForking] = useState(false);
  const offsetRef = useRef(0);
  const loadingRef = useRef(false);
  const doneRef = useRef(false);
  const viewportRef = useRef<HTMLDivElement | null>(null);
  const canForkSession = provider === "codex" && !customRolloutPath && !!session && !!codexDir;

  const loadMore = useCallback(async () => {
    if (loadingRef.current || doneRef.current || !rolloutPath) return;
    loadingRef.current = true;
    setLoading(true);
    try {
      const next = await api.previewRange(provider, rolloutPath, offsetRef.current, PAGE);
      if (next.length === 0) {
        doneRef.current = true;
        setDone(true);
      } else {
        offsetRef.current += next.length;
        setEvents((prev) => [...prev, ...next]);
        if (next.length < PAGE) {
          doneRef.current = true;
          setDone(true);
        }
      }
    } finally {
      loadingRef.current = false;
      setLoading(false);
    }
  }, [provider, rolloutPath]);

  useEffect(() => {
    if (!open || !rolloutPath) return;
    setEvents([]);
    setDone(false);
    doneRef.current = false;
    loadingRef.current = false;
    setFilter("");
    setOnlyMsg(true);
    offsetRef.current = 0;
    void loadMore();
  }, [open, rolloutPath, loadMore]);

  useEffect(() => {
    if (!open || loading || done) return;
    const viewport = viewportRef.current;
    if (!viewport) return;
    if (viewport.scrollHeight <= viewport.clientHeight + 20) {
      void loadMore();
    }
  }, [done, events.length, loadMore, loading, open]);

  const filtered = useMemo(() => {
    return events.filter((e) => {
      if (onlyMsg && !isConversationMessage(e)) return false;
      if (!filter) return true;
      const low = filter.toLowerCase();
      return (
        e.text_summary.toLowerCase().includes(low) ||
        e.kind.toLowerCase().includes(low) ||
        JSON.stringify(e.raw).toLowerCase().includes(low)
      );
    });
  }, [events, filter, onlyMsg]);

  const onScroll = (e: React.UIEvent<HTMLDivElement>) => {
    const el = e.currentTarget;
    if (el.scrollHeight - el.scrollTop - el.clientHeight < 200) {
      void loadMore();
    }
  };

  const onPreviewKeyDown = useCallback(
    (e: ReactKeyboardEvent<HTMLDivElement>) => {
      if (shouldIgnoreTextEditingHotkey(e.target)) return;

      const viewport = viewportRef.current;
      if (!viewport) return;

      const maxScrollTop = Math.max(viewport.scrollHeight - viewport.clientHeight, 0);
      const pageDelta = Math.max(Math.floor(viewport.clientHeight * 0.9), 120);
      let nextScrollTop: number | null = null;
      let keepAtBottomAfterLoad = false;

      switch (e.key) {
        case "Home":
          nextScrollTop = 0;
          break;
        case "End":
          nextScrollTop = maxScrollTop;
          keepAtBottomAfterLoad = true;
          break;
        case "PageUp":
          nextScrollTop = viewport.scrollTop - pageDelta;
          break;
        case "PageDown":
          nextScrollTop = viewport.scrollTop + pageDelta;
          break;
        default:
          return;
      }

      e.preventDefault();

      const clampedScrollTop = Math.max(0, Math.min(nextScrollTop, maxScrollTop));
      viewport.scrollTo({ top: clampedScrollTop });

      if (keepAtBottomAfterLoad) {
        void loadMore().then(() => {
          requestAnimationFrame(() => {
            const nextViewport = viewportRef.current;
            if (!nextViewport) return;
            nextViewport.scrollTo({
              top: Math.max(nextViewport.scrollHeight - nextViewport.clientHeight, 0),
            });
          });
        });
        return;
      }

      if (maxScrollTop - clampedScrollTop < 200) {
        void loadMore();
      }
    },
    [loadMore],
  );

  const copyResume = async () => {
    if (!session) return;
    try {
      const text = session.resume_command || (await api.copyResumeCommand(session.provider, session.id));
      await navigator.clipboard.writeText(text);
      toast.success(`已复制：${text}`);
    } catch (e: any) {
      toast.error("复制失败：" + String(e?.message ?? e));
    }
  };

  const copySessionId = async () => {
    if (!session) return;
    try {
      await navigator.clipboard.writeText(session.id);
      toast.success(`已复制会话 ID：${session.id}`);
    } catch (e: any) {
      toast.error("复制会话 ID 失败：" + String(e?.message ?? e));
    }
  };

  const reveal = async () => {
    if (!session) return;
    try {
      await api.revealCwd(session.cwd);
    } catch (e: any) {
      toast.error("打开失败：" + String(e?.message ?? e));
    }
  };

  const copyPath = () => {
    if (!rolloutPath) return;
    navigator.clipboard.writeText(rolloutPath);
    toast.success("已复制 rollout 路径");
  };

  const requestForkAt = (event: PreviewEvent) => {
    if (!canForkSession) return;
    setForkTarget(event);
  };

  const confirmForkAt = async () => {
    if (!session || !codexDir || !rolloutPath || !forkTarget) return;
    setForking(true);
    try {
      const report = await api.forkSessionAtEvent({
        codex_dir: codexDir,
        session_id: session.id,
        rollout_path: rolloutPath,
        event_index: forkTarget.index,
      });
      toast.success("已创建回溯分支", {
        description: `新会话 ${report.new_id.slice(0, 8)}，已复制 ${report.included_lines} 行`,
      });
      setForkTarget(null);
      onOpenChange(false);
      await onForked?.();
    } catch (e: any) {
      toast.error("创建回溯分支失败", {
        description: String(e?.message ?? e),
      });
    } finally {
      setForking(false);
    }
  };

  return (
    <>
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        className="flex h-[90vh] max-w-[96vw] min-w-0 flex-col gap-0 overflow-hidden p-0 sm:max-w-[1200px]"
        onKeyDown={onPreviewKeyDown}
      >
        <DialogHeader className="relative min-w-0 border-b border-border/60 px-6 pb-3.5 pt-4 after:pointer-events-none after:absolute after:inset-x-0 after:-bottom-px after:h-px after:bg-gradient-to-r after:from-transparent after:via-border/50 after:to-transparent">
          <div className="flex items-start gap-3.5">
            <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl border border-border/60 bg-gradient-to-br from-muted to-muted/40 shadow-sm">
              <Sparkles className="h-[18px] w-[18px] text-muted-foreground" />
            </div>
            <div className="min-w-0 flex-1">
              <DialogTitle className="truncate text-[15px] font-semibold tracking-tight">
                {session?.title || "预览会话"}
              </DialogTitle>
              {session && (
                <div className="mt-1.5 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground">
                  <span className="font-mono text-foreground/70">{session.id.slice(0, 8)}</span>
                  {session.model && (
                    <>
                      <Dot />
                      <Badge variant="secondary" className="h-5 px-1.5 font-normal">
                        {session.model}
                        {session.reasoning_effort ? ` · ${session.reasoning_effort}` : ""}
                      </Badge>
                    </>
                  )}
                  {session.tokens_used > 0 && (
                    <>
                      <Dot />
                      <span className="tabular-nums">
                        {humanTokens(session.tokens_used)} token
                      </span>
                    </>
                  )}
                  {session.cwd_display && (
                    <>
                      <Dot />
                      <span className="min-w-0 truncate" title={session.cwd}>
                        {session.cwd_display}
                      </span>
                    </>
                  )}
                </div>
              )}
            </div>
          </div>

          <div className="mt-3.5 flex flex-wrap items-center gap-2">
            <Input
              placeholder="在事件中过滤…"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              className="h-8 w-64 border-border/70"
            />
            <label
              htmlFor="only-msg"
              className="group flex h-8 cursor-pointer items-center gap-2 rounded-md border border-border/70 bg-muted/30 px-2.5 transition-colors hover:bg-muted/50"
            >
              <Switch id="only-msg" checked={onlyMsg} onCheckedChange={setOnlyMsg} />
              <Label htmlFor="only-msg" className="cursor-pointer text-xs">
                仅看对话消息
              </Label>
            </label>
            <span className="text-[11px] text-muted-foreground">
              显示 <span className="tabular-nums text-foreground/80">{filtered.length}</span>
              <span className="mx-1 text-muted-foreground/50">/</span>
              已加载 <span className="tabular-nums text-foreground/80">{events.length}</span> 条
              <span className="ml-1 text-muted-foreground/70">
                {!done ? "· 滚动加载更多" : "· 已到末尾"}
              </span>
            </span>
            <div className="ml-auto flex items-center gap-0.5">
              {session && (
                <>
                  <Button variant="ghost" size="sm" className="h-8 gap-1.5 px-2.5" onClick={copySessionId}>
                    <Copy className="h-3.5 w-3.5" />
                    复制会话 ID
                  </Button>
                  <Button variant="ghost" size="sm" className="h-8 gap-1.5 px-2.5" onClick={copyResume}>
                    <Copy className="h-3.5 w-3.5" />
                    复制 resume
                  </Button>
                  <Button variant="ghost" size="sm" className="h-8 gap-1.5 px-2.5" onClick={reveal}>
                    <FolderOpen className="h-3.5 w-3.5" />
                    打开目录
                  </Button>
                  <Separator orientation="vertical" className="mx-1 h-4 bg-border/60" />
                </>
              )}
              <Button variant="ghost" size="sm" className="h-8 gap-1.5 px-2.5" onClick={copyPath}>
                <FileJson className="h-3.5 w-3.5" />
                复制路径
              </Button>
            </div>
          </div>
        </DialogHeader>

        <ScrollArea
          className="min-h-0 flex-1 bg-muted/30"
          viewportRef={viewportRef}
          onViewportScroll={onScroll}
        >
          <div className="mx-auto w-full max-w-3xl min-w-0 space-y-4 overflow-x-hidden px-6 py-6">
            {filtered.length === 0 && !loading && (
              <div className="flex flex-col items-center justify-center gap-2 py-16 text-center text-muted-foreground">
                <Sparkles className="h-8 w-8 opacity-50" />
                <div className="text-sm">
                  {events.length === 0 ? "无事件" : "无匹配事件"}
                </div>
              </div>
            )}

            {filtered.map((e) => (
              <EventBubble
                key={e.index}
                e={e}
                forkAction={{
                  enabled: canForkSession && isStableForkNode(e),
                  pending: forking,
                  onSelect: requestForkAt,
                }}
              />
            ))}

            {loading && (
              <div className="flex justify-center py-4 text-xs text-muted-foreground">加载中…</div>
            )}
            {!done && events.length > 0 && (
              <div className="flex justify-center pt-2">
                <Button
                  variant="outline"
                  size="sm"
                  className="h-8"
                  disabled={loading}
                  onClick={() => void loadMore()}
                >
                  加载更多事件
                </Button>
              </div>
            )}
            {done && events.length > 0 && (
              <div className="flex justify-center pt-4 text-xs text-muted-foreground/70">
                — 会话末尾 —
              </div>
            )}
          </div>
        </ScrollArea>
      </DialogContent>
    </Dialog>
    <AlertDialog open={!!forkTarget} onOpenChange={(v) => !v && !forking && setForkTarget(null)}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>从此处创建回溯分支</AlertDialogTitle>
          <AlertDialogDescription>
            系统会只复制当前节点之前的有效会话历史，生成一个新的 active 会话分支；原会话会归档到分支历史中，不会被删除。
          </AlertDialogDescription>
        </AlertDialogHeader>
        <div className="rounded-md border bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
          <div className="font-mono">line {forkTarget ? forkTarget.index + 1 : ""}</div>
          {forkTarget?.text_summary && (
            <div className="mt-1 line-clamp-2 text-foreground">{forkTarget.text_summary}</div>
          )}
        </div>
        <AlertDialogFooter>
          <AlertDialogCancel disabled={forking}>取消</AlertDialogCancel>
          <AlertDialogAction disabled={forking} onClick={(e) => {
            e.preventDefault();
            void confirmForkAt();
          }}>
            创建分支
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
    </>
  );
}

/* ---------- 单条事件（聊天气泡）---------- */

function EventBubble({ e, forkAction }: { e: PreviewEvent; forkAction: ForkAction }) {
  const ts = formatTimeString(e.timestamp);

  if (isEventMessage(e)) {
    return <EventMessageBubble e={e} ts={ts} forkAction={forkAction} />;
  }
  if (e.role === "user") {
    return <UserBubble e={e} ts={ts} forkAction={forkAction} />;
  }
  if (e.role === "assistant") {
    return <AssistantBubble e={e} ts={ts} forkAction={forkAction} />;
  }
  if (e.role === "reasoning") {
    return <ReasoningBubble e={e} ts={ts} />;
  }
  if (e.role === "tool_call" || e.role === "tool_result") {
    return <ToolBubble e={e} ts={ts} />;
  }
  if (e.role === "meta") {
    return <MetaLine e={e} ts={ts} />;
  }
  return <DefaultBubble e={e} ts={ts} />;
}

function EventMessageBubble({
  e,
  ts,
  forkAction,
}: {
  e: PreviewEvent;
  ts: string;
  forkAction: ForkAction;
}) {
  const [open, setOpen] = useState(false);
  return (
    <div className="group flex gap-3">
      <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-sky-500/15 text-sky-600 dark:text-sky-400">
        <Sparkles className="h-4 w-4" />
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex w-full items-center gap-2 rounded-md border bg-card px-3 py-2 text-left text-xs shadow-sm hover:bg-accent">
          <button
            onClick={() => setOpen((x) => !x)}
            className="flex min-w-0 flex-1 items-center gap-2 text-left"
          >
            <ChevronDown className={cn("h-3.5 w-3.5 shrink-0 transition-transform", open && "rotate-180")} />
            <span className="shrink-0 font-medium">{eventMessageLabel(e)}</span>
            <EventSourceBadge e={e} />
            <span className="min-w-0 flex-1 truncate text-muted-foreground">
              {e.text_summary || ""}
            </span>
            {ts && <span className="shrink-0 font-mono text-muted-foreground/70">{ts}</span>}
          </button>
          <ForkNodeButton event={e} action={forkAction} />
        </div>
        {open && (
          <div className="mt-1.5 overflow-auto rounded-md border bg-card p-3 text-xs">
            <JsonView
              data={e.raw as object}
              style={defaultStyles}
              shouldExpandNode={(level) => level < 2}
            />
          </div>
        )}
      </div>
    </div>
  );
}

function UserBubble({ e, ts, forkAction }: { e: PreviewEvent; ts: string; forkAction: ForkAction }) {
  const text = extractText(e);
  const embeddedTranscript = parseEmbeddedTranscriptPrompt(text);
  if (embeddedTranscript) {
    return <EmbeddedTranscriptBubble e={e} ts={ts} prompt={embeddedTranscript} forkAction={forkAction} />;
  }

  const diffComments = parseDiffCommentPrompt(text);
  if (diffComments) {
    return <DiffCommentBubble e={e} ts={ts} prompt={diffComments} forkAction={forkAction} />;
  }

  return (
    <div className="group flex justify-end gap-3">
      <div className="flex min-w-0 max-w-[85%] flex-col items-end overflow-hidden">
        <div className="mb-1 flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <ForkNodeButton event={e} action={forkAction} />
          <span>你</span>
          <EventSourceBadge e={e} />
          {ts && <span className="font-mono">· {ts}</span>}
        </div>
        <div className="chat-md max-w-full rounded-2xl rounded-tr-sm bg-primary px-4 py-2.5 text-primary-foreground">
          {text ? <ReactMarkdown remarkPlugins={[remarkGfm]}>{text}</ReactMarkdown> : (
            <span className="italic opacity-70">(空消息)</span>
          )}
        </div>
      </div>
      <Avatar role="user" />
    </div>
  );
}

function EmbeddedTranscriptBubble({
  e,
  ts,
  prompt,
  forkAction,
}: {
  e: PreviewEvent;
  ts: string;
  prompt: EmbeddedTranscriptPrompt;
  forkAction: ForkAction;
}) {
  const [open, setOpen] = useState(false);
  return (
    <div className="group flex justify-end gap-3">
      <div className="flex min-w-0 max-w-[85%] flex-col items-end overflow-hidden">
        <div className="mb-1 flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <ForkNodeButton event={e} action={forkAction} />
          <span>你</span>
          <EventSourceBadge e={e} />
          {ts && <span className="font-mono">· {ts}</span>}
        </div>

        <div className="flex w-full flex-col items-end gap-2">
          <div className="inline-flex h-7 items-center gap-1.5 rounded-full border bg-card px-3 text-xs text-muted-foreground shadow-sm">
            <MessageSquare className="h-3.5 w-3.5" />
            <span>自动评审上下文</span>
          </div>

          {prompt.request && (
            <div className="chat-md max-w-full rounded-2xl rounded-tr-sm bg-primary px-4 py-2.5 text-primary-foreground">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>{prompt.request}</ReactMarkdown>
            </div>
          )}

          <button
            type="button"
            onClick={() => setOpen((x) => !x)}
            className="inline-flex h-7 max-w-full items-center gap-1.5 rounded-md border bg-card px-2.5 text-left text-xs text-muted-foreground shadow-sm hover:bg-accent"
          >
            <ChevronDown className={cn("h-3.5 w-3.5 shrink-0 transition-transform", open && "rotate-180")} />
            <span className="truncate">嵌入会话历史</span>
          </button>

          {open && (
            <pre className="max-h-80 max-w-full overflow-auto rounded-md border bg-card p-3 text-left font-mono text-xs leading-relaxed text-card-foreground">
              {prompt.transcript}
            </pre>
          )}
        </div>
      </div>
      <Avatar role="user" />
    </div>
  );
}

function DiffCommentBubble({
  e,
  ts,
  prompt,
  forkAction,
}: {
  e: PreviewEvent;
  ts: string;
  prompt: DiffCommentPrompt;
  forkAction: ForkAction;
}) {
  const countLabel = `${prompt.comments.length} 条批注`;

  return (
    <div className="group flex justify-end gap-3">
      <div className="flex min-w-0 max-w-[85%] flex-col items-end overflow-hidden">
        <div className="mb-1 flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <ForkNodeButton event={e} action={forkAction} />
          <span>你</span>
          <EventSourceBadge e={e} />
          {ts && <span className="font-mono">· {ts}</span>}
        </div>

        <div className="flex w-full flex-col items-end gap-2">
          <div className="inline-flex h-7 items-center gap-1.5 rounded-full border bg-card px-3 text-xs text-muted-foreground shadow-sm">
            <MessageSquare className="h-3.5 w-3.5" />
            <span>{countLabel}</span>
          </div>

          <div className="flex w-full flex-col items-end gap-2">
            {prompt.comments.map((comment) => (
              <div
                key={comment.number}
                className="w-full max-w-[28rem] overflow-hidden rounded-xl border bg-card px-4 py-3 text-left text-sm text-card-foreground shadow-sm"
              >
                {comment.context && (
                  <p className="mb-2 line-clamp-3 text-xs leading-relaxed text-muted-foreground">
                    {comment.context}
                  </p>
                )}
                <div className="chat-md font-medium">
                  <ReactMarkdown remarkPlugins={[remarkGfm]}>{comment.body}</ReactMarkdown>
                </div>
              </div>
            ))}

            {prompt.request && (
              <div className="chat-md max-w-full rounded-2xl rounded-tr-sm bg-primary px-4 py-2.5 text-primary-foreground">
                <ReactMarkdown remarkPlugins={[remarkGfm]}>{prompt.request}</ReactMarkdown>
              </div>
            )}
          </div>
        </div>
      </div>
      <Avatar role="user" />
    </div>
  );
}

function AssistantBubble({
  e,
  ts,
  forkAction,
}: {
  e: PreviewEvent;
  ts: string;
  forkAction: ForkAction;
}) {
  const text = extractText(e);
  return (
    <div className="group flex gap-3">
      <Avatar role="assistant" />
      <div className="flex min-w-0 max-w-[85%] flex-col overflow-hidden">
        <div className="mb-1 flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <span>Assistant</span>
          <EventSourceBadge e={e} />
          {ts && <span className="font-mono">· {ts}</span>}
          <ForkNodeButton event={e} action={forkAction} />
        </div>
        <div className="chat-md max-w-full rounded-2xl rounded-tl-sm border bg-card px-4 py-3 text-card-foreground shadow-sm">
          {text ? <ReactMarkdown remarkPlugins={[remarkGfm]}>{text}</ReactMarkdown> : (
            <span className="italic text-muted-foreground">(空消息)</span>
          )}
        </div>
      </div>
    </div>
  );
}

function ReasoningBubble({ e, ts }: { e: PreviewEvent; ts: string }) {
  const text = extractText(e);
  const [open, setOpen] = useState(false);
  return (
    <div className="flex gap-3">
      <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-muted">
        <Sparkles className="h-4 w-4 text-muted-foreground/70" />
      </div>
      <div className="min-w-0 flex-1">
        <button
          onClick={() => setOpen((x) => !x)}
          className="flex items-center gap-1.5 text-[11px] text-muted-foreground hover:text-foreground"
        >
          <ChevronDown className={cn("h-3 w-3 transition-transform", open && "rotate-180")} />
          推理过程
          {ts && <span className="font-mono">· {ts}</span>}
        </button>
        {open && text && (
          <pre className="mt-1.5 whitespace-pre-wrap break-words rounded-md border border-dashed bg-muted/40 px-3 py-2 font-mono text-xs text-muted-foreground">
            {text}
          </pre>
        )}
      </div>
    </div>
  );
}

function ToolBubble({ e, ts }: { e: PreviewEvent; ts: string }) {
  const [open, setOpen] = useState(false);
  const isCall = e.role === "tool_call";
  return (
    <div className="flex gap-3">
      <div
        className={cn(
          "flex h-8 w-8 shrink-0 items-center justify-center rounded-full",
          isCall ? "bg-purple-500/15 text-purple-600 dark:text-purple-400" : "bg-amber-500/15 text-amber-600 dark:text-amber-400",
        )}
      >
        {isCall ? <Wrench className="h-4 w-4" /> : <Terminal className="h-4 w-4" />}
      </div>
      <div className="min-w-0 flex-1">
        <button
          onClick={() => setOpen((x) => !x)}
          className="flex w-full items-center gap-2 rounded-md border bg-card px-3 py-2 text-left text-xs shadow-sm hover:bg-accent"
        >
          <ChevronDown className={cn("h-3.5 w-3.5 shrink-0 transition-transform", open && "rotate-180")} />
          <span className="font-medium">{isCall ? "工具调用" : "工具返回"}</span>
          <span className="truncate font-mono text-muted-foreground">{e.kind}</span>
          <span className="ml-auto min-w-0 flex-1 truncate text-muted-foreground">
            {e.text_summary || ""}
          </span>
          {ts && <span className="shrink-0 font-mono text-muted-foreground/70">{ts}</span>}
        </button>
        {open && (
          <div className="mt-1.5 overflow-auto rounded-md border bg-card p-3 text-xs">
            <JsonView
              data={e.raw as object}
              style={defaultStyles}
              shouldExpandNode={(level) => level < 2}
            />
          </div>
        )}
      </div>
    </div>
  );
}

function MetaLine({ e, ts }: { e: PreviewEvent; ts: string }) {
  return (
    <div className="my-2 flex items-center gap-3">
      <div className="h-px flex-1 bg-border" />
      <div className="flex min-w-0 items-center gap-1.5 text-[11px] text-muted-foreground">
        <Badge variant="outline" className="h-5 font-normal">
          {e.kind}
        </Badge>
        {e.text_summary && <span className="truncate">{e.text_summary}</span>}
        {ts && <span className="font-mono">{ts}</span>}
      </div>
      <div className="h-px flex-1 bg-border" />
    </div>
  );
}

function DefaultBubble({ e, ts }: { e: PreviewEvent; ts: string }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="flex gap-3">
      <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-slate-500/15 text-slate-600 dark:text-slate-400">
        <FileJson className="h-4 w-4" />
      </div>
      <div className="min-w-0 flex-1">
        <button
          onClick={() => setOpen((x) => !x)}
          className="flex w-full items-center gap-2 rounded-md border bg-card px-3 py-2 text-left text-xs shadow-sm hover:bg-accent"
        >
          <ChevronDown className={cn("h-3.5 w-3.5 shrink-0 transition-transform", open && "rotate-180")} />
          <Badge variant="outline" className="h-5 font-normal capitalize">
            {e.role}
          </Badge>
          <span className="truncate font-mono text-muted-foreground">{e.kind}</span>
          {ts && <span className="ml-auto shrink-0 font-mono text-muted-foreground/70">{ts}</span>}
        </button>
        {open && (
          <div className="mt-1.5 overflow-auto rounded-md border bg-card p-3 text-xs">
            <JsonView
              data={e.raw as object}
              style={defaultStyles}
              shouldExpandNode={(level) => level < 2}
            />
          </div>
        )}
      </div>
    </div>
  );
}

function ForkNodeButton({ event, action }: { event: PreviewEvent; action: ForkAction }) {
  if (!action.enabled) return null;
  return (
    <Button
      type="button"
      variant="ghost"
      size="sm"
      className="h-5 shrink-0 gap-1 px-1.5 text-[11px] opacity-0 transition-opacity duration-150 pointer-events-none group-hover:pointer-events-auto group-hover:opacity-100 group-focus-within:pointer-events-auto group-focus-within:opacity-100"
      disabled={action.pending}
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        action.onSelect(event);
      }}
    >
      <GitBranch className="h-3 w-3" />
      回溯
    </Button>
  );
}

function Avatar({ role }: { role: "user" | "assistant" }) {
  if (role === "user") {
    return (
      <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-primary text-primary-foreground">
        <User className="h-4 w-4" />
      </div>
    );
  }
  return (
    <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-emerald-500/15 text-emerald-600 dark:text-emerald-400">
      <Bot className="h-4 w-4" />
    </div>
  );
}

function Dot() {
  return (
    <span
      aria-hidden="true"
      className="inline-block h-1 w-1 shrink-0 rounded-full bg-muted-foreground/40"
    />
  );
}

function EventSourceBadge({ e }: { e: PreviewEvent }) {
  const outer = rawType(e);
  const payload = payloadType(e);
  if (outer !== "event_msg" && outer !== "response_item") return null;
  if (payload !== "user_message" && payload !== "agent_message" && payload !== "message") return null;

  const title =
    outer === "event_msg"
      ? "事件流消息：官方聊天展示层使用的用户/助手事件"
      : "响应项消息：模型对话历史中的消息项";

  return (
    <Badge
      variant="outline"
      title={title}
      className="h-4 px-1 py-0 font-mono text-[10px] font-normal text-muted-foreground"
    >
      {outer}/{payload}
    </Badge>
  );
}

function extractText(e: PreviewEvent): string {
  const r = e.raw as any;
  if (!r) return e.text_summary ?? "";
  if (r.message) {
    const content = r.message.content;
    if (typeof content === "string") return content;
    if (Array.isArray(content)) {
      return content
        .map((x: any) => {
          if (typeof x === "string") return x;
          if (x?.type === "thinking") {
            const t = typeof x.thinking === "string" ? x.thinking.trim() : "";
            return t || "(加密推理)";
          }
          if (x?.type === "redacted_thinking") return "(加密推理)";
          if (typeof x?.text === "string") return x.text;
          if (typeof x?.content === "string") return x.content;
          if (Array.isArray(x?.content)) {
            return x.content.map((c: any) => c?.text ?? c?.content ?? "").filter(Boolean).join("\n");
          }
          if (x?.type === "tool_use") return `[Tool: ${x.name ?? "unknown"}]`;
          return "";
        })
        .filter(Boolean)
        .join("\n\n");
    }
  }
  const payload = r.payload;
  if (!payload) return e.text_summary ?? "";
  if (typeof payload.message === "string") return payload.message;
  if (typeof payload.content === "string") return payload.content;
  if (typeof payload.text === "string") return payload.text;
  if (Array.isArray(payload.content)) {
    return payload.content
      .map((x: any) => (typeof x === "string" ? x : x?.text ?? ""))
      .filter(Boolean)
      .join("\n\n");
  }
  return e.text_summary ?? "";
}

function parseDiffCommentPrompt(text: string): DiffCommentPrompt | null {
  const normalized = normalizeDiffCommentPrompt(text);
  if (!/^Diff comments\s*:/i.test(normalized)) return null;

  const request = extractSection(
    normalized,
    /(?:^|\n)My request for Codex:\s*\n+/,
    [/\n+The next image shows\b/, /\n*<image>\s*<\/image>/, /\n+In app browser:/],
  );
  const commentsSection = normalized
    .split(/\n+In app browser:/)[0]
    .split(/\n+My request for Codex:/)[0]
    .split(/\n+The next image shows\b/)[0]
    .replace(/^Diff comments\s*:\s*/i, "");

  const comments: DiffComment[] = [];
  const commentPattern =
    /(?:^|\n+)Comment\s+(\d+)\s*:?\s*\n+([\s\S]*?)(?=\n+Comment\s+\d+\s*:?\s*\n+|\n+In app browser:|\n+My request for Codex:|\n+The next image shows\b|\n*<image>\s*<\/image>|$)/g;
  let match: RegExpExecArray | null;
  while ((match = commentPattern.exec(commentsSection)) !== null) {
    const number = Number.parseInt(match[1], 10);
    const block = match[2].trim();
    const body = extractCommentBody(block);
    comments.push({
      number: Number.isFinite(number) ? number : comments.length + 1,
      context: extractCommentContext(block),
      body: body || "未能解析批注正文。请展开该事件的 JSON 查看原始内容。",
    });
  }

  if (comments.length === 0) {
    comments.push({
      number: 1,
      context: "",
      body: "未能解析批注正文。请展开该事件的 JSON 查看原始内容。",
    });
  }

  return {
    comments,
    request: cleanDiffCommentText(request),
  };
}

function normalizeDiffCommentPrompt(text: string): string {
  return text
    .replace(/\r\n/g, "\n")
    .split("\n")
    .map((line) =>
      line
        .trim()
        .replace(/^#{1,6}\s+/, "")
        .replace(/^\*\*(.+)\*\*$/, "$1")
        .replace(/^__(.+)__$/, "$1")
        .trim(),
    )
    .join("\n")
    .trim();
}

function extractSection(text: string, start: RegExp, endPatterns: RegExp[]): string {
  const startMatch = start.exec(text);
  if (!startMatch) return "";
  const startIndex = startMatch.index + startMatch[0].length;
  const rest = text.slice(startIndex);
  const endIndex = endPatterns.reduce((min, pattern) => {
    const match = pattern.exec(rest);
    return match ? Math.min(min, match.index) : min;
  }, rest.length);
  return rest.slice(0, endIndex);
}

function extractCommentBody(block: string): string {
  const marker = "Comment:";
  const markerIndex = block.lastIndexOf(marker);
  if (markerIndex < 0) return "";
  return cleanDiffCommentText(block.slice(markerIndex + marker.length));
}

function extractCommentContext(block: string): string {
  const fileMatch = /File:\s*(.*?)(?:\s+Lines?:|\s+Line:|\n|$)/i.exec(block);
  if (!fileMatch) return "";
  return cleanDiffCommentText(fileMatch[1].replace(/^browser:/i, ""));
}

function cleanDiffCommentText(text: string): string {
  return text
    .replace(/<image>\s*<\/image>/gi, "")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

function isConversationMessage(e: PreviewEvent): boolean {
  if (isInternalCodexContextMessage(e)) return false;
  const raw = e.raw as { message?: { role?: unknown } } | null;
  if (typeof raw?.message?.role === "string") {
    return e.role === "user" || e.role === "assistant";
  }
  if (rawType(e) !== "response_item" || payloadType(e) !== "message") return false;
  if (e.role !== "user" && e.role !== "assistant") return false;
  return true;
}

function isEventMessage(e: PreviewEvent): boolean {
  if (rawType(e) !== "event_msg") return false;
  const payload = payloadType(e);
  return payload === "user_message" || payload === "agent_message";
}

function isStableForkNode(e: PreviewEvent): boolean {
  return isConversationMessage(e) || isEventMessage(e);
}

function eventMessageLabel(e: PreviewEvent): string {
  const payload = payloadType(e);
  if (payload === "user_message") return "用户事件消息";
  if (payload === "agent_message") return "agent事件消息";
  return "事件消息";
}

function isInternalCodexContextMessage(e: PreviewEvent): boolean {
  if (e.role !== "user") return false;
  const text = extractText(e).trim();
  if (!text) return false;
  const firstLine = normalizePromptHeading(text.split(/\r?\n/, 1)[0] ?? "");
  if (firstLine.startsWith("AGENTS.md instructions for ") && text.includes("<INSTRUCTIONS>")) {
    return true;
  }
  if (firstLine === "<environment_context>" && text.includes("</environment_context>")) {
    return true;
  }
  return false;
}

function normalizePromptHeading(line: string): string {
  return line.trim().replace(/^#{1,6}\s+/, "").trim();
}

function rawType(e: PreviewEvent): string {
  const raw = e.raw as { type?: unknown } | null;
  return typeof raw?.type === "string" ? raw.type : "";
}

function payloadType(e: PreviewEvent): string {
  const raw = e.raw as { payload?: { type?: unknown } } | null;
  return typeof raw?.payload?.type === "string" ? raw.payload.type : "";
}
