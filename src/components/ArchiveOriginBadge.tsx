import { Badge } from "@/components/ui/badge";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import type { ArchiveOrigin } from "@/lib/api";
import { archiveOriginPresentation } from "@/lib/archiveOrigin";
import { cn } from "@/lib/utils";

/**
 * 归档来源徽标：SessionCard 与 FamilyHistorySheet 共用。
 *
 * 为什么独立成组件：归档来源的解释文案与配色集中在 archiveOriginPresentation 纯函数，
 * 两个视图只要传入同一 origin 就会显示一致的原因徽标；presentation 返回 null
 * （manual/official/fork 这类高价值来源）时组件不渲染，避免叠加冗余徽章。
 *
 * 交互：仅当来源为 unknown/null 且提供 onSetOrigin 回调时，徽标变为可点击下拉，
 * 允许用户手动指定归档来源（写入 ledger 并同步 family 分支字段）。
 */
export function ArchiveOriginBadge({
  origin,
  onSetOrigin,
}: {
  origin: ArchiveOrigin | null;
  onSetOrigin?: (origin: ArchiveOrigin) => Promise<void> | void;
}) {
  const presentation = archiveOriginPresentation(origin);
  if (!presentation) return null;
  const interactive = Boolean(onSetOrigin) && (origin === "unknown" || origin === null);

  if (interactive) {
    return (
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Badge
            variant="outline"
            aria-label={`归档来源：${presentation.label}（点击切换）`}
            className={cn(
              "h-5 cursor-pointer px-1.5 text-[11px] font-normal transition-colors",
              "border-primary/40 text-primary hover:bg-primary/10 hover:border-primary/60",
            )}
          >
            {presentation.label}
          </Badge>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" className="min-w-40">
          <DropdownMenuLabel>指定归档来源</DropdownMenuLabel>
          <DropdownMenuItem onClick={() => onSetOrigin?.("manual")}>
            手动归档
          </DropdownMenuItem>
          <DropdownMenuItem onClick={() => onSetOrigin?.("provider_sync")}>
            同步分支（切换模型服务）
          </DropdownMenuItem>
          <DropdownMenuItem onClick={() => onSetOrigin?.("restore")}>
            迁移记录（备份恢复）
          </DropdownMenuItem>
          <DropdownMenuItem onClick={() => onSetOrigin?.("import")}>
            迁移记录（会话导入）
          </DropdownMenuItem>
          <DropdownMenuSeparator />
          <DropdownMenuItem onClick={() => onSetOrigin?.("unknown")}>
            保持未知
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    );
  }

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Badge
          variant="outline"
          aria-label={`归档来源：${presentation.label}`}
          className={cn("h-5 px-1.5 text-[11px] font-normal", presentation.className)}
        >
          {presentation.label}
        </Badge>
      </TooltipTrigger>
      <TooltipContent>归档来源：{presentation.label}</TooltipContent>
    </Tooltip>
  );
}