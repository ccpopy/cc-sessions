import type { SessionSummary } from "./api.ts";
import { dayBucket } from "./format.ts";
import { sessionIdentity } from "./sessionIdentity.ts";

export type SessionBucketKey = "today" | "yesterday" | "week" | "month" | "earlier";

export type SessionListRow = {
  type: "session";
  key: string;
  session: SessionSummary;
  showProject?: boolean;
};

export type TimeListRow =
  | {
      type: "bucket";
      key: string;
      bucket: SessionBucketKey;
      count: number;
      collapsed: boolean;
    }
  | SessionListRow;

export type ProjectListGroup = {
  cwd: string;
  cwdDisplay: string;
  items: SessionSummary[];
  latest: number;
};

export type ProjectListRow =
  | {
      type: "project";
      key: string;
      group: ProjectListGroup;
      open: boolean;
    }
  | SessionListRow;

const BUCKET_ORDER: SessionBucketKey[] = [
  "today",
  "yesterday",
  "week",
  "month",
  "earlier",
];

export function buildTimeListRows(
  sessions: readonly SessionSummary[],
  collapsed: Readonly<Record<SessionBucketKey, boolean>>,
): TimeListRow[] {
  const grouped = new Map<SessionBucketKey, SessionSummary[]>();
  for (const session of sessions) {
    const bucket = dayBucket(session.updated_at);
    const items = grouped.get(bucket);
    if (items) items.push(session);
    else grouped.set(bucket, [session]);
  }

  const rows: TimeListRow[] = [];
  for (const bucket of BUCKET_ORDER) {
    const items = grouped.get(bucket);
    if (!items) continue;
    rows.push({
      type: "bucket",
      key: `bucket:${bucket}`,
      bucket,
      count: items.length,
      collapsed: collapsed[bucket],
    });
    if (collapsed[bucket]) continue;
    for (const session of items) {
      rows.push({
        type: "session",
        key: `time:${sessionIdentity(session)}`,
        session,
      });
    }
  }
  return rows;
}

export function buildProjectListGroups(
  sessions: readonly SessionSummary[],
): ProjectListGroup[] {
  const grouped = new Map<string, SessionSummary[]>();
  for (const session of sessions) {
    const items = grouped.get(session.cwd);
    if (items) items.push(session);
    else grouped.set(session.cwd, [session]);
  }
  return Array.from(grouped.entries())
    .map(([cwd, items]) => ({
      cwd,
      cwdDisplay: items[0]?.cwd_display ?? cwd,
      items,
      latest: Math.max(...items.map((session) => session.updated_at)),
    }))
    .sort((left, right) => right.latest - left.latest);
}

export function buildProjectListRows(
  groups: readonly ProjectListGroup[],
  expanded: Readonly<Record<string, boolean>>,
): ProjectListRow[] {
  const rows: ProjectListRow[] = [];
  for (const group of groups) {
    const open = Boolean(expanded[group.cwd]);
    rows.push({ type: "project", key: `project:${group.cwd}`, group, open });
    if (!open) continue;
    for (const session of group.items) {
      rows.push({
        type: "session",
        key: `project-session:${group.cwd}:${sessionIdentity(session)}`,
        session,
        showProject: false,
      });
    }
  }
  return rows;
}
