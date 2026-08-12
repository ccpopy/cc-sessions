import type { RefObject } from "react";

import {
  SelectableSessionCard,
  type SessionCardHandlers,
} from "@/components/SelectableSessionCard";
import type { FamilyOverlay, SessionSummary } from "@/lib/api";
import type { SessionListRow } from "@/lib/sessionListModel";

export type SessionListViewProps = {
  sessions: SessionSummary[];
  handlers: SessionCardHandlers;
  query: string;
  scrollElementRef: RefObject<HTMLDivElement | null>;
  overlay?: Map<string, FamilyOverlay>;
  currentProvider?: string | null;
  syncingSessionIds?: ReadonlySet<string>;
  syncActionsDisabled?: boolean;
  duplicatingSessionIds?: ReadonlySet<string>;
};

export function SessionListRowCard({
  row,
  handlers,
  query,
  overlay,
  currentProvider,
  syncingSessionIds,
  syncActionsDisabled,
  duplicatingSessionIds,
}: Omit<SessionListViewProps, "sessions" | "scrollElementRef"> & {
  row: SessionListRow;
}) {
  const session = row.session;
  return (
    <SelectableSessionCard
      session={session}
      query={query}
      showProject={row.showProject}
      overlay={overlay?.get(session.id)}
      currentProvider={currentProvider}
      syncing={syncingSessionIds?.has(session.id)}
      syncDisabled={syncActionsDisabled}
      duplicating={duplicatingSessionIds?.has(session.id)}
      {...handlers}
    />
  );
}
