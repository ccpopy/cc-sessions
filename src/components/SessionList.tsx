import { useMemo, useState } from "react";
import { ChevronDown } from "lucide-react";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import type { FamilyOverlay, SessionSummary } from "@/lib/api";
import { SessionCard } from "@/components/SessionCard";
import { ProjectGroupView } from "@/components/ProjectGroup";
import { bucketLabel, dayBucket } from "@/lib/format";
import { useSelection } from "@/stores/selection";
import { useView } from "@/stores/view";
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
  onConvert?: (s: SessionSummary) => void;
  onRename?: (s: SessionSummary) => void;
  onMoveCwd?: (s: SessionSummary) => void;
};

type Props = Handlers & {
  sessions: SessionSummary[];
  backupIndex?: Record<string, string[]>;
  overlay?: Map<string, FamilyOverlay>;
  currentProvider?: string | null;
  syncingSessionIds?: ReadonlySet<string>;
  syncActionsDisabled?: boolean;
};

type BucketKey = "today" | "yesterday" | "week" | "month" | "earlier";

export function SessionList({
  sessions,
  backupIndex,
  overlay,
  currentProvider,
  syncingSessionIds,
  syncActionsDisabled,
  ...h
}: Props) {
  const view = useView((s) => s.view);
  const query = useView((s) => s.query);
  const prefillCwd = useView((s) => s.prefillCwd);

  const filtered = useMemo(() => {
    if (!prefillCwd) return sessions;
    return sessions.filter((s) => s.cwd === prefillCwd);
  }, [sessions, prefillCwd]);

  const enriched = useMemo(
    () =>
      filtered.map((s) => ({
        ...s,
        has_backup: backupIndex ? !!backupIndex[sessionIdentity(s)]?.length : s.has_backup,
      })),
    [filtered, backupIndex],
  );

  if (view === "project") {
    return (
      <ProjectView
        sessions={enriched}
        handlers={h}
        query={query}
        overlay={overlay}
        currentProvider={currentProvider}
        syncingSessionIds={syncingSessionIds}
        syncActionsDisabled={syncActionsDisabled}
      />
    );
  }

  if (view === "size") {
    return (
      <SizeView
        sessions={enriched}
        handlers={h}
        query={query}
        overlay={overlay}
        currentProvider={currentProvider}
        syncingSessionIds={syncingSessionIds}
        syncActionsDisabled={syncActionsDisabled}
      />
    );
  }

  return (
    <TimeView
      sessions={enriched}
      handlers={h}
      query={query}
      overlay={overlay}
      currentProvider={currentProvider}
      syncingSessionIds={syncingSessionIds}
      syncActionsDisabled={syncActionsDisabled}
    />
  );
}

