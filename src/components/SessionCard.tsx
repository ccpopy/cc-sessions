import { memo } from "react";
import {
  Archive,
  ArrowLeftRight,
  CheckCircle2,
  Copy,
  Eye,
  FileText,
  FolderOpen,
  Folders,
  GitBranch,
  Inbox,
  MoreHorizontal,
  Network,
  Pencil,
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
import type { FamilyOverlay, SessionConversionOrigin, SessionSummary } from "@/lib/api";
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
  onConvert?: (s: SessionSummary) => void;
  onRename?: (s: SessionSummary) => void;
  onMoveCwd?: (s: SessionSummary) => void;
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
  onConvert,
  onRename,
  onMoveCwd,
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
        "group relative w-full min-w-0 overflow-hidden rounded-lg border border-border/60 bg-card text-card-foreground shadow-[0_1px_2px_rgb(0_0_0/0.03)] transition-all duration-200",
        "before:pointer-events-none before:absolute before:bottom-3 before:left-0 before:top-3 before:w-[3px] before:rounded-r-full before:bg-emerald-500 before:opacity-0 before:transition-opacity before:duration-200",
        "hover:-translate-y-px hover:border-foreground/15 hover:shadow-[0_3px_10px_-3px_rgb(0_0_0/0.09)] motion-reduce:transition-none motion-reduce:hover:translate-y-0",
        s.archived && "opacity-60",
        selected &&
          "border-emerald-500/45 bg-emerald-500/[0.035] before:opacity-100 dark:border-emerald-500/35 dark:bg-emerald-500/[0.07]",
      )}
    >
      <div className="grid min-w-0 grid-cols-[auto_minmax(0,1fr)] gap-x-3 p-4">
        <Checkbox
          checked={selected}
          onCheckedChange={() => onToggleSelect(s.id)}
          className="mt-0.5"
          aria-label="选择会话"
        />

        <div className="min-w-0 flex-1">
          {/* 标题 + 更新时间 */}
          <div className="flex min-w-0 items-start gap-3">
            <div className="line-clamp-1 min-w-0 flex-1 wrap-anywhere text-sm font-semibold leading-snug">
              <Hl text={displayTitle} q={query} />
            </div>
            <Tooltip>
              <TooltipTrigger asChild>
                <span className="shrink-0 cursor-default whitespace-nowrap text-[11.5px] tabular-nums text-muted-foreground">
                  {relativeTime(s.updated_at)}
                </span>
              </TooltipTrigger>
              <TooltipContent align="end">更新 {absoluteTime(s.updated_at)}</TooltipContent>
            </Tooltip>
          </div>

          {/* 元信息：项目名（可选） + id + 模型 + 状态徽章 */}
          <div className="mt-1.5 flex min-w-0 flex-wrap items-center gap-x-1.5 gap-y-1 text-[11.5px]">
            {showProject && (
              <>
                <span
                  className="min-w-0 cursor-default truncate font-medium text-foreground/70"
                  title={s.cwd}
                >
                  <Hl text={s.cwd_display || s.cwd} q={query} />
                </span>
                <MetaDot />
              </>
            )}
            <span className="shrink-0 font-mono text-[11px] text-muted-foreground">{shortId(s.id)}</span>
            {s.model && (
              <Badge
                variant="secondary"
                className="h-5 max-w-44 truncate px-1.5 text-[11px] font-normal text-muted-foreground"
              >
                {s.model}
                {s.reasoning_effort ? ` · ${s.reasoning_effort}` : ""}
              </Badge>
            )}
            {s.archived && (
              <Badge variant="outline" className="h-5 px-1.5 text-[11px] font-normal">
                已归档
              </Badge>
            )}
            <ProviderBadge provider={s.provider} />
            {s.conversion_origin && <ConversionOriginBadge origin={s.conversion_origin} />}
            {overlay?.provider && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Badge
                    variant="outline"
                    className={
                      overlay.clone_state === "matches"
                        ? "h-5 border-emerald-500/30 px-1.5 text-[11px] font-normal text-emerald-600"
                        : "h-5 px-1.5 text-[11px] font-normal text-muted-foreground"
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
                    className="h-5 cursor-pointer gap-1 px-1.5 text-[11px] font-normal"
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
                    className="h-5 gap-1 border-violet-500/30 px-1.5 text-[11px] font-normal text-violet-600"
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
                  "h-5 gap-1 border-blue-500/40 px-1.5 text-[11px] font-normal text-blue-600",
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
                className="h-5 gap-1 border-emerald-500/30 px-1.5 text-[11px] font-normal text-emerald-600"
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

          {/* 首条用户消息预览 */}
          {displayFirstUserMessage && (
            <p className="mt-1.5 line-clamp-2 min-w-0 wrap-anywhere text-[13px] leading-relaxed text-muted-foreground">
              <Hl text={displayFirstUserMessage} q={query} />
            </p>
          )}

          {/* 底部：操作按钮 + 体积信息 */}
          <div className="mt-2.5 flex min-w-0 flex-wrap items-center justify-between gap-x-3 gap-y-1.5">
            <div className="flex min-w-0 flex-wrap items-center gap-1">
              <Button
                variant="outline"
                size="sm"
                onClick={() => onPreview(s)}
                className="h-7 gap-1.5 border-border/60 px-2.5 shadow-none hover:bg-muted/50"
              >
                <Eye className="h-3.5 w-3.5" />
                预览
              </Button>
              {canCopyResume && (
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => onCopyResume(s)}
                  className="h-7 gap-1.5 px-2.5 font-normal text-muted-foreground hover:text-foreground"
                >
                  <Copy className="h-3.5 w-3.5" />
                  resume
                </Button>
              )}
              {requiresActivation && (
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => onOpenFamily?.(s)}
                  className="h-7 gap-1.5 px-2.5 font-normal text-muted-foreground hover:text-foreground"
                >
                  <GitBranch className="h-3.5 w-3.5" />
                  先设为当前
                </Button>
              )}
              <Button
                variant="ghost"
                size="sm"
                onClick={() => onRevealCwd(s)}
                className="h-7 gap-1.5 px-2.5 font-normal text-muted-foreground hover:text-foreground"
              >
                <FolderOpen className="h-3.5 w-3.5" />
                打开目录
              </Button>
            </div>

            <div className="ml-auto flex shrink-0 items-center gap-1.5 font-mono text-[11px] tabular-nums text-muted-foreground">
              {s.tokens_used > 0 && (
                <span className="whitespace-nowrap">{humanTokens(s.tokens_used)} tok</span>
              )}
              {s.tokens_used > 0 && <MetaDot />}
              <span className="whitespace-nowrap">
                {humanBytes(s.rollout_bytes)}
              </span>

              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button variant="ghost" size="icon" className="ml-0.5 h-7 w-7 text-muted-foreground hover:text-foreground">
                    <MoreHorizontal className="h-4 w-4" />
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  {onRename && (
                    <DropdownMenuItem onClick={() => onRename(s)}>
                      <Pencil className="h-4 w-4" />
                      重命名
                    </DropdownMenuItem>
                  )}
                  {onMoveCwd && (
                    <DropdownMenuItem onClick={() => onMoveCwd(s)}>
                      <Folders className="h-4 w-4" />
                      移动项目目录
                    </DropdownMenuItem>
                  )}
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
                  {onConvert && !isSubagent && (
                    <DropdownMenuItem onClick={() => onConvert(s)}>
                      <ArrowLeftRight className="h-4 w-4" />
                      {s.provider === "codex" ? "转换为 Claude 会话" : "转换为 Codex 会话"}
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

function ProviderBadge({ provider }: { provider: string }) {
  const presentation = providerPresentation(provider);
  return (
    <Badge
      variant="outline"
      aria-label={`当前会话格式：${presentation.label}`}
      className={cn("h-5 px-1.5 text-[11px] font-normal", presentation.className)}
    >
      {presentation.label}
    </Badge>
  );
}

function ConversionOriginBadge({ origin }: { origin: SessionConversionOrigin }) {
  const source = providerPresentation(origin.source_provider);
  const mode =
    origin.conversion_mode === "native"
      ? "原生模式"
      : origin.conversion_mode === "simple"
        ? "简洁模式"
        : "转换模式未知";
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Badge
          variant="outline"
          aria-label={`转换来源：${source.label}`}
          className={cn("h-5 gap-1 px-1.5 text-[11px] font-normal", source.className)}
        >
          <ArrowLeftRight className="h-3 w-3" />
          来自 {source.label}
        </Badge>
      </TooltipTrigger>
      <TooltipContent className="max-w-80 space-y-1">
        <div>由 CC Sessions 从 {source.label} 会话转换 · {mode}</div>
        <div className="font-mono text-[11px] opacity-80">原会话 {origin.source_id}</div>
        <div className="text-[11px] opacity-70">转换于 {formatConversionTime(origin.converted_at)}</div>
      </TooltipContent>
    </Tooltip>
  );
}

function providerPresentation(provider: string): { label: string; className: string } {
  if (provider === "codex") {
    return {
      label: "Codex",
      className:
        "border-[#10A37F]/35 bg-[#10A37F]/10 text-[#087A60] dark:text-[#6EE7C2]",
    };
  }
  if (provider === "claude") {
    return {
      label: "Claude",
      className:
        "border-[#D97757]/40 bg-[#D97757]/10 text-[#B3563B] dark:text-[#F2A386]",
    };
  }
  return {
    label: provider || "未知",
    className: "border-border text-muted-foreground",
  };
}

function formatConversionTime(value: string): string {
  const timestamp = Date.parse(value);
  if (!Number.isFinite(timestamp)) return value;
  return new Date(timestamp).toLocaleString(undefined, { hour12: false });
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
