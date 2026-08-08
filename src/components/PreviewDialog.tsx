import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
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
  Undo2,
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
import { Textarea } from "@/components/ui/textarea";
import {
  api,
  type DeletePlan,
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
  type RelatedSubagentSession,
} from "@/lib/sessionSource";
import { parseEmbeddedTranscriptPrompt, type EmbeddedTranscriptPrompt } from "@/lib/sessionText";
import {
  buildConversationPreviewRows,
  isAssistantTextToolUseEvent,
  isOpenCodeConversationEvent,
  isProcessGroupExpanded,
  isVisibleConversationEvent,
  summarizeProcessGroupExpansion,
  toConversationDisplayEvent,
  type ConversationPreviewRow,
} from "@/lib/conversationDisplay";
import { cn } from "@/lib/utils";
import { useSettings } from "@/stores/settings";
import { toast } from "sonner";

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
    Record<number, boolean>
  >({});
  const [forkTarget, setForkTarget] = useState<PreviewEvent | null>(null);
  const [forking, setForking] = useState(false);
  const [editTarget, setEditTarget] = useState<PreviewEvent | null>(null);
  const [editText, setEditText] = useState("");
  const [mutating, setMutating] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<PreviewEvent | null>(null);
  const [isSelecting, setIsSelecting] = useState(false);
  const [selectionFirstIndex, setSelectionFirstIndex] = useState<number | null>(null);
  const [selectionSecondIndex, setSelectionSecondIndex] = useState<number | null>(null);
  const [deleteSelectedTarget, setDeleteSelectedTarget] = useState<{ start: number; end: number } | null>(null);
  const [deletePlan, setDeletePlan] = useState<DeletePlan | null>(null);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [editHistory, setEditHistory] = useState<EditHistory | null>(null);
  const [prompts, setPrompts] = useState<UserPromptBrief[] | null>(null);
  const [totalEvents, setTotalEvents] = useState(0);
  const [activeTimelineIndex, setActiveTimelineIndex] = useState<number | null>(null);
  const [loadingAll, setLoadingAll] = useState(false);
  const offsetRef = useRef(0);
  const loadingRef = useRef(false);
  const doneRef = useRef(false);
  const viewportRef = useRef<HTMLDivElement | null>(null);
  const pendingJumpRef = useRef<number | null>(null);
  const scrollSpyRafRef = useRef(0);
  const preferenceSaveRef = useRef<Promise<void>>(Promise.resolve());
  const appSettings = useSettings((state) => state.settings);
  const canForkSession = provider === "codex" && !customRolloutPath && !!session && !!codexDir;
  // 备份/导入预览（customRolloutPath）不允许编辑，只能编辑真实会话文件
  const canMutateSession =
    !customRolloutPath && !!session && !!backupDir && !!rolloutPath;
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
    (key: number, expanded: boolean) => {
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

  /** 等待进行中的分页请求结束，避免并发拉取重复区间 */
  const waitForIdle = useCallback(async () => {
    while (loadingRef.current) {
      await new Promise((resolve) => setTimeout(resolve, 40));
    }
  }, []);

  /** 一次性把事件加载到指定事件序号（时间线跳转用），带一页余量 */
  const loadUpTo = useCallback(
    async (targetOffset: number) => {
      await waitForIdle();
      if (doneRef.current || offsetRef.current > targetOffset || !rolloutPath) return;
      loadingRef.current = true;
      setLoading(true);
      try {
        const need = targetOffset - offsetRef.current + 1 + PAGE;
        const next = await api.previewRange(provider, rolloutPath, offsetRef.current, need);
        if (next.length > 0) {
          offsetRef.current += next.length;
          setEvents((prev) => [...prev, ...next]);
        }
        if (next.length < need) {
          doneRef.current = true;
          setDone(true);
        }
      } finally {
        loadingRef.current = false;
        setLoading(false);
      }
    },
    [provider, rolloutPath, waitForIdle],
  );

  /** 一次加载余下全部事件 */
  const loadAll = useCallback(async () => {
    await waitForIdle();
    if (doneRef.current || !rolloutPath) return;
    loadingRef.current = true;
    setLoading(true);
    setLoadingAll(true);
    try {
      const next = await api.previewRange(
        provider,
        rolloutPath,
        offsetRef.current,
        Number.MAX_SAFE_INTEGER,
      );
      if (next.length > 0) {
        offsetRef.current += next.length;
        setEvents((prev) => [...prev, ...next]);
      }
      doneRef.current = true;
      setDone(true);
    } finally {
      loadingRef.current = false;
      setLoading(false);
      setLoadingAll(false);
    }
  }, [provider, rolloutPath, waitForIdle]);

  /** 拉取全量用户提问（时间线数据）；属于增强功能，失败时静默降级为无时间线 */
  const loadPrompts = useCallback(async () => {
    if (!rolloutPath) {
      setPrompts(null);
      setTotalEvents(0);
      return;
    }
    try {
      const list = await api.previewUserPrompts(provider, rolloutPath);
      setPrompts(list.prompts);
      setTotalEvents(list.total_events);
    } catch {
      setPrompts(null);
      setTotalEvents(0);
    }
  }, [provider, rolloutPath]);

  const resetAndReload = useCallback(() => {
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
    void loadMore();
    void loadPrompts();
  }, [loadMore, loadPrompts]);

  useEffect(() => {
    if (!open || !rolloutPath) return;
    setFilter("");
    setIsSelecting(false);
    setSelectionFirstIndex(null);
    setSelectionSecondIndex(null);
    setDeleteSelectedTarget(null);
    setDeletePlan(null);
    resetAndReload();
  }, [open, rolloutPath, resetAndReload]);

  useEffect(() => {
    if (!open || loading || done) return;
    const viewport = viewportRef.current;
    if (!viewport) return;
    // 全部/单轮收起、切换消息过滤后，内容高度可能骤减；重新补页直到填满视口。
    if (viewport.scrollHeight <= viewport.clientHeight + 20) {
      void loadMore();
    }
  }, [
    done,
    events.length,
    filter,
    loadMore,
    loading,
    onlyMsg,
    open,
    processDefaultCollapsed,
    processExpansionOverrides,
  ]);

  const timelineIndexSet = useMemo(
    () => (prompts === null ? null : new Set(prompts.map((prompt) => prompt.index))),
    [prompts],
  );

  const filtered = useMemo(() => {
    return events.filter((e) => {
      if (
        onlyMsg &&
        (!isConversationMessage(e)
          || !isVisibleConversationEvent(
            e,
            timelineIndexSet,
            initialJump?.eventIndex ?? null,
          ))
      ) {
        return false;
      }
      if (!filter) return true;
      const low = filter.toLowerCase();
      return (
        e.text_summary.toLowerCase().includes(low) ||
        e.kind.toLowerCase().includes(low) ||
        JSON.stringify(e.raw).toLowerCase().includes(low)
      );
    });
  }, [events, filter, initialJump?.eventIndex, onlyMsg, timelineIndexSet]);

  /**
   * 有 phase 时仅 final_answer 作为最终答复，commentary 折叠为过程。
   * 无 phase 时使用每轮最后一条 assistant 消息。搜索结果不折叠。
   */
  const rows = useMemo<ConversationPreviewRow[]>(() => {
    const displayEvents = onlyMsg ? filtered.map(toConversationDisplayEvent) : filtered;
    if (!onlyMsg || filter) {
      return displayEvents.map((event) => ({ type: "event", event }));
    }
    return buildConversationPreviewRows(displayEvents);
  }, [filtered, filter, onlyMsg]);

  const processRowKeys = useMemo(
    () => rows.flatMap((row) => (row.type === "process" ? [row.key] : [])),
    [rows],
  );
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
    anchors.forEach((node) => {
      if (node.getBoundingClientRect().top <= threshold) {
        current = Number(node.dataset.eventIndex);
      }
    });
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

  // 加载/过滤变化后：完成待跳转的定位，并刷新当前提问高亮
  useEffect(() => {
    scrollPendingIntoView();
    updateActiveFromScroll();
  }, [filtered, scrollPendingIntoView, updateActiveFromScroll]);

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
      const text = await api.copyResumeCommand(session.provider, session.id, session.cwd);
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

  const requestEditAt = (event: PreviewEvent) => {
    if (!canMutateSession) return;
    setEditText(editableText(event));
    setEditTarget(event);
  };

  const confirmEdit = async () => {
    if (!session || !backupDir || !rolloutPath || !editTarget) return;
    setMutating(true);
    try {
      const report = await api.editSessionEventText({
        provider,
        rollout_path: rolloutPath,
        session_id: session.id,
        backup_dir: backupDir,
        line_no: editTarget.index,
        new_text: editText,
      });
      toast.success(
        provider === "opencode"
          ? `已改写 OpenCode 消息（${report.changed_lines} 个内容块）`
          : `已改写消息（含镜像共 ${report.changed_lines} 行）`,
        {
          description: report.snapshot_created
            ? `编辑前已自动保存原始快照 ${report.snapshot_created}`
            : "本次编辑已记入编辑历史，可随时撤销",
        },
      );
      setEditTarget(null);
      resetAndReload();
      await onEdited?.();
    } catch (e: any) {
      toast.error("改写失败", { description: String(e?.message ?? e) });
    } finally {
      setMutating(false);
    }
  };

  const requestDeleteAt = (event: PreviewEvent) => {
    if (!canMutateSession || !rolloutPath) return;
    setDeletePlan(null);
    setDeleteTarget(event);
    api
      .planSessionEventDeletion(provider, rolloutPath, [event.index])
      .then(setDeletePlan)
      .catch((e: any) => {
        toast.error("生成删除计划失败", { description: String(e?.message ?? e) });
        setDeleteTarget(null);
      });
  };

  const confirmDelete = async () => {
    if (!session || !backupDir || !rolloutPath || !deleteTarget) return;
    setMutating(true);
    try {
      const report = await api.deleteSessionEvents({
        provider,
        rollout_path: rolloutPath,
        session_id: session.id,
        backup_dir: backupDir,
        line_nos: [deleteTarget.index],
      });
      toast.success(`已删除 ${report.deleted_lines} 个事件`, {
        description: report.snapshot_created
          ? `删除前已自动保存原始快照 ${report.snapshot_created}`
          : "本次删除已记入编辑历史，可随时撤销",
      });
      setDeleteTarget(null);
      setDeletePlan(null);
      resetAndReload();
      await onEdited?.();
    } catch (e: any) {
      toast.error("删除失败", { description: String(e?.message ?? e) });
    } finally {
      setMutating(false);
    }
  };

  const requestDeleteSelected = () => {
    if (!canMutateSession || !rolloutPath || selectionFirstIndex === null || selectionSecondIndex === null) return;
    const start = Math.min(selectionFirstIndex, selectionSecondIndex);
    const end = Math.max(selectionFirstIndex, selectionSecondIndex);
    setDeletePlan(null);
    setDeleteSelectedTarget({ start, end });
    const indices = events
      .filter((e) => e.index >= start && e.index <= end && canDeleteEvent(provider, e))
      .map((e) => e.index);
    api
      .planSessionEventDeletion(provider, rolloutPath, indices)
      .then(setDeletePlan)
      .catch((e: any) => {
        toast.error("生成删除计划失败", { description: String(e?.message ?? e) });
        setDeleteSelectedTarget(null);
      });
  };

  const confirmDeleteSelected = async () => {
    if (!session || !backupDir || !rolloutPath || !deleteSelectedTarget) return;
    setMutating(true);
    try {
      const { start, end } = deleteSelectedTarget;
      const indices = events
        .filter((e) => e.index >= start && e.index <= end && canDeleteEvent(provider, e))
        .map((e) => e.index);
      const report = await api.deleteSessionEvents({
        provider,
        rollout_path: rolloutPath,
        session_id: session.id,
        backup_dir: backupDir,
        line_nos: indices,
      });
      toast.success(`已删除 ${report.deleted_lines} 个事件`, {
        description: report.snapshot_created
          ? `删除前已自动保存原始快照 ${report.snapshot_created}`
          : "本次删除已记入编辑历史，可随时撤销",
      });
      setDeleteSelectedTarget(null);
      setDeletePlan(null);
      setIsSelecting(false);
      setSelectionFirstIndex(null);
      setSelectionSecondIndex(null);
      resetAndReload();
      await onEdited?.();
    } catch (e: any) {
      toast.error("删除失败", { description: String(e?.message ?? e) });
    } finally {
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
    setMutating(true);
    try {
      await api.undoLastSessionEdit({
        provider,
        rollout_path: rolloutPath,
        session_id: session.id,
        backup_dir: backupDir,
      });
      toast.success("已撤销最近一次编辑");
      await loadEditHistory();
      resetAndReload();
      await onEdited?.();
    } catch (e: any) {
      toast.error("撤销失败", { description: String(e?.message ?? e) });
    } finally {
      setMutating(false);
    }
  };

  const restoreSnapshot = async (name: string) => {
    if (!session || !backupDir || !rolloutPath) return;
    setMutating(true);
    try {
      const report = await api.restoreSessionEditSnapshot({
        provider,
        rollout_path: rolloutPath,
        session_id: session.id,
        backup_dir: backupDir,
        snapshot_name: name,
      });
      toast.success(`已还原快照 ${name}`, {
        description: report.snapshot_created
          ? `还原前状态已另存为 ${report.snapshot_created}`
          : undefined,
      });
      await loadEditHistory();
      resetAndReload();
      await onEdited?.();
    } catch (e: any) {
      toast.error("还原快照失败", { description: String(e?.message ?? e) });
    } finally {
      setMutating(false);
    }
  };

  const editActions: EditActions = {
    enabled: canMutateSession,
    pending: mutating,
    canEditText: (e) => canEditEventText(provider, e),
    canDelete: (e) => canDeleteEvent(provider, e),
    onEdit: requestEditAt,
    onDelete: requestDeleteAt,
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
                  <Dot />
                  <span className="text-[11px] text-muted-foreground">
                    显示 <span className="tabular-nums text-foreground/80">{filtered.length}</span>
                    <span className="mx-1 text-muted-foreground/50">/</span>
                    已加载 <span className="tabular-nums text-foreground/80">{events.length}</span>
                    {totalEvents > 0 && (
                      <>
                        <span className="mx-1 text-muted-foreground/50">/</span>
                        共 <span className="tabular-nums text-foreground/80">{totalEvents}</span>
                      </>
                    )}{" "}
                    条
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
              placeholder="在事件中过滤…"
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
                disabled={loadingAll}
                onClick={() => void loadAll()}
              >
                {loadingAll ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                ) : (
                  <ChevronsDown className="h-3.5 w-3.5" />
                )}
                {loadingAll ? "加载中…" : "加载全部"}
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
                          canDeleteEvent(provider, e),
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
              canOpenEditHistory={canMutateSession}
              onCopySessionId={copySessionId}
              onCopyResume={copyResume}
              onRevealDirectory={reveal}
              onOpenEditHistory={openEditHistory}
              onCopyPath={copyPath}
            />
          </div>
        </DialogHeader>

        <div className="relative min-h-0 flex-1">
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
                    {events.length === 0 ? "无事件" : "无匹配事件"}
                  </div>
                </div>
              )}

              {rows.map((row) =>
                row.type === "process" ? (
                  <ProcessTurnGroup
                    key={`process-${row.key}`}
                    events={row.events}
                    expanded={isProcessGroupExpanded(
                      row.key,
                      processDefaultCollapsed,
                      processExpansionOverrides,
                    )}
                    onExpandedChange={(expanded) =>
                      changeProcessGroupExpanded(row.key, expanded)
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
                        key={event.index}
                        data-event-index={event.index}
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
                              enabled: canForkSession && isStableForkNode(event),
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
                    key={row.event.index}
                    data-event-index={row.event.index}
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
                          enabled: canForkSession && isStableForkNode(row.event),
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

    {/* 编辑消息文本 */}
    <Dialog open={!!editTarget} onOpenChange={(v) => !v && !mutating && setEditTarget(null)}>
      <DialogContent className="sm:max-w-[640px]">
        <DialogHeader>
          <DialogTitle>改写消息文本</DialogTitle>
          <DialogDescription className="sr-only">
            修改当前会话事件中的可编辑文本。
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          <div className="rounded-md border bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
            <span className="font-mono">line {editTarget ? editTarget.index + 1 : ""}</span>
            <span className="mx-2 text-muted-foreground/50">·</span>
            {provider === "opencode" ? (
              <>
                只更新 opencode.db 中当前会话的 text 内容块（会话 ID 与时间戳不变，可直接续聊）；
                推理、工具调用及其他会话保持原样。编辑前会保存会话级快照，可在「编辑历史」中撤销或还原。
              </>
            ) : (
              <>
                会话文件会原地改写（会话 ID 不变，可直接 resume 续聊）；Codex 镜像行会同步更新，
                思考/推理与工具块保持原样。编辑前会自动保存原始快照，可在「编辑历史」中撤销或还原。
              </>
            )}
          </div>
          <Textarea
            value={editText}
            onChange={(e) => setEditText(e.target.value)}
            rows={10}
            className="max-h-[50vh] font-mono text-sm"
            placeholder="消息文本"
          />
        </div>
        <div className="flex justify-end gap-2">
          <Button variant="outline" disabled={mutating} onClick={() => setEditTarget(null)}>
            取消
          </Button>
          <Button disabled={mutating || !editText.trim()} onClick={() => void confirmEdit()}>
            {mutating ? "保存中…" : "保存改写"}
          </Button>
        </div>
      </DialogContent>
    </Dialog>

    {/* 删除事件（展示级联计划） */}
    <AlertDialog
      open={!!deleteTarget}
      onOpenChange={(v) => {
        if (!v && !mutating) {
          setDeleteTarget(null);
          setDeletePlan(null);
        }
      }}
    >
      <AlertDialogContent className="sm:max-w-[640px]">
        <AlertDialogHeader>
          <AlertDialogTitle>删除会话事件</AlertDialogTitle>
          <AlertDialogDescription>
            {provider === "opencode" ? (
              <>
                OpenCode 会按同轮消息安全删除：选择用户消息会同时删除本轮完整响应；选择推理、工具或回答时，
                会删除该轮完整 assistant 响应链并保留用户提问。只快照当前会话，不会覆盖整个数据库。
              </>
            ) : (
              <>
                为保证续聊不报错，配对的工具调用/返回、镜像行与关联推理会一起删除。
                删除前会自动保存原始快照，可在「编辑历史」中撤销或还原。
              </>
            )}
          </AlertDialogDescription>
        </AlertDialogHeader>
        {!deletePlan && (
          <div className="py-2 text-center text-xs text-muted-foreground">正在生成删除计划…</div>
        )}
        {deletePlan && deletePlan.blocked.length > 0 && (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
            {deletePlan.blocked.map((b, i) => (
              <div key={i}>{b}</div>
            ))}
          </div>
        )}
        {deletePlan && deletePlan.blocked.length === 0 && (
          <ScrollArea className="rounded-md border bg-muted/40" viewportClassName="max-h-72">
            <div className="space-y-1.5 p-2 pr-3">
              {deletePlan.lines.map((l) => (
                <div key={l.line_no} className="flex items-start gap-2 text-xs leading-[1.45]">
                  <span className="w-16 shrink-0 select-none text-right font-mono text-[11px] tabular-nums text-muted-foreground">
                    line {l.line_no + 1}
                  </span>
                  <Badge
                    variant={l.reason === "selected" ? "default" : "outline"}
                    className="mt-px h-4 shrink-0 px-1 py-0 text-[10px] font-normal"
                  >
                    {deleteReasonLabel(l.reason)}
                  </Badge>
                  <span className="shrink-0 text-muted-foreground">{l.role}</span>
                  <span className="min-w-0 flex-1 wrap-anywhere">{l.summary}</span>
                </div>
              ))}
            </div>
          </ScrollArea>
        )}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={mutating}>取消</AlertDialogCancel>
          <AlertDialogAction
            disabled={mutating || !deletePlan || deletePlan.blocked.length > 0}
            className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
            onClick={(e) => {
              e.preventDefault();
              void confirmDelete();
            }}
          >
            {mutating
              ? "删除中…"
              : `删除 ${deletePlan?.lines.length ?? 0} 个事件`}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>

    {/* 删除选中事件 */}
    <AlertDialog
      open={!!deleteSelectedTarget}
      onOpenChange={(v) => {
        if (!v && !mutating) {
          setDeleteSelectedTarget(null);
          setDeletePlan(null);
        }
      }}
    >
      <AlertDialogContent className="sm:max-w-[640px]">
        <AlertDialogHeader>
          <AlertDialogTitle>删除选中事件</AlertDialogTitle>
          <AlertDialogDescription>
            {provider === "opencode" ? (
              <>
                将删除选取范围内的事件（含首尾），并按同轮消息补全安全删除范围；
                用户消息会连同本轮响应删除，assistant 过程或回答会删除本轮完整响应链。
                删除前会保存当前会话快照，不影响数据库中的其他会话。
              </>
            ) : (
              <>
                将删除选取范围内的事件（含首尾）。为保证续聊不报错，配对的工具调用/返回、镜像行与关联推理会一起删除。
                删除前会自动保存原始快照，可在「编辑历史」中撤销或还原。
              </>
            )}
          </AlertDialogDescription>
        </AlertDialogHeader>
        {!deletePlan && (
          <div className="py-2 text-center text-xs text-muted-foreground">正在生成删除计划…</div>
        )}
        {deletePlan && deletePlan.blocked.length > 0 && (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
            {deletePlan.blocked.map((b, i) => (
              <div key={i}>{b}</div>
            ))}
          </div>
        )}
        {deletePlan && deletePlan.blocked.length === 0 && (
          <ScrollArea className="rounded-md border bg-muted/40" viewportClassName="max-h-72">
            <div className="space-y-1.5 p-2 pr-3">
              <div className="mb-2 text-[11px] font-medium text-muted-foreground">
                共 {deletePlan.lines.length} 个事件将被删除
              </div>
              {deletePlan.lines.map((l) => (
                <div key={l.line_no} className="flex items-start gap-2 text-xs leading-[1.45]">
                  <span className="w-16 shrink-0 select-none text-right font-mono text-[11px] tabular-nums text-muted-foreground">
                    line {l.line_no + 1}
                  </span>
                  <Badge
                    variant={l.reason === "selected" ? "default" : "outline"}
                    className="mt-px h-4 shrink-0 px-1 py-0 text-[10px] font-normal"
                  >
                    {deleteReasonLabel(l.reason)}
                  </Badge>
                  <span className="shrink-0 text-muted-foreground">{l.role}</span>
                  <span className="min-w-0 flex-1 wrap-anywhere">{l.summary}</span>
                </div>
              ))}
            </div>
          </ScrollArea>
        )}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={mutating}>取消</AlertDialogCancel>
          <AlertDialogAction
            disabled={mutating || !deletePlan || deletePlan.blocked.length > 0}
            className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
            onClick={(e) => {
              e.preventDefault();
              void confirmDeleteSelected();
            }}
          >
            {mutating
              ? "删除中…"
              : `删除选中 ${deletePlan?.lines.length ?? 0} 个事件`}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>

    {/* 编辑历史：撤销 / 快照还原 */}
    <Dialog open={historyOpen} onOpenChange={(v) => !mutating && setHistoryOpen(v)}>
      <DialogContent className="sm:max-w-[640px]">
        <DialogHeader>
          <DialogTitle>编辑历史</DialogTitle>
          <DialogDescription className="sr-only">
            查看、撤销或还原当前会话的编辑记录。
          </DialogDescription>
        </DialogHeader>
        {!editHistory ? (
          <div className="py-6 text-center text-xs text-muted-foreground">加载中…</div>
        ) : (
          <div className="space-y-4">
            <div className="flex items-center justify-between">
              <div className="text-xs text-muted-foreground">
                {editHistory.entries.length > 0
                  ? `共 ${editHistory.entries.length} 次操作`
                  : "该会话还没有编辑记录"}
              </div>
              <Button
                size="sm"
                variant="outline"
                className="h-8 gap-1.5"
                disabled={mutating || !editHistory.undo_available}
                title={editHistory.undo_blocked_reason ?? undefined}
                onClick={() => void undoLastEdit()}
              >
                <Undo2 className="h-3.5 w-3.5" />
                撤销最近一次
              </Button>
            </div>
            {editHistory.undo_blocked_reason && (
              <div className="rounded-md border bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
                {editHistory.undo_blocked_reason}
              </div>
            )}
            {editHistory.entries.length > 0 && (
              <div className="max-h-48 space-y-1 overflow-auto rounded-md border bg-muted/30 p-2">
                {editHistory.entries.map((entry) => (
                  <div key={entry.op_id} className="flex items-center gap-2 text-xs">
                    <span className="shrink-0 font-mono text-muted-foreground">
                      {formatTimeString(entry.ts)}
                    </span>
                    <Badge variant="outline" className="h-4 shrink-0 px-1 py-0 text-[10px] font-normal">
                      {editKindLabel(entry.kind)}
                    </Badge>
                    <span className="min-w-0 flex-1 truncate">{entry.description}</span>
                  </div>
                ))}
              </div>
            )}
            <div>
              <div className="mb-1.5 text-xs font-medium">原始快照</div>
              {editHistory.snapshots.length === 0 ? (
                <div className="text-xs text-muted-foreground">
                  暂无快照（首次改写或删除时会自动创建）
                </div>
              ) : (
                <div className="max-h-40 space-y-1 overflow-auto rounded-md border bg-muted/30 p-2">
                  {editHistory.snapshots.map((snap) => (
                    <div key={snap.name} className="flex items-center gap-2 text-xs">
                      <span className="min-w-0 flex-1 truncate font-mono">{snap.name}</span>
                      <span className="shrink-0 text-muted-foreground">
                        {formatTimeString(snap.created_at)}
                      </span>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-6 shrink-0 px-2 text-xs"
                        disabled={mutating}
                        onClick={() => void restoreSnapshot(snap.name)}
                      >
                        还原
                      </Button>
                    </div>
                  ))}
                </div>
              )}
            </div>
          </div>
        )}
      </DialogContent>
    </Dialog>
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
    <div className="space-y-4">
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
          {message.markdown ? <ReactMarkdown remarkPlugins={[remarkGfm]}>{message.markdown}</ReactMarkdown> : message.images.length === 0 ? (
            <span className="italic opacity-70">(空消息)</span>
          ) : null}
          <LocalImageAttachments images={message.images} />
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
          回溯
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
          if (x?.type === "tool_use") {
            return e.role === "assistant" ? "" : `[Tool: ${x.name ?? "unknown"}]`;
          }
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
  if (e.role === "subagent") return false;
  if (isInternalCodexContextMessage(e)) return false;
  if (isAssistantTextToolUseEvent(e)) return true;
  const raw = e.raw as {
    message?: { role?: unknown };
    opencode?: unknown;
  } | null;
  if (raw?.opencode) return isOpenCodeConversationEvent(e);
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

function subagentEventLabel(e: PreviewEvent): string {
  if (e.kind === "sub_agent_activity") {
    const raw = e.raw as { payload?: { kind?: unknown } } | null;
    switch (raw?.payload?.kind) {
      case "started":
        return "子智能体开始工作";
      case "interacted":
        return "子智能体有新活动";
      case "interrupted":
        return "子智能体已中断";
      case "completed":
        return "子智能体已完成";
      default:
        return "子智能体活动";
    }
  }

  switch (e.kind) {
    case "spawn_agent":
      return "启动子智能体";
    case "spawn_agent_result":
      return "启动子智能体结果";
    case "list_agents":
      return "查看子智能体";
    case "list_agents_result":
      return "子智能体列表结果";
    case "send_message":
      return "发送子智能体消息";
    case "send_message_result":
      return "发送消息结果";
    case "followup_task":
      return "安排后续任务";
    case "followup_task_result":
      return "后续任务结果";
    case "interrupt_agent":
      return "中断子智能体";
    case "interrupt_agent_result":
      return "中断操作结果";
    case "wait_agent":
      return "等待子智能体";
    case "wait_agent_result":
      return "等待结果";
    default:
      return "子智能体事件";
  }
}

function subagentEventTime(e: PreviewEvent, fallback: string): string {
  if (e.kind !== "sub_agent_activity") return fallback;
  const raw = e.raw as { payload?: { occurred_at_ms?: unknown } } | null;
  const occurredAtMs = raw?.payload?.occurred_at_ms;
  if (typeof occurredAtMs !== "number" || !Number.isFinite(occurredAtMs)) return fallback;
  return formatTimeString(new Date(occurredAtMs).toISOString());
}

function isInternalCodexContextMessage(e: PreviewEvent): boolean {
  if (e.role !== "user") return false;
  const text = extractText(e).trim();
  if (!text) return false;
  const firstLine = normalizePromptHeading(text.split(/\r?\n/, 1)[0] ?? "");
  if (firstLine.startsWith("AGENTS.md instructions") && text.includes("<INSTRUCTIONS>")) {
    return true;
  }
  if (firstLine === "<environment_context>" && text.includes("</environment_context>")) {
    return true;
  }
  if (firstLine === "<recommended_plugins>" && text.includes("</recommended_plugins>")) {
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

/* ---------- 编辑 / 删除能力判断（与后端 edit.rs 规则对应）---------- */

const CODEX_DELETABLE_RESPONSE_ITEMS = new Set([
  "message",
  "reasoning",
  "function_call",
  "custom_tool_call",
  "local_shell_call",
  "web_search_call",
  "function_call_output",
  "custom_tool_call_output",
]);

function canEditEventText(provider: string, e: PreviewEvent): boolean {
  if (provider === "codex") {
    const outer = rawType(e);
    const pt = payloadType(e);
    if (outer === "event_msg") return pt === "user_message" || pt === "agent_message";
    if (outer === "response_item" && pt === "message") {
      return editableText(e).length > 0;
    }
    return false;
  }
  if (provider === "opencode") {
    const raw = e.raw as any;
    return (
      typeof raw?.opencode?.part_id === "string" &&
      raw?.opencode?.part_type === "text" &&
      (raw?.message?.role === "user" || raw?.message?.role === "assistant") &&
      editableText(e).length > 0
    );
  }
  // Claude：user/assistant 消息且含文本块（thinking 带签名、工具块结构化，均不可改写）
  const raw = e.raw as any;
  if (!raw?.message || (raw?.type !== "user" && raw?.type !== "assistant")) return false;
  return editableText(e).length > 0;
}

function canDeleteEvent(provider: string, e: PreviewEvent): boolean {
  if (provider === "codex") {
    const outer = rawType(e);
    const pt = payloadType(e);
    if (outer === "event_msg") return pt === "user_message" || pt === "agent_message";
    if (outer === "response_item") return CODEX_DELETABLE_RESPONSE_ITEMS.has(pt);
    return false;
  }
  if (provider === "opencode") {
    const raw = e.raw as any;
    return (
      typeof raw?.opencode?.part_id === "string" &&
      typeof raw?.opencode?.message_id === "string" &&
      (raw?.message?.role === "user" || raw?.message?.role === "assistant")
    );
  }
  const raw = e.raw as any;
  return (
    !!raw?.message &&
    typeof raw?.uuid === "string" &&
    (raw?.type === "user" || raw?.type === "assistant")
  );
}

/** 后端改写的目标文本：与 edit.rs 的 replace_text_items 语义一致（text 项按换行拼接） */
function editableText(e: PreviewEvent): string {
  const r = e.raw as any;
  if (!r) return "";
  if (r.payload) {
    // Codex
    if (typeof r.payload.message === "string") return r.payload.message;
    const c = r.payload.content;
    if (typeof c === "string") return c;
    if (Array.isArray(c)) {
      return c
        .filter((x: any) => typeof x?.text === "string")
        .map((x: any) => x.text)
        .join("\n");
    }
    return "";
  }
  // Claude
  const c = r?.message?.content;
  if (typeof c === "string") return c;
  if (Array.isArray(c)) {
    return c
      .filter((x: any) => x?.type === "text" && typeof x.text === "string")
      .map((x: any) => x.text)
      .join("\n");
  }
  return "";
}

function deleteReasonLabel(reason: string): string {
  switch (reason) {
    case "selected":
      return "选中";
    case "tool_pair":
      return "工具配对";
    case "mirror":
      return "镜像行";
    case "reasoning_attached":
      return "关联推理";
    case "context_message":
      return "同轮消息";
    default:
      return reason;
  }
}

function editKindLabel(kind: string): string {
  switch (kind) {
    case "edit_text":
      return "改写";
    case "delete_events":
      return "删除";
    case "undo":
      return "撤销";
    case "restore_snapshot":
      return "还原";
    default:
      return kind;
  }
}
