import { memo, useMemo } from "react";

import {
  SessionListRowCard,
  type SessionListViewProps,
} from "@/components/SessionListRowCard";
import { VirtualList } from "@/components/VirtualList";
import type { ArchiveOrigin } from "@/lib/api";
import { sessionIdentity } from "@/lib/sessionIdentity";
import {
  groupArchivedByOrigin,
  type ArchivedOriginGroup,
  type ArchivedOriginGroupKey,
} from "@/lib/sessionVisibility";

type OriginFilter = ArchivedOriginGroupKey | "all";

type Props = SessionListViewProps & {
  ledgerBySession: ReadonlyMap<string, ArchiveOrigin>;
  originFilter: OriginFilter;
};

type ArchivedOriginRow =
  | {
      type: "origin";
      key: string;
      group: ArchivedOriginGroup;
    }
  | {
      type: "session";
      key: string;
      session: SessionListViewProps["sessions"][number];
    };

/**
 * 归档视图的来源分组列表。
 *
 * 为什么独立成视图：归档会话来自多个来源（手动/自动/迁移），把组头行与
 * 会话行混合进同一个 VirtualList（沿用 TimeSessionView 的 bucket 行模式），
 * 既能显示"我的归档 / 同步归档 / 迁移记录"三个分组，又保持
 * 虚拟滚动在单个滚动容器内工作。
 */
export function ArchivedSessionView({
  sessions,
  scrollElementRef,
  ledgerBySession,
  originFilter,
  ...cardProps
}: Props) {
  const rows = useMemo<ArchivedOriginRow[]>(() => {
    const groups = groupArchivedByOrigin(sessions, ledgerBySession).filter(
      (group) => originFilter === "all" || group.key === originFilter,
    );
    const result: ArchivedOriginRow[] = [];
    for (const group of groups) {
      result.push({ type: "origin", key: `origin:${group.key}`, group });
      for (const session of group.sessions) {
        result.push({
          type: "session",
          key: `archived:${sessionIdentity(session)}`,
          session,
        });
      }
    }
    return result;
  }, [ledgerBySession, originFilter, sessions]);

  return (
    <VirtualList
      rows={rows}
      scrollElementRef={scrollElementRef}
      getRowKey={(row) => row.key}
      estimateSize={(row) => (row.type === "origin" ? 44 : 190)}
      renderRow={(row) =>
        row.type === "origin" ? (
          <OriginGroupHeader label={row.group.label} count={row.group.sessions.length} />
        ) : (
          <SessionListRowCard row={row} {...cardProps} />
        )
      }
    />
  );
}

const OriginGroupHeader = memo(function OriginGroupHeader({
  label,
  count,
}: {
  label: string;
  count: number;
}) {
  return (
    <div className="flex w-full items-center gap-2.5 rounded-md px-1.5 py-1">
      <h2 className="text-[13px] font-semibold tracking-tight text-foreground">{label}</h2>
      <span className="inline-flex h-5 min-w-[1.5rem] items-center justify-center rounded-md border border-border/60 bg-muted/40 px-1.5 text-[10.5px] font-medium tabular-nums text-muted-foreground">
        {count}
      </span>
      <div
        aria-hidden="true"
        className="ml-1 h-px flex-1 bg-gradient-to-r from-border via-border/60 to-transparent"
      />
    </div>
  );
});