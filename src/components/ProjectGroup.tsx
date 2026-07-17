import { useState } from "react";
import { ChevronDown, FolderKanban } from "lucide-react";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { SessionCard } from "@/components/SessionCard";
import type { FamilyOverlay, SessionSummary } from "@/lib/api";
import { useSelection } from "@/stores/selection";
import { humanTokens, relativeTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import { sessionIdentity } from "@/lib/sessionIdentity";

type Handlers = {
  onPreview: (s: SessionSummary) => void;
  onCopyResume: (s: SessionSummary) => void;
  onRevealCwd: (s: SessionSummary) => void;
  onArchiveToggle?: (s: SessionSummary) => void;
  onBackup: (s: SessionSummary) => void;
  onDelete?: (s: SessionSummary) => void;
  onClone?: (s: SessionSummary) => void;
  onOpenFamily?: (s: SessionSummary) => void;
  onExportMarkdown?: (s: SessionSummary) => void;
};

type Props = {
  cwd: string;
  cwdDisplay: string;
  sessions: SessionSummary[];
  query: string;
  handlers: Handlers;
  defaultOpen?: boolean;
  overlay?: Map<string, FamilyOverlay>;
  currentProvider?: string | null;
  syncingSessionIds?: ReadonlySet<string>;
  syncActionsDisabled?: boolean;
};

export function ProjectGroupView({
  cwd,
  cwdDisplay,
  sessions,
  query,
  handlers,
  defaultOpen = false,
  overlay,
  currentProvider,
  syncingSessionIds,
  syncActionsDisabled,
}: Props) {
  const [open, setOpen] = useState(defaultOpen);
  const selected = useSelection((s) => s.selected);
  const toggle = useSelection((s) => s.toggle);
  const addMany = useSelection((s) => s.addMany);
  const removeMany = useSelection((s) => s.removeMany);

  const ids = sessions.map(sessionIdentity);
  const allSelected = ids.every((id) => selected.has(id));
  const someSelected = !allSelected && ids.some((id) => selected.has(id));

  const latest = Math.max(...sessions.map((s) => s.updated_at));
  const tokens = sessions.reduce((a, b) => a + b.tokens_used, 0);

  return (
    <Collapsible open={open} onOpenChange={setOpen} className="min-w-0 overflow-hidden rounded-lg border bg-card shadow-sm">
      <CollapsibleTrigger asChild>
        <div
          className={cn(
            "flex min-w-0 cursor-pointer items-center gap-3 px-4 py-3 transition-colors hover:bg-accent/40",
            open && "border-b bg-muted/30",
          )}
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
          <Button
            variant="ghost"
            size="sm"
            onClick={(e) => {
              e.stopPropagation();
              if (allSelected) removeMany(ids);
              else addMany(ids);
            }}
            className="h-8"
          >
            {allSelected ? "全不选" : someSelected ? "补全选" : "全选"}
          </Button>
          <ChevronDown
            className={cn("h-4 w-4 shrink-0 text-muted-foreground transition-transform", open && "rotate-180")}
          />
        </div>
      </CollapsibleTrigger>
      <CollapsibleContent>
        <div className="min-w-0 space-y-3 bg-muted/20 p-4">
          {sessions.map((s) => (
            <SessionCard
              key={sessionIdentity(s)}
              s={s}
              selected={selected.has(sessionIdentity(s))}
              onToggleSelect={() => toggle(sessionIdentity(s))}
              query={query}
              showProject={false}
              overlay={overlay?.get(s.id)}
              currentProvider={currentProvider}
              syncing={syncingSessionIds?.has(s.id)}
              syncDisabled={syncActionsDisabled}
              {...handlers}
            />
          ))}
        </div>
      </CollapsibleContent>
    </Collapsible>
  );
}
