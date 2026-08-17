import { memo, useCallback } from "react";

import { SessionCard } from "@/components/SessionCard";
import type { ArchiveOrigin, FamilyOverlay, SessionSummary } from "@/lib/api";
import { sessionIdentity } from "@/lib/sessionIdentity";
import { useSelection } from "@/stores/selection";

export type SessionCardHandlers = {
  onPreview: (session: SessionSummary) => void;
  onCopyResume: (session: SessionSummary) => void;
  onRevealCwd: (session: SessionSummary) => void;
  onArchiveToggle?: (session: SessionSummary) => void;
  onBackup?: (session: SessionSummary) => void;
  onDelete?: (session: SessionSummary) => void;
  onClone?: (session: SessionSummary) => void;
  onDuplicate?: (session: SessionSummary) => void;
  onOpenFamily?: (session: SessionSummary) => void;
  onExportMarkdown?: (session: SessionSummary) => void;
  onConvert?: (session: SessionSummary) => void;
  onRename?: (session: SessionSummary) => void;
  onMoveCwd?: (session: SessionSummary) => void;
  onSetArchiveOrigin?: (session: SessionSummary, origin: ArchiveOrigin) => Promise<void> | void;
};

type Props = SessionCardHandlers & {
  session: SessionSummary;
  query?: string;
  showProject?: boolean;
  overlay?: FamilyOverlay;
  currentProvider?: string | null;
  syncing?: boolean;
  syncDisabled?: boolean;
  duplicating?: boolean;
};

export const SelectableSessionCard = memo(function SelectableSessionCard({
  session,
  ...props
}: Props) {
  const identity = sessionIdentity(session);
  const selected = useSelection((state) => state.selected.has(identity));
  const toggle = useSelection((state) => state.toggle);
  const onToggleSelect = useCallback(() => toggle(identity), [identity, toggle]);

  return (
    <SessionCard
      s={session}
      selected={selected}
      onToggleSelect={onToggleSelect}
      {...props}
    />
  );
});
