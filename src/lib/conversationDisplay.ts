import type { PreviewEvent } from "./api";

export type ConversationPreviewRow =
  | { type: "event"; event: PreviewEvent }
  | {
      type: "process";
      key: number;
      events: PreviewEvent[];
      hasFinalResponse: boolean;
    };

export type ProcessGroupExpansionState = "collapsed" | "expanded" | "mixed";

/** Claude 的一条 assistant 记录可同时包含文字和 tool_use；对话视图需要保留文字。 */
export function isAssistantTextToolUseEvent(event: PreviewEvent): boolean {
  if (event.role !== "tool_call") return false;
  const raw = event.raw as {
    message?: {
      role?: unknown;
      content?: unknown;
    };
  } | null;
  if (raw?.message?.role !== "assistant" || !Array.isArray(raw.message.content)) {
    return false;
  }

  let hasText = false;
  let hasToolUse = false;
  for (const block of raw.message.content) {
    if (!block || typeof block !== "object") continue;
    const item = block as { type?: unknown; text?: unknown };
    if (item.type === "tool_use") {
      hasToolUse = true;
    } else if (
      item.type === "text" &&
      typeof item.text === "string" &&
      item.text.trim().length > 0
    ) {
      hasText = true;
    }
  }
  return hasText && hasToolUse;
}

/** 对话视图按 assistant 展示，raw 和索引不变；完整事件视图仍按 tool_call 展示。 */
export function toConversationDisplayEvent(event: PreviewEvent): PreviewEvent {
  if (event.role !== "tool_call" || !isAssistantTextToolUseEvent(event)) {
    return event;
  }
  return { ...event, role: "assistant" };
}

/**
 * 与 Codex 的对话视图保持一致：只保留用户消息、assistant 文本和真实推理；
 * tool part 即使继承了父 assistant 消息的 commentary phase，也仍是工具事件。
 */
export function isOpenCodeConversationEvent(event: PreviewEvent): boolean {
  const raw = event.raw as {
    message?: { role?: unknown };
    opencode?: unknown;
  } | null;
  if (!raw?.opencode) return false;
  if (raw.message?.role === "user") return event.role === "user";
  if (raw.message?.role !== "assistant") return false;
  return event.role === "assistant" || event.role === "reasoning";
}

/**
 * Codex 会明确标记 assistant 消息是过程播报（commentary）还是最终答复
 * （final_answer）。旧版 Codex 与 Claude 没有 phase，此时退回到“一轮最后一条”。
 */
export function buildConversationPreviewRows(
  events: readonly PreviewEvent[],
): ConversationPreviewRow[] {
  const rows: ConversationPreviewRow[] = [];
  let assistantRun: PreviewEvent[] = [];
  let openCodeRun: PreviewEvent[] = [];
  let openCodeTurnKey: string | null = null;

  const flushAssistantRun = () => {
    if (assistantRun.length === 0) return;

    const hasExplicitPhase = assistantRun.some(
      (event) => assistantMessagePhase(event) !== null,
    );
    const finalIndex = hasExplicitPhase
      ? findLastIndex(
          assistantRun,
          (event) => assistantMessagePhase(event) === "final_answer",
        )
      : assistantRun.length - 1;
    const intermediate = assistantRun.filter((_, index) => index !== finalIndex);

    if (intermediate.length > 0) {
      rows.push({
        type: "process",
        key: intermediate[0].index,
        events: intermediate,
        hasFinalResponse: finalIndex >= 0,
      });
    }
    if (finalIndex >= 0) {
      rows.push({ type: "event", event: assistantRun[finalIndex] });
    }
    assistantRun = [];
  };

  const flushOpenCodeRun = () => {
    if (openCodeRun.length === 0) return;

    const finalIndexes = new Set<number>();
    openCodeRun.forEach((event, index) => {
      if (openCodeMessagePhase(event) === "final_answer") {
        finalIndexes.add(index);
      }
    });
    if (finalIndexes.size === 0) {
      const finalIndex = findLastIndex(
        openCodeRun,
        (event) => event.role === "assistant" && openCodeMessagePhase(event) === null,
      );
      if (finalIndex >= 0) finalIndexes.add(finalIndex);
    }

    const intermediate = openCodeRun.filter(
      (event, index) =>
        !finalIndexes.has(index) && isOpenCodeConversationEvent(event),
    );
    if (intermediate.length > 0) {
      rows.push({
        type: "process",
        key: intermediate[0].index,
        events: intermediate,
        hasFinalResponse: finalIndexes.size > 0,
      });
    }
    openCodeRun.forEach((event, index) => {
      if (finalIndexes.has(index)) rows.push({ type: "event", event });
    });
    openCodeRun = [];
    openCodeTurnKey = null;
  };

  for (const event of events) {
    const openCode = openCodeAssistantTurn(event);
    if (openCode) {
      flushAssistantRun();
      if (openCodeRun.length > 0 && openCode.turnKey !== openCodeTurnKey) {
        flushOpenCodeRun();
      }
      openCodeTurnKey = openCode.turnKey;
      openCodeRun.push(event);
      continue;
    }
    flushOpenCodeRun();
    if (event.role === "assistant") {
      assistantRun.push(event);
      continue;
    }
    flushAssistantRun();
    rows.push({ type: "event", event });
  }
  flushOpenCodeRun();
  flushAssistantRun();
  return rows;
}

