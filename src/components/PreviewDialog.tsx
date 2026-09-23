import { createContext, useContext, useCallback, useDeferredValue, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import {
  Bot,
  Check,
  ChevronDown,
  ChevronsDown,
  FileJson,
  GitBranch,
  Loader2,
  MessageSquare,
  MousePointer2,
  Network,
  Pencil,
  Sparkles,
  Terminal,
  Trash2,
  User,
  Wrench,
  X,
} from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { JsonView, defaultStyles } from "react-json-view-lite";
import "react-json-view-lite/dist/index.css";

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { LocalImageAttachments } from "@/components/LocalImageAttachments";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Label } from "@/components/ui/label";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import { PreviewEditHistoryDialog } from "@/components/PreviewEditHistoryDialog";
import { PreviewMutationDialogs } from "@/components/PreviewMutationDialogs";
import {
  api,
  type DeletePlan,
  type DeleteTurnSelection,
  type DeletePlanMessage,
  type TextBlockEdit,
  type EditApplyReport,
  type EditCapability,
  type EditHistory,
  type PreviewEvent,
  type SessionSummary,
  type Settings,
  type UserPromptBrief,
} from "@/lib/api";
import { copyText } from "@/lib/clipboard";
import { absoluteTime, formatTimeString, humanTokens } from "@/lib/format";
import { shouldIgnoreTextEditingHotkey } from "@/lib/keyboard";
import { parseUserMessageAttachments } from "@/lib/messageAttachments";
import { PromptTimeline } from "@/components/PromptTimeline";
import { PreviewToolbarActions } from "@/components/PreviewToolbarActions";
import {
  collectRelatedSubagents,
  isSubagentSession,
  type RelatedSubagentSession,
} from "@/lib/sessionSource";
import { parseEmbeddedTranscriptPrompt, type EmbeddedTranscriptPrompt } from "@/lib/sessionText";
import {
  buildConversationPreviewRows,
  isProcessGroupExpanded,
  isVisibleConversationEvent,
  summarizeProcessGroupExpansion,
  toConversationDisplayEvent,
  type ConversationPreviewRow,
} from "@/lib/conversationDisplay";
import {
  canDeleteEvent,
  canEditEventText,
  canonicalMessageImages,
  paginatedTargets,
  latestCanonicalEvents,
  previewEventKey,
  previewProcessKey,
  survivingPreviewAnchor,
  editableTextBlocks,
  contentMappingCounts,
  editableText,
  eventMessageLabel,
  extractPreviewEventText as extractText,
  isConversationMessage,
  isEventMessage,
  isStableForkNode,
  isSessionWideMutationFailure,
  openCodeForkPoint,
  parseDiffCommentPrompt,
  payloadType,
  previewEventSearchText,
  rawType,
  subagentEventLabel,
  subagentEventTime,
  type DiffCommentPrompt,
} from "@/lib/previewEvent";
import { cn } from "@/lib/utils";
import { useSettings } from "@/stores/settings";
import { toast } from "sonner";
import { isTauriRuntime } from "@/lib/runtime";

type Props = {
  open: boolean;
  onOpenChange: (v: boolean) => void;
  session: SessionSummary | null;
  allSessions?: readonly SessionSummary[];
  customRolloutPath?: string;
  codexDir?: string;
  backupDir?: string;
  onForked?: () => void | Promise<void>;
  onEdited?: () => void | Promise<void>;
  initialJump?: PreviewJump | null;
};

export type PreviewJump = {
  eventIndex: number;
  eventOffset: number;
  query: string;
};

type ForkAction = {
  enabled: boolean;
  label: string;
  pending: boolean;
  onSelect: (event: PreviewEvent) => void;
};

type EditActions = {
  enabled: boolean;
  pending: boolean;
  canEditText: (event: PreviewEvent) => boolean;
  canDelete: (event: PreviewEvent) => boolean;
  onEdit: (event: PreviewEvent) => void;
  onDelete: (event: PreviewEvent) => void;
};

type NodeActionSet = {
  fork: ForkAction;
  edit: EditActions;
};

const PAGE = 200;
const PreviewTechnicalContext = createContext(false);

