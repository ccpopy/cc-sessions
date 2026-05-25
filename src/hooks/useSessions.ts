import { useCallback, useEffect, useRef, useState } from "react";
import { api, type SessionProvider, type SessionSummary, type ProjectGroup } from "@/lib/api";
import { useSettings } from "@/stores/settings";

export function useSessions(provider: SessionProvider, query: string) {
  const settings = useSettings((s) => s.settings);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const timer = useRef<number | null>(null);
  const requestSeq = useRef(0);

  const refresh = useCallback(async () => {
    if (!settings?.codex_dir) return;
    const providerDir = provider === "gemini" ? settings.gemini_dir : settings.claude_dir;
    const requestId = ++requestSeq.current;
    setLoading(true);
    setError(null);
    try {
      const list = query.trim()
        ? await api.searchSessions(provider, settings.codex_dir, providerDir, query.trim())
        : await api.listSessions(provider, settings.codex_dir, providerDir);
      if (requestSeq.current !== requestId) return;
      setSessions(list);
    } catch (e: any) {
      if (requestSeq.current !== requestId) return;
      setError(String(e?.message ?? e));
      setSessions([]);
    } finally {
      if (requestSeq.current === requestId) setLoading(false);
    }
  }, [settings?.codex_dir, settings?.claude_dir, settings?.gemini_dir, provider, query]);

  useEffect(() => {
    if (timer.current) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(refresh, 150);
    return () => {
      if (timer.current) window.clearTimeout(timer.current);
    };
  }, [refresh]);

  return { sessions, loading, error, refresh };
}

export function useProjectGroups(provider: SessionProvider) {
  const settings = useSettings((s) => s.settings);
  const [groups, setGroups] = useState<ProjectGroup[]>([]);
  const [loading, setLoading] = useState(false);

  const refresh = useCallback(async () => {
    if (!settings?.codex_dir) return;
    const providerDir = provider === "gemini" ? settings.gemini_dir : settings.claude_dir;
    setLoading(true);
    try {
      setGroups(await api.groupByProject(provider, settings.codex_dir, providerDir));
    } finally {
      setLoading(false);
    }
  }, [settings?.codex_dir, settings?.claude_dir, settings?.gemini_dir, provider]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return { groups, loading, refresh };
}
