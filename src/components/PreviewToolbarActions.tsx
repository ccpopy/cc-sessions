import { Copy, FileJson, FolderOpen, History, Info, MoreHorizontal, RefreshCw } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";

type Props = {
  hasSession: boolean;
  canOpenEditHistory: boolean;
  onCopySessionId: () => void | Promise<void>;
  onCopyResume: () => void | Promise<void>;
  onRevealDirectory: () => void | Promise<void>;
  onOpenEditHistory: () => void;
  onCopyPath: () => void | Promise<void>;
  onRefresh: () => void;
  refreshing: boolean;
  onSessionInfo: () => void;
};

export function PreviewToolbarActions({
  hasSession,
  canOpenEditHistory,
  onCopySessionId,
  onCopyResume,
  onRevealDirectory,
  onOpenEditHistory,
  onCopyPath,
  onRefresh,
  refreshing,
  onSessionInfo,
}: Props) {
  return (
    <div className="ml-auto flex shrink-0 items-center gap-0.5">
      <Button variant="ghost" size="icon" className="h-8 w-8 text-muted-foreground" aria-label="刷新预览" title="刷新预览" disabled={refreshing} onClick={onRefresh}><RefreshCw className={refreshing ? "h-4 w-4 animate-spin" : "h-4 w-4"} /></Button>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button
            variant="ghost"
            size="icon"
            className="h-8 w-8 text-muted-foreground hover:text-foreground"
            aria-label="更多"
            title="更多"
          >
            <MoreHorizontal className="h-4 w-4" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-44">
          <DropdownMenuItem onSelect={onSessionInfo}><Info className="h-4 w-4" />会话信息</DropdownMenuItem>
          {hasSession && (
            <>
              <DropdownMenuItem onSelect={() => void onCopySessionId()}>
                <Copy className="h-4 w-4" />
                复制会话 ID
              </DropdownMenuItem>
              <DropdownMenuItem onSelect={() => void onCopyResume()}>
                <Copy className="h-4 w-4" />
                复制 resume
              </DropdownMenuItem>
              <DropdownMenuItem onSelect={() => void onRevealDirectory()}>
                <FolderOpen className="h-4 w-4" />
                打开目录
              </DropdownMenuItem>
            </>
          )}
          <DropdownMenuItem onSelect={() => void onCopyPath()}>
            <FileJson className="h-4 w-4" />
            复制路径
          </DropdownMenuItem>
          {canOpenEditHistory && (
            <>
              <DropdownMenuSeparator />
              <DropdownMenuItem onSelect={onOpenEditHistory}>
                <History className="h-4 w-4" />
                编辑历史
              </DropdownMenuItem>
            </>
          )}
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}
