import type { FamilyOverlay, SessionSummary } from "@/lib/api";

export type CodexThreadSpawnSource = {
  parentThreadId: string;
  depth: number;
  /** 新版 Codex 不再写入 agent_path（实测为 null），路径信息改为可选。 */
  agentPath: string | null;
  agentNickname: string | null;
  agentRole: string | null;
};

export type RelatedSubagentSession = {
  id: string;
  parentThreadId: string;
  depth: number;
  relativeDepth: number;
  agentPath: string | null;
  nickname: string | null;
  role: string | null;
  createdAt: number;
  updatedAt: number;
};

export type SubagentChildEntry = {
  session: SessionSummary;
  source: CodexThreadSpawnSource;
};

const SESSION_ID_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function isSubagentSession(
  session: SessionSummary,
  overlay?: FamilyOverlay,
): boolean {
  return (
    overlay?.clone_state === "subagent" ||
    hasText(session.agent_nickname) ||
    hasText(session.agent_role) ||
    isSubagentSource(session.source)
  );
}

export function isSubagentSource(source: string | null | undefined): boolean {
  const normalized = source?.trim();
  if (!normalized) return false;
  if (normalized.toLowerCase() === "subagent") return true;
  const separator = normalized.indexOf(":");
  if (
    separator > 0 &&
    normalized.slice(0, separator).toLowerCase() === "parent" &&
    normalized.slice(separator + 1).trim()
  ) {
    return true;
  }
  try {
    const parsed = JSON.parse(normalized);
    return !!parsed && typeof parsed === "object" && "subagent" in parsed;
  } catch {
    return false;
  }
}

export function parseCodexThreadSpawnSource(
  source: string | null | undefined,
): CodexThreadSpawnSource | null {
  const normalized = source?.trim();
  if (!normalized) return null;

  let parsed: unknown;
  try {
    parsed = JSON.parse(normalized);
  } catch {
    return null;
  }

  if (!isRecord(parsed)) return null;
  const subagent = parsed.subagent;
  if (!isRecord(subagent)) return null;
  const threadSpawn = subagent.thread_spawn;
  if (!isRecord(threadSpawn)) return null;

  const parentThreadId = requiredText(threadSpawn.parent_thread_id);
  const depth = threadSpawn.depth;
  if (
    !parentThreadId ||
    !SESSION_ID_PATTERN.test(parentThreadId) ||
    typeof depth !== "number" ||
    !Number.isSafeInteger(depth) ||
    depth < 1
  ) {
    return null;
  }

  return {
    parentThreadId,
    depth,
    agentPath: optionalText(threadSpawn.agent_path),
    agentNickname: optionalText(threadSpawn.agent_nickname),
    agentRole: optionalText(threadSpawn.agent_role),
  };
}

export function collectRelatedSubagents(
  rootSessionId: string,
  sessions: readonly SessionSummary[],
): RelatedSubagentSession[] {
  if (!SESSION_ID_PATTERN.test(rootSessionId)) return [];

  const childrenByParent = buildSubagentChildrenIndex(sessions);

  const related: RelatedSubagentSession[] = [];
  const visited = new Set<string>([rootSessionId]);
  const visit = (parentThreadId: string, relativeDepth: number) => {
    for (const { session, source } of childrenByParent.get(parentThreadId) ?? []) {
      if (visited.has(session.id)) continue;
      visited.add(session.id);
      related.push({
        id: session.id,
        parentThreadId: source.parentThreadId,
        depth: source.depth,
        relativeDepth,
        agentPath: source.agentPath,
        nickname: optionalText(session.agent_nickname) ?? source.agentNickname,
        role: optionalText(session.agent_role) ?? source.agentRole,
        createdAt: session.created_at,
        updatedAt: session.updated_at,
      });
      visit(session.id, relativeDepth + 1);
    }
  };

  visit(rootSessionId, 1);
  return related;
}

/**
 * 构建 Codex 子代理的 parent_thread_id → 直接子节点索引。
 *
 * 为什么独立成函数：单会话的后代收集（collectRelatedSubagents）与子代理
 * 视图的父分组（buildSubagentParentGroups）共用同一份索引与排序规则
 * （创建时间升序、id 稳定排序），抽出来避免两处各写一遍。
 */
export function buildSubagentChildrenIndex(
  sessions: readonly SessionSummary[],
): Map<string, SubagentChildEntry[]> {
  const childrenByParent = new Map<string, SubagentChildEntry[]>();
  for (const session of sessions) {
    if (session.provider !== "codex" || !SESSION_ID_PATTERN.test(session.id)) continue;
    const source = parseCodexThreadSpawnSource(session.source);
    if (!source) continue;
    const children = childrenByParent.get(source.parentThreadId) ?? [];
    children.push({ session, source });
    childrenByParent.set(source.parentThreadId, children);
  }

  for (const children of childrenByParent.values()) {
    children.sort(
      (left, right) =>
        left.session.created_at - right.session.created_at ||
        left.session.id.localeCompare(right.session.id),
    );
  }
  return childrenByParent;
}

/** 子代理视图末尾「父会话缺失」固定分组的 key。 */
export const MISSING_PARENT_GROUP_KEY = "missing-parent";

