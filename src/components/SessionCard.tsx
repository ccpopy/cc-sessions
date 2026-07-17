import { memo } from "react";
import {
  Archive,
  CheckCircle2,
  Copy,
  Eye,
  FileText,
  FolderOpen,
  GitBranch,
  Inbox,
  MoreHorizontal,
  Network,
  RotateCw,
  ShieldCheck,
  Trash2,
  Undo2,
} from "lucide-react";
import { Checkbox } from "@/components/ui/checkbox";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { FamilyOverlay, SessionSummary } from "@/lib/api";
import {
  absoluteTime,
  highlight,
  humanBytes,
  humanTokens,
  relativeTime,
  shortId,
} from "@/lib/format";
import { isSubagentSession } from "@/lib/sessionSource";
import { sessionDisplayPreview, sessionDisplayTitle } from "@/lib/sessionText";
import { cn } from "@/lib/utils";

type Props = {
  s: SessionSummary;
  selected: boolean;
  onToggleSelect: (id: string) => void;
  onPreview: (s: SessionSummary) => void;
  onCopyResume: (s: SessionSummary) => void;
  onRevealCwd: (s: SessionSummary) => void;
  onArchiveToggle?: (s: SessionSummary) => void;
  onBackup: (s: SessionSummary) => void;
  onDelete?: (s: SessionSummary) => void;
  onClone?: (s: SessionSummary) => void;
  onOpenFamily?: (s: SessionSummary) => void;
  onExportMarkdown?: (s: SessionSummary) => void;
  query?: string;
  showProject?: boolean;
  overlay?: FamilyOverlay;
  currentProvider?: string | null;
  syncing?: boolean;
  syncDisabled?: boolean;
};

