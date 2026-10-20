import type { SessionSummary } from "./api";

/** List ordering keeps millisecond precision; older responses use update seconds. */
export function sessionActivityMs(session: SessionSummary): number {
  const activity = session.activity_at_ms;
  return typeof activity === "number" && Number.isFinite(activity) && activity > 0
    ? activity
    : session.updated_at * 1000;
}

/** The date formatting and bucket helpers accept Unix seconds. */
export function sessionActivitySeconds(session: SessionSummary): number {
  return sessionActivityMs(session) / 1000;
}

export function compareSessionActivityDesc(left: SessionSummary, right: SessionSummary): number {
  const activityDelta = sessionActivityMs(right) - sessionActivityMs(left);
  if (activityDelta !== 0) return activityDelta;
  if (left.provider !== right.provider) {
    return left.provider.localeCompare(right.provider);
  }
  if (left.provider === "codex") {
    return right.id.localeCompare(left.id);
  }
  const createdDelta = right.created_at - left.created_at;
  if (createdDelta !== 0) return createdDelta;
  return left.id.localeCompare(right.id);
}
