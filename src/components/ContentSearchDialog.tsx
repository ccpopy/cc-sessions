import { useCallback, useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { FileSearch, Loader2, Search, Square, User, Bot } from "lucide-react";

import {
  api,
  type ContentSearchMatch,
  type ContentSearchStatus,
  type SessionProvider,
  type SessionSummary,
} from "@/lib/api";
import { formatTimeString, highlight, humanBytes } from "@/lib/format";
import { sessionDisplayTitle } from "@/lib/sessionText";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";

type Props = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  provider: SessionProvider;
  codexDir: string;
  claudeDir: string;
  showSubagentSessions: boolean;
  showArchivedSessions: boolean;
  onOpenResult: (
    session: SessionSummary,
    match: ContentSearchMatch,
    query: string,
  ) => void;
};

export function ContentSearchDialog({
  open,
  onOpenChange,
  provider,
  codexDir,
  claudeDir,
  showSubagentSessions,
  showArchivedSessions,
  onOpenResult,
}: Props) {
  const [query, setQuery] = useState("");
  const [jobId, setJobId] = useState<number | null>(null);
  const [status, setStatus] = useState<ContentSearchStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const timerRef = useRef<number | null>(null);
  const activeJobRef = useRef<number | null>(null);
  const jobScopeKeyRef = useRef<string | null>(null);
  const mountedRef = useRef(false);
  const openRef = useRef(open);
  const requestGenerationRef = useRef(0);
  const pendingStartGenerationRef = useRef<number | null>(null);
  const scopeKey = `${provider}\u0000${codexDir}\u0000${claudeDir}\u0000${showSubagentSessions}\u0000${showArchivedSessions}`;
  const scopeKeyRef = useRef(scopeKey);
  const previousScopeKeyRef = useRef(scopeKey);

  openRef.current = open;
  scopeKeyRef.current = scopeKey;

  const cancelActiveSearch = useCallback(async (reportError: boolean) => {
    requestGenerationRef.current += 1;

    const activeJobId = activeJobRef.current;
    if (activeJobId === null) return;
    if (mountedRef.current) setCancelling(true);
    try {
      await api.cancelContentSearch(activeJobId);
    } catch (cancelError) {
      if (mountedRef.current && activeJobRef.current === activeJobId) {
        setCancelling(false);
        if (reportError) {
          setError(String((cancelError as Error)?.message ?? cancelError));
        }
      }
    }
  }, []);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      requestGenerationRef.current += 1;
      const activeJobId = activeJobRef.current;
      activeJobRef.current = null;
      jobScopeKeyRef.current = null;
      if (activeJobId !== null) {
        void api.cancelContentSearch(activeJobId).catch(() => undefined);
      }
    };
  }, []);

  useEffect(() => {
    if (previousScopeKeyRef.current === scopeKey) return;
    previousScopeKeyRef.current = scopeKey;
    setStatus(null);
    setError(null);
    void cancelActiveSearch(false);
  }, [cancelActiveSearch, scopeKey]);

  useEffect(() => {
    if (!open || jobId !== null || starting) return;
    let disposed = false;
    const generation = requestGenerationRef.current;

    void api.activeContentSearch().then((active) => {
      if (
        disposed
        || !active
        || !mountedRef.current
        || !openRef.current
        || activeJobRef.current !== null
        || pendingStartGenerationRef.current !== null
        || requestGenerationRef.current !== generation
      ) {
        return;
      }
      activeJobRef.current = active.job_id;
      jobScopeKeyRef.current = null;
      setStatus(null);
      setError("检测到仍在运行的全文搜索，可先停止后重新搜索");
      setJobId(active.job_id);
    }).catch(() => undefined);

    return () => {
      disposed = true;
    };
  }, [jobId, open, starting]);

  useEffect(() => {
    if (jobId === null) return;
    let disposed = false;

    const poll = async () => {
      try {
        const next = await api.contentSearchStatus(jobId);
        if (disposed || activeJobRef.current !== jobId) return;
        const ownsCurrentScope = jobScopeKeyRef.current === scopeKeyRef.current;
        if (ownsCurrentScope) {
          setStatus(next);
          setError(null);
        }
        if (next.state === "running") {
          timerRef.current = window.setTimeout(poll, 300);
        } else {
          activeJobRef.current = null;
          jobScopeKeyRef.current = null;
          setJobId((current) => current === jobId ? null : current);
          setCancelling(false);
          if (!ownsCurrentScope) setError(null);
        }
      } catch (pollError) {
        if (disposed || activeJobRef.current !== jobId) return;
        const active = await api.activeContentSearch().catch(() => undefined);
        if (disposed || activeJobRef.current !== jobId) return;
        if (active !== undefined && active?.job_id !== jobId) {
          activeJobRef.current = null;
          jobScopeKeyRef.current = null;
          setJobId((current) => current === jobId ? null : current);
          setCancelling(false);
          setError("全文搜索任务已结束或服务已重启");
          return;
        }
        if (jobScopeKeyRef.current === scopeKeyRef.current) {
          setError(String((pollError as Error)?.message ?? pollError));
        }
        timerRef.current = window.setTimeout(poll, 300);
      }
    };

    void poll();
    return () => {
      disposed = true;
      if (timerRef.current !== null) window.clearTimeout(timerRef.current);
      timerRef.current = null;
    };
  }, [jobId]);

  const running = jobId !== null || starting;
  const progress = useMemo(() => {
    if (!status || status.total_bytes === 0) return 0;
    return Math.min(100, Math.round((status.scanned_bytes / status.total_bytes) * 100));
  }, [status]);
  const skippedLabel = status?.skipped_files
    ? `，跳过 ${status.skipped_files} 个无正文记录`
    : "";

  const startSearch = async (event: FormEvent) => {
    event.preventDefault();
    const normalized = query.trim();
    if (normalized.length < 2 || running) return;
    const generation = requestGenerationRef.current + 1;
    requestGenerationRef.current = generation;
    pendingStartGenerationRef.current = generation;
    const requestScopeKey = scopeKey;
    setStarting(true);
    setError(null);
    setStatus(null);
    try {
      const started = await api.startContentSearch({
        provider,
        codexDir,
        claudeDir,
        query: normalized,
        showSubagentSessions,
        showArchivedSessions,
      });
      if (
        !mountedRef.current
        || requestGenerationRef.current !== generation
        || scopeKeyRef.current !== requestScopeKey
        || !openRef.current
      ) {
        await api.cancelContentSearch(started.job_id).catch(() => undefined);
        return;
      }
      activeJobRef.current = started.job_id;
      jobScopeKeyRef.current = requestScopeKey;
      setJobId(started.job_id);
    } catch (startError) {
      if (
        mountedRef.current
        && requestGenerationRef.current === generation
        && scopeKeyRef.current === requestScopeKey
        && openRef.current
      ) {
        setError(String((startError as Error)?.message ?? startError));
      }
    } finally {
      if (pendingStartGenerationRef.current === generation) {
        pendingStartGenerationRef.current = null;
      }
      if (mountedRef.current && pendingStartGenerationRef.current === null) {
        setStarting(false);
      }
    }
  };

  const stopSearch = async () => {
    if (activeJobRef.current === null) return;
    setError(null);
    await cancelActiveSearch(true);
  };

  const changeOpen = (next: boolean) => {
    if (!next) void cancelActiveSearch(false);
    onOpenChange(next);
  };

  const scopeLabel = showSubagentSessions
    ? "子代理"
    : provider === "codex" && showArchivedSessions
      ? "已归档"
      : "主会话";

  return (
    <Dialog open={open} onOpenChange={changeOpen}>
      <DialogContent className="flex h-[min(82vh,760px)] max-w-[min(960px,calc(100vw-2rem))] min-w-0 flex-col gap-0 p-0">
        <DialogHeader className="shrink-0 border-b border-border/60 px-5 pb-4 pt-5 pr-12 sm:px-6 sm:pr-12">
          <div className="flex min-w-0 items-center gap-3">
            <div className="grid h-9 w-9 shrink-0 place-items-center rounded-md bg-muted text-muted-foreground">
              <FileSearch className="h-[18px] w-[18px]" />
            </div>
            <div className="min-w-0">
              <DialogTitle className="text-[15px] leading-tight">对话全文搜索</DialogTitle>
              <div className="mt-1 flex flex-wrap items-center gap-1.5">
                <Badge variant="outline" className="h-5 px-1.5 text-[10px] font-normal">
                  {provider === "codex" ? "Codex" : "Claude"}
                </Badge>
                <Badge variant="secondary" className="h-5 px-1.5 text-[10px] font-normal">
                  {scopeLabel}
                </Badge>
              </div>
            </div>
          </div>
          <DialogDescription className="sr-only">
            手动扫描当前范围内的用户与助手对话内容
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={startSearch} className="shrink-0 border-b border-border/60 p-4 sm:px-6">
          <div className="flex min-w-0 gap-2">
            <div className="relative min-w-0 flex-1">
              <Search className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
              <Input
                autoFocus
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                maxLength={256}
                disabled={running}
                placeholder="搜索用户与助手对话"
                className="h-10 pl-9"
              />
            </div>
            {jobId !== null ? (
              <Button
                type="button"
                variant="outline"
                onClick={stopSearch}
                disabled={cancelling}
                className="h-10 gap-2 active:scale-95"
              >
                {cancelling ? <Loader2 className="h-4 w-4 animate-spin" /> : <Square className="h-3.5 w-3.5" />}
                停止
              </Button>
            ) : (
              <Button
                type="submit"
                disabled={query.trim().length < 2 || starting}
                className="h-10 gap-2 active:scale-95"
              >
                {starting ? <Loader2 className="h-4 w-4 animate-spin" /> : <Search className="h-4 w-4" />}
                搜索
              </Button>
            )}
          </div>

          {status && (
            <div className="mt-3 space-y-1.5">
              <div className="flex min-w-0 items-center justify-between gap-4 text-[11px] text-muted-foreground">
                <span className="min-w-0 truncate">
                  {status.state === "running"
                    ? status.total_files === 0
                      ? "正在读取会话列表"
                      : `正在扫描 ${status.scanned_files}/${status.total_files} 个会话${skippedLabel}`
                    : status.state === "completed"
                      ? `已扫描 ${status.scanned_files} 个会话${skippedLabel}`
                      : status.state === "cancelled"
                        ? `搜索已停止${skippedLabel}`
                        : "搜索失败"}
                </span>
                <span className="shrink-0 tabular-nums">
                  {humanBytes(status.scanned_bytes)} / {humanBytes(status.total_bytes)}
                </span>
              </div>
              <div className="h-1 overflow-hidden rounded-full bg-muted">
                <div
                  className="h-full rounded-full bg-primary transition-[width] duration-200"
                  style={{ width: `${progress}%` }}
                />
              </div>
            </div>
          )}

          {(error || status?.error) && (
            <p className="mt-3 text-xs text-destructive">{error ?? status?.error}</p>
          )}
        </form>

        <ScrollArea className="min-h-0 flex-1">
          {!status ? (
            <div className="grid min-h-64 place-items-center px-6 text-center text-sm text-muted-foreground">
              <div>
                <FileSearch className="mx-auto mb-3 h-7 w-7 opacity-45" />
                {jobId !== null ? (cancelling ? "正在停止搜索" : "正在读取会话列表") : "尚未搜索"}
              </div>
            </div>
          ) : status.results.length === 0 ? (
            <div className="grid min-h-64 place-items-center px-6 text-center text-sm text-muted-foreground">
              {status.state === "running" ? "正在查找匹配内容" : "没有匹配的对话"}
            </div>
          ) : (
            <div className="divide-y divide-border/60">
              {status.results.map((result) => (
                <section key={`${result.session.provider}:${result.session.id}`} className="px-4 py-4 sm:px-6">
                  <div className="mb-2.5 flex min-w-0 items-center gap-2">
                    <h3 className="min-w-0 flex-1 truncate text-sm font-semibold">
                      {sessionDisplayTitle(result.session.title, result.session.first_user_message)}
                    </h3>
                    <span className="shrink-0 font-mono text-[10px] text-muted-foreground">
                      {result.session.id.slice(0, 8)}
                    </span>
                  </div>
                  <div className="space-y-1.5">
                    {result.matches.map((match) => (
                      <button
                        key={`${result.session.id}:${match.event_index}`}
                        type="button"
                        onClick={() => {
                          changeOpen(false);
                          onOpenResult(result.session, match, status.query);
                        }}
                        className="group grid w-full min-w-0 grid-cols-[auto_minmax(0,1fr)] gap-2.5 rounded-md px-2.5 py-2 text-left transition-colors hover:bg-muted/70 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring active:scale-[0.995]"
                      >
                        <span className="mt-0.5 grid h-6 w-6 place-items-center rounded-md bg-muted text-muted-foreground group-hover:text-foreground">
                          {match.role === "user" ? <User className="h-3.5 w-3.5" /> : <Bot className="h-3.5 w-3.5" />}
                        </span>
                        <span className="min-w-0">
                          <span className="flex min-w-0 items-center gap-2 text-[10px] text-muted-foreground">
                            <span>{match.role === "user" ? "用户" : "助手"}</span>
                            {match.timestamp && <span>{formatTimeString(match.timestamp)}</span>}
                            <span className="font-mono">第 {match.event_index + 1} 行</span>
                          </span>
                          <span className="mt-0.5 block min-w-0 text-[13px] leading-relaxed text-foreground/85 wrap-anywhere">
                            <HighlightedText text={match.snippet} query={status.query} />
                          </span>
                        </span>
                      </button>
                    ))}
                  </div>
                </section>
              ))}
              {status.truncated && (
                <div className="px-6 py-3 text-center text-xs text-muted-foreground">
                  结果已达到 100 个会话
                </div>
              )}
            </div>
          )}
        </ScrollArea>
      </DialogContent>
    </Dialog>
  );
}

function HighlightedText({ text, query }: { text: string; query: string }) {
  return highlight(text, query).map((part, index) =>
    part.hit ? <mark key={index}>{part.t}</mark> : <span key={index}>{part.t}</span>,
  );
}