function TimeView({
  sessions,
  handlers,
  query,
  overlay,
  currentProvider,
  syncingSessionIds,
  syncActionsDisabled,
}: {
  sessions: SessionSummary[];
  handlers: Handlers;
  query: string;
  overlay?: Map<string, FamilyOverlay>;
  currentProvider?: string | null;
  syncingSessionIds?: ReadonlySet<string>;
  syncActionsDisabled?: boolean;
}) {
  const selected = useSelection((s) => s.selected);
  const toggle = useSelection((s) => s.toggle);
  const [collapsed, setCollapsed] = useState<Record<BucketKey, boolean>>({
    today: false,
    yesterday: false,
    week: false,
    month: false,
    earlier: true,
  });

  const groups = useMemo(() => {
    const map = new Map<BucketKey, SessionSummary[]>();
    for (const s of sessions) {
      const k = dayBucket(s.updated_at);
      if (!map.has(k)) map.set(k, []);
      map.get(k)!.push(s);
    }
    const order: BucketKey[] = ["today", "yesterday", "week", "month", "earlier"];
    return order.filter((k) => map.has(k)).map((k) => ({ key: k, items: map.get(k)! }));
  }, [sessions]);

  return (
    <div className="min-w-0 max-w-full space-y-5 overflow-hidden px-6 py-5">
      {groups.map((g) => (
        <Collapsible
          key={g.key}
          open={!collapsed[g.key]}
          onOpenChange={(open) =>
            setCollapsed((s) => ({ ...s, [g.key]: !open }))
          }
          className="min-w-0"
        >
          <CollapsibleTrigger asChild>
            <button className="group flex w-full items-center gap-2.5 rounded-md px-1.5 py-1 transition-colors hover:bg-muted/40">
              <ChevronDown
                className={cn(
                  "h-3.5 w-3.5 shrink-0 text-muted-foreground/80 transition-transform duration-200 group-hover:text-foreground",
                  collapsed[g.key] && "-rotate-90",
                )}
              />
              <h2 className="text-[13px] font-semibold tracking-tight text-foreground">
                {bucketLabel[g.key]}
              </h2>
              <span className="inline-flex h-5 min-w-[1.5rem] items-center justify-center rounded-md border border-border/60 bg-muted/40 px-1.5 text-[10.5px] font-medium tabular-nums text-muted-foreground">
                {g.items.length}
              </span>
              <div
                aria-hidden="true"
                className="ml-1 h-px flex-1 bg-gradient-to-r from-border via-border/60 to-transparent"
              />
            </button>
          </CollapsibleTrigger>
          <CollapsibleContent className="data-[state=open]:animate-accordion-down data-[state=closed]:animate-accordion-up overflow-hidden">
            <div className="mt-3 min-w-0 space-y-3">
              {g.items.map((s) => (
                <SessionCard
                  key={sessionIdentity(s)}
                  s={s}
                  selected={selected.has(sessionIdentity(s))}
                  onToggleSelect={() => toggle(sessionIdentity(s))}
                  query={query}
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
      ))}
    </div>
  );
}

function ProjectView({
  sessions,
  handlers,
  query,
  overlay,
  currentProvider,
  syncingSessionIds,
  syncActionsDisabled,
}: {
  sessions: SessionSummary[];
  handlers: Handlers;
  query: string;
  overlay?: Map<string, FamilyOverlay>;
  currentProvider?: string | null;
  syncingSessionIds?: ReadonlySet<string>;
  syncActionsDisabled?: boolean;
}) {
  const groups = useMemo(() => {
    const map = new Map<string, SessionSummary[]>();
    for (const s of sessions) {
      if (!map.has(s.cwd)) map.set(s.cwd, []);
      map.get(s.cwd)!.push(s);
    }
    return Array.from(map.entries())
      .map(([cwd, items]) => ({
        cwd,
        cwd_display: items[0]?.cwd_display ?? cwd,
        items,
        latest: Math.max(...items.map((x) => x.updated_at)),
      }))
      .sort((a, b) => b.latest - a.latest);
  }, [sessions]);

  return (
    <div className="min-w-0 max-w-full space-y-3 overflow-hidden px-6 py-5">
      {groups.map((g) => (
        <ProjectGroupView
          key={g.cwd}
          cwd={g.cwd}
          cwdDisplay={g.cwd_display}
          sessions={g.items}
          query={query}
          handlers={handlers}
          overlay={overlay}
          currentProvider={currentProvider}
          syncingSessionIds={syncingSessionIds}
          syncActionsDisabled={syncActionsDisabled}
        />
      ))}
    </div>
  );
}

function SizeView({
  sessions,
  handlers,
  query,
  overlay,
  currentProvider,
  syncingSessionIds,
  syncActionsDisabled,
}: {
  sessions: SessionSummary[];
  handlers: Handlers;
  query: string;
  overlay?: Map<string, FamilyOverlay>;
  currentProvider?: string | null;
  syncingSessionIds?: ReadonlySet<string>;
  syncActionsDisabled?: boolean;
}) {
  const selected = useSelection((s) => s.selected);
  const toggle = useSelection((s) => s.toggle);

  const sorted = useMemo(
    () => [...sessions].sort(compareSessionSizeDesc),
    [sessions],
  );

  return (
    <div className="min-w-0 max-w-full space-y-3 overflow-hidden px-6 py-5">
      {sorted.map((s) => (
        <SessionCard
          key={sessionIdentity(s)}
          s={s}
          selected={selected.has(sessionIdentity(s))}
          onToggleSelect={() => toggle(sessionIdentity(s))}
          query={query}
          overlay={overlay?.get(s.id)}
          currentProvider={currentProvider}
          syncing={syncingSessionIds?.has(s.id)}
          syncDisabled={syncActionsDisabled}
          {...handlers}
        />
      ))}
    </div>
  );
}

function compareSessionSizeDesc(a: SessionSummary, b: SessionSummary): number {
  const tokenDelta = b.tokens_used - a.tokens_used;
  if (tokenDelta !== 0) return tokenDelta;
  const bytesDelta = b.rollout_bytes - a.rollout_bytes;
  if (bytesDelta !== 0) return bytesDelta;
  const updatedDelta = b.updated_at - a.updated_at;
  if (updatedDelta !== 0) return updatedDelta;
  return a.id.localeCompare(b.id);
}