export function PreviewDialog({
  open,
  onOpenChange,
  session,
  allSessions = [],
  customRolloutPath,
  codexDir,
  backupDir,
  onForked,
  onEdited,
  initialJump,
}: Props) {
  const rolloutPath = customRolloutPath ?? session?.rollout_path ?? "";
  const provider = session?.provider ?? "codex";
  const [events, setEvents] = useState<PreviewEvent[]>([]);
  const [loading, setLoading] = useState(false);
  const [done, setDone] = useState(false);
  const [filter, setFilter] = useState("");
  const [onlyMsg, setOnlyMsg] = useState(true);
  const [processDefaultCollapsed, setProcessDefaultCollapsed] = useState(true);
  const [processExpansionOverrides, setProcessExpansionOverrides] = useState<
    Record<string, boolean>
  >({});
  const [forkTarget, setForkTarget] = useState<PreviewEvent | null>(null);
  const [forking, setForking] = useState(false);
  const [editTarget, setEditTarget] = useState<PreviewEvent | null>(null);
  const [editText, setEditText] = useState("");
  const [editBlocks, setEditBlocks] = useState<TextBlockEdit[]>([]);
  const [sessionInfoOpen, setSessionInfoOpen] = useState(false);
  const [mutationError, setMutationError] = useState("");
  const [mutating, setMutating] = useState(false);
  const mutationInFlightRef = useRef(false);
  const [deleteTarget, setDeleteTarget] = useState<PreviewEvent | null>(null);
  const [isSelecting, setIsSelecting] = useState(false);
  const [selectionFirstIndex, setSelectionFirstIndex] = useState<number | null>(null);
  const [selectionSecondIndex, setSelectionSecondIndex] = useState<number | null>(null);
  const [deleteSelectedTarget, setDeleteSelectedTarget] = useState<{ start: number; end: number; events: PreviewEvent[] } | null>(null);
  const [deletePlan, setDeletePlan] = useState<DeletePlan | null>(null);
  const [deleteScope, setDeleteScope] = useState<DeletePlanMessage[]>([]);
  const deleteRequestRef = useRef(0);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [editHistory, setEditHistory] = useState<EditHistory | null>(null);
  const [prompts, setPrompts] = useState<UserPromptBrief[] | null>(null);
  const [totalEvents, setTotalEvents] = useState(0);
  const [activeTimelineIndex, setActiveTimelineIndex] = useState<number | null>(null);
  const [loadingAll, setLoadingAll] = useState(false);
  const [capability, setCapability] = useState<EditCapability | null>(null);
  const [readError, setReadError] = useState("");
  const [lastReport, setLastReport] = useState<EditApplyReport | null>(null);
  const revisionRef = useRef<string | null>(null);
  const generationRef = useRef(0);
  const cancelLoadAllRef = useRef(false);
  const readErrorRef = useRef(false);
  const offsetRef = useRef(0);
  const loadingRef = useRef(false);
  const doneRef = useRef(false);
  const viewportRef = useRef<HTMLDivElement | null>(null);
  const pendingJumpRef = useRef<number | null>(null);
  const pendingReadingRef = useRef<{ keys: string[]; anchor: string; offset: number; scrollTop: number } | null>(null);
  const scrollSpyRafRef = useRef(0);
  const preferenceSaveRef = useRef<Promise<void>>(Promise.resolve());
  const appSettings = useSettings((state) => state.settings);
  const claudeDir = appSettings?.claude_dir;
  const opencodeDir = appSettings?.opencode_dir;
  const canForkSession = !customRolloutPath && !!session && (
    (provider === "codex" && !!codexDir && capability?.format === "legacy" && capability.blocked_reasons.length === 0)
    || (provider === "claude" && !!claudeDir && !isSubagentSession(session))
    || (provider === "opencode" && !!opencodeDir)
  );
  const forkLabel = provider === "codex" ? "回溯" : "复制到此处";
  // 备份/导入预览（customRolloutPath）不允许编辑，只能编辑真实会话文件
  const canMutateSession =
    !customRolloutPath && !!session && !!backupDir && !!rolloutPath && !readError
    && lastReport?.status !== "needs_recovery"
    && !isSessionWideMutationFailure(mutationError)
    && (provider === "opencode" || (capability !== null && capability.blocked_reasons.length === 0));
  const sourceLabel = `${isTauriRuntime() ? "本地" : `WebUI · ${window.location.host}`} · ${provider}`;
  const canDeletePreviewEvent = (event: PreviewEvent) => canDeleteEvent(provider, event)
    && (capability?.format !== "paginated" || paginatedTargets([event]).length === 1);
  const relatedSubagents = useMemo(() => {
    if (!session || session.provider !== "codex" || customRolloutPath) return [];
    return collectRelatedSubagents(session.id, allSessions);
  }, [allSessions, customRolloutPath, session]);

  const previewOnlyMessages = appSettings?.preview_only_messages;
  const previewCollapseProcess = appSettings?.preview_collapse_process;

  useEffect(() => {
    if (previewOnlyMessages === undefined) return;
    setOnlyMsg(previewOnlyMessages);
  }, [previewOnlyMessages]);

  useEffect(() => {
    if (previewCollapseProcess === undefined) return;
    setProcessDefaultCollapsed(previewCollapseProcess);
    setProcessExpansionOverrides({});
  }, [previewCollapseProcess]);

  const persistPreviewPreference = useCallback((patch: Partial<Settings>) => {
    preferenceSaveRef.current = preferenceSaveRef.current.then(async () => {
      try {
        await useSettings.getState().save(patch);
      } catch (error) {
        toast.error("保存预览偏好失败", {
          description: String((error as Error)?.message ?? error),
        });
        await useSettings.getState().load().catch(() => undefined);
      }
    });
  }, []);

  const changeOnlyMsg = useCallback(
    (checked: boolean) => {
      setOnlyMsg(checked);
      persistPreviewPreference({ preview_only_messages: checked });
    },
    [persistPreviewPreference],
  );

  const changeProcessDefaultCollapsed = useCallback(
    (collapsed: boolean) => {
      setProcessDefaultCollapsed(collapsed);
      setProcessExpansionOverrides({});
      persistPreviewPreference({ preview_collapse_process: collapsed });
    },
    [persistPreviewPreference],
  );

  const changeProcessGroupExpanded = useCallback(
    (key: string, expanded: boolean) => {
      setProcessExpansionOverrides((current) => {
        const defaultExpanded = !processDefaultCollapsed;
        if (expanded === defaultExpanded) {
          if (!(key in current)) return current;
          const next = { ...current };
          delete next[key];
          return next;
        }
        return { ...current, [key]: expanded };
      });
    },
    [processDefaultCollapsed],
  );

  const readPage = useCallback(async (offset: number, limit: number) => {
    const generation = generationRef.current;
    try {
      const page = await api.previewPage(provider, rolloutPath, offset, limit, revisionRef.current);
      if (generation !== generationRef.current) return null;
      revisionRef.current = page.capability?.revision ?? null;
      setCapability(page.capability);
      return page.events;
    } catch (error) {
      if (generation === generationRef.current) {
        readErrorRef.current = true;
        setReadError(String((error as Error)?.message ?? error));
        setIsSelecting(false);
        setSelectionFirstIndex(null);
        setSelectionSecondIndex(null);
        setDeletePlan(null);
      }
      return null;
    }
  }, [provider, rolloutPath]);

  const loadMore = useCallback(async () => {
    if (loadingRef.current || doneRef.current || readErrorRef.current || !rolloutPath) return;
    const generation = generationRef.current;
    loadingRef.current = true;
    setLoading(true);
    try {
      const next = await readPage(offsetRef.current, PAGE);
      if (!next) return;
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
      if (generation === generationRef.current) {
        loadingRef.current = false;
        setLoading(false);
      }
    }
  }, [readPage, rolloutPath]);

  /** 等待进行中的分页请求结束，避免并发拉取重复区间 */
  const waitForIdle = useCallback(async () => {
    while (loadingRef.current) {
      await new Promise((resolve) => setTimeout(resolve, 40));
    }
  }, []);

  /** 一次性把事件加载到指定事件序号（时间线跳转用），带一页余量 */
  const loadUpTo = useCallback(
    async (targetOffset: number) => {
      const generation = generationRef.current;
      await waitForIdle();
      if (generation !== generationRef.current || readErrorRef.current || doneRef.current || offsetRef.current > targetOffset || !rolloutPath) return;
      loadingRef.current = true;
      setLoading(true);
      try {
        const need = targetOffset - offsetRef.current + 1 + PAGE;
        const next = await readPage(offsetRef.current, need);
        if (!next) return;
        if (next.length > 0) {
          offsetRef.current += next.length;
          setEvents((prev) => [...prev, ...next]);
        }
        if (next.length < need) {
          doneRef.current = true;
          setDone(true);
        }
      } finally {
        if (generation === generationRef.current) {
          loadingRef.current = false;
          setLoading(false);
        }
      }
    },
    [readPage, rolloutPath, waitForIdle],
  );

  /** 一次加载余下全部事件 */
  const loadAll = useCallback(async () => {
    const generation = generationRef.current;
    await waitForIdle();
    if (generation !== generationRef.current || doneRef.current || readErrorRef.current || !rolloutPath) return;
    loadingRef.current = true;
    setLoading(true);
    setLoadingAll(true);
    cancelLoadAllRef.current = false;
    try {
      while (!cancelLoadAllRef.current && generation === generationRef.current) {
        const next = await readPage(offsetRef.current, PAGE);
        if (!next) break;
        offsetRef.current += next.length;
        setEvents((prev) => [...prev, ...next]);
        if (next.length < PAGE) {
          doneRef.current = true;
          setDone(true);
          break;
        }
      }
    } finally {
      if (generation === generationRef.current) {
        loadingRef.current = false;
        setLoading(false);
        setLoadingAll(false);
      }
    }
  }, [readPage, rolloutPath, waitForIdle]);

  /** 拉取全量用户提问（时间线数据）；属于增强功能，失败时静默降级为无时间线 */
  const loadPrompts = useCallback(async () => {
    const generation = generationRef.current;
    if (!rolloutPath) {
      setPrompts(null);
      setTotalEvents(0);
      return;
    }
    try {
      const list = await api.previewUserPrompts(provider, rolloutPath);
      if (generation !== generationRef.current) return;
      setPrompts(list.prompts);
      setTotalEvents(list.total_events);
    } catch {
      if (generation !== generationRef.current) return;
      setPrompts(null);
      setTotalEvents(0);
    }
  }, [provider, rolloutPath]);

  const resetAndReload = useCallback(() => {
    generationRef.current += 1;
    deleteRequestRef.current += 1;
    revisionRef.current = null;
    readErrorRef.current = false;
    cancelLoadAllRef.current = true;
    setCapability(null);
    setReadError("");
    setLoadingAll(false);
    setEditTarget(null);
    setDeleteTarget(null);
    setDeleteSelectedTarget(null);
    setDeletePlan(null);
    setSelectionFirstIndex(null);
    setSelectionSecondIndex(null);
    setEvents([]);
    setProcessExpansionOverrides({});
    setDone(false);
    doneRef.current = false;
    loadingRef.current = false;
    offsetRef.current = 0;
    pendingJumpRef.current = null;
    setActiveTimelineIndex(null);
    setPrompts(null);
    setTotalEvents(0);
    pendingReadingRef.current = null;
    setMutationError("");
    void loadMore();
    void loadPrompts();
  }, [loadMore, loadPrompts]);

  // Replace the loaded range atomically so stable React keys preserve expanded
  // message details. Do not blank the list or reset filters during a mutation.
  const reloadPreservingView = async () => {
    const viewport = viewportRef.current;
    const nodes = viewport ? [...viewport.querySelectorAll<HTMLElement>("[data-reading-key]")] : [];
    const top = viewport?.getBoundingClientRect().top ?? 0;
    const anchor = nodes.find((node) => node.getBoundingClientRect().bottom > top) ?? nodes[0];
    const reading = anchor ? { keys: nodes.map((node) => node.dataset.readingKey!), anchor: anchor.dataset.readingKey!,
      offset: anchor.getBoundingClientRect().top - top, scrollTop: viewport?.scrollTop ?? 0 } : null;
    const needed = Math.max(PAGE, offsetRef.current + PAGE);
    const generation = ++generationRef.current;
    deleteRequestRef.current += 1;
    revisionRef.current = null;
    readErrorRef.current = false;
    cancelLoadAllRef.current = true;
    setLoadingAll(false);
    setReadError("");
    setMutationError("");
    setDeletePlan(null);
    setIsSelecting(false);
    setSelectionFirstIndex(null);
    setSelectionSecondIndex(null);
    pendingJumpRef.current = null;
    loadingRef.current = true;
    setLoading(true);
    try {
      const next: PreviewEvent[] = [];
      let exhausted = false;
      while (!exhausted && generation === generationRef.current) {
        const limit = next.length === 0 ? needed : PAGE;
        const page = await readPage(next.length, limit);
        if (!page) return;
        next.push(...page);
        exhausted = page.length < limit;
        if (!reading || next.some((e) => previewEventKey(e, !onlyMsg) === reading.anchor) || exhausted) break;
        // A deleted anchor can only be resolved after reaching a known successor.
        const successors = reading.keys.slice(reading.keys.indexOf(reading.anchor) + 1);
        if (next.some((e) => successors.includes(previewEventKey(e, !onlyMsg)))) break;
      }
      if (generation !== generationRef.current) return;
      offsetRef.current = next.length;
      doneRef.current = exhausted;
      setDone(exhausted);
      try {
        const list = await api.previewUserPrompts(provider, rolloutPath);
        if (generation !== generationRef.current) return;
        setPrompts(list.prompts);
        setTotalEvents(list.total_events);
      } catch { if (generation === generationRef.current) setPrompts(null); }
      if (generation !== generationRef.current) return;
      pendingReadingRef.current = reading;
      setEvents(next);
    } finally {
      if (generation === generationRef.current) { loadingRef.current = false; setLoading(false); }
    }
  };

  useEffect(() => {
    if (!open || !rolloutPath) return;
    setFilter("");
    setIsSelecting(false);
    setSelectionFirstIndex(null);
    setSelectionSecondIndex(null);
    setDeleteSelectedTarget(null);
    setDeletePlan(null);
    setLastReport(null);
    setHistoryOpen(false);
    setSessionInfoOpen(false);
    resetAndReload();
    return () => { generationRef.current += 1; cancelLoadAllRef.current = true; };
  }, [open, rolloutPath, resetAndReload]);

  const timelineIndexSet = useMemo(
    () => (prompts === null ? null : new Set(prompts.map((prompt) => prompt.index))),
    [prompts],
  );

  const deferredFilter = useDeferredValue(filter);
  const normalizedFilter = deferredFilter.trim().toLowerCase();
  const searchableEvents = useMemo(
    () => (onlyMsg ? latestCanonicalEvents(events) : events).map((event) => ({ event, searchText: previewEventSearchText(event) })),
    [events, onlyMsg],
  );

  const filtered = useMemo(() => {
    return searchableEvents.flatMap(({ event, searchText }) => {
      if (
        onlyMsg &&
        (!isConversationMessage(event)
          || !isVisibleConversationEvent(
            event,
            timelineIndexSet,
            initialJump?.eventIndex ?? null,
          ))
      ) {
        return [];
      }
      if (normalizedFilter && !searchText.includes(normalizedFilter)) return [];
      return [event];
    });
  }, [initialJump?.eventIndex, normalizedFilter, onlyMsg, searchableEvents, timelineIndexSet]);

  useEffect(() => {
    if (!open || loading || done || readError || normalizedFilter) return;
    const viewport = viewportRef.current;
    if (!viewport) return;
    // 收起过程、切换消息模式或应用延迟搜索后，继续补页直到结果填满视口。
    if (viewport.scrollHeight <= viewport.clientHeight + 20) {
      void loadMore();
    }
  }, [
    done,
    readError,
    events.length,
    filtered.length,
    loadMore,
    loading,
    normalizedFilter,
    onlyMsg,
    open,
    processDefaultCollapsed,
    processExpansionOverrides,
  ]);

  /**
   * 有 phase 时仅 final_answer 作为最终答复，commentary 折叠为过程。
   * 无 phase 时使用每轮最后一条 assistant 消息。搜索结果不折叠。
   */
  const rows = useMemo<ConversationPreviewRow[]>(() => {
    const displayEvents = onlyMsg ? filtered.map(toConversationDisplayEvent) : filtered;
    if (!onlyMsg || normalizedFilter) {
      return displayEvents.map((event) => ({ type: "event", event }));
    }
    return buildConversationPreviewRows(displayEvents);
  }, [filtered, normalizedFilter, onlyMsg]);

  const processRowKeys = useMemo(
    () => rows.flatMap((row) => (row.type === "process" ? [previewProcessKey(row.events)] : [])),
    [rows],
  );

  useLayoutEffect(() => {
    const saved = pendingReadingRef.current;
    const viewport = viewportRef.current;
    if (!saved || !viewport) return;
    const nodes = [...viewport.querySelectorAll<HTMLElement>("[data-reading-key]")];
    const key = survivingPreviewAnchor(saved.keys, saved.anchor, nodes.map((node) => node.dataset.readingKey!));
    const anchor = nodes.find((node) => node.dataset.readingKey === key);
    viewport.scrollTop = anchor ? viewport.scrollTop + anchor.getBoundingClientRect().top - viewport.getBoundingClientRect().top - saved.offset : saved.scrollTop;
    pendingReadingRef.current = null;
  }, [rows]);
  const processExpansionState = useMemo(
    () =>
      summarizeProcessGroupExpansion(
        processRowKeys,
        processDefaultCollapsed,
        processExpansionOverrides,
      ),
    [processDefaultCollapsed, processExpansionOverrides, processRowKeys],
  );

  /** 把待跳转的目标消息滚动到视口顶部并闪烁高亮 */
  const scrollPendingIntoView = useCallback(() => {
    const target = pendingJumpRef.current;
    if (target === null) return;
    const viewport = viewportRef.current;
    if (!viewport) return;
    const el = viewport.querySelector<HTMLElement>(`[data-event-index="${target}"]`);
    if (!el) return;
    pendingJumpRef.current = null;
    const viewportRect = viewport.getBoundingClientRect();
    const elRect = el.getBoundingClientRect();
    viewport.scrollTo({ top: viewport.scrollTop + (elRect.top - viewportRect.top) - 16 });
    el.classList.remove("preview-jump-flash");
    // 强制 reflow 以便重复跳转同一条时也能重新触发动画
    void el.offsetWidth;
    el.classList.add("preview-jump-flash");
    window.setTimeout(() => el.classList.remove("preview-jump-flash"), 1700);
  }, []);

  useEffect(() => {
    if (!open || !rolloutPath || !initialJump) return;
    setFilter(initialJump.query);
    pendingJumpRef.current = initialJump.eventIndex;
    void loadUpTo(initialJump.eventOffset).then(() => scrollPendingIntoView());
  }, [initialJump, loadUpTo, open, rolloutPath, scrollPendingIntoView]);

  /** 滚动跟随：视口上沿 1/3 处上方最近的一条用户提问视为当前时间线位置。 */
  const updateActiveFromScroll = useCallback(() => {
    const viewport = viewportRef.current;
    if (!viewport) return;
    const anchors = viewport.querySelectorAll<HTMLElement>("[data-timeline-anchor]");
    if (anchors.length === 0) return;
    const threshold =
      viewport.getBoundingClientRect().top + viewport.clientHeight * 0.33;
    let current: number | null = null;
    for (const node of anchors) {
      if (node.getBoundingClientRect().top > threshold) break;
      current = Number(node.dataset.eventIndex);
    }
    if (current === null) current = Number(anchors[0].dataset.eventIndex);
    if (Number.isFinite(current)) setActiveTimelineIndex(current);
  }, []);

  const jumpToTimelineMessage = useCallback(
    (prompt: UserPromptBrief) => {
      setActiveTimelineIndex(prompt.index);
      pendingJumpRef.current = prompt.index;
      // 文本过滤可能把目标消息隐藏，跳转时清空
      setFilter("");
      void loadUpTo(prompt.offset).then(() => scrollPendingIntoView());
    },
    [loadUpTo, scrollPendingIntoView],
  );

  // 加载/过滤变化后：完成待跳转的定位。搜索期间无需扫描整段 DOM 更新时间线。
  useEffect(() => {
    scrollPendingIntoView();
    if (normalizedFilter) return;
    if (scrollSpyRafRef.current) cancelAnimationFrame(scrollSpyRafRef.current);
    scrollSpyRafRef.current = requestAnimationFrame(updateActiveFromScroll);
  }, [filtered, normalizedFilter, scrollPendingIntoView, updateActiveFromScroll]);

  useEffect(() => {
    return () => {
      if (scrollSpyRafRef.current) cancelAnimationFrame(scrollSpyRafRef.current);
    };
  }, []);

  const onScroll = (e: React.UIEvent<HTMLDivElement>) => {
    const el = e.currentTarget;
    if (el.scrollHeight - el.scrollTop - el.clientHeight < 200) {
      void loadMore();
    }
    if (scrollSpyRafRef.current) cancelAnimationFrame(scrollSpyRafRef.current);
    scrollSpyRafRef.current = requestAnimationFrame(updateActiveFromScroll);
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
      const text = await api.copyResumeCommand(
        session.provider,
        session.id,
        session.cwd,
        session.resume_command,
      );
      toast.success(`已复制：${text}`);
    } catch (e: any) {
      toast.error("复制失败：" + String(e?.message ?? e));
    }
  };

  const copySessionId = async () => {
    if (!session) return;
    try {
      await copyText(session.id);
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

  const copyPath = async () => {
    if (!rolloutPath) return;
    try {
      await copyText(rolloutPath);
      toast.success("已复制 rollout 路径");
    } catch (e: any) {
      toast.error("复制 rollout 路径失败：" + String(e?.message ?? e));
    }
  };

  const requestForkAt = (event: PreviewEvent) => {
    if (!canForkSession || forking || !isStableForkNode(event, provider)) return;
    setForkTarget(event);
  };

  const confirmForkAt = async () => {
    if (!canForkSession || !session || !rolloutPath || !forkTarget || forking) return;
    setForking(true);
    try {
      const target = {
        session_id: session.id,
        rollout_path: rolloutPath,
        event_index: forkTarget.index,
      };
      const opencodeCutoff = provider === "opencode" ? openCodeForkPoint(forkTarget) : null;
      if (provider === "opencode" && !opencodeCutoff) throw new Error("所选消息不支持复制，请刷新预览后重试");
      const report = provider === "opencode"
        ? await api.copyOpenCodeSession({ ...target, opencode_dir: opencodeDir!, cutoff: opencodeCutoff! })
        : provider === "claude"
          ? await api.forkClaudeSessionAtEvent({
              ...target,
              claude_dir: claudeDir!,
              message_uuid: (forkTarget.raw as { uuid: string }).uuid,
            })
          : await api.forkSessionAtEvent({ ...target, codex_dir: codexDir! });
      const count = "message_count" in report
        ? `${report.message_count} 条消息、${report.part_count} 个内容块`
        : `${report.included_lines} 行`;
      toast.success(provider === "codex" ? "已创建回溯分支" : "已复制到所选消息", {
        description: `新会话 ${report.new_id.slice(0, 8)}，已复制 ${count}`,
      });
      if ("desktop_restart_required" in report && report.desktop_restart_required) {
        toast.info("操作已完成，重启 Codex App 后刷新会话列表");
      }
      setForkTarget(null);
      onOpenChange(false);
      await onForked?.();
    } catch (e: any) {
      toast.error(provider === "codex" ? "创建回溯分支失败" : "复制会话失败", {
        description: String(e?.message ?? e),
      });
    } finally {
      setForking(false);
    }
  };

  const recordEditResult = (report: EditApplyReport) => {
    setLastReport(report);
    if (report.status === "needs_recovery") {
      toast.warning("部分完成，请核对提交状态", { description: report.warning ?? report.op_id });
    } else {
      toast.success("本地修改已保存", { action: { label: "详情", onClick: () => setSessionInfoOpen(true) } });
    }
  };

  const recordMutationFailure = (title: string, error: unknown) => {
    const message = String((error as Error)?.message ?? error);
    if (/EDIT_CONFLICT|EDIT_RECOVERY|EDIT_INCONSISTENT/.test(message)) setMutationError(message);
    toast.error(title, { description: message });
  };

  const refreshAfterEdit = async () => {
    try { await onEdited?.(); }
    catch (error) { toast.warning("本地变更已保存，列表刷新失败", { description: String((error as Error)?.message ?? error) }); }
  };

  const requestEditAt = (event: PreviewEvent) => {
    if (!canMutateSession) return;
    const target = paginatedTargets([event])[0];
    const restricted = capability?.content_mappings.find((m) => target && m.thread_id === target.thread_id && m.turn_id === target.turn_id && m.item_id === target.item_id && !m.operations.edit_text.supported);
    if (restricted) {
      toast.warning("当前消息无法改写", { description: restricted.operations.edit_text.reason ?? "请查看会话信息中的操作详情" });
      return;
    }
    setEditText(editableText(event));
    setEditBlocks(editableTextBlocks(event));
    setEditTarget(event);
  };

  const confirmEdit = async () => {
    if (!session || !backupDir || !rolloutPath || !editTarget) return;
    if (mutationInFlightRef.current) return;
    mutationInFlightRef.current = true;
    setMutating(true);
    try {
      const report = await api.editSessionEventText({
        provider,
        expected_revision: revisionRef.current,
        rollout_path: rolloutPath,
        session_id: session.id,
        backup_dir: backupDir,
        line_no: editTarget.index,
        targets: paginatedTargets([editTarget]),
        new_text: editText,
        text_blocks: editBlocks.length ? editBlocks.filter((b) => b.text !== editableTextBlocks(editTarget).find((old) => old.content_index === b.content_index)?.text) : undefined,
      });
      recordEditResult(report);
      setEditTarget(null);
      await reloadPreservingView();
      await refreshAfterEdit();
    } catch (e: any) {
      recordMutationFailure("改写失败", e);
    } finally {
      mutationInFlightRef.current = false;
      setMutating(false);
    }
  };

  const requestDeleteAt = (event: PreviewEvent) => {
    if (!canMutateSession || !rolloutPath) return;
    setDeletePlan(null);
    setDeleteTarget(event);
    const requestId = ++deleteRequestRef.current;
    api
      .planSessionEventDeletion(provider, rolloutPath, [event.index], revisionRef.current, paginatedTargets([event]))
      .then((plan) => { if (requestId === deleteRequestRef.current) { setDeletePlan(plan); setDeleteScope(plan.messages.filter((m) => m.reason === "selected")); } })
      .catch((e: any) => {
        if (requestId !== deleteRequestRef.current) return;
        recordMutationFailure("生成删除计划失败", e);
        setDeleteTarget(null);
      });
  };

  const selectWholeTurn = async (turn: DeleteTurnSelection) => {
    const scope = [...new Map([...deleteScope, ...turn.messages].map((m) => [JSON.stringify(m.target), m])).values()];
    const requestId = ++deleteRequestRef.current;
    setDeletePlan(null);
    try {
      const plan = await api.planSessionEventDeletion(provider, rolloutPath, scope.map((m) => m.line_no), revisionRef.current, scope.flatMap((m) => m.target ? [m.target] : []));
      if (requestId !== deleteRequestRef.current) return;
      setDeleteScope(scope);
      setDeletePlan(plan);
    } catch (error) {
      if (requestId !== deleteRequestRef.current) return;
      recordMutationFailure("选择整轮失败", error);
      setDeleteTarget(null);
      setDeleteSelectedTarget(null);
    }
  };

  const confirmDelete = async () => {
    if (!session || !backupDir || !rolloutPath || !deleteTarget) return;
    if (!deletePlan || deletePlan.blocked.length > 0 || !canMutateSession) return;
    if (mutationInFlightRef.current) return;
    mutationInFlightRef.current = true;
    setMutating(true);
    try {
      const report = await api.deleteSessionEvents({
        provider,
        expected_revision: deletePlan?.revision ?? null,
        rollout_path: rolloutPath,
        session_id: session.id,
        backup_dir: backupDir,
        line_nos: deleteScope.length ? deleteScope.map((m) => m.line_no) : [deleteTarget.index],
        targets: deleteScope.length ? deleteScope.flatMap((m) => m.target ? [m.target] : []) : paginatedTargets([deleteTarget]),
      });
      recordEditResult(report);
      setDeleteTarget(null);
      setDeletePlan(null);
      await reloadPreservingView();
      await refreshAfterEdit();
    } catch (e: any) {
      recordMutationFailure("删除失败", e);
    } finally {
      mutationInFlightRef.current = false;
      setMutating(false);
    }
  };

  const requestDeleteSelected = () => {
    if (!canMutateSession || !rolloutPath || selectionFirstIndex === null || selectionSecondIndex === null) return;
    const start = Math.min(selectionFirstIndex, selectionSecondIndex);
    const end = Math.max(selectionFirstIndex, selectionSecondIndex);
    setDeletePlan(null);
    const requestId = ++deleteRequestRef.current;
    const selected = latestCanonicalEvents(events)
      .filter((e) => e.index >= start && e.index <= end && canDeletePreviewEvent(e));
    setDeleteSelectedTarget({ start, end, events: selected });
    const indices = selected
      .map((e) => e.index);
    api
      .planSessionEventDeletion(provider, rolloutPath, indices, revisionRef.current, paginatedTargets(selected))
      .then((plan) => { if (requestId === deleteRequestRef.current) { setDeletePlan(plan); setDeleteScope(plan.messages.filter((m) => m.reason === "selected")); } })
      .catch((e: any) => {
        if (requestId !== deleteRequestRef.current) return;
        recordMutationFailure("生成删除计划失败", e);
        setDeleteSelectedTarget(null);
      });
  };

  const confirmDeleteSelected = async () => {
    if (!session || !backupDir || !rolloutPath || !deleteSelectedTarget) return;
    if (!deletePlan || deletePlan.blocked.length > 0 || !canMutateSession) return;
    if (mutationInFlightRef.current) return;
    mutationInFlightRef.current = true;
    setMutating(true);
    try {
      const indices = deletePlan.lines.filter((line) => line.reason === "selected").map((line) => line.line_no);
      const report = await api.deleteSessionEvents({
        provider,
        expected_revision: deletePlan?.revision ?? null,
        rollout_path: rolloutPath,
        session_id: session.id,
        backup_dir: backupDir,
        line_nos: deleteScope.length ? deleteScope.map((m) => m.line_no) : indices,
        targets: deleteScope.length ? deleteScope.flatMap((m) => m.target ? [m.target] : []) : paginatedTargets(deleteSelectedTarget.events),
      });
      recordEditResult(report);
      setDeleteSelectedTarget(null);
      setDeletePlan(null);
      setIsSelecting(false);
      setSelectionFirstIndex(null);
      setSelectionSecondIndex(null);
      await reloadPreservingView();
      await refreshAfterEdit();
    } catch (e: any) {
      recordMutationFailure("删除失败", e);
    } finally {
      mutationInFlightRef.current = false;
      setMutating(false);
    }
  };

  const loadEditHistory = useCallback(async () => {
    if (!session || !backupDir || !rolloutPath) return;
    try {
      const h = await api.sessionEditHistory({
        provider,
        rollout_path: rolloutPath,
        session_id: session.id,
        backup_dir: backupDir,
      });
      setEditHistory(h);
    } catch (e: any) {
      setHistoryOpen(false);
      toast.error("读取编辑历史失败", { description: String(e?.message ?? e) });
    }
  }, [backupDir, provider, rolloutPath, session]);

  const openEditHistory = () => {
    setEditHistory(null);
    setHistoryOpen(true);
    void loadEditHistory();
  };

  const undoLastEdit = async () => {
    if (!session || !backupDir || !rolloutPath) return;
    if (mutationInFlightRef.current) return;
    mutationInFlightRef.current = true;
    setMutating(true);
    try {
      const report = await api.undoLastSessionEdit({
        provider,
        expected_revision: editHistory?.revision ?? revisionRef.current,
        rollout_path: rolloutPath,
        session_id: session.id,
        backup_dir: backupDir,
      });
      recordEditResult(report);
      await loadEditHistory();
      await reloadPreservingView();
      await refreshAfterEdit();
    } catch (e: any) {
      recordMutationFailure("撤销失败", e);
    } finally {
      mutationInFlightRef.current = false;
      setMutating(false);
    }
  };

  const restoreSnapshot = async (name: string) => {
    if (!session || !backupDir || !rolloutPath) return;
    if (mutationInFlightRef.current) return;
    mutationInFlightRef.current = true;
    setMutating(true);
    try {
      const report = await api.restoreSessionEditSnapshot({
        provider,
        expected_revision: editHistory?.revision ?? revisionRef.current,
        rollout_path: rolloutPath,
        session_id: session.id,
        backup_dir: backupDir,
        snapshot_name: name,
      });
      recordEditResult(report);
      await loadEditHistory();
      await reloadPreservingView();
      await refreshAfterEdit();
    } catch (e: any) {
      recordMutationFailure("还原快照失败", e);
    } finally {
      mutationInFlightRef.current = false;
      setMutating(false);
    }
  };

  const reconcileEdit = async () => {
    if (!session || !backupDir || !editHistory || mutationInFlightRef.current) return;
    mutationInFlightRef.current = true;
    setMutating(true);
    try {
      await api.reconcileSessionEdit({ provider, rollout_path: rolloutPath, session_id: session.id,
        backup_dir: backupDir, expected_revision: editHistory.revision });
      setLastReport(null);
      await loadEditHistory();
      await reloadPreservingView();
      toast.info("操作记录已核对；会话内容未再次修改");
    } catch (error) {
      toast.error("核对未完成", { description: String((error as Error)?.message ?? error) });
    } finally {
      mutationInFlightRef.current = false;
      setMutating(false);
    }
  };

  const editActions: EditActions = {
    enabled: canMutateSession,
    pending: mutating,
    canEditText: (e) => canEditEventText(provider, e) && (capability?.format !== "paginated" || paginatedTargets([e]).length === 1),
    canDelete: canDeletePreviewEvent,
    onEdit: requestEditAt,
    onDelete: requestDeleteAt,
  };

  return (
    <>
    <Dialog open={open} onOpenChange={(next) => !mutating && onOpenChange(next)}>
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
              <DialogTitle
                className="truncate pr-4 text-[15px] font-semibold tracking-tight"
                title={session?.title || "预览会话"}
              >
                {session?.title || "预览会话"}
              </DialogTitle>
              <DialogDescription className="sr-only">
                查看会话消息、过程事件和对话时间线。
              </DialogDescription>
              {session && (
                <div className="mt-1.5 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground">
                  <span className="text-foreground/70">{sourceLabel}</span>
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
                  <Dot />
                  <span className="text-[11px] text-muted-foreground">
                    {onlyMsg ? "筛选后对话/状态" : "筛选后事件"} <span className="tabular-nums text-foreground/80">{filtered.length}</span>
                    <span className="mx-1 text-muted-foreground/50">/</span>
                    {done ? "全部已加载" : "部分已加载"}
                    <span className="ml-1 text-muted-foreground/70">
                      {!done ? "· 滚动加载更多" : "· 已到末尾"}
                    </span>
                  </span>
                </div>
              )}
            </div>
          </div>

          <div className="mt-3.5 flex flex-wrap items-center gap-2">
            <Input
              placeholder="搜索已加载内容…"
              aria-label="搜索已加载的会话内容"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              className="h-8 w-64 border-border/70"
            />
            <label
              htmlFor="only-msg"
              className="group flex h-8 cursor-pointer items-center gap-2 rounded-md border border-border/70 bg-muted/30 px-2.5 transition-colors hover:bg-muted/50"
            >
              <Switch id="only-msg" checked={onlyMsg} onCheckedChange={changeOnlyMsg} />
              <Label htmlFor="only-msg" className="cursor-pointer text-xs">
                仅看对话消息
              </Label>
            </label>
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <Button
                  variant="outline"
                  size="sm"
                  className="h-8 gap-1.5 border-border/70 bg-muted/30 px-2.5 text-xs font-normal hover:bg-muted/50"
                  disabled={!onlyMsg || Boolean(filter)}
                  title={
                    !onlyMsg
                      ? "仅看对话消息开启后可统一收起或展开过程消息"
                      : filter
                        ? "过滤结果会直接显示命中的消息"
                        : "统一收起或展开当前会话的过程消息"
                  }
                >
                  <Bot className="h-3.5 w-3.5" />
                  过程消息
                  <ChevronDown className="h-3 w-3 text-muted-foreground" />
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="start" className="min-w-[7rem]">
                <DropdownMenuItem onSelect={() => changeProcessDefaultCollapsed(true)}>
                  <span>全部收起</span>
                  {processExpansionState === "collapsed" && (
                    <Check className="h-4 w-4 text-primary" />
                  )}
                </DropdownMenuItem>
                <DropdownMenuItem onSelect={() => changeProcessDefaultCollapsed(false)}>
                  <span>全部展开</span>
                  {processExpansionState === "expanded" && (
                    <Check className="h-4 w-4 text-primary" />
                  )}
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
            {!done && events.length > 0 && (
              <Button
                variant="outline"
                size="sm"
                className="h-8 gap-1.5 border-border/70 bg-muted/30 px-2.5 text-xs font-normal hover:bg-muted/50"
                disabled={Boolean(readError)}
                onClick={() => loadingAll ? (cancelLoadAllRef.current = true) : void loadAll()}
              >
                {loadingAll ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                ) : (
                  <ChevronsDown className="h-3.5 w-3.5" />
                )}
                {loadingAll ? `停止加载 · ${events.length}` : filter ? "搜索整个会话" : "加载全部"}
              </Button>
            )}
            {canMutateSession && !isSelecting && (
              <Button
                variant="outline"
                size="sm"
                className="h-8 gap-1.5 border-border/70 bg-muted/30 px-2.5 text-xs font-normal hover:bg-muted/50"
                onClick={() => {
                  setIsSelecting(true);
                  setSelectionFirstIndex(null);
                  setSelectionSecondIndex(null);
                }}
              >
                <MousePointer2 className="h-3.5 w-3.5" />
                开始选取
              </Button>
            )}
            {canMutateSession && isSelecting && (
              <>
                <Button
                  variant="outline"
                  size="sm"
                  className="h-8 gap-1.5 border-border/70 bg-muted/30 px-2.5 text-xs font-normal hover:bg-muted/50"
                  onClick={() => {
                    setIsSelecting(false);
                    setSelectionFirstIndex(null);
                    setSelectionSecondIndex(null);
                  }}
                >
                  <X className="h-3.5 w-3.5" />
                  结束选取
                </Button>
                {selectionFirstIndex !== null && selectionSecondIndex !== null && (
                  <Button
                    variant="outline"
                    size="sm"
                    className="h-8 gap-1.5 border-destructive/50 bg-destructive/10 px-2.5 text-xs font-normal text-destructive hover:bg-destructive/20"
                    onClick={requestDeleteSelected}
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                    删除选中
                    <span className="tabular-nums">
                      {events.filter(
                        (e) =>
                          e.index >= Math.min(selectionFirstIndex, selectionSecondIndex) &&
                          e.index <= Math.max(selectionFirstIndex, selectionSecondIndex) &&
                          canDeletePreviewEvent(e),
                      ).length}
                      条
                    </span>
                  </Button>
                )}
                {selectionFirstIndex !== null && selectionSecondIndex === null && (
                  <span className="text-xs text-muted-foreground">请点击第二个事件完成选取</span>
                )}
              </>
            )}
            <PreviewToolbarActions
              hasSession={!!session}
              canOpenEditHistory={!customRolloutPath && !!session && !!backupDir && provider !== "cursor"}
              onCopySessionId={copySessionId}
              onCopyResume={copyResume}
              onRevealDirectory={reveal}
              onOpenEditHistory={openEditHistory}
              onCopyPath={copyPath}
              onRefresh={() => void reloadPreservingView()}
              refreshing={mutating || loading}
              onSessionInfo={() => setSessionInfoOpen(true)}
            />
          </div>
          {capability && capability.blocked_reasons.length > 0 && (
            <div role="status" className="mt-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs">
              <strong>当前无法写入 · {capability.format === "paginated" ? "分页历史需核对" : "暂不支持消息写入"}</strong>
              <div className="mt-1">{capability.blocked_reasons.join("；")}</div>
            </div>
          )}
          {!!capability?.diagnostics?.length && <div role="alert" className="mt-2 rounded-md border border-amber-500/40 bg-amber-500/10 p-2 text-xs">部分消息正文不一致，受影响写入已拦截；其他消息仍可编辑。<button className="ml-2 underline" onClick={() => setSessionInfoOpen(true)}>查看诊断</button></div>}
          {(readError || mutationError) && <div role="alert" className="mt-2 rounded-md border border-destructive/40 p-2 text-xs text-destructive">{readError || mutationError}<div className="mt-1 flex gap-3"><button className="underline" disabled={mutating || loading} onClick={() => void reloadPreservingView()}>刷新并重新选择</button>{mutationError && !isSessionWideMutationFailure(mutationError) ? <button className="underline" onClick={() => setSessionInfoOpen(true)}>查看内容块映射</button> : <button className="underline" onClick={openEditHistory}>核对操作与恢复状态</button>}</div></div>}
          {lastReport?.status === "needs_recovery" && (
            <div role="alert" className="mt-2 rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs">
              <strong>部分完成，需要处理</strong>
              <div>{lastReport.warning}</div>
              <button className="mt-1 underline" onClick={openEditHistory}>查看操作记录与恢复状态</button>
            </div>
          )}
        </DialogHeader>

        <div className="relative min-h-0 flex-1">
          <PreviewTechnicalContext.Provider value={!onlyMsg}>
          <ScrollArea
            className="h-full bg-muted/30"
            viewportRef={viewportRef}
            onViewportScroll={onScroll}
          >
            <div className="mx-auto w-full max-w-3xl min-w-0 space-y-4 overflow-x-hidden px-6 py-6">
              {!onlyMsg && relatedSubagents.length > 0 && (
                <SubagentOverview key={session?.id} items={relatedSubagents} />
              )}

              {filtered.length === 0 && !loading && (onlyMsg || relatedSubagents.length === 0) && (
                <div className="flex flex-col items-center justify-center gap-2 py-16 text-center text-muted-foreground">
                  <Sparkles className="h-8 w-8 opacity-50" />
                  <div className="text-sm">
                    {readError ? "读取未完成，请刷新预览" : !done ? "已加载范围暂无匹配，尚未搜索完整会话" : events.length === 0 ? "无事件" : "当前筛选下无匹配事件"}
                  </div>
                </div>
              )}

              {rows.map((row) =>
                row.type === "process" ? (
                  <ProcessTurnGroup
                    key={previewProcessKey(row.events)}
                    events={row.events}
                    expanded={isProcessGroupExpanded(
                      previewProcessKey(row.events),
                      processDefaultCollapsed,
                      processExpansionOverrides,
                    )}
                    onExpandedChange={(expanded) =>
                      changeProcessGroupExpanded(previewProcessKey(row.events), expanded)
                    }
                  >
                    {(event) => {
                      const inRange =
                        isSelecting &&
                        selectionFirstIndex !== null &&
                        selectionSecondIndex !== null &&
                        event.index >= Math.min(selectionFirstIndex, selectionSecondIndex) &&
                        event.index <= Math.max(selectionFirstIndex, selectionSecondIndex);
                      const isStart =
                        isSelecting &&
                        selectionFirstIndex !== null &&
                        selectionSecondIndex === null &&
                        event.index === selectionFirstIndex;
                      return (
                      <div
                        key={previewEventKey(event)}
                        data-event-index={event.index}
                        data-reading-key={previewEventKey(event)}
                        className={cn(
                          isSelecting && "cursor-pointer",
                          inRange && "bg-destructive/10 ring-1 ring-destructive/30",
                          isStart && "bg-primary/10 ring-1 ring-primary/30",
                        )}
                        onClick={
                          isSelecting
                            ? () => {
                                if (selectionFirstIndex === null) {
                                  setSelectionFirstIndex(event.index);
                                } else if (selectionSecondIndex === null) {
                                  setSelectionSecondIndex(event.index);
                                } else {
                                  setSelectionFirstIndex(event.index);
                                  setSelectionSecondIndex(null);
                                }
                              }
                            : undefined
                        }
                      >
                        <EventBubble
                          e={event}
                          actions={{
                            fork: {
                              enabled: canForkSession && isStableForkNode(event, provider),
                              label: forkLabel,
                              pending: forking,
                              onSelect: requestForkAt,
                            },
                            edit: editActions,
                          }}
                        />
                      </div>
                      );
                    }}
                  </ProcessTurnGroup>
                ) : (
                  <div
                    key={previewEventKey(row.event, !onlyMsg)}
                    data-event-index={row.event.index}
                    data-reading-key={previewEventKey(row.event, !onlyMsg)}
                    data-timeline-anchor={timelineIndexSet?.has(row.event.index) || undefined}
                    className={cn(
                      isSelecting && "cursor-pointer",
                      isSelecting &&
                        selectionFirstIndex !== null &&
                        selectionSecondIndex !== null &&
                        row.event.index >= Math.min(selectionFirstIndex, selectionSecondIndex) &&
                        row.event.index <= Math.max(selectionFirstIndex, selectionSecondIndex) &&
                        "bg-destructive/10 ring-1 ring-destructive/30",
                      isSelecting &&
                        selectionFirstIndex !== null &&
                        selectionSecondIndex === null &&
                        row.event.index === selectionFirstIndex &&
                        "bg-primary/10 ring-1 ring-primary/30",
                    )}
                    onClick={
                      isSelecting
                        ? () => {
                            if (selectionFirstIndex === null) {
                              setSelectionFirstIndex(row.event.index);
                            } else if (selectionSecondIndex === null) {
                              setSelectionSecondIndex(row.event.index);
                            } else {
                              setSelectionFirstIndex(row.event.index);
                              setSelectionSecondIndex(null);
                            }
                          }
                        : undefined
                    }
                  >
                    <EventBubble
                      e={row.event}
                      actions={{
                        fork: {
                          enabled: canForkSession && isStableForkNode(row.event, provider),
                          label: forkLabel,
                          pending: forking,
                          onSelect: requestForkAt,
                        },
                        edit: editActions,
                      }}
                    />
                  </div>
                ),
              )}

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
          </PreviewTechnicalContext.Provider>

          {prompts && prompts.length > 0 && (
            <PromptTimeline
              prompts={prompts}
              activeIndex={activeTimelineIndex}
              onJump={jumpToTimelineMessage}
            />
          )}
        </div>
      </DialogContent>
    </Dialog>
    <Dialog open={sessionInfoOpen} onOpenChange={setSessionInfoOpen}>
      <DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-[640px]">
        <DialogHeader><DialogTitle>会话信息</DialogTitle><DialogDescription>数据来源、历史诊断与本地修改详情。</DialogDescription></DialogHeader>
        <dl className="space-y-2 break-all text-xs"><dt className="text-muted-foreground">来源</dt><dd>{sourceLabel}</dd><dt className="text-muted-foreground">会话 ID</dt><dd className="font-mono">{session?.id}</dd><dt className="text-muted-foreground">路径</dt><dd className="font-mono">{rolloutPath}</dd><dt className="text-muted-foreground">工作目录</dt><dd>{session?.cwd}</dd><dt className="text-muted-foreground">已加载 / 总底层记录</dt><dd>{events.length} / {totalEvents || "未知"}</dd></dl>
        {capability?.diagnostics?.map((reason, i) => <div key={i} className="break-all rounded-md border border-amber-500/40 p-3 text-xs">{reason}</div>)}
        {!!capability?.content_mappings?.length && <details className="rounded-md border p-3 text-xs"><summary className="cursor-pointer">内容块映射依据 · 正常 {contentMappingCounts(capability.content_mappings).matched} / 不支持 {contentMappingCounts(capability.content_mappings).unsupported} / 不一致 {contentMappingCounts(capability.content_mappings).inconsistent}</summary><p className="my-2 text-muted-foreground">索引与差异位置从 0 开始；正文差异偏移按 UTF-8 字节计。这里只展示结构，不包含正文或媒体数据。</p>{capability.content_mappings.map((mapping) => <details key={`${mapping.turn_id}:${mapping.item_id}:${mapping.context_ordinal}`} className="mt-2 border-t pt-2"><summary className="cursor-pointer break-all">消息 {mapping.item_id} · {mapping.status === "matched" ? "已对应" : mapping.status === "inconsistent" ? "正文不一致" : "映射尚未支持"}</summary><div className="my-2 space-y-1">{([["edit_text", "改写文本块"], ["delete_message", "删除消息"], ["delete_turn", "删除整轮"]] as const).map(([key, label]) => <div key={key}>{label}：{mapping.operations[key].supported ? "映射允许，执行前仍检查依赖与冲突" : mapping.operations[key].reason}</div>)}</div><pre className="mt-2 whitespace-pre-wrap break-all">{JSON.stringify(mapping, null, 2)}</pre></details>)}</details>}
        {lastReport && <div className="space-y-2 rounded-md border p-3 text-xs"><strong>{lastReport.status === "needs_recovery" ? "需要处理" : "本地修改已保存"}</strong><div className="break-all">操作 {lastReport.op_id}</div><div>改写 {lastReport.changed_lines} · 删除 {lastReport.deleted_lines} · 恢复 {lastReport.restored_lines} 条底层记录</div><div>{lastReport.warning}</div><p>{lastReport.status === "needs_recovery" ? "提交尚未完成核对，请先查看操作记录与恢复状态。" : "已保存本地会话与相关投影。"}本次操作未自动执行原生读取或 Codex App 冷启动验证；测试样本的验证结果不代表当前会话已验收。</p><Button variant="outline" size="sm" onClick={() => { setSessionInfoOpen(false); openEditHistory(); }}>查看编辑历史与恢复</Button></div>}
      </DialogContent>
    </Dialog>
    <PreviewMutationDialogs
      provider={provider}
      sourceLabel={sourceLabel}
      sessionId={session?.id ?? ""}
      rolloutPath={rolloutPath}
      onSelectTurn={(turn) => void selectWholeTurn(turn)}
      fork={{
        target: forkTarget,
        running: forking,
        onClose: () => setForkTarget(null),
        onConfirm: () => void confirmForkAt(),
      }}
      edit={{
        target: editTarget,
        running: mutating,
        text: editText,
        onTextChange: setEditText,
        blocks: editBlocks,
        onBlockChange: (index, text) => setEditBlocks((current) => current.map((block) => block.content_index === index ? { ...block, text } : block)),
        onClose: () => setEditTarget(null),
        onConfirm: () => void confirmEdit(),
      }}
      deleteEvent={{
        target: deleteTarget,
        plan: deletePlan,
        running: mutating,
        onClose: () => {
          deleteRequestRef.current += 1;
          setDeleteTarget(null);
          setDeletePlan(null);
        },
        onConfirm: () => void confirmDelete(),
      }}
      deleteSelection={{
        target: deleteSelectedTarget,
        plan: deletePlan,
        running: mutating,
        onClose: () => {
          deleteRequestRef.current += 1;
          setDeleteSelectedTarget(null);
          setDeletePlan(null);
        },
        onConfirm: () => void confirmDeleteSelected(),
      }}
    />
    <PreviewEditHistoryDialog
      open={historyOpen}
      history={editHistory}
      mutating={mutating}
      blockedReason={capability?.blocked_reasons.join("；") || readError || null}
      onReconcile={() => void reconcileEdit()}
      onOpenChange={setHistoryOpen}
      onUndo={() => void undoLastEdit()}
      onRestore={(snapshotName) => void restoreSnapshot(snapshotName)}
    />
    </>
  );
}

