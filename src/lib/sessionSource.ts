import type { FamilyOverlay, SessionSummary } from "@/lib/api";

export type CodexThreadSpawnSource = {
  parentThreadId: string;
  depth: number;
  agentPath: string;
  agentNickname: string | null;
  agentRole: string | null;
};

export type RelatedSubagentSession = {
  id: string;
  parentThreadId: string;
  depth: number;
  relativeDepth: number;
  agentPath: string;
  nickname: string | null;
  role: string | null;
  createdAt: number;
  updatedAt: number;
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
  const agentPath = requiredText(threadSpawn.agent_path);
  const depth = threadSpawn.depth;
  if (
    !parentThreadId ||
    !SESSION_ID_PATTERN.test(parentThreadId) ||
    !agentPath ||
    !agentPath.startsWith("/") ||
    typeof depth !== "number" ||
    !Number.isSafeInteger(depth) ||
    depth < 1
  ) {
    return null;
  }

  return {
    parentThreadId,
    depth,
    agentPath,
    agentNickname: optionalText(threadSpawn.agent_nickname),
    agentRole: optionalText(threadSpawn.agent_role),
  };
}

export function collectRelatedSubagents(
  rootSessionId: string,
  sessions: readonly SessionSummary[],
): RelatedSubagentSession[] {
  if (!SESSION_ID_PATTERN.test(rootSessionId)) return [];

  const childrenByParent = new Map<
    string,
    Array<{ session: SessionSummary; source: CodexThreadSpawnSource }>
  >();
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
