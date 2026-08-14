import { Badge } from "@/components/ui/badge";
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
 */
export function ArchiveOriginBadge({ origin }: { origin: ArchiveOrigin | null }) {
  const presentation = archiveOriginPresentation(origin);
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
