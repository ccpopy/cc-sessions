import type { ArchiveOrigin, FamilyOverlay, SessionSummary } from "./api";
import { isSubagentSession } from "./sessionSource.ts";

/**
 * 把 ledger 的归档来源合并进 overlay，统一"徽标显示"与"来源筛选"两个数据源。
 *
 * 为什么需要合并：family 分支的 archive_origin 字段只在用户显式指定来源
 * （set_archive_origin 命令同步写 ledger + 分支字段）时更新；backfill 和 fork
 * 创建路径只写 ledger（repair.rs），分支字段仍是 None。若不合并，会出现徽标显示
 * "来源未知"（分支字段为空）但筛选 unknown 时该会话被 ledger 归入其他组的矛盾。
 *
 * 合并规则：ledger 有记录优先用 ledger 值；ledger 缺失时回退 family 分支字段
 * （再回退 null）。返回新数组，不修改入参。
 */
export function mergeLedgerIntoOverlay(
  overlays: readonly FamilyOverlay[],
  ledgerBySession: ReadonlyMap<string, ArchiveOrigin>,
): FamilyOverlay[] {
  return overlays.map((o) => ({
    ...o,
    archive_origin: ledgerBySession.get(o.session_id) ?? o.archive_origin ?? null,
  }));
}

export type SessionVisibilityOptions = {
  provider: SessionSummary["provider"];
  showSubagentSessions: boolean;
  showArchivedSessions: boolean;
  includeHiddenRecords?: boolean;
};

/**
 * Apply the same provider, subagent, archive and family visibility rules used by
 * the sessions page. Full-text search reuses this selector so it cannot surface
 * family branches that the current page deliberately hides.
 */
export function selectSessionsForView(
  sessions: readonly SessionSummary[],
  overlayBySession: ReadonlyMap<string, FamilyOverlay>,
  currentProvider: string | null,
  options: SessionVisibilityOptions,
): SessionSummary[] {
  const isCodex = options.provider === "codex";
  const supportsArchive = isCodex || options.provider === "opencode";
  const includeHiddenRecords = options.includeHiddenRecords ?? false;
  const candidates: SessionSummary[] = [];

  for (const session of sessions) {
    const overlay = isCodex ? overlayBySession.get(session.id) : undefined;
    if (isSubagentSession(session, overlay) !== options.showSubagentSessions) {
      continue;
    }
    if (
      supportsArchive
      && !includeHiddenRecords
      && session.archived !== options.showArchivedSessions
    ) {
      continue;
    }
    candidates.push(session);
  }

  if (isCodex && !includeHiddenRecords && !options.showArchivedSessions) {
    return selectNormalFamilySessions(candidates, overlayBySession, currentProvider);
  }
  return sortSessionsByActivity(candidates);
}

type FamilyCandidate = {
  session: SessionSummary;
  overlay: FamilyOverlay;
};

/**
 * Collapse normal (non-archived) Codex rows into one card per logical family.
 *
 * Scatter provider switches deliberately leave multiple branches usable in
 * `sessions/`. The manager-owned `active_id` can therefore become stale when
 * the user later resumes another branch directly in Codex. The current
 * provider's usable branch is the least surprising representative; only when
 * that branch does not exist do we fall back to the stored active branch.
 */
export function selectNormalFamilySessions(
  sessions: readonly SessionSummary[],
  overlayBySession: ReadonlyMap<string, FamilyOverlay>,
  currentProvider: string | null,
): SessionSummary[] {
  const standalone: SessionSummary[] = [];
  const families = new Map<string, FamilyCandidate[]>();

  for (const session of sessions) {
    const overlay = overlayBySession.get(session.id);
    if (!overlay?.family_id) {
      standalone.push(session);
      continue;
    }
    const candidates = families.get(overlay.family_id) ?? [];
    candidates.push({ session, overlay });
    families.set(overlay.family_id, candidates);
  }

  const selected = [...standalone];
  for (const candidates of families.values()) {
    selected.push(selectFamilyRepresentative(candidates, currentProvider).session);
  }
  return selected.sort(compareSessionActivityDesc);
}

export function sortSessionsByActivity(
  sessions: readonly SessionSummary[],
): SessionSummary[] {
  return [...sessions].sort(compareSessionActivityDesc);
}

function selectFamilyRepresentative(
  candidates: readonly FamilyCandidate[],
  currentProvider: string | null,
): FamilyCandidate {
  const currentProviderCandidates = currentProvider
    ? candidates.filter((candidate) => candidate.overlay.provider === currentProvider)
    : [];
  const activeCandidates = candidates.filter((candidate) => candidate.overlay.is_active_branch);
  const pool =
    currentProviderCandidates.length > 0
      ? currentProviderCandidates
      : activeCandidates.length > 0
        ? activeCandidates
        : candidates;

  return pool.reduce((preferred, candidate) =>
    isNewerFamilyCandidate(candidate, preferred) ? candidate : preferred,
  );
}

