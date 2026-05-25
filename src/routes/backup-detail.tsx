import { useEffect, useMemo, useState } from "react";
import { useLocation, useNavigate, useParams } from "react-router-dom";
import { ArrowLeft, Copy, Eye, RotateCcw, ShieldCheck } from "lucide-react";
import { toast } from "sonner";

import { TopBar } from "@/components/TopBar";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { ScrollArea } from "@/components/ui/scroll-area";
import { DangerDialog } from "@/components/DangerDialog";
import { PreviewDialog } from "@/components/PreviewDialog";
import { EmptyState } from "@/components/EmptyState";
import {
  RadioGroup,
  RadioGroupItem,
} from "@/components/ui/radio-group";
import { Label } from "@/components/ui/label";
import {
  api,
  type BackupDetail,
  type ManifestSession,
  type SessionProvider,
  type SessionSummary,
} from "@/lib/api";
import { useSettings } from "@/stores/settings";
import { humanBytes, humanTokens, relativeTime } from "@/lib/format";
import { basename } from "@/lib/cwd";

export default function BackupDetailRoute({ provider = "codex" }: { provider?: SessionProvider }) {
  const { name } = useParams();
  const loc = useLocation();
  const nav = useNavigate();
  const settings = useSettings((s) => s.settings);
  const [detail, setDetail] = useState<BackupDetail | null>(null);
  const [loading, setLoading] = useState(false);
  const [verifyResult, setVerifyResult] = useState<Record<string, "ok" | "fail" | "missing"> | null>(null);
  const [preview, setPreview] = useState<{ session: SessionSummary; rollout: string } | null>(null);
  const [restoreTarget, setRestoreTarget] = useState<ManifestSession | null>(null);
  const [overwrite, setOverwrite] = useState<"skip" | "overwrite">("skip");

  const backupPath = useMemo(() => {
    const fromState = (loc.state as any)?.path as string | undefined;
    if (fromState) return fromState;
    if (!settings?.backup_dir || !name) return undefined;
    return joinPath(settings.backup_dir, name);
  }, [loc.state, name, settings?.backup_dir]);

  useEffect(() => {
    if (!backupPath) return;
    setLoading(true);
    api.openBackup(backupPath).then(setDetail).finally(() => setLoading(false));
  }, [backupPath]);

  const items = useMemo(() => detail?.manifest.sessions ?? [], [detail]);

  const onVerify = async () => {
    if (!backupPath) return;
    const r = await api.verifyBackup(backupPath);
    const map: Record<string, "ok" | "fail" | "missing"> = {};
    for (const it of r.items) {
      map[it.id] = it.missing ? "missing" : it.ok ? "ok" : "fail";
    }
    setVerifyResult(map);
    toast.success(r.all_ok ? "校验通过" : "存在损坏项，请查看标记");
  };

  if (!backupPath) {
    return (
      <>
        <TopBar title="备份详情" stats={name} />
        <EmptyState title="缺少备份路径" description="请从备份列表进入" />
      </>
    );
  }

  return (
    <>
      <TopBar title="备份详情" stats={name} />
      <ScrollArea className="flex-1">
      <div className="space-y-4 p-6">
        <div className="flex items-center gap-2">
          <Button variant="ghost" size="sm" onClick={() => nav(`/${provider}/backups`)} className="gap-1.5">
            <ArrowLeft className="h-4 w-4" />
            返回
          </Button>
          <ShieldCheck className="h-4 w-4 text-emerald-500" />
          <div className="text-sm font-semibold">{name}</div>
          {detail && (
            <Badge variant="secondary" className="h-5 font-normal">
              {detail.manifest.sessions.length} 条
            </Badge>
          )}
          <Button variant="outline" size="sm" onClick={onVerify} className="ml-auto">
            校验完整性
          </Button>
        </div>

        {loading ? (
          <EmptyState title="加载中…" />
        ) : items.length === 0 ? (
          <EmptyState title="备份为空" />
        ) : (
          <div className="space-y-3">
            {items.map((s) => {
              const itemProvider = manifestSessionProvider(s, detail?.manifest.provider, provider);
              const sess = toSessionSummary(s, backupPath, itemProvider);
              const v = verifyResult?.[s.id];
              return (
                <Card
                  key={s.id}
                  className="p-0 shadow-sm transition-all hover:shadow-md"
                >
                  <CardContent className="flex items-center gap-4 p-4">
                    <div className="min-w-0 flex-1 space-y-1.5">
                      <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
                        <span className="font-mono">{s.id.slice(0, 8)}</span>
                        {s.model && (
                          <Badge variant="secondary" className="h-5 px-1.5 font-normal">
                            {s.model}
                          </Badge>
                        )}
                        <Badge variant="outline" className="h-5 px-1.5 font-normal text-muted-foreground">
                          {itemProvider}
                        </Badge>
                        <Badge variant="outline" className="h-5 px-1.5 font-normal">
                          {basename(s.cwd)}
                        </Badge>
                        {v === "ok" && (
                          <Badge
                            variant="outline"
                            className="h-5 border-emerald-500/40 bg-emerald-500/10 px-1.5 font-normal text-emerald-600 dark:text-emerald-400"
                          >
                            校验通过
                          </Badge>
                        )}
                        {v === "fail" && (
                          <Badge variant="destructive" className="h-5 px-1.5 font-normal">
                            损坏
                          </Badge>
                        )}
                        {v === "missing" && (
                          <Badge variant="destructive" className="h-5 px-1.5 font-normal">
                            缺失
                          </Badge>
                        )}
                      </div>
                      <div className="line-clamp-1 text-sm font-semibold">
                        {s.title || "(无标题)"}
                      </div>
                      <div className="text-xs text-muted-foreground">
                        {relativeTime(s.updated_at)} · {humanBytes(s.bytes_rollout)} ·{" "}
                        {humanTokens(s.tokens_used)} token · {s.logs_count} 条日志
                      </div>
                    </div>
                    <div className="flex shrink-0 items-center gap-1.5">
                      <Button
                        variant="outline"
                        size="sm"
                        className="gap-1.5"
                        onClick={() =>
                          setPreview({
                            session: sess,
                            rollout: rolloutAbs(backupPath, s.rollout_relpath || s.source_relpath || ""),
                          })
                        }
                      >
                        <Eye className="h-3.5 w-3.5" />
                        预览
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        className="gap-1.5"
                        onClick={async () => {
                          const text = sess.resume_command || (await api.copyResumeCommand(itemProvider, s.id));
                          await navigator.clipboard.writeText(text);
                          toast.success("已复制：" + text);
                        }}
                      >
                        <Copy className="h-3.5 w-3.5" />
                        resume
                      </Button>
                      <Button
                        variant="outline"
                        size="sm"
                        className="gap-1.5"
                        onClick={() => setRestoreTarget(s)}
                      >
                        <RotateCcw className="h-3.5 w-3.5" />
                        还原
                      </Button>
                    </div>
                  </CardContent>
                </Card>
              );
            })}
          </div>
        )}
      </div>
      </ScrollArea>

      <PreviewDialog
        open={!!preview}
        onOpenChange={(v) => !v && setPreview(null)}
        session={preview?.session ?? null}
        customRolloutPath={preview?.rollout}
      />

      <DangerDialog
        open={!!restoreTarget}
        onOpenChange={(v) => !v && setRestoreTarget(null)}
        title="还原会话"
        confirmText="还原"
        onConfirm={async () => {
          if (!restoreTarget || !settings || !backupPath) return;
          const itemProvider = manifestSessionProvider(restoreTarget, detail?.manifest.provider, provider);
          const r = await api.restoreSession({
            provider: itemProvider,
            backup_path: backupPath,
            codex_dir: settings.codex_dir,
            claude_dir: itemProvider === "gemini" ? settings.gemini_dir : settings.claude_dir,
            id: restoreTarget.id,
            overwrite: overwrite === "overwrite",
          });
          if (r.conflict) {
            toast.warning("目标 id 已存在，已跳过。选择「覆盖」再试一次。");
          } else if (r.ok) {
            toast.success("已还原");
          } else {
            toast.error("还原失败：" + (r.error ?? "未知错误"));
          }
        }}
      >
        <div className="space-y-2">
          <div className="min-w-0">
            会话 <code className="font-mono">{restoreTarget?.id.slice(0, 8)}</code>
            —— <span className="break-all">{restoreTarget?.title}</span>
          </div>
          <div>若目标已存在，如何处理：</div>
          <RadioGroup value={overwrite} onValueChange={(v) => setOverwrite(v as any)} className="gap-1">
            <div className="flex items-center gap-2">
              <RadioGroupItem id="skip" value="skip" />
              <Label htmlFor="skip" className="cursor-pointer">
                跳过（保留当前数据）
              </Label>
            </div>
            <div className="flex items-center gap-2">
              <RadioGroupItem id="overwrite" value="overwrite" />
              <Label htmlFor="overwrite" className="cursor-pointer">
                覆盖（不可撤销）
              </Label>
            </div>
          </RadioGroup>
        </div>
      </DangerDialog>
    </>
  );
}

