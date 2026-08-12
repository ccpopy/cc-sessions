import type { PreviewEvent } from "./api.ts";
import {
  isAssistantTextToolUseEvent,
  isOpenCodeConversationEvent,
} from "./conversationDisplay.ts";
import { formatTimeString } from "./format.ts";

export type DiffCommentPrompt = {
  comments: DiffComment[];
  request: string;
};

const previewEventSearchTextCache = new WeakMap<PreviewEvent, string>();

export function buildPreviewEventSearchText(event: PreviewEvent): string {
  const rawText = event.raw == null ? "" : JSON.stringify(event.raw);
  return `${event.text_summary ?? ""}\n${event.kind}\n${rawText}`.toLowerCase();
}

export function previewEventSearchText(event: PreviewEvent): string {
  const cached = previewEventSearchTextCache.get(event);
  if (cached !== undefined) return cached;
  const searchText = buildPreviewEventSearchText(event);
  previewEventSearchTextCache.set(event, searchText);
  return searchText;
}

type DiffComment = {
  number: number;
  context: string;
  body: string;
};

export function extractPreviewEventText(event: PreviewEvent): string {
  const raw = event.raw as any;
  if (!raw) return event.text_summary ?? "";
  if (raw.message) {
    const content = raw.message.content;
    if (typeof content === "string") return content;
    if (Array.isArray(content)) {
      return content
        .map((item: any) => {
          if (typeof item === "string") return item;
          if (item?.type === "thinking") {
            const thinking = typeof item.thinking === "string" ? item.thinking.trim() : "";
            return thinking || "(加密推理)";
          }
          if (item?.type === "redacted_thinking") return "(加密推理)";
          if (typeof item?.text === "string") return item.text;
          if (typeof item?.content === "string") return item.content;
          if (Array.isArray(item?.content)) {
            return item.content
              .map((child: any) => child?.text ?? child?.content ?? "")
              .filter(Boolean)
              .join("\n");
          }
          if (item?.type === "tool_use") {
            return event.role === "assistant" ? "" : `[Tool: ${item.name ?? "unknown"}]`;
          }
          return "";
        })
        .filter(Boolean)
        .join("\n\n");
    }
  }
  const payload = raw.payload;
  if (!payload) return event.text_summary ?? "";
  if (typeof payload.message === "string") return payload.message;
  if (typeof payload.content === "string") return payload.content;
  if (typeof payload.text === "string") return payload.text;
  if (Array.isArray(payload.content)) {
    return payload.content
      .map((item: any) => (typeof item === "string" ? item : item?.text ?? ""))
      .filter(Boolean)
      .join("\n\n");
  }
  return event.text_summary ?? "";
}

export function parseDiffCommentPrompt(text: string): DiffCommentPrompt | null {
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

export function isConversationMessage(event: PreviewEvent): boolean {
  if (event.role === "subagent") return false;
  if (isInternalCodexContextMessage(event)) return false;
  if (isAssistantTextToolUseEvent(event)) return true;
  const raw = event.raw as {
    message?: { role?: unknown };
    opencode?: unknown;
  } | null;
  if (raw?.opencode) return isOpenCodeConversationEvent(event);
  if (typeof raw?.message?.role === "string") {
    return event.role === "user" || event.role === "assistant";
  }
  if (rawType(event) !== "response_item" || payloadType(event) !== "message") return false;
  return event.role === "user" || event.role === "assistant";
}

export function isEventMessage(event: PreviewEvent): boolean {
  if (rawType(event) !== "event_msg") return false;
  const payload = payloadType(event);
  return payload === "user_message" || payload === "agent_message";
}

export function isStableForkNode(event: PreviewEvent): boolean {
  return isConversationMessage(event) || isEventMessage(event);
}

export function eventMessageLabel(event: PreviewEvent): string {
  const payload = payloadType(event);
  if (payload === "user_message") return "用户事件消息";
  if (payload === "agent_message") return "agent事件消息";
  return "事件消息";
}

export function subagentEventLabel(event: PreviewEvent): string {
  if (event.kind === "sub_agent_activity") {
    const raw = event.raw as { payload?: { kind?: unknown } } | null;
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

  switch (event.kind) {
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

export function subagentEventTime(event: PreviewEvent, fallback: string): string {
  if (event.kind !== "sub_agent_activity") return fallback;
  const raw = event.raw as { payload?: { occurred_at_ms?: unknown } } | null;
  const occurredAtMs = raw?.payload?.occurred_at_ms;
  if (typeof occurredAtMs !== "number" || !Number.isFinite(occurredAtMs)) return fallback;
  return formatTimeString(new Date(occurredAtMs).toISOString());
}

function isInternalCodexContextMessage(event: PreviewEvent): boolean {
  if (event.role !== "user") return false;
  const text = extractPreviewEventText(event).trim();
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

export function rawType(event: PreviewEvent): string {
  const raw = event.raw as { type?: unknown } | null;
  return typeof raw?.type === "string" ? raw.type : "";
}

export function payloadType(event: PreviewEvent): string {
  const raw = event.raw as { payload?: { type?: unknown } } | null;
  return typeof raw?.payload?.type === "string" ? raw.payload.type : "";
}

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

export function canEditEventText(provider: string, event: PreviewEvent): boolean {
  if (provider === "codex") {
    const outer = rawType(event);
    const payload = payloadType(event);
    if (outer === "event_msg") return payload === "user_message" || payload === "agent_message";
    if (outer === "response_item" && payload === "message") {
      return editableText(event).length > 0;
    }
    return false;
  }
  if (provider === "opencode") {
    const raw = event.raw as any;
    return (
      typeof raw?.opencode?.part_id === "string" &&
      raw?.opencode?.part_type === "text" &&
      (raw?.message?.role === "user" || raw?.message?.role === "assistant") &&
      editableText(event).length > 0
    );
  }
  const raw = event.raw as any;
  if (!raw?.message || (raw?.type !== "user" && raw?.type !== "assistant")) return false;
  return editableText(event).length > 0;
}

export function canDeleteEvent(provider: string, event: PreviewEvent): boolean {
  if (provider === "codex") {
    const outer = rawType(event);
    const payload = payloadType(event);
    if (outer === "event_msg") return payload === "user_message" || payload === "agent_message";
    if (outer === "response_item") return CODEX_DELETABLE_RESPONSE_ITEMS.has(payload);
    return false;
  }
  if (provider === "opencode") {
    const raw = event.raw as any;
    return (
      typeof raw?.opencode?.part_id === "string" &&
      typeof raw?.opencode?.message_id === "string" &&
      (raw?.message?.role === "user" || raw?.message?.role === "assistant")
    );
  }
  const raw = event.raw as any;
  return (
    !!raw?.message &&
    typeof raw?.uuid === "string" &&
    (raw?.type === "user" || raw?.type === "assistant")
  );
}

export function editableText(event: PreviewEvent): string {
  const raw = event.raw as any;
  if (!raw) return "";
  if (raw.payload) {
    if (typeof raw.payload.message === "string") return raw.payload.message;
    const content = raw.payload.content;
    if (typeof content === "string") return content;
    if (Array.isArray(content)) {
      return content
        .filter((item: any) => typeof item?.text === "string")
        .map((item: any) => item.text)
        .join("\n");
    }
    return "";
  }
  const content = raw?.message?.content;
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .filter((item: any) => item?.type === "text" && typeof item.text === "string")
      .map((item: any) => item.text)
      .join("\n");
  }
  return "";
}

export function deleteReasonLabel(reason: string): string {
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

export function editKindLabel(kind: string): string {
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