export const SessionCard = memo(function SessionCard({
  s,
  selected,
  onToggleSelect,
  onPreview,
  onCopyResume,
  onRevealCwd,
  onArchiveToggle,
  onBackup,
  onDelete,
  onClone,
  onOpenFamily,
  onExportMarkdown,
  query = "",
  showProject = true,
  overlay,
  currentProvider,
  syncing = false,
  syncDisabled = false,
}: Props) {
  const displayTitle = sessionDisplayTitle(s.title, s.first_user_message);
  const displayFirstUserMessage = sessionDisplayPreview(s.first_user_message);
  const syncAction = syncActionLabel(overlay?.clone_state, currentProvider);
  const syncBlocked = syncing || syncDisabled;
  const isSubagent = isSubagentSession(s, overlay);
  const subagent = subagentLabel(s, isSubagent);
  const isUsableCurrentProviderBranch =
    !s.archived && overlay?.clone_state === "matches";
  const requiresActivation = Boolean(
    overlay?.family_id && !overlay.is_active_branch && !isUsableCurrentProviderBranch,
  );
  const canCopyResume = !(s.provider === "claude" && isSubagent) && !requiresActivation;

  return (
    <div
      className={cn(
        "group relative w-full min-w-0 overflow-hidden rounded-lg border border-border/70 bg-card text-card-foreground shadow-sm transition-all duration-200",
        "before:pointer-events-none before:absolute before:bottom-3 before:left-0 before:top-3 before:w-[3px] before:rounded-r-full before:bg-emerald-500 before:opacity-0 before:transition-opacity before:duration-200",
        "hover:-translate-y-[0.5px] hover:border-foreground/15 hover:shadow-[0_2px_8px_-3px_rgb(0_0_0/0.08)]",
        s.archived && "opacity-60",
        selected &&
          "border-emerald-500/45 bg-emerald-500/[0.035] before:opacity-100 dark:border-emerald-500/35 dark:bg-emerald-500/[0.07]",
      )}
    >
      <div className="grid min-w-0 grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-2 p-4 sm:grid-cols-[auto_minmax(0,1fr)_auto]">
        <Checkbox
          checked={selected}
          onCheckedChange={() => onToggleSelect(s.id)}
          className="mt-1.5"
          aria-label="选择会话"
        />

        <div className="min-w-0 flex-1 space-y-2">
          {/* 顶部元信息：项目名（可选） + id + 模型 */}
          <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-xs">
            {showProject && (
              <>
                <span
                  className="min-w-0 cursor-default truncate font-medium text-foreground"
                  title={s.cwd}
                >
                  <Hl text={s.cwd_display || s.cwd} q={query} />
                </span>
                <MetaDot />
              </>
            )}
            <span className="shrink-0 font-mono text-muted-foreground">{shortId(s.id)}</span>
            {s.model && (
              <Badge variant="secondary" className="h-5 max-w-44 truncate px-1.5 font-normal">
                {s.model}
                {s.reasoning_effort ? ` · ${s.reasoning_effort}` : ""}
              </Badge>
            )}
            {s.archived && (
              <Badge variant="outline" className="h-5 px-1.5">
                已归档
              </Badge>
            )}
            <Badge variant="outline" className="h-5 px-1.5 font-normal text-muted-foreground">
              {s.provider}
            </Badge>
            {overlay?.provider && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Badge
                    variant="outline"
                    className={
                      overlay.clone_state === "matches"
                        ? "h-5 border-emerald-500/30 px-1.5 font-normal text-emerald-600"
                        : "h-5 px-1.5 font-normal text-muted-foreground"
                    }
                  >
                    {overlay.provider}
                  </Badge>
                </TooltipTrigger>
                <TooltipContent>
                  model_provider（threads）
                  {currentProvider && overlay.provider !== currentProvider
                    ? ` · 当前 provider: ${currentProvider}`
                    : ""}
                </TooltipContent>
              </Tooltip>
            )}
            {overlay && overlay.branch_count > 1 && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Badge
                    variant="outline"
                    className="h-5 cursor-pointer gap-1 px-1.5 font-normal"
                    onClick={(e) => {
                      e.stopPropagation();
                      onOpenFamily?.(s);
                    }}
                  >
                    <GitBranch className="h-3 w-3" />
                    {overlay.branch_count} 分支
                  </Badge>
                </TooltipTrigger>
                <TooltipContent>
                  共 {overlay.branch_count} 个分支
                  {overlay.is_active_branch
                    ? `（含 ${overlay.branch_count - 1} 个未在列表显示的历史分支）`
                    : ""}
                  ，点击查看 / 切换 / 恢复
                </TooltipContent>
              </Tooltip>
            )}
            {subagent && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Badge
                    variant="outline"
                    className="h-5 gap-1 border-violet-500/30 px-1.5 font-normal text-violet-600"
                  >
                    <Network className="h-3 w-3" />
                    {subagent.label}
                  </Badge>
                </TooltipTrigger>
                <TooltipContent>{subagent.title}</TooltipContent>
              </Tooltip>
            )}
            {syncAction && (
              <Badge
                variant="outline"
                aria-disabled={syncBlocked}
                className={cn(
                  "h-5 gap-1 border-blue-500/40 px-1.5 font-normal text-blue-600",
                  syncBlocked
                    ? "cursor-not-allowed opacity-60"
                    : "cursor-pointer hover:bg-blue-500/10",
                )}
                onClick={(e) => {
                  e.stopPropagation();
                  if (syncBlocked) return;
                  onClone?.(s);
                }}
              >
                <RotateCw className={cn("h-3 w-3", syncing && "animate-spin")} />
                {syncing ? "同步中…" : syncAction}
              </Badge>
            )}
            {overlay?.clone_state === "has_clone" && (
              <Badge
                variant="outline"
                className="h-5 gap-1 border-emerald-500/30 px-1.5 font-normal text-emerald-600"
              >
                <CheckCircle2 className="h-3 w-3" />
                已克隆
              </Badge>
            )}
            {s.has_backup && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <ShieldCheck className="h-3.5 w-3.5 shrink-0 text-emerald-500" />
                </TooltipTrigger>
                <TooltipContent>已有备份</TooltipContent>
              </Tooltip>
            )}
          </div>

          {/* 标题 */}
          <div className="line-clamp-1 min-w-0 break-all text-sm font-semibold leading-snug">
            <Hl text={displayTitle} q={query} />
          </div>

          {/* 首条用户消息预览 */}
          {displayFirstUserMessage && (
            <p className="line-clamp-2 min-w-0 [overflow-wrap:anywhere] text-sm leading-relaxed text-muted-foreground">
              <Hl text={displayFirstUserMessage} q={query} />
            </p>
          )}

          {/* 底部：操作按钮 */}
          <div className="flex min-w-0 flex-wrap items-center gap-1 pt-1">
            <Button variant="outline" size="sm" onClick={() => onPreview(s)} className="h-8 gap-1.5 border-border/70">
              <Eye className="h-3.5 w-3.5" />
              预览
            </Button>
            {canCopyResume && (
              <Button variant="ghost" size="sm" onClick={() => onCopyResume(s)} className="h-8 gap-1.5 text-muted-foreground hover:text-foreground">
                <Copy className="h-3.5 w-3.5" />
                resume
              </Button>
            )}
            {requiresActivation && (
              <Button
                variant="ghost"
                size="sm"
                onClick={() => onOpenFamily?.(s)}
                className="h-8 gap-1.5 text-muted-foreground hover:text-foreground"
              >
                <GitBranch className="h-3.5 w-3.5" />
                先设为当前
              </Button>
            )}
            <Button variant="ghost" size="sm" onClick={() => onRevealCwd(s)} className="h-8 gap-1.5 text-muted-foreground hover:text-foreground">
              <FolderOpen className="h-3.5 w-3.5" />
              打开目录
            </Button>
          </div>
        </div>

        <div className="col-start-2 flex min-w-0 items-center justify-between gap-2 sm:col-start-3 sm:row-start-1 sm:min-w-[9rem] sm:flex-col sm:items-end sm:self-stretch">
          <Tooltip>
            <TooltipTrigger asChild>
              <span className="shrink-0 cursor-default whitespace-nowrap text-xs text-muted-foreground">
                {relativeTime(s.updated_at)}
              </span>
            </TooltipTrigger>
            <TooltipContent align="end">更新 {absoluteTime(s.updated_at)}</TooltipContent>
          </Tooltip>

          <div className="flex shrink-0 items-center gap-1.5 text-[11.5px] text-muted-foreground">
            {s.tokens_used > 0 && (
              <span className="inline-flex items-baseline gap-1 tabular-nums">
                <span className="font-medium text-foreground/75">{humanTokens(s.tokens_used)}</span>
                <span className="text-[10px] uppercase tracking-wide text-muted-foreground/60">tok</span>
              </span>
            )}
            {s.tokens_used > 0 && <MetaDot />}
            <span className="whitespace-nowrap tabular-nums font-medium text-foreground/75">
              {humanBytes(s.rollout_bytes)}
            </span>

            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <Button variant="ghost" size="icon" className="ml-0.5 h-8 w-8 text-muted-foreground hover:text-foreground">
                  <MoreHorizontal className="h-4 w-4" />
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end">
                <DropdownMenuItem onClick={() => onBackup(s)}>
                  <Archive className="h-4 w-4" />
                  单条备份
                </DropdownMenuItem>
                {onExportMarkdown && (
                  <DropdownMenuItem onClick={() => onExportMarkdown(s)}>
                    <FileText className="h-4 w-4" />
                    导出为 Markdown
                  </DropdownMenuItem>
                )}
                {onOpenFamily && (
                  <DropdownMenuItem onClick={() => onOpenFamily(s)}>
                    <Network className="h-4 w-4" />
                    查看分支
                  </DropdownMenuItem>
                )}
                {onClone && syncAction && (
                  <DropdownMenuItem disabled={syncBlocked} onClick={() => onClone(s)}>
                    <RotateCw className={cn("h-4 w-4", syncing && "animate-spin")} />
                    {syncing ? "同步中…" : syncAction}
                  </DropdownMenuItem>
                )}
                {onArchiveToggle && (
                  <DropdownMenuItem onClick={() => onArchiveToggle(s)}>
                    {s.archived ? <Undo2 className="h-4 w-4" /> : <Inbox className="h-4 w-4" />}
                    {s.archived ? "取消归档" : "归档"}
                  </DropdownMenuItem>
                )}
                {onDelete && (
                  <>
                    <DropdownMenuSeparator />
                    <DropdownMenuItem
                      onClick={() => onDelete(s)}
                      className="text-destructive focus:text-destructive"
                    >
                      <Trash2 className="h-4 w-4" />
                      删除会话
                    </DropdownMenuItem>
                  </>
                )}
              </DropdownMenuContent>
            </DropdownMenu>
          </div>
        </div>
      </div>
    </div>
  );
});

