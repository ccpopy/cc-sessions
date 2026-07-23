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

/**
 * Codex 会明确标记 assistant 消息是过程播报（commentary）还是最终答复
 * （final_answer）。旧版 Codex 与 Claude 没有 phase，此时退回到“一轮最后一条”。
 */
export function buildConversationPreviewRows(
  events: readonly PreviewEvent[],
): ConversationPreviewRow[] {
  const rows: ConversationPreviewRow[] = [];
  let assistantRun: PreviewEvent[] = [];

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

  for (const event of events) {
    if (event.role === "assistant") {
      assistantRun.push(event);
      continue;
    }
    flushAssistantRun();
    rows.push({ type: "event", event });
  }
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
): boolean {
  return (
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

function findLastIndex<T>(items: readonly T[], predicate: (item: T) => boolean): number {
  for (let index = items.length - 1; index >= 0; index -= 1) {
    if (predicate(items[index])) return index;
  }
  return -1;
}
