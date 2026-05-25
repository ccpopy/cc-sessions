import { lazy, Suspense, useEffect } from "react";
import { Navigate, Route, Routes } from "react-router-dom";
import { Toaster } from "sonner";

import { SidebarProvider } from "@/components/ui/sidebar";
import { Sidebar } from "@/components/Sidebar";
import { useHotkeys } from "@/hooks/useHotkeys";
import { useSettings } from "@/stores/settings";
import { useTheme } from "@/stores/theme";

const SessionsRoute = lazy(() => import("@/routes/sessions"));
const BackupsRoute = lazy(() => import("@/routes/backups"));
const BackupDetailRoute = lazy(() => import("@/routes/backup-detail"));
const StatsRoute = lazy(() => import("@/routes/stats"));
const RepairRoute = lazy(() => import("@/routes/repair"));
const TransferRoute = lazy(() => import("@/routes/transfer"));

export default function App() {
  const load = useSettings((s) => s.load);
  const settings = useSettings((s) => s.settings);
  const initTheme = useTheme((s) => s.init);
  const toggleTheme = useTheme((s) => s.toggle);

  useEffect(() => {
    void load();
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
            <Route path="/" element={<Navigate to="/codex/sessions" replace />} />
            <Route path="/codex/sessions" element={<SessionsRoute provider="codex" />} />
            <Route path="/codex/repair" element={<RepairRoute provider="codex" />} />
            <Route path="/codex/backups" element={<BackupsRoute provider="codex" />} />
            <Route path="/codex/backups/:name" element={<BackupDetailRoute provider="codex" />} />
            <Route path="/codex/transfer" element={<TransferRoute provider="codex" />} />
            <Route path="/claude/sessions" element={<SessionsRoute provider="claude" />} />
            <Route path="/claude/repair" element={<RepairRoute provider="claude" />} />
            <Route path="/claude/backups" element={<BackupsRoute provider="claude" />} />
            <Route path="/claude/backups/:name" element={<BackupDetailRoute provider="claude" />} />
            <Route path="/claude/transfer" element={<TransferRoute provider="claude" />} />
            <Route path="/gemini/sessions" element={<SessionsRoute provider="gemini" />} />
            <Route path="/gemini/repair" element={<RepairRoute provider="gemini" />} />
            <Route path="/gemini/backups" element={<BackupsRoute provider="gemini" />} />
            <Route path="/gemini/backups/:name" element={<BackupDetailRoute provider="gemini" />} />
            <Route path="/sessions" element={<Navigate to="/codex/sessions" replace />} />
            <Route path="/repair" element={<Navigate to="/codex/repair" replace />} />
            <Route path="/backups" element={<Navigate to="/codex/backups" replace />} />
            <Route path="/backups/:name" element={<BackupDetailRoute provider="codex" />} />
            <Route path="/transfer" element={<Navigate to="/codex/transfer" replace />} />
            <Route path="/stats" element={<StatsRoute />} />
            <Route path="*" element={<Navigate to="/codex/sessions" replace />} />
          </Routes>
        </Suspense>
      </main>
      <Toaster position="top-center" richColors closeButton />
      {!settings && <LoadingBoot />}
    </SidebarProvider>
  );
}

function RouteLoading() {
  return (
    <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
      正在加载页面…
    </div>
  );
}

function LoadingBoot() {
  return (
    <div className="pointer-events-none fixed inset-0 flex items-center justify-center bg-background/80">
      <div className="rounded-md border bg-card px-4 py-2 text-sm text-muted-foreground">
        正在加载设置…
      </div>
    </div>
  );
}
