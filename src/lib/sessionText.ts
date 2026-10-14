import { parseUserMessageAttachments } from "./messageAttachments.ts";
import { parseUserQuestionReply } from "./previewEvent.ts";

const AUTO_REVIEW_PREFIX =
  "The following is the Codex agent history whose request action you are assessing.";

export type EmbeddedTranscriptPrompt = {
  request: string;
  transcript: string;
};

export function parseEmbeddedTranscriptPrompt(text: string): EmbeddedTranscriptPrompt | null {
  const normalized = text.replace(/\r\n/g, "\n").trim();
  if (!normalized.includes("TRANSCRIPT START")) return null;
  if (!normalized.startsWith(AUTO_REVIEW_PREFIX)) return null;

  const markerIndex = normalized.indexOf("TRANSCRIPT START");
  const afterMarker = normalized.slice(markerIndex + "TRANSCRIPT START".length).trim();
  const endIndex = afterMarker.indexOf("\nTRANSCRIPT END");
  const transcript = (endIndex >= 0 ? afterMarker.slice(0, endIndex) : afterMarker).trim();
  if (!transcript) return null;

  return {
    request: extractFirstTranscriptUserMessage(transcript),
    transcript,
  };
}

export function sessionDisplayPreview(text: string): string {
  const embedded = parseEmbeddedTranscriptPrompt(text);
  if (embedded) return embedded.request || text;
  const replies = parseUserQuestionReply(text);
  if (replies) return replies.map(reply => reply.answer || "（未填写回答）").join("；");
  const message = parseUserMessageAttachments(text);
  return message.hasWrapper
    ? message.markdown || [...message.images, ...message.files].map(file => file.name).join("、") || text
    : text;
}

export function sessionDisplayTitle(title: string, firstUserMessage: string): string {
  const embedded = parseEmbeddedTranscriptPrompt(firstUserMessage) ?? parseEmbeddedTranscriptPrompt(title);
  if (!embedded) return title || firstUserMessage || "(无标题)";

  const request = singleLine(embedded.request);
  return request ? `自动评审：${request}` : "自动评审会话";
}

function extractFirstTranscriptUserMessage(transcript: string): string {
  const start = /(?:^|\n)\[\d+\]\s+user:\s*/.exec(transcript);
  if (!start) return "";

  const contentStart = start.index + start[0].length;
  const rest = transcript.slice(contentStart);
  const next = /\n\[\d+\]\s+(?:user|assistant|tool|system|developer)\b[^\n:]*:/i.exec(rest);
  return (next ? rest.slice(0, next.index) : rest).trim();
}

function singleLine(text: string): string {
  return text.replace(/\s+/g, " ").trim();
}
