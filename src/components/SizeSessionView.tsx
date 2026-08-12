import { useMemo } from "react";

import {
  SessionListRowCard,
  type SessionListViewProps,
} from "@/components/SessionListRowCard";
import { VirtualList } from "@/components/VirtualList";
import { sessionIdentity } from "@/lib/sessionIdentity";
import { compareSessionSize } from "@/lib/sessionSort";
import { useView } from "@/stores/view";

export function SizeSessionView({
  sessions,
  scrollElementRef,
  ...cardProps
}: SessionListViewProps) {
  const sizeSortDirection = useView((state) => state.sizeSortDirection);
  const rows = useMemo(
    () =>
      [...sessions]
        .sort((left, right) => compareSessionSize(left, right, sizeSortDirection))
        .map((session) => ({
          type: "session" as const,
          key: `size:${sessionIdentity(session)}`,
          session,
        })),
    [sessions, sizeSortDirection],
  );

  return (
    <VirtualList
      rows={rows}
      scrollElementRef={scrollElementRef}
      getRowKey={(row) => row.key}
      estimateSize={() => 190}
      renderRow={(row) => <SessionListRowCard row={row} {...cardProps} />}
    />
  );
}
