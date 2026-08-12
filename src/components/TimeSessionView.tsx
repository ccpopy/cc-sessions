import { memo, useCallback, useMemo, useState } from "react";
import { ChevronDown } from "lucide-react";

import {
  SessionListRowCard,
  type SessionListViewProps,
} from "@/components/SessionListRowCard";
import { VirtualList } from "@/components/VirtualList";
import { bucketLabel } from "@/lib/format";
import {
  buildTimeListRows,
  type SessionBucketKey,
} from "@/lib/sessionListModel";
import { cn } from "@/lib/utils";

export function TimeSessionView({
  sessions,
  scrollElementRef,
  ...cardProps
}: SessionListViewProps) {
  const [collapsed, setCollapsed] = useState<Record<SessionBucketKey, boolean>>({
    today: false,
    yesterday: false,
    week: false,
    month: false,
    earlier: true,
  });
  const rows = useMemo(
    () => buildTimeListRows(sessions, collapsed),
    [collapsed, sessions],
  );
  const toggleBucket = useCallback((bucket: SessionBucketKey) => {
    setCollapsed((current) => ({ ...current, [bucket]: !current[bucket] }));
  }, []);

  return (
    <VirtualList
      rows={rows}
      scrollElementRef={scrollElementRef}
      getRowKey={(row) => row.key}
      estimateSize={(row) => (row.type === "bucket" ? 28 : 190)}
      renderRow={(row) =>
        row.type === "bucket" ? (
          <TimeBucketHeader
            bucket={row.bucket}
            count={row.count}
            collapsed={row.collapsed}
            onToggle={toggleBucket}
          />
        ) : (
          <SessionListRowCard row={row} {...cardProps} />
        )
      }
    />
  );
}

const TimeBucketHeader = memo(function TimeBucketHeader({
  bucket,
  count,
  collapsed,
  onToggle,
}: {
  bucket: SessionBucketKey;
  count: number;
  collapsed: boolean;
  onToggle: (bucket: SessionBucketKey) => void;
}) {
  return (
    <button
      type="button"
      className="group flex w-full items-center gap-2.5 rounded-md px-1.5 py-1 transition-colors hover:bg-muted/40"
      onClick={() => onToggle(bucket)}
      aria-expanded={!collapsed}
    >
      <ChevronDown
        className={cn(
          "h-3.5 w-3.5 shrink-0 text-muted-foreground/80 transition-transform duration-200 group-hover:text-foreground",
          collapsed && "-rotate-90",
        )}
      />
      <h2 className="text-[13px] font-semibold tracking-tight text-foreground">
        {bucketLabel[bucket]}
      </h2>
      <span className="inline-flex h-5 min-w-[1.5rem] items-center justify-center rounded-md border border-border/60 bg-muted/40 px-1.5 text-[10.5px] font-medium tabular-nums text-muted-foreground">
        {count}
      </span>
      <div
        aria-hidden="true"
        className="ml-1 h-px flex-1 bg-gradient-to-r from-border via-border/60 to-transparent"
      />
    </button>
  );
});
