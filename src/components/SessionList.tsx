import { memo, useMemo, type RefObject } from "react";

import { ProjectSessionView } from "@/components/ProjectSessionView";
import type { SessionCardHandlers } from "@/components/SelectableSessionCard";
import { SizeSessionView } from "@/components/SizeSessionView";
import { TimeSessionView } from "@/components/TimeSessionView";
import type { SessionListViewProps } from "@/components/SessionListRowCard";
import type { FamilyOverlay, SessionSummary } from "@/lib/api";
import { sessionIdentity } from "@/lib/sessionIdentity";
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

  const viewProps: SessionListViewProps = {
    sessions: visibleSessions,
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
  return <TimeSessionView {...viewProps} />;
});
