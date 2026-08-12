import { useCallback, useMemo, useState } from "react";

import { ProjectGroupHeader } from "@/components/ProjectGroup";
import {
  SessionListRowCard,
  type SessionListViewProps,
} from "@/components/SessionListRowCard";
import { VirtualList } from "@/components/VirtualList";
import {
  buildProjectListGroups,
  buildProjectListRows,
} from "@/lib/sessionListModel";

export function ProjectSessionView({
  sessions,
  scrollElementRef,
  ...cardProps
}: SessionListViewProps) {
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const groups = useMemo(() => buildProjectListGroups(sessions), [sessions]);
  const rows = useMemo(
    () => buildProjectListRows(groups, expanded),
    [expanded, groups],
  );
  const changeProjectOpen = useCallback((cwd: string, open: boolean) => {
    setExpanded((current) => ({ ...current, [cwd]: open }));
  }, []);

  return (
    <VirtualList
      rows={rows}
      scrollElementRef={scrollElementRef}
      getRowKey={(row) => row.key}
      estimateSize={(row) => (row.type === "project" ? 66 : 180)}
      renderRow={(row) =>
        row.type === "project" ? (
          <ProjectGroupHeader
            cwd={row.group.cwd}
            cwdDisplay={row.group.cwdDisplay}
            sessions={row.group.items}
            open={row.open}
            onOpenChange={changeProjectOpen}
          />
        ) : (
          <div className="pl-4">
            <SessionListRowCard row={row} {...cardProps} />
          </div>
        )
      }
    />
  );
}
