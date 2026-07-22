import type { PreviewEvent } from "./api";

export type ConversationPreviewRow =
  | { type: "event"; event: PreviewEvent }
  | {
      type: "collapsed";
      key: number;
      events: PreviewEvent[];
      hasFinalResponse: boolean;
    };

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
        type: "collapsed",
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
