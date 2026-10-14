import { useState } from "react";
import { Check, ChevronDown, FileText } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { LocalImageAttachments } from "@/components/LocalImageAttachments";
import { parseUserMessageAttachments, type LocalImageAttachment } from "@/lib/messageAttachments";
import { parseUserQuestionReply } from "@/lib/previewEvent";

export function UserMessageContent({ text, nativeImages }: {
  text: string;
  nativeImages: LocalImageAttachment[];
}) {
  const [showOriginal, setShowOriginal] = useState(false);
  const replies = parseUserQuestionReply(text);
  const message = replies
    ? { markdown: "", files: [], images: nativeImages, hasWrapper: false }
    : parseUserMessageAttachments(text, nativeImages);

  return (
    <>
      <LocalImageAttachments images={message.images} />
      {replies ? (
        <section aria-label="问答回复" className="space-y-3 py-0.5">
          <div className="flex items-center gap-1.5 text-xs opacity-80">
            <Check className="h-3.5 w-3.5" aria-hidden="true" />
            <span>已回答{replies.length > 1 ? ` · ${replies.length} 个问题` : ""}</span>
          </div>
          <dl className="space-y-4">
            {replies.map((reply, index) => (
              <div key={index} className="space-y-1.5">
                <dt className="whitespace-pre-wrap break-words text-xs leading-relaxed opacity-80">{reply.question}</dt>
                <dd className="whitespace-pre-wrap break-words border-l-2 border-primary-foreground/50 pl-3 text-sm font-medium leading-relaxed">
                  {reply.answer || "（未填写回答）"}
                </dd>
              </div>
            ))}
          </dl>
        </section>
      ) : message.markdown ? (
        <ReactMarkdown remarkPlugins={[remarkGfm]}>{message.markdown}</ReactMarkdown>
      ) : message.images.length === 0 && message.files.length === 0 ? (
        <span className="italic opacity-70">(空消息)</span>
      ) : null}
      {message.files.length > 0 && (
        <div className="mt-3 flex flex-wrap gap-2" aria-label="消息附件">
          {message.files.map((file, index) => (
            <div key={`${file.path}:${index}`} title={file.path}
              className="flex min-w-0 max-w-full items-center gap-2 rounded-lg border border-primary-foreground/25 bg-primary-foreground/10 px-3 py-2 text-xs">
              <FileText className="h-4 w-4 shrink-0" aria-hidden="true" />
              <span className="truncate">{file.name}</span>
            </div>
          ))}
        </div>
      )}
      {(replies || message.hasWrapper) && (
        <div className="mt-3 border-t border-primary-foreground/20 pt-2">
          <button type="button" aria-expanded={showOriginal}
            className="flex items-center gap-1 text-[11px] opacity-75 hover:opacity-100 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary-foreground"
            onClick={() => setShowOriginal(value => !value)}>
            <ChevronDown className={`h-3 w-3 transition-transform ${showOriginal ? "rotate-180" : ""}`} aria-hidden="true" />
            {showOriginal ? "收起原始消息" : "查看原始消息"}
          </button>
          {showOriginal && <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-all rounded-md bg-background/10 p-2 font-mono text-[11px] leading-relaxed">{text}</pre>}
        </div>
      )}
    </>
  );
}
