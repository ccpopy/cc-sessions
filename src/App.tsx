import { lazy, Suspense, useEffect } from "react";
import { Navigate, Route, Routes } from "react-router-dom";
import { Toaster } from "sonner";

import { SidebarProvider } from "@/components/ui/sidebar";
import { Button } from "@/components/ui/button";
import { Sidebar } from "@/components/Sidebar";
import { useHotkeys } from "@/hooks/useHotkeys";
import { webuiDefaultProvider } from "@/lib/runtime";
import { useSettings } from "@/stores/settings";
import { useTheme } from "@/stores/theme";

const SessionsRoute = lazy(() => import("@/routes/sessions"));
const BackupsRoute = lazy(() => import("@/routes/backups"));
const BackupDetailRoute = lazy(() => import("@/routes/backup-detail"));
const StatsRoute = lazy(() => import("@/routes/stats"));
const RepairRoute = lazy(() => import("@/routes/repair"));
const TransferRoute = lazy(() => import("@/routes/transfer"));
const MemoryRoute = lazy(() => import("@/routes/memory"));

export default function App() {
  const load = useSettings((s) => s.load);
  const settings = useSettings((s) => s.settings);
  const settingsLoading = useSettings((s) => s.loading);
  const settingsError = useSettings((s) => s.error);
  const initTheme = useTheme((s) => s.init);
  const toggleTheme = useTheme((s) => s.toggle);
  const defaultProvider = webuiDefaultProvider();
  const defaultCoreProvider = defaultProvider === "opencode" ? "codex" : defaultProvider;
  const defaultSessionsPath = `/${defaultProvider}/sessions`;
  const defaultRepairPath = `/${defaultCoreProvider}/repair`;
  const defaultBackupsPath = `/${defaultCoreProvider}/backups`;
  const defaultTransferPath = `/${defaultCoreProvider}/transfer`;

  useEffect(() => {
    void load().catch(() => {
      // load 已把完整错误写入 store，由启动错误界面显示。
    });
  }, [load]);

  useEffect(() => {
    return initTheme();
  }, [initTheme]);

  useHotkeys([
    {
      combo: "mod+shift+l",
      handler: (e) => {
        e.preventDefault();
        toggleTheme();
      },
    },
  ]);

  return (
    <SidebarProvider
      tooltipDelayDuration={200}
      className="h-full min-h-0 overflow-hidden"
    >
      <Sidebar />
      <main className="flex min-w-0 flex-1 flex-col overflow-hidden">
        <Suspense fallback={<RouteLoading />}>
          <Routes>
            <Route path="/" element={<Navigate to={defaultSessionsPath} replace />} />
            <Route path="/codex/sessions" element={<SessionsRoute key="codex-sessions" provider="codex" />} />
            <Route path="/codex/repair" element={<RepairRoute key="codex-repair" provider="codex" />} />
            <Route path="/codex/backups" element={<BackupsRoute key="codex-backups" provider="codex" />} />
            <Route path="/codex/backups/:name" element={<BackupDetailRoute key="codex-backup-detail" provider="codex" />} />
            <Route path="/codex/transfer" element={<TransferRoute key="codex-transfer" provider="codex" />} />
            <Route path="/claude/sessions" element={<SessionsRoute key="claude-sessions" provider="claude" />} />
            <Route path="/claude/repair" element={<RepairRoute key="claude-repair" provider="claude" />} />
            <Route path="/claude/backups" element={<BackupsRoute key="claude-backups" provider="claude" />} />
            <Route path="/claude/backups/:name" element={<BackupDetailRoute key="claude-backup-detail" provider="claude" />} />
            <Route path="/claude/transfer" element={<TransferRoute key="claude-transfer" provider="claude" />} />
            <Route path="/claude/memory" element={<MemoryRoute />} />
            <Route path="/opencode/sessions" element={<SessionsRoute key="opencode-sessions" provider="opencode" />} />
            <Route path="/sessions" element={<Navigate to={defaultSessionsPath} replace />} />
            <Route path="/repair" element={<Navigate to={defaultRepairPath} replace />} />
            <Route path="/backups" element={<Navigate to={defaultBackupsPath} replace />} />
            <Route path="/backups/:name" element={<BackupDetailRoute provider={defaultCoreProvider} />} />
            <Route path="/transfer" element={<Navigate to={defaultTransferPath} replace />} />
            <Route path="/stats" element={<StatsRoute />} />
            <Route path="*" element={<Navigate to="/codex/sessions" replace />} />
          </Routes>
        </Suspense>
      </main>
      <Toaster position="top-center" richColors closeButton />
      {!settings && (
        <LoadingBoot
          error={settingsError}
          loading={settingsLoading}
          onRetry={() => {
            void load().catch(() => {
              // 重试错误仍由启动错误界面显示。
            });
          }}
        />
      )}
    </SidebarProvider>
  );
}

function RouteLoading() {
  return (
    <div className="flex h-full items-center justify-center p-6">
      <BootCard subtitle="正在加载页面" />
    </div>
  );
}

function LoadingBoot({
  error,
  loading,
  onRetry,
}: {
  error: string | null;
  loading: boolean;
  onRetry: () => void;
}) {
  return (
    <div className="boot-splash">
      <BootCard subtitle={error ? "设置加载失败" : "正在加载设置"} showProgress={!error}>
        {error && (
          <>
            <div
              role="alert"
              className="max-h-40 overflow-auto whitespace-pre-wrap break-words rounded-md border border-destructive/40 bg-destructive/10 p-2.5 text-xs leading-relaxed text-destructive"
            >
              {error}
            </div>
            <Button type="button" onClick={onRetry} disabled={loading} className="w-full">
              {loading ? "正在重试…" : "重试"}
            </Button>
          </>
        )}
      </BootCard>
    </div>
  );
}

function BootCard({
  subtitle,
  showProgress = true,
  children,
}: {
  subtitle: string;
  showProgress?: boolean;
  children?: React.ReactNode;
}) {
  return (
    <div className="boot-card">
      <div className="boot-brand">
        <div className="boot-mark">
          <svg viewBox="0 0 24 24" aria-hidden="true">
            <path
              d="M7 7.5 12 12l-5 4.5"
              fill="none"
              stroke="currentColor"
              strokeWidth="2.4"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
          </svg>
        </div>
        <div className="boot-copy">
          <div className="boot-title">CC Sessions</div>
          <div className="boot-subtitle">{subtitle}</div>
        </div>
      </div>
      {children}
      {showProgress && (
        <div className="boot-track">
          <div className="boot-bar" />
        </div>
      )}
    </div>
  );
}
