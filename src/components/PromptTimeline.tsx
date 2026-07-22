import {
  useCallback,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type PointerEvent as ReactPointerEvent,
} from "react";
import { Bot, ListOrdered, User, X } from "lucide-react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";

import type { UserPromptBrief } from "@/lib/api";
import { formatTimeString } from "@/lib/format";
import { cn } from "@/lib/utils";

type Props = {
  prompts: UserPromptBrief[];
  /** 当前滚动位置对应的用户提问，用于刻度和列表高亮。 */
  activeIndex: number | null;
  onJump: (prompt: UserPromptBrief) => void;
};

type Marker = {
  prompt: UserPromptBrief;
  ordinal: number;
  listIndex: number;
};

const CARD_FALLBACK_HEIGHT = 132;
const MARKER_ROW_HEIGHT = 10;
const MARKER_WIDTH = 26;
const MARKER_BASE_SCALE = 0.2308;
const MARKER_PROGRESS_SCALE = 0.7692;
const NEIGHBOR_PROGRESS = [1, 0.7, 0.4, 0.2] as const;
const MARKER_EASING =
  "linear(0, .398 10%, .682 20%, .843 30%, .925 40%, .972 50%, 1.004 60%, 1.008 70%, 1.003 80%, 1)";

/**
 * 时间线处在可点击卡片/列表项内部，不能直接嵌套 Markdown 的链接和块级节点。
 * 这里保留强调、删除线和行内代码等语义，同时把链接压缩为标签文本、把块级
 * 内容压平为行内摘要，避免 `[名称](本地长路径)` 在问题列表中完整展开。
 */
const timelineMarkdownComponents: Components = {
  p: ({ children }) => <>{children} </>,
  h1: ({ children }) => <><strong>{children}</strong> </>,
  h2: ({ children }) => <><strong>{children}</strong> </>,
  h3: ({ children }) => <><strong>{children}</strong> </>,
  h4: ({ children }) => <><strong>{children}</strong> </>,
  h5: ({ children }) => <><strong>{children}</strong> </>,
  h6: ({ children }) => <><strong>{children}</strong> </>,
  a: ({ children, href }) => (
    <span
      title={href}
      className="font-medium underline decoration-current/35 underline-offset-2"
    >
      {children}
    </span>
  ),
  code: ({ children }) => (
    <code className="rounded bg-muted px-1 py-px font-mono text-[0.92em] text-foreground/85">
      {children}
    </code>
  ),
  pre: ({ children }) => <>{children} </>,
  blockquote: ({ children }) => <><span className="opacity-65">“</span>{children}<span className="opacity-65">”</span> </>,
  ul: ({ children }) => <>{children}</>,
  ol: ({ children }) => <>{children}</>,
  li: ({ children }) => <>• {children} </>,
  table: ({ children }) => <>{children}</>,
  thead: ({ children }) => <>{children}</>,
  tbody: ({ children }) => <>{children}</>,
  tr: ({ children }) => <>{children} </>,
  th: ({ children }) => <><strong>{children}</strong><span className="opacity-50"> · </span></>,
  td: ({ children }) => <>{children}<span className="opacity-50"> · </span></>,
  br: () => <> </>,
  hr: () => <span className="opacity-50"> · </span>,
  img: ({ alt }) => (
    <span className="font-normal text-muted-foreground">
      {alt ? `[图片：${alt}]` : "[图片]"}
    </span>
  ),
  input: ({ checked }) => <span aria-hidden="true">{checked ? "☑ " : "☐ "}</span>,
};

/**
 * 对话预览左侧用户提问导航。
 *
 * 与 Codex App 一致，刻度按提问顺序组成固定 10px 间距的紧凑消息簇，
 * 不再用工具调用、推理等原始事件数量拉伸距离。悬停时当前刻度与前后三个
 * 相邻刻度按 1 / 0.7 / 0.4 / 0.2 渐进放大。
 */
