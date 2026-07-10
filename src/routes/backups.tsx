import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { AlertTriangle, Archive, ChevronRight, RotateCcw, ShieldCheck, Trash2 } from "lucide-react";
import { TopBar } from "@/components/TopBar";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { ScrollArea } from "@/components/ui/scroll-area";
import { DangerDialog } from "@/components/DangerDialog";
import { EmptyState } from "@/components/EmptyState";
import { useBackups } from "@/hooks/useBackups";
import { useSettings } from "@/stores/settings";
import { api, type BackupSummary, type SessionProvider } from "@/lib/api";
import { humanBytes } from "@/lib/format";
import { format } from "date-fns";
import { toast } from "sonner";

export default function BackupsRoute({ provider = "codex" }: { provider?: SessionProvider }) {
  const settings = useSettings((s) => s.settings);
  const { backups, loading, error, refresh } = useBackups(provider);
  const [delTarget, setDelTarget] = useState<BackupSummary | null>(null);
  const [restoreTarget, setRestoreTarget] = useState<BackupSummary | null>(null);

  const totalSize = useMemo(() => backups.reduce((a, b) => a + b.total_bytes, 0), [backups]);
  const providerLabel = provider === "codex" ? "Codex" : "Claude";

  return (
    <>
      <TopBar
        title={`${providerLabel} 备份`}
        stats={backups.length > 0 ? `${backups.length} 份 · ${humanBytes(totalSize)}` : undefined}
        onRefresh={refresh}
      />
      <ScrollArea className="flex-1">
      <div className="space-y-4 p-6">
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <Archive className="h-4 w-4" />
          备份目录：
          <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs">
            {settings?.backup_dir}
          </code>
        </div>

        {loading ? (
          <EmptyState title="加载中…" />
        ) : error ? (
          <EmptyState
            icon={<AlertTriangle className="h-10 w-10" />}
            title="读取备份失败"
            description={error}
            action={
              <Button variant="outline" onClick={() => void refresh()}>
                重试
              </Button>
            }
          />
        ) : backups.length === 0 ? (
          <EmptyState
            icon={<Archive className="h-10 w-10" />}
            title="还没有备份"
            description="在会话页勾选若干条点击「批量备份」即可创建"
          />
        ) : (
          <div className="space-y-3">
            {backups.map((b) => (
              <Card key={b.path} className="p-0 shadow-sm transition-all hover:shadow-md">
                <CardContent className="flex items-center gap-4 p-4">
                  <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-md bg-emerald-500/10">
                    <ShieldCheck className="h-5 w-5 text-emerald-500" />
                  </div>
                  <div className="min-w-0 flex-1 space-y-1">
                    <div className="flex items-center gap-2">
                      <div className="truncate text-sm font-semibold">{b.name}</div>
                      <Badge variant="outline" className="h-5 px-1.5 font-normal text-muted-foreground">
                        {b.provider ?? "codex"}
                      </Badge>
                      <Badge variant="secondary" className="h-5 px-1.5 font-normal">
                        {b.sessions_count} 条
                      </Badge>
                      <Badge variant="outline" className="h-5 px-1.5 font-normal text-muted-foreground">
                        {humanBytes(b.total_bytes)}
                      </Badge>
                    </div>
                    <div className="flex items-center gap-2 text-xs text-muted-foreground">
                      <span>{safeFormat(b.created_at)}</span>
                      <span>·</span>
                      <span className="truncate font-mono">{b.path}</span>
                    </div>
                    {b.note && (
                      <div className="line-clamp-1 text-xs text-muted-foreground">
                        备注：{b.note}
                      </div>
                    )}
                  </div>
                  <div className="flex shrink-0 items-center gap-1.5">
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => setRestoreTarget(b)}
                      className="gap-1.5"
                    >
                      <RotateCcw className="h-3.5 w-3.5" />
                      全部还原
                    </Button>
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => setDelTarget(b)}
                      className="gap-1.5 border-destructive/30 text-destructive hover:bg-destructive/10 hover:text-destructive"
                    >
                      <Trash2 className="h-3.5 w-3.5" />
                      删除
                    </Button>
                    <Button asChild variant="ghost" size="sm" className="gap-1">
                      <Link
                        to={`/${provider}/backups/${encodeURIComponent(b.name)}`}
                        state={{ path: b.path }}
                      >
                        详情
                        <ChevronRight className="h-4 w-4" />
                      </Link>
                    </Button>
                  </div>
                </CardContent>
              </Card>
            ))}
          </div>
        )}
      </div>
      </ScrollArea>

      <DangerDialog
        open={!!delTarget}
        onOpenChange={(v) => !v && setDelTarget(null)}
        title="删除备份"
        confirmText="删除备份"
        onConfirm={async () => {
          if (!delTarget || !settings) return;
          await api.deleteBackup(settings.backup_dir, delTarget.path);
          toast.success("备份已删除");
          await refresh();
        }}
      >
        删除后将无法从此备份还原。备份名：<code>{delTarget?.name}</code>，共 {delTarget?.sessions_count} 条，
        {humanBytes(delTarget?.total_bytes ?? 0)}。
      </DangerDialog>

      <DangerDialog
        open={!!restoreTarget}
        onOpenChange={(v) => !v && setRestoreTarget(null)}
        title={`还原 ${restoreTarget?.sessions_count ?? 0} 条会话`}
        confirmText="全部还原"
        onConfirm={async () => {
          if (!restoreTarget || !settings) return;
          const r = await api.restoreAll({
            provider,
            backup_dir: settings.backup_dir,
            backup_path: restoreTarget.path,
            codex_dir: settings.codex_dir,
            claude_dir: settings.claude_dir,
            overwrite: false,
          });
          const ok = r.filter((x) => x.ok).length;
          const conflict = r.filter((x) => x.conflict).length;
          const failed = r.filter((x) => !x.ok && !x.conflict);
          const summary = `已还原 ${ok}/${r.length}${conflict ? `（${conflict} 条跳过冲突）` : ""}`;
          const description = failed
            .map((item) => `${item.id.slice(0, 8)}：${item.error ?? "还原未完成"}`)
            .slice(0, 3)
            .join("\n");
          if (failed.length === r.length) {
            throw new Error(description || "所有会话都还原失败");
          }
          if (failed.length > 0) {
            toast.warning(`${summary}，${failed.length} 条失败`, {
              description: description || undefined,
            });
          } else if (ok === 0 && conflict > 0) {
            toast.info(`未写入会话：${conflict} 条均因本地已存在而跳过`);
          } else {
            toast.success(summary);
          }
        }}
      >
        将把备份中的所有会话回写至 {providerLabel} 目录。已存在的 session id 会自动跳过（不覆盖）；
        如需覆盖请到备份详情页按条还原。
      </DangerDialog>
    </>
  );
}

function safeFormat(iso: string): string {
  try {
    return format(new Date(iso), "yyyy-MM-dd HH:mm:ss");
  } catch {
    return iso;
  }
}
