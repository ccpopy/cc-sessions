import { memo, useMemo } from "react";
import { ChevronDown, FolderKanban } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import type { SessionSummary } from "@/lib/api";
import { humanTokens, relativeTime } from "@/lib/format";
import { sessionIdentity } from "@/lib/sessionIdentity";
import { cn } from "@/lib/utils";
import { useSelection } from "@/stores/selection";

type Props = {
  cwd: string;
  cwdDisplay: string;
  sessions: SessionSummary[];
  open: boolean;
  onOpenChange: (cwd: string, open: boolean) => void;
};

export const ProjectGroupHeader = memo(function ProjectGroupHeader({
  cwd,
  cwdDisplay,
  sessions,
  open,
  onOpenChange,
}: Props) {
  const ids = useMemo(() => sessions.map(sessionIdentity), [sessions]);
  const selectedCount = useSelection((state) => {
    let count = 0;
    for (const id of ids) {
      if (state.selected.has(id)) count += 1;
    }
    return count;
  });
  const addMany = useSelection((state) => state.addMany);
  const removeMany = useSelection((state) => state.removeMany);

  const allSelected = ids.length > 0 && selectedCount === ids.length;
  const someSelected = selectedCount > 0 && !allSelected;
  const latest = Math.max(...sessions.map((session) => session.updated_at));
  const tokens = sessions.reduce((total, session) => total + session.tokens_used, 0);

  return (
    <div
      className={cn(
        "flex min-w-0 items-center gap-3 overflow-hidden rounded-lg border bg-card px-4 py-3 shadow-sm transition-colors",
        open && "bg-muted/30",
      )}
    >
      <button
        type="button"
        className="flex min-w-0 flex-1 items-center gap-3 text-left"
        onClick={() => onOpenChange(cwd, !open)}
        aria-expanded={open}
      >
        <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-muted">
          <FolderKanban className="h-4 w-4 text-muted-foreground" />
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <div className="min-w-0 truncate text-sm font-semibold">{cwdDisplay}</div>
            <Badge variant="secondary" className="h-5 px-1.5 font-normal">
              {sessions.length} 条
            </Badge>
            {tokens > 0 && (
              <Badge variant="outline" className="h-5 px-1.5 font-normal text-muted-foreground">
                {humanTokens(tokens)} token
              </Badge>
            )}
          </div>
          <div className="mt-0.5 min-w-0 truncate font-mono text-[11px] text-muted-foreground">
            {cwd}
          </div>
        </div>
        <div className="shrink-0 text-xs text-muted-foreground">{relativeTime(latest)}</div>
        <ChevronDown
          className={cn(
            "h-4 w-4 shrink-0 text-muted-foreground transition-transform",
            open && "rotate-180",
          )}
        />
      </button>
      <Button
        variant="ghost"
        size="sm"
        onClick={() => {
          if (allSelected) removeMany(ids);
          else addMany(ids);
        }}
        className="h-8 shrink-0"
      >
        {allSelected ? "全不选" : someSelected ? "补全选" : "全选"}
      </Button>
    </div>
  );
});