function MetaDot() {
  return (
    <span
      aria-hidden="true"
      className="inline-block h-1 w-1 shrink-0 rounded-full bg-muted-foreground/35"
    />
  );
}

function Hl({ text, q }: { text: string; q: string }) {
  const parts = highlight(text, q);
  return (
    <>
      {parts.map((p, i) => (p.hit ? <mark key={i}>{p.t}</mark> : <span key={i}>{p.t}</span>))}
    </>
  );
}

function syncActionLabel(cloneState: string | undefined, currentProvider: string | null | undefined): string {
  if (cloneState === "resync") return "修复本地索引";
  if (cloneState === "clonable" && currentProvider) return `同步到 ${currentProvider}`;
  return "";
}

function subagentLabel(
  s: SessionSummary,
  isSubagent: boolean,
): { label: string; title: string } | null {
  if (!isSubagent) {
    return null;
  }
  const role = s.agent_role?.trim();
  const nickname = s.agent_nickname?.trim();
  const providerLabel = s.provider === "claude" ? "Claude" : "Codex";
  return {
    label: role ? `子代理 · ${role}` : "子代理",
    title: nickname
      ? `${providerLabel} 子代理线程：${nickname}${role ? `（${role}）` : ""}`
      : role
        ? `${providerLabel} 子代理线程：${role}`
        : `${providerLabel} 子代理线程`,
  };
}