function toSessionSummary(m: ManifestSession, backupPath: string, provider: SessionProvider): SessionSummary {
  return {
    provider,
    id: m.id,
    rollout_path: rolloutAbs(backupPath, m.rollout_relpath || m.source_relpath || ""),
    cwd: m.cwd,
    cwd_display: basename(m.cwd),
    title: m.title,
    first_user_message: "",
    model: m.model,
    reasoning_effort: null,
    source: null,
    agent_nickname: null,
    agent_role: null,
    tokens_used: m.tokens_used,
    created_at: m.created_at,
    updated_at: m.updated_at,
    archived: false,
    git_branch: null,
    rollout_bytes: m.bytes_rollout,
    logs_count: m.logs_count,
    has_backup: true,
    resume_command: resumeCommandForBackup(provider, m.id, m.source_relpath || m.rollout_relpath || ""),
  };
}

function resumeCommandForBackup(provider: SessionProvider, id: string, relpath: string): string {
  if (provider === "claude") return `claude --resume ${id}`;
  if (provider === "gemini") {
    const normalized = relpath.replace(/\\/g, "/");
    return normalized.includes("/antigravity") || normalized.startsWith("antigravity")
      ? "agy"
      : `gemini --resume ${id}`;
  }
  return `codex resume ${id}`;
}

function rolloutAbs(backupPath: string, rel: string): string {
  if (!rel) return backupPath;
  const sep = backupPath.includes("\\") ? "\\" : "/";
  return backupPath + sep + rel.replace(/\//g, sep);
}

function joinPath(dir: string, child: string): string {
  const sep = dir.includes("\\") ? "\\" : "/";
  const base = dir.endsWith("\\") || dir.endsWith("/") ? dir.slice(0, -1) : dir;
  return base + sep + child;
}

function manifestSessionProvider(
  session: ManifestSession,
  manifestProvider: SessionProvider | null | undefined,
  routeProvider: SessionProvider,
): SessionProvider {
  return session.provider ?? manifestProvider ?? routeProvider;
}
