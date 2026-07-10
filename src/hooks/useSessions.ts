import { useCallback, useEffect, useRef, useState } from "react";
import { api, type SessionProvider, type SessionSummary, type ProjectGroup } from "@/lib/api";
import { useSettings } from "@/stores/settings";

export function useSessions(provider: SessionProvider, query: string) {
  const settings = useSettings((s) => s.settings);
  const settingsReady = settings !== null;
  const codexDir = settings?.codex_dir ?? "";
  const claudeDir = settings?.claude_dir ?? "";
  const scope = JSON.stringify([settingsReady, provider, codexDir, claudeDir, query]);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
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
          const normalizedQuery = query.trim();
          const list = normalizedQuery
            ? await api.searchSessions(provider, codexDir, claudeDir, normalizedQuery)
            : await api.listSessions(provider, codexDir, claudeDir);
          if (!isCurrent()) return;
          setSessions(list);
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
  }, [claudeDir, codexDir, provider, query, scope, settingsReady]);

  useEffect(() => {
    requestSeq.current += 1;
    inFlight.current = null;
    setSessions([]);
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

  return { sessions, loading, error, refresh };
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
