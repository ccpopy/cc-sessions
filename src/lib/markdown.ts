import type { PreviewEvent } from "@/lib/api";
import { eventEpochSeconds } from "@/lib/exportTimeRange";

export type ConversationMessage = {
  index: number;
  role: "user" | "assistant";
  /** 事件时间戳换算的 epoch 秒；无法解析时为 null（不参与时间范围过滤） */
  ts: number | null;
  timestamp: string;
  text: string;
};

/**
 * 从预览事件中提取"可选择"的对话消息（user / assistant）。
 *
 * 这是后端 `markdown_export::segment` 的前端镜像，只用于：
 * - 在导出对话框里列出可勾选的消息片段；
 * - 实际渲染仍以后端为准（前端把选中的 index 传回后端）。
 *
 * 关键点（与后端一致）：
 * - Codex 的 `event_msg` 与 `response_item/message` 内容重复，这里只取后者去重；
 * - Claude 的 assistant 回合即使夹带 tool_use，也保留其中的正文；
 * - 过滤掉 Codex 注入的 AGENTS.md / environment_context 内部上下文。
 */
export function extractConversationMessages(events: PreviewEvent[]): ConversationMessage[] {
  const out: ConversationMessage[] = [];
  for (const e of events) {
    const seg = classifyMessage(e);
    if (seg) {
      out.push({
        index: e.index,
        role: seg.role,
        ts: eventEpochSeconds(e.timestamp),
        timestamp: e.timestamp,
        text: seg.text,
      });
    }
  }
  return out;
}

function classifyMessage(e: PreviewEvent): { role: "user" | "assistant"; text: string } | null {
  const raw = e.raw as any;
  if (!raw) return null;

  // Claude 形态
  if (raw.message) {
    const role = raw.message.role;
    const content = raw.message.content;
    const text = collectClaudeText(content);
    if (role === "assistant") {
      return text.trim() ? { role: "assistant", text } : null;
    }
    if (role === "user") {
      if (!text.trim() || isInternalText(text)) return null;
      return { role: "user", text };
    }
    return null;
  }

  // Codex 形态
  if (raw.type !== "response_item") return null;
  const payload = raw.payload;
  if (!payload || payload.type !== "message") return null;
  const role = payload.role;
  const text = collectCodexText(payload.content);
  if (!text.trim()) return null;
  if (role === "assistant") return { role: "assistant", text };
  if (role === "user") return isInternalText(text) ? null : { role: "user", text };
  return null;
}

function collectClaudeText(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .map((item: any) => {
      if (typeof item === "string") return item;
      if (item?.type === "text" && typeof item.text === "string") return item.text;
      return "";
    })
    .filter(Boolean)
    .join("\n\n");
}

function collectCodexText(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .map((item: any) => {
      if (typeof item === "string") return item;
      if (typeof item?.text === "string") return item.text;
      return "";
    })
    .filter(Boolean)
    .join("\n");
}

function isInternalText(text: string): boolean {
  const trimmed = text.trim();
  if (!trimmed) return false;
  const firstLine = (trimmed.split(/\r?\n/, 1)[0] ?? "").trim().replace(/^#{1,6}\s+/, "").trim();
  if (firstLine.startsWith("AGENTS.md instructions for ") && trimmed.includes("<INSTRUCTIONS>")) {
    return true;
  }
  if (firstLine === "<environment_context>" && trimmed.includes("</environment_context>")) {
    return true;
  }
  return false;
}