/* ---------- 单条事件（聊天气泡）---------- */

/**
 * 一轮里最终答复之前的过程性 Agent 消息。Codex App 不在对话流中展示这些消息，
 * 默认状态由“全部收起/全部展开”决定，同时允许当前会话中的每一轮单独切换。
 */
function ProcessTurnGroup({
  events,
  expanded,
  onExpandedChange,
  children,
}: {
  events: PreviewEvent[];
  expanded: boolean;
  onExpandedChange: (expanded: boolean) => void;
  children: (event: PreviewEvent) => React.ReactNode;
}) {
  return (
    <div className="space-y-4" data-reading-key={previewProcessKey(events)}>
      <button
        type="button"
        aria-expanded={expanded}
        onClick={() => onExpandedChange(!expanded)}
        className="mx-auto flex items-center gap-1.5 rounded-full border border-border/60 bg-background/60 px-3 py-1 text-[11px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        <Bot className="h-3 w-3" />
        <span>
          {expanded
            ? "收起本轮过程消息"
            : `已收起 ${events.length} 条过程消息`}
        </span>
        <ChevronDown
          className={cn("h-3 w-3 transition-transform", expanded && "rotate-180")}
        />
      </button>
      {expanded && (
        <div className="space-y-4 border-l-2 border-border/50 pl-3 opacity-90">
          {events.map((event) => children(event))}
        </div>
      )}
    </div>
  );
}