export type SubagentParentGroup = {
  key: string;
  parent: SessionSummary | null;
  descendants: Array<{ session: SessionSummary; relativeDepth: number }>;
  latest: number;
};

type SubagentRootResolution = {
  /** present：根会话在数据集内；missing：只解析到已删除父会话的 id。 */
  kind: "present" | "missing";
  rootId: string;
};

/**
 * 把可见子代理按"根父对话"分组（仅 Codex：只有 source JSON 能解析出
 * parent_thread_id 与嵌套深度）。
 *
 * 数据流契约：subagentSessions 是搜索/视图过滤后的可见集合，allSessions
 * 是全量数据——父链回溯与组头卡片查找都用 allSessions，不受搜索影响，
 * 因此搜索只命中子代理时它仍归入原父分组；根会话不在数据集（已删除）
 * 或父子链成环无法解析时，才落入末尾的「父会话缺失」组。分组按组内
 * （含父卡片）最新 updated_at 降序，缺失组固定排在最后。
 */
export function buildSubagentParentGroups(
  subagentSessions: readonly SessionSummary[],
  allSessions: readonly SessionSummary[],
): SubagentParentGroup[] {
  const sessionById = new Map<string, SessionSummary>();
  for (const session of allSessions) sessionById.set(session.id, session);
  const visibleById = new Map<string, SessionSummary>();
  for (const session of subagentSessions) visibleById.set(session.id, session);

  const roots = new Map<string, SessionSummary>();
  const missingRootIds: string[] = [];
  const seenMissingRootIds = new Set<string>();
  const unresolvable: SessionSummary[] = [];
  for (const session of subagentSessions) {
    const resolution = resolveSubagentRoot(session, sessionById);
    if (!resolution) {
      unresolvable.push(session);
    } else if (resolution.kind === "present") {
      const root = sessionById.get(resolution.rootId);
      if (root && !roots.has(resolution.rootId)) roots.set(resolution.rootId, root);
    } else if (!seenMissingRootIds.has(resolution.rootId)) {
      seenMissingRootIds.add(resolution.rootId);
      missingRootIds.push(resolution.rootId);
    }
  }

  const assigned = new Set<string>();
  const rootGroups: SubagentParentGroup[] = [];
  for (const [rootId, root] of roots) {
    const descendants: SubagentParentGroup["descendants"] = [];
    let latest = root.updated_at;
    for (const item of collectRelatedSubagents(rootId, allSessions)) {
      if (assigned.has(item.id)) continue;
      const session = visibleById.get(item.id);
      if (!session) continue;
      assigned.add(item.id);
      descendants.push({ session, relativeDepth: item.relativeDepth });
      latest = Math.max(latest, session.updated_at);
    }
    if (descendants.length === 0) continue;
    rootGroups.push({ key: rootId, parent: root, descendants, latest });
  }
  rootGroups.sort(
    (left, right) => right.latest - left.latest || left.key.localeCompare(right.key),
  );

  const missingDescendants: SubagentParentGroup["descendants"] = [];
  let missingLatest = Number.NEGATIVE_INFINITY;
  const pushMissing = (session: SessionSummary, relativeDepth: number) => {
    missingDescendants.push({ session, relativeDepth });
    missingLatest = Math.max(missingLatest, session.updated_at);
  };
  for (const rootId of missingRootIds) {
    for (const item of collectRelatedSubagents(rootId, allSessions)) {
      if (assigned.has(item.id)) continue;
      const session = visibleById.get(item.id);
      if (!session) continue;
      assigned.add(item.id);
      pushMissing(session, item.relativeDepth);
    }
  }
  for (const session of unresolvable) {
    if (assigned.has(session.id)) continue;
    assigned.add(session.id);
    pushMissing(session, 1);
  }

  if (missingDescendants.length === 0) return rootGroups;
  return [
    ...rootGroups,
    {
      key: MISSING_PARENT_GROUP_KEY,
      parent: null,
      descendants: missingDescendants,
      latest: missingLatest,
    },
  ];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function requiredText(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

function optionalText(value: unknown): string | null {
  return requiredText(value);
}

function hasText(value: string | null | undefined): boolean {
  return !!value?.trim();
}

/**
 * 沿 parent 链向上找根会话（visited 防环）。
 *
 * 返回 null 表示无法解析（初始会话没有可解析的 Codex source，或父子链
 * 成环）；missing 表示链条顶端父 id 不在数据集内——该 id 仍可作为
 * collectRelatedSubagents 的 DFS 根，保住缺失组内部的层级与顺序。
 */
function resolveSubagentRoot(
  session: SessionSummary,
  sessionById: ReadonlyMap<string, SessionSummary>,
): SubagentRootResolution | null {
  let current: SessionSummary = session;
  const visited = new Set<string>([session.id]);
  for (;;) {
    const source =
      current.provider === "codex" ? parseCodexThreadSpawnSource(current.source) : null;
    if (!source) {
      return current === session ? null : { kind: "present", rootId: current.id };
    }
    if (visited.has(source.parentThreadId)) return null;
    visited.add(source.parentThreadId);
    const parent = sessionById.get(source.parentThreadId);
    if (!parent) return { kind: "missing", rootId: source.parentThreadId };
    current = parent;
  }
}
