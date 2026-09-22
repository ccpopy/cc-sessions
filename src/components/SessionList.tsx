import { memo, useMemo, type RefObject } from "react";

import { ArchivedSessionView } from "@/components/ArchivedSessionView";
import { ProjectSessionView } from "@/components/ProjectSessionView";
import type { SessionCardHandlers } from "@/components/SelectableSessionCard";
import { SizeSessionView } from "@/components/SizeSessionView";
import { SubagentSessionView } from "@/components/SubagentSessionView";
import { TimeSessionView } from "@/components/TimeSessionView";
import type { SessionListViewProps } from "@/components/SessionListRowCard";
import type { ArchiveOrigin, FamilyOverlay, SessionSummary } from "@/lib/api";
import { sessionIdentity } from "@/lib/sessionIdentity";
import {
  filterSessionsByOrigin,
  type ArchivedOriginGroupKey,
} from "@/lib/sessionVisibility";
import { useView } from "@/stores/view";

type Props = SessionCardHandlers & {
  sessions: SessionSummary[];
  scrollElementRef: RefObject<HTMLDivElement | null>;
  backupIndex?: Record<string, string[]>;
  overlay?: Map<string, FamilyOverlay>;
  currentProvider?: string | null;
  syncingSessionIds?: ReadonlySet<string>;
  syncActionsDisabled?: boolean;
  duplicatingSessionIds?: ReadonlySet<string>;
  subagentGrouping?: { allSessions: SessionSummary[] } | null;
  archivedGrouping?: {
    ledgerBySession: ReadonlyMap<string, ArchiveOrigin>;
    originFilter: ArchivedOriginGroupKey | "all";
  } | null;
};

export const SessionList = memo(function SessionList({
  sessions,
  scrollElementRef,
  backupIndex,
  overlay,
  currentProvider,
  syncingSessionIds,
  syncActionsDisabled,
  duplicatingSessionIds,
  subagentGrouping,
  archivedGrouping,
  ...handlers
}: Props) {
  const view = useView((state) => state.view);
  const query = useView((state) => state.query);
  const prefillCwd = useView((state) => state.prefillCwd);

  const visibleSessions = useMemo(() => {
    const filtered = prefillCwd
      ? sessions.filter((session) => session.cwd === prefillCwd)
      : sessions;
    return filtered.map((session) => ({
      ...session,
      has_backup: backupIndex
        ? Boolean(backupIndex[sessionIdentity(session)]?.length)
        : session.has_backup,
    }));
  }, [backupIndex, prefillCwd, sessions]);

  // 归档来源筛选只过滤会话集合，不改变视图分组方式：
  // 工具栏的视图切换（时间/项目/大小）始终优先。
  const viewSessions = useMemo(() => {
    if (!archivedGrouping) return visibleSessions;
    return filterSessionsByOrigin(
      visibleSessions,
      archivedGrouping.ledgerBySession,
      archivedGrouping.originFilter,
    );
  }, [archivedGrouping, visibleSessions]);

  // 父组头卡片取自全量 allSessions（不受搜索影响），需要与可见列表一样
  // 用 backupIndex 补齐 has_backup，避免备份徽标失真。
  const subagentAllSessions = useMemo(() => {
    if (!subagentGrouping) return [];
    return subagentGrouping.allSessions.map((session) => ({
      ...session,
      has_backup: backupIndex
        ? Boolean(backupIndex[sessionIdentity(session)]?.length)
        : session.has_backup,
    }));
  }, [backupIndex, subagentGrouping]);

  const viewProps: SessionListViewProps = {
    sessions: viewSessions,
    handlers,
    query,
    scrollElementRef,
    overlay,
    currentProvider,
    syncingSessionIds,
    syncActionsDisabled,
    duplicatingSessionIds,
  };

  if (view === "project") return <ProjectSessionView {...viewProps} />;
  if (view === "size") return <SizeSessionView {...viewProps} />;
  if (archivedGrouping) {
    return (
      <ArchivedSessionView
        {...viewProps}
        ledgerBySession={archivedGrouping.ledgerBySession}
        originFilter={archivedGrouping.originFilter}
      />
    );
  }
  if (subagentGrouping) {
    return (
      <SubagentSessionView
        {...viewProps}
        allSessions={subagentAllSessions}
      />
    );
  }
  return <TimeSessionView {...viewProps} />;
});