function EventBubble({ e, actions }: { e: PreviewEvent; actions: NodeActionSet }) {
  const ts = formatTimeString(e.timestamp);

  if (e.kind === "turn_error") {
    return <div className="rounded-md border border-destructive/30 bg-destructive/5 px-4 py-3 text-sm"><strong>本轮未完成</strong><div className="mt-1 break-words">{e.text_summary}</div><div className="mt-1 text-xs text-muted-foreground">{ts}</div></div>;
  }

  if (e.role === "subagent") {
    return <SubagentEventBubble e={e} ts={ts} actions={actions} />;
  }
  if (isEventMessage(e)) {
    return <EventMessageBubble e={e} ts={ts} actions={actions} />;
  }
  if (e.role === "user") {
    return <UserBubble e={e} ts={ts} actions={actions} />;
  }
  if (e.role === "assistant") {
    return <AssistantBubble e={e} ts={ts} actions={actions} />;
  }
  if (e.role === "reasoning") {
    return <ReasoningBubble e={e} ts={ts} actions={actions} />;
  }
  if (e.role === "tool_call" || e.role === "tool_result") {
    return <ToolBubble e={e} ts={ts} actions={actions} />;
  }
  if (e.role === "meta") {
    return <MetaLine e={e} ts={ts} />;
  }
  return <DefaultBubble e={e} ts={ts} />;
}