function isNewerFamilyCandidate(
  candidate: FamilyCandidate,
  current: FamilyCandidate,
): boolean {
  if (candidate.session.updated_at !== current.session.updated_at) {
    return candidate.session.updated_at > current.session.updated_at;
  }
  if (candidate.overlay.is_active_branch !== current.overlay.is_active_branch) {
    return candidate.overlay.is_active_branch;
  }
  if (candidate.session.created_at !== current.session.created_at) {
    return candidate.session.created_at > current.session.created_at;
  }
  return candidate.session.id.localeCompare(current.session.id) < 0;
}

function compareSessionActivityDesc(left: SessionSummary, right: SessionSummary): number {
  const updatedDelta = right.updated_at - left.updated_at;
  if (updatedDelta !== 0) return updatedDelta;
  const createdDelta = right.created_at - left.created_at;
  if (createdDelta !== 0) return createdDelta;
  return left.id.localeCompare(right.id);
}

/** 归档视图的分组键：按归档来源聚合，是跨 provider 的统一维度（计划 §F3）。
 *  只有三组：未知/无 ledger 记录的归档视为用户主动归档（官方应用归档、旧版
 *  手动归档都不写来源标识），归入 mine，不再单列 unknown 分组。 */
export type ArchivedOriginGroupKey = "mine" | "auto" | "migrated";

export type ArchivedOriginGroup = {
  key: ArchivedOriginGroupKey;
  label: string;
  sessions: SessionSummary[];
};

/**
 * 按归档来源过滤会话列表（"all" 表示不过滤，原样返回）。
 *
 * 为什么独立成函数：归档来源筛选只是归档视图内部的会话过滤器，不能劫持
 * 工具栏的视图切换（时间/项目/大小）。工具栏视图优先决定分组方式，这里
 * 只负责把不匹配来源的会话剔除，再由对应视图组件去分组/排序。
 */
export function filterSessionsByOrigin(
  sessions: readonly SessionSummary[],
  ledgerBySession: ReadonlyMap<string, ArchiveOrigin>,
  originFilter: ArchivedOriginGroupKey | "all",
): SessionSummary[] {
  if (originFilter === "all") return [...sessions];
  return sessions.filter(
    (session) => archiveOriginGroupKey(ledgerBySession.get(session.id)) === originFilter,
  );
}

const ARCHIVED_GROUP_LABELS: Record<ArchivedOriginGroupKey, string> = {
  mine: "我的归档",
  auto: "同步归档",
  migrated: "迁移记录",
};

/**
 * 把归档会话按来源分成三个固定分组（空组不返回）：
 * - mine：manual/official/fork，以及 unknown/无 ledger 记录（官方应用归档、
 *   旧版手动归档都不写来源标识，视为用户主动归档）——用户主动归档的高价值来源
 * - auto：provider_sync，切换模型服务配置时由工具自动归档
 * - migrated：restore/import，备份恢复或会话包导入产生的归档
 *
 * 分组天然跨 provider：各 provider 的归档分支按 origin 混入同一组，
 * 这正是 ledger 的价值——origin 是跨 provider 统一维度。组内保持传入顺序
 * （调用方已按 updated_at 排序）。
 */
export function groupArchivedByOrigin(
  sessions: readonly SessionSummary[],
  ledgerBySession: ReadonlyMap<string, ArchiveOrigin>,
): ArchivedOriginGroup[] {
  const buckets: Record<ArchivedOriginGroupKey, SessionSummary[]> = {
    mine: [],
    auto: [],
    migrated: [],
  };
  for (const session of sessions) {
    const key = archiveOriginGroupKey(ledgerBySession.get(session.id));
    buckets[key].push(session);
  }
  return (Object.keys(ARCHIVED_GROUP_LABELS) as ArchivedOriginGroupKey[])
    .filter((key) => buckets[key].length > 0)
    .map((key) => ({ key, label: ARCHIVED_GROUP_LABELS[key], sessions: buckets[key] }));
}

function archiveOriginGroupKey(origin: ArchiveOrigin | undefined): ArchivedOriginGroupKey {
  switch (origin) {
    case "manual":
    case "official":
    case "fork":
      return "mine";
    case "provider_sync":
      return "auto";
    case "restore":
    case "import":
      return "migrated";
    default:
      // unknown 或 ledger 中无记录（undefined）：视为用户主动归档
      // （Codex 官方应用归档与旧版手动归档都不写来源标识），归入"我的归档"
      return "mine";
  }
}
