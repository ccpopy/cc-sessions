import type { SessionSummary } from "./api";

export type SortDirection = "asc" | "desc";

export const DEFAULT_SIZE_SORT_DIRECTION: SortDirection = "desc";

export function oppositeSortDirection(direction: SortDirection): SortDirection {
  return direction === "asc" ? "desc" : "asc";
}

export function compareSessionSize(
  a: SessionSummary,
  b: SessionSummary,
  direction: SortDirection,
): number {
  const directionFactor = direction === "asc" ? 1 : -1;
  const tokenDelta = (a.tokens_used - b.tokens_used) * directionFactor;
  if (tokenDelta !== 0) return tokenDelta;

  const bytesDelta = (a.rollout_bytes - b.rollout_bytes) * directionFactor;
  if (bytesDelta !== 0) return bytesDelta;

  // 大小相同时仍优先显示最近更新的会话，避免切换方向后同大小项目来回跳动。
  const updatedDelta = b.updated_at - a.updated_at;
  if (updatedDelta !== 0) return updatedDelta;
  return a.id.localeCompare(b.id);
}
