import { useEffect, useState } from "react";
import {
  AlertDialog, AlertDialogAction, AlertDialogContent, AlertDialogDescription,
  AlertDialogFooter, AlertDialogHeader, AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { SESSION_BUSY_EVENT } from "@/lib/session-activity";

export function SessionBusyDialog() {
  const [messages, setMessages] = useState<string[]>([]);
  useEffect(() => {
    const listener = (event: Event) => {
      const next = (event as CustomEvent<string[]>).detail;
      setMessages((current) => [...new Set([...current, ...next])]);
    };
    window.addEventListener(SESSION_BUSY_EVENT, listener);
    return () => window.removeEventListener(SESSION_BUSY_EVENT, listener);
  }, []);
  return (
    <AlertDialog open={messages.length > 0} onOpenChange={(open) => { if (!open) setMessages([]); }}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>会话正在写入，操作已阻止</AlertDialogTitle>
          <AlertDialogDescription className="max-h-64 overflow-y-auto whitespace-pre-line">
            {messages.join("\n\n")}
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter><AlertDialogAction>知道了</AlertDialogAction></AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
