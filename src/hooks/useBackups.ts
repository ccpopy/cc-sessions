import { useCallback, useEffect, useRef, useState } from "react";
import { api, type BackupSummary, type SessionProvider } from "@/lib/api";
import { joinPath } from "@/lib/cwd";
import { sessionIdentityFromPath } from "@/lib/sessionIdentity";
import { useSettings } from "@/stores/settings";

export function useBackups(provider?: SessionProvider, enabled = true) {
  const settings = useSettings((s) => s.settings);
  const [backups, setBackups] = useState<BackupSummary[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestSeq = useRef(0);

  const refresh = useCallback(async () => {
    const request = ++requestSeq.current;
    setBackups([]);
    if (!enabled || !settings?.backup_dir) {
      setLoading(false);
      setError(null);
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const list = await api.listBackups(settings.backup_dir, provider);
      if (request === requestSeq.current) setBackups(list);
    } catch (error) {
      if (request === requestSeq.current) {
        setError(String((error as Error)?.message ?? error));
      }
    } finally {
      if (request === requestSeq.current) setLoading(false);
    }
  }, [enabled, settings?.backup_dir, provider]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return { backups, loading, error, refresh };
}

export function useBackupIndex(provider?: SessionProvider, enabled = true) {
  const settings = useSettings((s) => s.settings);
  const backupDir = settings?.backup_dir ?? "";
  const codexDir = settings?.codex_dir ?? "";
  const claudeDir = settings?.claude_dir ?? "";
  const { backups } = useBackups(provider, enabled);
  const [index, setIndex] = useState<Record<string, string[]>>({});
  const [error, setError] = useState<string | null>(null);
  const requestSeq = useRef(0);

  useEffect(() => {
    const request = ++requestSeq.current;
    setIndex({});
    setError(null);
    if (!enabled || !backupDir) return;
    (async () => {
      const map: Record<string, string[]> = {};
      const errors: string[] = [];
      for (const b of backups) {
        try {
          const detail = await api.openBackup(backupDir, b.path);
          for (const s of detail.manifest.sessions) {
            const itemProvider = s.provider ?? detail.manifest.provider ?? provider ?? "codex";
            if (itemProvider === "opencode") {
              const identity = sessionIdentityFromPath(itemProvider, "", s.id);
              (map[identity] ||= []).push(b.path);
              continue;
            }
            const rolloutPath = itemProvider === "claude"
              ? s.source_relpath && claudeDir
                ? joinPath(joinPath(claudeDir, "projects"), s.source_relpath)
                : null
              : codexDir
                ? joinPath(codexDir, s.rollout_relpath)
                : null;
            if (!rolloutPath) continue;
            const identity = sessionIdentityFromPath(itemProvider, rolloutPath, s.id);
            (map[identity] ||= []).push(b.path);
          }
        } catch (error) {
          errors.push(`${b.name}: ${String((error as Error)?.message ?? error)}`);
        }
      }
      if (request === requestSeq.current) {
        setIndex(map);
        setError(errors.length > 0 ? errors.slice(0, 3).join("\n") : null);
      }
    })();
  }, [backupDir, backups, claudeDir, codexDir, enabled, provider]);

  return { index, error };
}
