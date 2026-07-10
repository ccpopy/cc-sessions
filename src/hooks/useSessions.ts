import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, type SessionProvider, type SessionSummary, type ProjectGroup } from "@/lib/api";
import { useSettings } from "@/stores/settings";

export function useSessions(provider: SessionProvider, query: string) {
  const settings = useSettings((s) => s.settings);
  const settingsReady = settings !== null;
  const codexDir = settings?.codex_dir ?? "";
  const claudeDir = settings?.claude_dir ?? "";
  const scope = JSON.stringify([settingsReady, provider, codexDir, claudeDir]);
  const [allSessions, setAllSessions] = useState<SessionSummary[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const timer = useRef<number | null>(null);
  const requestSeq = useRef(0);
  const scopeRef = useRef(scope);
  const inFlight = useRef<{
    scope: string;
    requestId: number;
    promise: Promise<void>;
  } | null>(null);
  scopeRef.current = scope;

  const refresh = useCallback((): Promise<void> => {
    const active = inFlight.current;
    if (active?.scope === scope) return active.promise;

    const requestId = ++requestSeq.current;
    const isCurrent = () => requestSeq.current === requestId && scopeRef.current === scope;
    const promise: Promise<void> = Promise.resolve().then(async () => {
      try {
        if (!isCurrent()) return;
        if (!settingsReady) {
          setLoading(false);
          setError(null);
          return;
        }
        const providerDir = provider === "claude" ? claudeDir : codexDir;
        if (!providerDir) {
          setError(provider === "claude" ? "尚未配置 Claude 目录" : "尚未配置 Codex 目录");
          setLoading(false);
          return;
        }

        setLoading(true);
        setError(null);
        try {
          const list = await api.listSessions(provider, codexDir, claudeDir);
          if (!isCurrent()) return;
          setAllSessions(list);
        } catch (error) {
          if (!isCurrent()) return;
          setError(String((error as Error)?.message ?? error));
        } finally {
          if (isCurrent()) setLoading(false);
        }
      } finally {
        const current = inFlight.current;
        if (current?.scope === scope && current.requestId === requestId) {
          inFlight.current = null;
        }
      }
    });
    inFlight.current = { scope, requestId, promise };
    return promise;
  }, [claudeDir, codexDir, provider, scope, settingsReady]);

  useEffect(() => {
    requestSeq.current += 1;
    inFlight.current = null;
    setAllSessions([]);
    setLoading(false);
    setError(null);
    if (timer.current) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      timer.current = null;
      void refresh();
    }, 150);
    return () => {
      requestSeq.current += 1;
      if (timer.current) window.clearTimeout(timer.current);
      timer.current = null;
    };
  }, [refresh, scope]);

  const sessions = useMemo(() => filterSessions(allSessions, query), [allSessions, query]);
  return { sessions, allSessions, loading, error, refresh };
}

function filterSessions(sessions: readonly SessionSummary[], query: string): SessionSummary[] {
  const normalized = query.trim();
  if (!normalized) return sessions.slice();

  const lower = normalized.toLowerCase();
  const separator = normalized.indexOf(":");
  const rawKey = separator >= 0 ? normalized.slice(0, separator).trim().toLowerCase() : "";
  const key = ["id", "cwd", "model", "archived"].includes(rawKey) ? rawKey : null;
  const value = key ? normalized.slice(separator + 1).trim().toLowerCase() : lower;

  return sessions.filter((session) => {
    switch (key) {
      case "id":
        return session.id.toLowerCase().startsWith(value);
      case "cwd":
        return session.cwd.toLowerCase().includes(value);
      case "model":
        return session.model?.toLowerCase().includes(value) ?? false;
      case "archived": {
        const truthy = ["true", "1", "yes", "on"].includes(value);
        return session.archived === truthy;
      }
      default: {
        const idLike = value.length >= 4 && /^[0-9a-f-]+$/.test(value);
        return (
          (idLike && session.id.toLowerCase().startsWith(value)) ||
          session.title.toLowerCase().includes(value) ||
          session.first_user_message.toLowerCase().includes(value) ||
          session.source?.toLowerCase().includes(value) === true ||
          session.agent_nickname?.toLowerCase().includes(value) === true ||
          session.agent_role?.toLowerCase().includes(value) === true ||
          session.cwd.toLowerCase().includes(value)
        );
      }
    }
  });
}

export function useProjectGroups(provider: SessionProvider) {
  const settings = useSettings((s) => s.settings);
  const [groups, setGroups] = useState<ProjectGroup[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestSeq = useRef(0);

  const refresh = useCallback(async () => {
    const request = ++requestSeq.current;
    setGroups([]);
    if (!settings) {
      setLoading(false);
      setError(null);
      return;
    }
    const providerDir = provider === "claude" ? settings.claude_dir : settings.codex_dir;
    if (!providerDir) {
      setGroups([]);
      setLoading(false);
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const next = await api.groupByProject(provider, settings.codex_dir, settings.claude_dir);
      if (request === requestSeq.current) setGroups(next);
    } catch (error) {
      if (request === requestSeq.current) {
        setError(String((error as Error)?.message ?? error));
      }
    } finally {
      if (request === requestSeq.current) setLoading(false);
    }
  }, [settings?.codex_dir, settings?.claude_dir, provider]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return { groups, loading, error, refresh };
}