/**
 * 全局偏好只定义新过程分组的默认状态；单轮手动操作通过稳定的事件索引覆盖。
 */
export function isProcessGroupExpanded(
  key: number,
  collapseByDefault: boolean,
  overrides: Readonly<Record<number, boolean>>,
): boolean {
  return overrides[key] ?? !collapseByDefault;
}

/** 当前已加载的过程分组是否全部收起、全部展开或处于混合状态。 */
export function summarizeProcessGroupExpansion(
  keys: readonly number[],
  collapseByDefault: boolean,
  overrides: Readonly<Record<number, boolean>>,
): ProcessGroupExpansionState {
  if (keys.length === 0) {
    return collapseByDefault ? "collapsed" : "expanded";
  }

  let expandedCount = 0;
  for (const key of keys) {
    if (isProcessGroupExpanded(key, collapseByDefault, overrides)) {
      expandedCount += 1;
    }
  }
  if (expandedCount === 0) return "collapsed";
  if (expandedCount === keys.length) return "expanded";
  return "mixed";
}

/**
 * 时间线会排除“在下一次用户提问前完全没有 Agent 活动”的旧轮次，但保留会话
 * 末尾仍待处理的提问。时间线数据尚未加载或加载失败时保持兼容，不提前隐藏消息。
 */
export function isVisibleConversationEvent(
  event: PreviewEvent,
  visiblePromptIndexes: ReadonlySet<number> | null,
  forcedVisibleEventIndex: number | null = null,
): boolean {
  return (
    event.index === forcedVisibleEventIndex ||
    event.role !== "user" ||
    visiblePromptIndexes === null ||
    visiblePromptIndexes.has(event.index)
  );
}

function assistantMessagePhase(event: PreviewEvent): string | null {
  if (event.role !== "assistant") return null;
  const raw = event.raw as {
    payload?: { phase?: unknown };
    message?: { phase?: unknown };
  } | null;
  const phase = raw?.payload?.phase ?? raw?.message?.phase;
  return typeof phase === "string" ? phase : null;
}

function openCodeAssistantTurn(event: PreviewEvent): { turnKey: string } | null {
  const raw = event.raw as {
    message?: { role?: unknown };
    opencode?: {
      message_id?: unknown;
      parent_id?: unknown;
    };
  } | null;
  if (raw?.message?.role !== "assistant" || !raw.opencode) return null;
  const parentId = raw.opencode.parent_id;
  const messageId = raw.opencode.message_id;
  const turnKey =
    typeof parentId === "string" && parentId.length > 0
      ? parentId
      : typeof messageId === "string" && messageId.length > 0
        ? messageId
        : null;
  return turnKey ? { turnKey } : null;
}

function openCodeMessagePhase(event: PreviewEvent): string | null {
  const raw = event.raw as { opencode?: { phase?: unknown } } | null;
  return typeof raw?.opencode?.phase === "string" ? raw.opencode.phase : null;
}

function findLastIndex<T>(items: readonly T[], predicate: (item: T) => boolean): number {
  for (let index = items.length - 1; index >= 0; index -= 1) {
    if (predicate(items[index])) return index;
  }
  return -1;
}
