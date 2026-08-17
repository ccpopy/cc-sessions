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
 * （manual/official/fork 这类高价值来源，以及方案 A 后的 unknown）时组件不渲染，
 * 避免叠加冗余徽章。
 *
 * 交互：unknown/null 且提供 onSetOrigin 回调时，徽标变为可点击下拉（显示
 * "来源未知"交互徽标），允许用户手动指定归档来源（写入 ledger 并同步 family
 * 分支字段）。注意：交互判断必须先于 presentation 检查——方案 A 后 unknown 的
 * presentation 为 null，若先 return 会让手动指定来源的入口失效。
 */
export function ArchiveOriginBadge({
  origin,
  onSetOrigin,
}: {
  origin: ArchiveOrigin | null;
  onSetOrigin?: (origin: ArchiveOrigin) => Promise<void> | void;
}) {
  const interactive = Boolean(onSetOrigin) && (origin === "unknown" || origin === null);
  const presentation = archiveOriginPresentation(origin);
  if (interactive) {
    return (
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Badge
            variant="outline"
            aria-label="归档来源：未知（点击指定）"
            className={cn(
              "h-5 cursor-pointer px-1.5 text-[11px] font-normal transition-colors",
              "border-primary/40 text-primary hover:bg-primary/10 hover:border-primary/60",
            )}
          >
            来源未知
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

  if (!presentation) return null;

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