export function PromptTimeline({ prompts, activeIndex, onJump }: Props) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const railListRef = useRef<HTMLDivElement | null>(null);
  const cardRef = useRef<HTMLButtonElement | null>(null);
  const panelListRef = useRef<HTMLDivElement | null>(null);
  const [containerHeight, setContainerHeight] = useState(0);
  const [cardHeight, setCardHeight] = useState(CARD_FALLBACK_HEIGHT);
  const [hoverAnchorY, setHoverAnchorY] = useState(0);
  const [hovered, setHovered] = useState<Marker | null>(null);
  const [listOpen, setListOpen] = useState(false);

  useLayoutEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const update = () => setContainerHeight(el.clientHeight);
    update();
    const observer = new ResizeObserver(update);
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  useLayoutEffect(() => {
    const el = cardRef.current;
    if (!el || !hovered) return;
    const update = () => setCardHeight(el.offsetHeight || CARD_FALLBACK_HEIGHT);
    update();
    const observer = new ResizeObserver(update);
    observer.observe(el);
    return () => observer.disconnect();
  }, [hovered]);

  const markers = useMemo<Marker[]>(
    () => prompts.map((prompt, listIndex) => ({
      prompt,
      ordinal: listIndex + 1,
      listIndex,
    })),
    [prompts],
  );

  // 左侧紧凑刻度和展开列表都自动保持当前提问可见。
  useLayoutEffect(() => {
    if (activeIndex === null) return;
    revealActiveRow(railListRef.current, activeIndex);
    if (listOpen) revealActiveRow(panelListRef.current, activeIndex);
  }, [activeIndex, listOpen, prompts.length]);

  const updateAnchor = useCallback((button: HTMLElement) => {
    const container = containerRef.current;
    if (!container) return;
    const containerRect = container.getBoundingClientRect();
    const buttonRect = button.getBoundingClientRect();
    setHoverAnchorY(buttonRect.top - containerRect.top + buttonRect.height / 2);
  }, []);

  const selectMarker = useCallback(
    (marker: Marker, button: HTMLElement | null) => {
      setHovered(marker);
      if (button) updateAnchor(button);
    },
    [updateAnchor],
  );

  const clearScrub = useCallback(() => setHovered(null), []);

  const scrubRail = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      if (listOpen || markers.length === 0) return;
      const rail = event.currentTarget;
      const rect = rail.getBoundingClientRect();
      const localY = event.clientY - rect.top + rail.scrollTop;
      const listIndex = clamp(Math.floor(localY / MARKER_ROW_HEIGHT), 0, markers.length - 1);
      const marker = markers[listIndex];
      const button = rail.querySelector<HTMLElement>(`[data-marker-index="${listIndex}"]`);
      selectMarker(marker, button);
    },
    [listOpen, markers, selectMarker],
  );

  const refreshHoveredAnchor = useCallback(() => {
    if (!hovered) return;
    const button = railListRef.current?.querySelector<HTMLElement>(
      `[data-marker-index="${hovered.listIndex}"]`,
    );
    if (button) updateAnchor(button);
  }, [hovered, updateAnchor]);

  if (prompts.length === 0) return null;

  const hoverCardTop = clamp(
    hoverAnchorY - 22,
    4,
    Math.max(containerHeight - cardHeight - 4, 4),
  );

  return (
    <div
      ref={containerRef}
      className="pointer-events-none absolute inset-y-0 left-0 z-20 hidden w-9 sm:block"
      onMouseLeave={clearScrub}
    >
      <button
        type="button"
        aria-label={listOpen ? "收起对话时间线" : "展开对话时间线"}
        aria-expanded={listOpen}
        title={listOpen ? "收起对话时间线" : `对话时间线（${prompts.length} 条提问）`}
        onClick={() => {
          clearScrub();
          setListOpen((value) => !value);
        }}
        className={cn(
          "pointer-events-auto absolute left-2 top-3 z-30 flex h-6 w-6 items-center justify-center rounded-md border shadow-sm transition-colors",
          "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-1",
          listOpen
            ? "border-border bg-accent text-foreground"
            : "border-border/70 bg-background/95 text-muted-foreground hover:bg-accent hover:text-foreground",
        )}
      >
        <ListOrdered className="h-3.5 w-3.5" />
      </button>

      <div
        ref={railListRef}
        aria-label="用户提问"
        className="pointer-events-auto absolute left-0 top-1/2 flex max-h-[min(70vh,40rem)] w-9 -translate-y-1/2 flex-col overflow-y-auto overscroll-contain [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
        onPointerMove={scrubRail}
        onScroll={refreshHoveredAnchor}
      >
        {markers.map((marker) => {
          const isActive = marker.prompt.index === activeIndex;
          const distance = hovered
            ? Math.abs(marker.listIndex - hovered.listIndex)
            : Number.POSITIVE_INFINITY;
          const progress = distance < NEIGHBOR_PROGRESS.length
            ? NEIGHBOR_PROGRESS[distance]
            : 0;
          const scale = MARKER_BASE_SCALE + MARKER_PROGRESS_SCALE * progress;
          const isHovered = distance === 0;

          return (
            <button
              key={marker.prompt.index}
              type="button"
              data-marker-index={marker.listIndex}
              data-active={isActive || undefined}
              aria-current={isActive || undefined}
              aria-label={`第 ${marker.ordinal} 条用户提问`}
              className="group flex h-2.5 w-9 shrink-0 cursor-pointer items-center pl-[7px] outline-none"
              onMouseEnter={(event) => selectMarker(marker, event.currentTarget)}
              onFocus={(event) => selectMarker(marker, event.currentTarget)}
              onClick={(event) => {
                event.stopPropagation();
                onJump(marker.prompt);
              }}
            >
              <span className="flex h-0.5 w-[26px] shrink-0 items-center">
                <span
                  aria-hidden="true"
                  className={cn(
                    "block h-0.5 shrink-0 origin-left rounded-full",
                    isHovered
                      ? "bg-foreground opacity-100"
                      : isActive
                        ? "bg-foreground opacity-60"
                        : "bg-muted-foreground opacity-40",
                    "group-focus-visible:bg-foreground group-focus-visible:opacity-100",
                  )}
                  style={{
                    width: MARKER_WIDTH,
                    transform: `scaleX(${scale})`,
                    transition: `transform 160ms ${MARKER_EASING}, background-color 120ms ease, opacity 120ms ease`,
                  }}
                />
              </span>
            </button>
          );
        })}
      </div>

      {hovered && !listOpen && (
        <button
          ref={cardRef}
          type="button"
          className={cn(
            "pointer-events-auto absolute left-9 z-20 w-[21rem] max-w-[calc(100vw-5rem)] rounded-xl border border-border/80 bg-popover/[0.96] p-0 text-left",
            "shadow-[0_12px_34px_-16px_hsl(var(--foreground)/0.38)] backdrop-blur-md",
            "transition-[top,box-shadow] duration-300",
            "animate-in fade-in zoom-in-[0.985] duration-150 motion-reduce:animate-none motion-reduce:transition-none",
            "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2",
          )}
          style={{
            top: hoverCardTop,
            transitionTimingFunction: "cubic-bezier(0.22, 1, 0.36, 1)",
          }}
          onClick={() => onJump(hovered.prompt)}
        >
          <span
            aria-hidden="true"
            className="absolute -left-1.5 top-5 h-3 w-3 rotate-45 border-b border-l border-border/80 bg-popover"
          />
          <div
            key={hovered.prompt.index}
            className="relative overflow-hidden rounded-xl px-3.5 py-3 animate-in fade-in slide-in-from-bottom-1 duration-200 motion-reduce:animate-none"
          >
            <div className="flex gap-2 px-2 py-1">
              <User className="mt-0.5 h-3.5 w-3.5 shrink-0 text-foreground/60" />
              <TimelineMarkdownExcerpt
                text={messageText(hovered.prompt)}
                className="line-clamp-2 min-w-0 text-[12px] font-medium leading-[1.45] text-popover-foreground"
              />
            </div>

            {hovered.prompt.response && (
              <div className="mt-1 flex gap-2 border-t border-border/45 px-2 pt-2">
                <Bot className="mt-0.5 h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                <TimelineMarkdownExcerpt
                  text={messageText(hovered.prompt.response)}
                  className="line-clamp-3 min-w-0 text-[11px] leading-[1.5] text-muted-foreground"
                />
              </div>
            )}

            <div className="mt-2 flex items-center gap-2 border-t border-border/45 px-2 pt-2 text-[10px] tabular-nums text-muted-foreground/80">
              <span>#{hovered.ordinal}/{prompts.length}</span>
              {hovered.prompt.timestamp && (
                <span>{formatTimeString(hovered.prompt.timestamp)}</span>
              )}
              <span className="ml-auto">用户提问</span>
            </div>
          </div>
        </button>
      )}

      {listOpen && (
        <div className="pointer-events-auto absolute bottom-4 left-2 top-3 z-10 flex w-[22rem] max-w-[74vw] flex-col overflow-hidden rounded-xl border border-border/80 bg-popover/[0.98] shadow-xl backdrop-blur-md animate-in fade-in slide-in-from-left-1 duration-150 motion-reduce:animate-none">
          <div className="flex shrink-0 items-center gap-2 border-b border-border/60 py-2 pl-9 pr-2">
            <span className="text-xs font-medium">对话时间线</span>
            <span className="text-[11px] tabular-nums text-muted-foreground">
              {prompts.length} 条提问
            </span>
            <button
              type="button"
              aria-label="关闭对话时间线"
              onClick={() => setListOpen(false)}
              className="ml-auto flex h-6 w-6 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
            >
              <X className="h-3.5 w-3.5" />
            </button>
          </div>
          <div ref={panelListRef} className="thin-scrollbar min-h-0 flex-1 overflow-y-auto">
            {prompts.map((prompt, index) => {
              const promptActive = prompt.index === activeIndex;
              return (
                <div key={prompt.index} className="border-b border-border/40 last:border-b-0">
                  <div className="px-3 pb-1 pt-2 text-[10px] tabular-nums text-muted-foreground/75">
                    <span className={cn(promptActive && "font-semibold text-foreground")}>#{index + 1}</span>
                    {prompt.timestamp && (
                      <span className="ml-2">{formatTimeString(prompt.timestamp)}</span>
                    )}
                  </div>
                  <button
                    type="button"
                    data-active={promptActive || undefined}
                    onClick={() => onJump(prompt)}
                    className={cn(
                      "flex w-full gap-2 px-3 pb-2 pt-1.5 text-left transition-colors hover:bg-accent/60 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring",
                      promptActive && "bg-accent",
                    )}
                  >
                    <User className="mt-0.5 h-3.5 w-3.5 shrink-0 text-foreground/60" />
                    <TimelineMarkdownExcerpt
                      text={messageText(prompt)}
                      className="line-clamp-2 min-w-0 text-xs font-medium leading-relaxed text-popover-foreground"
                    />
                  </button>
                </div>
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}

function revealActiveRow(container: HTMLElement | null, activeIndex: number) {
  if (!container) return;
  const active = container.querySelector<HTMLElement>(`[data-active="true"]`);
  if (!active) return;
  if (active.offsetTop < container.scrollTop) {
    container.scrollTop = active.offsetTop;
  } else if (active.offsetTop + active.offsetHeight > container.scrollTop + container.clientHeight) {
    container.scrollTop = active.offsetTop + active.offsetHeight - container.clientHeight;
  }
}

function messageText(message: { text: string }): string {
  const text = message.text.trim();
  return text.replace(/^\s{0,3}#{1,6}\s+/, "") || "(无文本内容)";
}

function TimelineMarkdownExcerpt({ text, className }: { text: string; className?: string }) {
  return (
    <span className={cn("block max-w-full overflow-hidden break-words", className)}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={timelineMarkdownComponents}
        skipHtml
      >
        {text}
      </ReactMarkdown>
    </span>
  );
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), max);
}
