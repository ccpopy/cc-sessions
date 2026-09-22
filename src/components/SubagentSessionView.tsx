import { memo, useCallback, useMemo, useState } from "react";
import { ChevronDown } from "lucide-react";

import {
  SessionListRowCard,
  type SessionListViewProps,
} from "@/components/SessionListRowCard";
import { VirtualList } from "@/components/VirtualList";
import type { SessionSummary } from "@/lib/api";
import { sessionIdentity } from "@/lib/sessionIdentity";
import {
  buildSubagentParentGroups,
  MISSING_PARENT_GROUP_KEY,
  type SubagentParentGroup,
} from "@/lib/sessionSource";
import { cn } from "@/lib/utils";

type Props = SessionListViewProps & {
  /** 全量会话（未过搜索框）：父链回溯与组头卡片查找都用它。 */
  allSessions: SessionSummary[];
};

type SubagentGroupRow =
  | {
      type: "parent-group";
      key: string;
      group: SubagentParentGroup;
      collapsed: boolean;
    }
  | {
      type: "session";
      key: string;
      session: SessionSummary;
      relativeDepth: number;
    };

/**
 * Codex 子代理视图：按父对话分组、层级嵌套展示。
 *
 * 模式沿用归档视图——组头行与会话行混入同一个 VirtualList，保证分组
 * 信息与虚拟滚动共存；组头是父对话的完整 SessionCard（预览/菜单/勾选
 * 能力保留），独立 chevron 按钮控制折叠，命中区与卡片操作互不重叠。
 */
export function SubagentSessionView({
  sessions,
  allSessions,
  scrollElementRef,
  ...cardProps
}: Props) {
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
  const groups = useMemo(
    () => buildSubagentParentGroups(sessions, allSessions),
    [allSessions, sessions],
  );
  const rows = useMemo<SubagentGroupRow[]>(() => {
    const result: SubagentGroupRow[] = [];
    for (const group of groups) {
      const groupCollapsed = Boolean(collapsed[group.key]);
      result.push({
        type: "parent-group",
        key: `group:${group.key}`,
        group,
        collapsed: groupCollapsed,
      });
      if (groupCollapsed) continue;
      for (const item of group.descendants) {
        result.push({
          type: "session",
          key: `subagent:${group.key}:${sessionIdentity(item.session)}`,
          session: item.session,
          relativeDepth: item.relativeDepth,
        });
      }
    }
    return result;
  }, [collapsed, groups]);

  const toggleGroup = useCallback((key: string) => {
    setCollapsed((current) => ({ ...current, [key]: !current[key] }));
  }, []);

  return (
    <VirtualList
      rows={rows}
      scrollElementRef={scrollElementRef}
      getRowKey={(row) => row.key}
      estimateSize={(row) => (row.type === "parent-group" ? 210 : 190)}
      renderRow={(row) =>
        row.type === "parent-group" ? (
          <ParentGroupHeader
            group={row.group}
            collapsed={row.collapsed}
            onToggle={toggleGroup}
            {...cardProps}
          />
        ) : (
          <div style={{ paddingLeft: subagentIndent(row.relativeDepth) }}>
            <SessionListRowCard
              row={{ type: "session", key: row.key, session: row.session }}
              {...cardProps}
            />
          </div>
        )
      }
    />
  );
}

const ParentGroupHeader = memo(function ParentGroupHeader({
  group,
  collapsed,
  onToggle,
  handlers,
  query,
  overlay,
  currentProvider,
  syncingSessionIds,
  syncActionsDisabled,
  duplicatingSessionIds,
}: Omit<SessionListViewProps, "sessions" | "scrollElementRef"> & {
  group: SubagentParentGroup;
  collapsed: boolean;
  onToggle: (key: string) => void;
}) {
  const label = group.key === MISSING_PARENT_GROUP_KEY ? "父会话缺失" : null;
  if (!group.parent) {
    return (
      <button
        type="button"
        className="group flex w-full items-center gap-2.5 rounded-md px-1.5 py-1 transition-colors hover:bg-muted/40"
        onClick={() => onToggle(group.key)}
        aria-expanded={!collapsed}
      >
        <ChevronDown
          className={cn(
            "h-3.5 w-3.5 shrink-0 text-muted-foreground/80 transition-transform duration-200 group-hover:text-foreground",
            collapsed && "-rotate-90",
          )}
        />
        <h2 className="text-[13px] font-semibold tracking-tight text-foreground">{label}</h2>
        <span className="inline-flex h-5 min-w-[1.5rem] items-center justify-center rounded-md border border-border/60 bg-muted/40 px-1.5 text-[10.5px] font-medium tabular-nums text-muted-foreground">
          {group.descendants.length}
        </span>
        <div
          aria-hidden="true"
          className="ml-1 h-px flex-1 bg-gradient-to-r from-border via-border/60 to-transparent"
        />
      </button>
    );
  }
  return (
    <div className="flex w-full items-start gap-1.5">
      <button
        type="button"
        className="group/toggle mt-4 inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-md border border-border/60 bg-background/90 text-muted-foreground shadow-sm transition-colors hover:bg-muted/70 hover:text-foreground"
        aria-label={collapsed ? `展开 ${groupDescendantCount(group)} 个子代理` : `折叠 ${groupDescendantCount(group)} 个子代理`}
        aria-expanded={!collapsed}
        onClick={(event) => {
          event.stopPropagation();
          onToggle(group.key);
        }}
      >
        <ChevronDown
          className={cn(
            "h-3.5 w-3.5 shrink-0 transition-transform duration-200",
            collapsed && "-rotate-90",
          )}
        />
      </button>
      <div className="min-w-0 flex-1">
        <SessionListRowCard
          row={{ type: "session", key: `group-card:${group.key}`, session: group.parent }}
          handlers={handlers}
          query={query}
          overlay={overlay}
          currentProvider={currentProvider}
          syncingSessionIds={syncingSessionIds}
          syncActionsDisabled={syncActionsDisabled}
          duplicatingSessionIds={duplicatingSessionIds}
        />
      </div>
    </div>
  );
});

function groupDescendantCount(group: SubagentParentGroup): number {
  return group.descendants.length;
}

function subagentIndent(relativeDepth: number): number {
  return Math.min((relativeDepth - 1) * 18, 72);
}
