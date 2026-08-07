import type { SessionSummary } from "@/lib/api";
import { stripVerbatim } from "@/lib/cwd";

function normalizedIdentityPath(path: string): string {
  const stripped = stripVerbatim(path.trim());
  const isWindowsPath = /^[a-zA-Z]:[\\/]/.test(stripped) || stripped.startsWith("\\\\");
  const normalized = stripped
    .replace(/\\/g, "/")
    .replace(/\/{2,}/g, "/")
    .replace(/\/$/, "");
  return isWindowsPath ? normalized.toLowerCase() : normalized;
}

export function sessionIdentityFromPath(
  provider: SessionSummary["provider"],
  rolloutPath: string,
  fallbackId: string,
): string {
  if (provider === "opencode") return `${provider}\u0000${fallbackId}`;
  return `${provider}\u0000${normalizedIdentityPath(rolloutPath) || fallbackId}`;
}

/** UI identity must include the physical transcript path because Claude can reuse an ID. */
export function sessionIdentity(session: SessionSummary): string {
  return sessionIdentityFromPath(session.provider, session.rollout_path, session.id);
}

export const sessionIdentityKey = sessionIdentity;