function SubagentOverview({ items }: { items: RelatedSubagentSession[] }) {
  const [open, setOpen] = useState(true);
  return (
    <section className="border-y border-border/70 bg-background/30" aria-label="子智能体概览">
      <button
        type="button"
        className="flex w-full items-center gap-2 px-1 py-2.5 text-left text-xs hover:text-foreground"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      >
        <Network className="h-3.5 w-3.5 text-cyan-700 dark:text-cyan-400" />
        <span className="font-medium">子智能体</span>
        <span className="tabular-nums text-muted-foreground">{items.length}</span>
        <ChevronDown
          className={cn(
            "ml-auto h-3.5 w-3.5 text-muted-foreground transition-transform",
            open && "rotate-180",
          )}
        />
      </button>
      {open && (
        <div className="border-t border-border/60 px-1">
          {items.map((item) => (
            <div
              key={item.id}
              className="flex min-w-0 items-start gap-2 border-t border-border/40 py-2.5 first:border-t-0"
              style={{ paddingLeft: Math.min((item.relativeDepth - 1) * 18, 72) }}
            >
              <div className="mt-0.5 flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-cyan-500/10 text-cyan-700 dark:text-cyan-400">
                <Bot className="h-3 w-3" />
              </div>
              <div className="min-w-0 flex-1">
                <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-xs">
                  <span className="font-medium">{item.nickname ?? "子智能体"}</span>
                  <span className="font-mono text-[10px] text-muted-foreground" title={item.id}>
                    {item.id.slice(0, 8)}
                  </span>
                  {item.role && (
                    <Badge variant="outline" className="h-4 px-1 py-0 text-[10px] font-normal">
                      {item.role}
                    </Badge>
                  )}
                  <span className="ml-auto shrink-0 font-mono text-[10px] text-muted-foreground">
                    L{item.depth}
                  </span>
                </div>
                <div className="mt-0.5 truncate font-mono text-[11px] text-foreground/70" title={item.agentPath}>
                  {item.agentPath}
                </div>
                <div className="mt-1 flex flex-wrap gap-x-3 gap-y-0.5 text-[10px] text-muted-foreground">
                  <span>开始 {absoluteTime(item.createdAt)}</span>
                  <span>最后活动 {absoluteTime(item.updatedAt)}</span>
                </div>
              </div>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}

function SubagentEventBubble({
  e,
  ts,
  actions,
}: {
  e: PreviewEvent;
  ts: string;
  actions: NodeActionSet;
}) {
  const [open, setOpen] = useState(false);
  const eventTime = subagentEventTime(e, ts);
  return (
    <div className="group flex gap-3">
      <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-cyan-500/10 text-cyan-700 dark:text-cyan-400">
        <Network className="h-4 w-4" />
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex w-full items-start gap-2 border-l-2 border-cyan-500/30 bg-background/50 px-3 py-2 text-xs">
          <button
            type="button"
            onClick={() => setOpen((value) => !value)}
            className="flex min-w-0 flex-1 items-start gap-2 text-left"
            aria-expanded={open}
          >
            <ChevronDown
              className={cn(
                "mt-0.5 h-3.5 w-3.5 shrink-0 text-muted-foreground transition-transform",
                open && "rotate-180",
              )}
            />
            <span className="shrink-0 font-medium">{subagentEventLabel(e)}</span>
            {e.text_summary && (
              <span className="min-w-0 flex-1 break-words text-muted-foreground">
                {e.text_summary}
              </span>
            )}
            {eventTime && (
              <span className="shrink-0 font-mono text-muted-foreground/70">{eventTime}</span>
            )}
          </button>
          <NodeActionButtons event={e} actions={actions} />
        </div>
        {open && (
          <div className="mt-1.5 overflow-auto border-l-2 border-border/70 bg-card p-3 text-xs">
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

function EventMessageBubble({
  e,
  ts,
  actions,
}: {
  e: PreviewEvent;
  ts: string;
  actions: NodeActionSet;
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
          <NodeActionButtons event={e} actions={actions} />
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

function UserBubble({ e, ts, actions }: { e: PreviewEvent; ts: string; actions: NodeActionSet }) {
  const text = extractText(e);
  const embeddedTranscript = parseEmbeddedTranscriptPrompt(text);
  if (embeddedTranscript) {
    return <EmbeddedTranscriptBubble e={e} ts={ts} prompt={embeddedTranscript} actions={actions} />;
  }

  const diffComments = parseDiffCommentPrompt(text);
  if (diffComments) {
    return <DiffCommentBubble e={e} ts={ts} prompt={diffComments} actions={actions} />;
  }

  const message = parseUserMessageAttachments(text);
  const images = [...new Map([...message.images, ...canonicalMessageImages(e)].map((image) => [image.path, image])).values()];

  return (
    <div className="group flex justify-end gap-3">
      <div className="flex min-w-0 max-w-[85%] flex-col items-end overflow-hidden">
        <div className="mb-1 flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <NodeActionButtons event={e} actions={actions} />
          <span>你</span>
          <EventSourceBadge e={e} />
          {ts && <span className="font-mono">· {ts}</span>}
        </div>
        <div className="chat-md max-w-full rounded-2xl rounded-tr-sm bg-primary px-4 py-2.5 text-primary-foreground">
          {message.markdown ? <ReactMarkdown remarkPlugins={[remarkGfm]}>{message.markdown}</ReactMarkdown> : images.length === 0 ? (
            <span className="italic opacity-70">(空消息)</span>
          ) : null}
          <LocalImageAttachments images={images} />
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
  actions,
}: {
  e: PreviewEvent;
  ts: string;
  prompt: EmbeddedTranscriptPrompt;
  actions: NodeActionSet;
}) {
  const [open, setOpen] = useState(false);
  return (
    <div className="group flex justify-end gap-3">
      <div className="flex min-w-0 max-w-[85%] flex-col items-end overflow-hidden">
        <div className="mb-1 flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <NodeActionButtons event={e} actions={actions} />
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
  actions,
}: {
  e: PreviewEvent;
  ts: string;
  prompt: DiffCommentPrompt;
  actions: NodeActionSet;
}) {
  const countLabel = `${prompt.comments.length} 条批注`;

  return (
    <div className="group flex justify-end gap-3">
      <div className="flex min-w-0 max-w-[85%] flex-col items-end overflow-hidden">
        <div className="mb-1 flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <NodeActionButtons event={e} actions={actions} />
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
  actions,
}: {
  e: PreviewEvent;
  ts: string;
  actions: NodeActionSet;
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
          <NodeActionButtons event={e} actions={actions} />
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

function ReasoningBubble({ e, ts, actions }: { e: PreviewEvent; ts: string; actions: NodeActionSet }) {
  const text = extractText(e);
  const [open, setOpen] = useState(false);
  return (
    <div className="group flex gap-3">
      <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-muted">
        <Sparkles className="h-4 w-4 text-muted-foreground/70" />
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-1.5">
          <button
            onClick={() => setOpen((x) => !x)}
            className="flex items-center gap-1.5 text-[11px] text-muted-foreground hover:text-foreground"
          >
            <ChevronDown className={cn("h-3 w-3 transition-transform", open && "rotate-180")} />
            推理过程
            {ts && <span className="font-mono">· {ts}</span>}
          </button>
          <NodeActionButtons event={e} actions={actions} />
        </div>
        {open && text && (
          <pre className="mt-1.5 whitespace-pre-wrap break-words rounded-md border border-dashed bg-muted/40 px-3 py-2 font-mono text-xs text-muted-foreground">
            {text}
          </pre>
        )}
      </div>
    </div>
  );
}

function ToolBubble({ e, ts, actions }: { e: PreviewEvent; ts: string; actions: NodeActionSet }) {
  const [open, setOpen] = useState(false);
  const isCall = e.role === "tool_call";
  return (
    <div className="group flex gap-3">
      <div
        className={cn(
          "flex h-8 w-8 shrink-0 items-center justify-center rounded-full",
          isCall ? "bg-purple-500/15 text-purple-600 dark:text-purple-400" : "bg-amber-500/15 text-amber-600 dark:text-amber-400",
        )}
      >
        {isCall ? <Wrench className="h-4 w-4" /> : <Terminal className="h-4 w-4" />}
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex w-full items-center gap-2 rounded-md border bg-card px-3 py-2 text-left text-xs shadow-sm hover:bg-accent">
          <button
            onClick={() => setOpen((x) => !x)}
            className="flex min-w-0 flex-1 items-center gap-2 text-left"
          >
            <ChevronDown className={cn("h-3.5 w-3.5 shrink-0 transition-transform", open && "rotate-180")} />
            <span className="font-medium">{isCall ? "工具调用" : "工具返回"}</span>
            <span className="truncate font-mono text-muted-foreground">{e.kind}</span>
            <span className="ml-auto min-w-0 flex-1 truncate text-muted-foreground">
              {e.text_summary || ""}
            </span>
            {ts && <span className="shrink-0 font-mono text-muted-foreground/70">{ts}</span>}
          </button>
          <NodeActionButtons event={e} actions={actions} />
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

function NodeActionButtons({ event, actions }: { event: PreviewEvent; actions: NodeActionSet }) {
  const showFork = actions.fork.enabled;
  const showEdit = actions.edit.enabled && actions.edit.canEditText(event);
  const showDelete = actions.edit.enabled && actions.edit.canDelete(event);
  if (!showFork && !showEdit && !showDelete) return null;
  const btnClass =
    "h-5 shrink-0 gap-1 px-1.5 text-[11px] opacity-0 transition-opacity duration-150 pointer-events-none group-hover:pointer-events-auto group-hover:opacity-100 group-focus-within:pointer-events-auto group-focus-within:opacity-100";
  return (
    <span className="inline-flex shrink-0 items-center gap-0.5">
      {showFork && (
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className={btnClass}
          disabled={actions.fork.pending}
          onClick={(e) => {
            e.preventDefault();
            e.stopPropagation();
            actions.fork.onSelect(event);
          }}
        >
          <GitBranch className="h-3 w-3" />
          {actions.fork.label}
        </Button>
      )}
      {showEdit && (
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className={btnClass}
          disabled={actions.edit.pending}
          onClick={(e) => {
            e.preventDefault();
            e.stopPropagation();
            actions.edit.onEdit(event);
          }}
        >
          <Pencil className="h-3 w-3" />
          编辑
        </Button>
      )}
      {showDelete && (
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className={cn(btnClass, "text-destructive hover:text-destructive")}
          disabled={actions.edit.pending}
          onClick={(e) => {
            e.preventDefault();
            e.stopPropagation();
            actions.edit.onDelete(event);
          }}
        >
          <Trash2 className="h-3 w-3" />
          删除
        </Button>
      )}
    </span>
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
  const showTechnical = useContext(PreviewTechnicalContext);
  const outer = rawType(e);
  const payload = payloadType(e);
  if (payload === "item_completed" && !showTechnical) return null;
  if (outer !== "event_msg" && outer !== "response_item") return null;
  if (payload !== "user_message" && payload !== "agent_message" && payload !== "message" && payload !== "item_completed") return null;

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
      {payload === "item_completed" ? "正式消息" : `${outer}/${payload}`}
    </Badge>
  );
}
