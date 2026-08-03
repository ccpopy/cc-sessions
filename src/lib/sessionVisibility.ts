import type { FamilyOverlay, SessionSummary } from "./api";
import { isSubagentSession } from "./sessionSource.ts";

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
  const includeHiddenRecords = options.includeHiddenRecords ?? false;
  const candidates: SessionSummary[] = [];

  for (const session of sessions) {
    const overlay = isCodex ? overlayBySession.get(session.id) : undefined;
    if (isSubagentSession(session, overlay) !== options.showSubagentSessions) {
      continue;
    }
    if (
      isCodex
      && !includeHiddenRecords
      && session.archived !== options.showArchivedSessions
    ) {
      continue;
    }
    if (
      isCodex
      && !includeHiddenRecords
      && options.showArchivedSessions
      && isHiddenArchivedFamilyBranch(overlay, currentProvider)
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

/**
 * Archived view mirrors Codex App by showing the current provider's family
 * branch. If provider metadata is unavailable, fall back to the stored active
 * branch so one logical family still has one row.
 */
function isHiddenArchivedFamilyBranch(
  overlay: FamilyOverlay | undefined,
  currentProvider: string | null,
): boolean {
  if (!overlay?.family_id) return false;
  if (currentProvider) return overlay.provider !== currentProvider;
  return !overlay.is_active_branch;
}
