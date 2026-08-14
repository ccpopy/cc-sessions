import { useCallback, useEffect, useState } from "react";
import {
  AlertCircle,
  CheckCircle2,
  Copy,
  Database,
  Eraser,
  Info,
  List,
  Loader2,
  ShieldAlert,
  ShieldCheck,
  Trash2,
  Wand2,
  Wrench,
} from "lucide-react";
import { toast } from "sonner";

import { TopBar } from "@/components/TopBar";
import { EmptyState } from "@/components/EmptyState";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import { Switch } from "@/components/ui/switch";
import { Label } from "@/components/ui/label";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";

import { useSettings } from "@/stores/settings";
import {
  api,
  type DiagnosticReport,
  type FamilyIntegrityReport,
  type GuiVisibilityReport,
  type HistoryOrphanReport,
  type OrphanPruneReport,
  type ProjectConfigReport,
  type ProviderInfo,
  type ProviderSyncStatus,
  type SessionProvider,
  type SwitchStrategy,
} from "@/lib/api";
import { copyText } from "@/lib/clipboard";

export default function RepairRoute({ provider = "codex" }: { provider?: SessionProvider }) {
  return provider === "claude" ? <ClaudeRepairRoute /> : <CodexRepairRoute />;
}

function CodexRepairRoute() {
  const settings = useSettings((s) => s.settings);
  const codexDir = settings?.codex_dir ?? "";

  const [diag, setDiag] = useState<DiagnosticReport | null>(null);
  const [provider, setProvider] = useState<ProviderInfo | null>(null);
  const [projectConfig, setProjectConfig] = useState<ProjectConfigReport | null>(null);
  const [integrity, setIntegrity] = useState<FamilyIntegrityReport | null>(null);
  const [familyPrunePreview, setFamilyPrunePreview] = useState<OrphanPruneReport | null>(null);
  const [loading, setLoading] = useState(false);
  const [strategy, setStrategy] = useState<SwitchStrategy>("continuous");
  const [confirmBatch, setConfirmBatch] = useState(false);
  const [confirmProjectConfigRepair, setConfirmProjectConfigRepair] = useState(false);
  const [confirmPrune, setConfirmPrune] = useState<null | "index" | "threads" | "family">(
    null,
  );
  const [running, setRunning] = useState<string | null>(null);
  const [providerSyncProgress, setProviderSyncProgress] = useState<ProviderSyncStatus | null>(null);
  const [dryRun, setDryRun] = useState(false);
  const providerSyncRunning = running === "clone" || running === "clone_do";
  const expectedThreadsCount =
    diag == null ? null : diag.rollout_count + diag.archived_rollout_count;
  const threadsStatHint =
    diag == null
      ? undefined
      : `threads 表包含 ${diag.threads_active_count} 条活跃会话和 ${diag.threads_archived_count} 条本工具归档备份；归档备份存放在 archived_sessions/，不等同于 Codex 官方设置页的“已归档对话”。`;
  const familyCleanupCount = familyPrunePreview
    ? familyPrunePreview.family_branches_removed +
      familyPrunePreview.families_removed +
      familyPrunePreview.families_recovered +
      familyPrunePreview.families_normalized
    : 0;

  const refresh = useCallback(async () => {
    if (!codexDir) return;
    setLoading(true);
    try {
      const [d, p, c, i, familyPreview] = await Promise.all([
        api.diagnoseCodexState(codexDir),
        api.getProviderInfo(codexDir),
        api.diagnoseProjectConfigs(codexDir),
        api.verifyFamilyIntegrity(codexDir),
        api.pruneOrphanEntries({
          codex_dir: codexDir,
          prune_index: false,
          prune_threads: false,
          prune_family: true,
          dry_run: true,
        }),
      ]);
      setDiag(d);
      setProvider(p);
      setProjectConfig(c);
      setIntegrity(i);
      setFamilyPrunePreview(familyPreview);
    } catch (e) {
      toast.error(`诊断失败：${String((e as Error)?.message ?? e)}`);
    } finally {
      setLoading(false);
    }
  }, [codexDir]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const run = async (key: string, fn: () => Promise<void>) => {
    setRunning(key);
    try {
      await fn();
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setProviderSyncProgress(null);
      setRunning(null);
    }
  };

  const showFamilyPruneResult = (report: OrphanPruneReport, preview: boolean) => {
    const action = preview ? "预览：将移除" : "已移除";
    const recovery = report.families_recovered > 0
      ? `，${preview ? "将恢复" : "已恢复"} ${report.families_recovered} 个可确定当前分支的 family`
      : "";
    const normalization = report.families_normalized > 0
      ? `，${preview ? "将校正" : "已校正"} ${report.families_normalized} 个 family 的 active 状态`
      : "";
    const message = `${action} ${report.family_branches_removed} 个孤儿分支、${report.families_removed} 个空 family${recovery}${normalization}`;
    if (report.families_skipped.length > 0) {
      toast.warning(`${message}；跳过 ${report.families_skipped.length} 个需人工确认的 family`, {
        description: report.families_skipped.slice(0, 3).join("\n"),
      });
    } else {
      toast.success(message);
    }
  };

  return (
    <>
      <TopBar title="修复" onRefresh={refresh} showListTools={false} />
      <ScrollArea className="flex-1">
        <div className="min-w-0 max-w-full space-y-4 p-4 sm:p-6">
          {!codexDir ? (
            <EmptyState
              icon={<Wrench className="h-10 w-10" />}
              title="尚未配置 Codex 目录"
              description="请先在设置里填写 ~/.codex 路径"
            />
          ) : (
            <>
              <Card className="min-w-0 overflow-hidden">
                <CardHeader className="pb-3">
                  <CardTitle className="flex min-w-0 flex-wrap items-center gap-2 text-base">
                    <ShieldCheck className="h-4 w-4" />
                    诊断总览
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Info className="h-3.5 w-3.5 text-muted-foreground" />
                      </TooltipTrigger>
                      <TooltipContent className="max-w-sm text-xs">
                        检查本地会话文件、索引文件（session_index.jsonl）和数据库（threads
                        表）是否一致。如果有数量差异，可能会导致某些会话记录在界面上“看不到”或“点不开”。
                      </TooltipContent>
                    </Tooltip>
                    {loading && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                  </CardTitle>
                </CardHeader>
                <CardContent className="min-w-0 space-y-3">
                  <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
                    <Stat label="本地会话文件" value={diag?.rollout_count ?? "-"} />
                    <Stat
                      label="会话索引 (index)"
                      value={diag?.index_count ?? "-"}
                      warn={
                        diag != null && diag.index_count !== diag.rollout_count
                      }
                    />
                    <Stat
                      label="数据库表 (threads)"
                      value={diag?.threads_count ?? "-"}
                      warn={
                        diag != null &&
                        expectedThreadsCount != null &&
                        (diag.threads_count !== expectedThreadsCount ||
                          diag.threads_active_count !== diag.rollout_count ||
                          diag.threads_archived_count !== diag.archived_rollout_count)
                      }
                      hint={threadsStatHint}
                    />
                    <Stat
                      label="工具归档备份"
                      value={diag?.archived_rollout_count ?? "-"}
                      hint="这里统计的是本工具放在 archived_sessions/ 的历史分支备份，不是 Codex 官方“已归档对话”页面。"
                    />
                  </div>
                  <Separator />
                  <DiffRow
                    label="存在会话文件，但在应用列表中漏显"
                    ids={diag?.missing_in_index ?? []}
                    color="amber"
                    recommendation="下方点「修复会话索引 (index)」，会从本地文件重建 session_index.jsonl"
                  />
                  <DiffRow
                    label="存在会话文件，但左侧边栏看不到"
                    ids={diag?.missing_in_threads ?? []}
                    color="rose"
                    recommendation="下方点「重建会话索引表」，会把会话文件同步到 threads 表"
                  />
                  <DiffRow
                    label="列表中残留：实际上会话文件已丢失"
                    ids={diag?.orphan_in_index ?? []}
                    color="muted"
                    recommendation="若不打算恢复，下方点「清理索引残留」即可（或直接跑「修复会话索引」也会顺带清掉）"
                  />
                  <DiffRow
                    label="数据库残留：实际上会话文件已丢失"
                    ids={diag?.orphan_in_threads ?? []}
                    color="muted"
                    recommendation="若不打算恢复，下方点「清理数据库残留」即可"
                  />
                  <div className="flex flex-wrap items-center gap-2 pt-1">
                    <div className="flex items-center gap-2 rounded-md border bg-muted/20 px-2.5 py-1">
                      <Switch
                        id="dry-run"
                        checked={dryRun}
                        onCheckedChange={setDryRun}
                      />
                      <Label htmlFor="dry-run" className="flex items-center gap-1 text-xs">
                        效果预览
                        <Tooltip>
                          <TooltipTrigger asChild>
                            <Info className="h-3 w-3 text-muted-foreground" />
                          </TooltipTrigger>
                          <TooltipContent className="max-w-xs text-xs">
                            打开后下面的修复按钮只会扫描并报告"将要做什么"，不会实际写入
                            任何文件或数据库。想先看看效果再动手，就打开它。
                          </TooltipContent>
                        </Tooltip>
                      </Label>
                    </div>

                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          size="sm"
                          variant={dryRun ? "outline" : "default"}
                          onClick={() =>
                            run("index", async () => {
                              const r = await api.repairSessionIndex(codexDir, dryRun);
                              toast.success(
                                dryRun
                                  ? `预览：将写入 ${r.written} 行（救援 ${r.salvaged}），扫描 ${r.scanned}`
                                  : `已写入 ${r.written} 行（救援 ${r.salvaged}）`,
                              );
                              if (!dryRun) await refresh();
                            })
                          }
                          disabled={!!running}
                          className="gap-1.5"
                        >
                          <Wand2 className="h-3.5 w-3.5" />
                          修复会话索引 (index)
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent className="max-w-sm text-xs">
                        扫描所有本地会话文件，重新生成
                        <code>session_index.jsonl</code> 索引文件。适用于：会话列表出现条目遗漏、排序错乱，
                        或是不小心删除了索引文件的情况。不会修改 Codex Desktop 的项目或目录归属。
                      </TooltipContent>
                    </Tooltip>

                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          size="sm"
                          variant={dryRun ? "outline" : "default"}
                          onClick={() =>
                            run("threads", async () => {
                              const r = await api.rebuildThreadsTable(codexDir, dryRun);
                              toast.success(
                                dryRun
                                  ? `预览：将同步 ${r.upserted} 条（跳过 ${r.skipped}）`
                                  : `已同步 ${r.upserted} 条（跳过 ${r.skipped}）`,
                              );
                              if (!dryRun) await refresh();
                            })
                          }
                          disabled={!!running}
                          className="gap-1.5"
                        >
                          <Database className="h-3.5 w-3.5" />
                          重建会话索引表
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent className="max-w-sm text-xs">
                        读取所有的会话文件，将它们的信息批量同步到
                        应用数据库中。
                        适用于：当你发现左侧边栏的历史会话消失或列表变为空白时，可以使用此功能进行修复。
                        不会修改 Codex Desktop 的项目或目录归属。
                      </TooltipContent>
                    </Tooltip>

                    <Separator orientation="vertical" className="mx-1 h-6" />

                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          size="sm"
                          variant={dryRun ? "outline" : "destructive"}
                          onClick={() => {
                            if (dryRun) {
                              void run("prune_index", async () => {
                                const r = await api.pruneOrphanEntries({
                                  codex_dir: codexDir,
                                  prune_index: true,
                                  prune_threads: false,
                                  prune_family: false,
                                  dry_run: true,
                                });
                                toast.success(
                                  `预览：将从 session_index 删除 ${r.index_removed} 行`,
                                );
                              });
                            } else {
                              setConfirmPrune("index");
                            }
                          }}
                          disabled={
                            !!running || (diag?.orphan_in_index?.length ?? 0) === 0
                          }
                          className="gap-1.5"
                        >
                          <Eraser className="h-3.5 w-3.5" />
                          清理索引残留
                          {(diag?.orphan_in_index?.length ?? 0) > 0 && (
                            <Badge
                              variant="outline"
                              className="ml-0.5 h-4 border-current/30 bg-background/20 px-1 text-[10px] font-normal"
                            >
                              {diag?.orphan_in_index.length}
                            </Badge>
                          )}
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent className="max-w-sm text-xs">
                        仅删除 <code>session_index.jsonl</code> 中指向已消失会话文件的"残留行"，
                        不会重建，不动应用数据库。适合不打算恢复这些会话、只想把列表清干净的场景。
                      </TooltipContent>
                    </Tooltip>

                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          size="sm"
                          variant={dryRun ? "outline" : "destructive"}
                          onClick={() => {
                            if (dryRun) {
                              void run("prune_threads", async () => {
                                const r = await api.pruneOrphanEntries({
                                  codex_dir: codexDir,
                                  prune_index: false,
                                  prune_threads: true,
                                  prune_family: false,
                                  dry_run: true,
                                });
                                toast.success(
                                  `预览：将从 threads 表删除 ${r.threads_removed} 行`,
                                );
                              });
                            } else {
                              setConfirmPrune("threads");
                            }
                          }}
                          disabled={
                            !!running || (diag?.orphan_in_threads?.length ?? 0) === 0
                          }
                          className="gap-1.5"
                        >
                          <Trash2 className="h-3.5 w-3.5" />
                          清理数据库残留
                          {(diag?.orphan_in_threads?.length ?? 0) > 0 && (
                            <Badge
                              variant="outline"
                              className="ml-0.5 h-4 border-current/30 bg-background/20 px-1 text-[10px] font-normal"
                            >
                              {diag?.orphan_in_threads.length}
                            </Badge>
                          )}
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent className="max-w-sm text-xs">
                        仅从应用数据库的 <code>threads</code> 表删除指向已消失会话文件的行，
                        不会重建，不动索引文件。适合不打算恢复这些会话、只想把左侧边栏清干净的场景。
                      </TooltipContent>
                    </Tooltip>
                  </div>
                </CardContent>
              </Card>

              <Card className="min-w-0 overflow-hidden">
                <CardHeader className="pb-3">
                  <CardTitle className="flex min-w-0 flex-wrap items-center gap-2 text-base">
                    <ShieldAlert className="h-4 w-4" />
                    项目级 Codex 配置
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Info className="h-3.5 w-3.5 text-muted-foreground" />
                      </TooltipTrigger>
                      <TooltipContent className="max-w-sm text-xs">
                        扫描每条 Codex 会话的工作目录，检查 <code>.codex/config.toml</code>{" "}
                        是否包含会导致新版 Codex 恢复失败的 multi_agent_v2 超时配置。
                      </TooltipContent>
                    </Tooltip>
                  </CardTitle>
                </CardHeader>
                <CardContent className="min-w-0 space-y-3">
                  <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
                    <Stat label="会话项目目录" value={projectConfig?.scanned_projects ?? "-"} />
                    <Stat label="项目配置文件" value={projectConfig?.config_files ?? "-"} />
                    <Stat
                      label="配置问题"
                      value={projectConfig?.issue_count ?? "-"}
                      warn={(projectConfig?.issue_count ?? 0) > 0}
                    />
                    <Stat
                      label="可自动修复"
                      value={projectConfig?.repairable_count ?? "-"}
                      warn={(projectConfig?.repairable_count ?? 0) > 0}
                    />
                  </div>

                  {projectConfig == null ? (
                    <div className="text-xs text-muted-foreground">—</div>
                  ) : projectConfig.issues.length === 0 ? (
                    <div className="flex items-center gap-2 text-xs text-muted-foreground">
                      <CheckCircle2 className="h-3.5 w-3.5 text-emerald-500" />
                      暂未发现会阻断会话恢复的项目级 Codex 配置。
                    </div>
                  ) : (
                    <div className="max-h-56 space-y-2 overflow-auto rounded-md border p-2">
                      {projectConfig.issues.map((issue) => (
                        <div
                          key={issue.config_path}
                          className="min-w-0 rounded-md bg-muted/25 p-2 text-xs"
                        >
                          <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                            <Badge
                              variant="outline"
                              className={
                                issue.repairable
                                  ? "h-5 border-amber-500/30 px-1.5 text-amber-600"
                                  : "h-5 border-rose-500/30 px-1.5 text-rose-500"
                              }
                            >
                              {issue.repairable ? "可修复" : "需手动处理"}
                            </Badge>
                            <span className="text-muted-foreground">
                              影响 {issue.session_count} 条会话
                            </span>
                            {issue.suggested_default_wait_timeout_ms != null && (
                              <code className="rounded bg-background/70 px-1 py-0.5 text-[11px]">
                                default = {issue.suggested_default_wait_timeout_ms}
                              </code>
                            )}
                          </div>
                          <div className="mt-1 min-w-0 [overflow-wrap:anywhere]">
                            {issue.message}
                          </div>
                          <div className="mt-1 min-w-0 font-mono text-[11px] text-muted-foreground [overflow-wrap:anywhere]">
                            {issue.config_path}
                          </div>
                          {issue.session_ids.length > 0 && (
                            <div className="mt-1 min-w-0 truncate font-mono text-[11px] text-muted-foreground">
                              {issue.session_ids.slice(0, 3).join("  ")}
                              {issue.session_ids.length > 3 ? "  …" : ""}
                            </div>
                          )}
                        </div>
                      ))}
                    </div>
                  )}

                  <div className="flex flex-wrap items-center gap-2">
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          size="sm"
                          variant={dryRun ? "outline" : "default"}
                          onClick={() => {
                            if (dryRun) {
                              void run("project_config_preview", async () => {
                                const r = await api.repairProjectConfigs(codexDir, true);
                                toast.success(
                                  `预览：将修复 ${r.repaired_count} 个项目配置`,
                                );
                              });
                            } else {
                              setConfirmProjectConfigRepair(true);
                            }
                          }}
                          disabled={
                            !!running || (projectConfig?.repairable_count ?? 0) === 0
                          }
                          className="gap-1.5"
                        >
                          <Wrench className="h-3.5 w-3.5" />
                          {dryRun ? "预览项目配置修复" : "修复项目 config.toml"}
                          {(projectConfig?.repairable_count ?? 0) > 0 && (
                            <Badge
                              variant="outline"
                              className="ml-0.5 h-4 border-current/30 bg-background/20 px-1 text-[10px] font-normal"
                            >
                              {projectConfig?.repairable_count}
                            </Badge>
                          )}
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent className="max-w-sm text-xs">
                        只补写或改写{" "}
                        <code>features.multi_agent_v2.default_wait_timeout_ms</code>，
                        让它满足已配置的 min/max 约束。不自动修改 min/max 边界值。
                      </TooltipContent>
                    </Tooltip>
                    <span className="text-xs text-muted-foreground">
                      {dryRun ? "当前：效果预览（不写入）" : "当前：实际执行（会写入项目配置）"}
                    </span>
                  </div>
                </CardContent>
              </Card>

              <Card className="min-w-0 overflow-hidden">
                <CardHeader className="pb-3">
                  <CardTitle className="flex min-w-0 flex-wrap items-center gap-2 text-base">
                    <Wand2 className="h-4 w-4" />
                    服务商 (Provider) 适配
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Info className="h-3.5 w-3.5 text-muted-foreground" />
                      </TooltipTrigger>
                      <TooltipContent className="max-w-sm text-xs">
                        切换 model_provider 后，旧会话可能因为 provider、source 或数据库状态不同步而不可见。
                        这里会把需要处理的会话同步到当前 provider，让官方 App 能继续列出和恢复。
                      </TooltipContent>
                    </Tooltip>
                  </CardTitle>
                </CardHeader>
                <CardContent className="min-w-0 space-y-3">
                  <div className="grid grid-cols-1 gap-3 md:grid-cols-3">
                    <Stat
                      label="当前服务商 (provider)"
                      value={
                        provider?.current
                          ? provider.is_explicit
                            ? provider.current
                            : `${provider.current}（默认）`
                          : "-"
                      }
                      hint={
                        provider && !provider.is_explicit
                          ? "config.toml 未显式写 model_provider，Codex 默认使用 openai（官方 ChatGPT 登录与 OpenAI API key 都走这个 id）"
                          : undefined
                      }
                    />
                    <Stat
                      label="需要同步的对话"
                      value={diag?.provider_mismatched_families ?? 0}
                      warn={(diag?.provider_mismatched_families ?? 0) > 0}
                    />
                    <div className="flex min-w-0 flex-col gap-1 rounded-md border bg-muted/20 p-3 text-xs">
                      <span className="text-muted-foreground">切换策略</span>
                      <Select
                        value={strategy}
                        onValueChange={(v) => setStrategy(v as SwitchStrategy)}
                      >
                        <SelectTrigger className="h-8 min-w-0">
                          <SelectValue />
                        </SelectTrigger>
                        <SelectContent>
                          <SelectItem value="continuous">
                            连续模式（推荐，自动归档旧节点）
                          </SelectItem>
                          <SelectItem value="scatter">
                            散点模式（每个 provider 独立副本）
                          </SelectItem>
                          <SelectItem value="follow">
                            跟随模式（就地改写，不克隆）
                          </SelectItem>
                        </SelectContent>
                      </Select>
                    </div>
                  </div>
                  <div className="min-w-0 rounded-md bg-muted/30 p-3 text-xs text-muted-foreground [overflow-wrap:anywhere]">
                    {strategy === "continuous" && (
                      <>
                        为当前服务商（provider）创建一份最新的会话副本，原记录会立即自动归档备用至 <code>archived_sessions/</code>，
                        在应用中每条会话始终只会显示一个最新入口，保持整洁。归档时固化 sha256 + 行数用于完整性校验。
                      </>
                    )}
                    {strategy === "scatter" && (
                      <>
                        每个服务商下都保留一份独立的会话副本。
                        不归档旧会话，它们会随着你在不同服务商下的使用各自独立发展。
                      </>
                    )}
                    {strategy === "follow" && (
                      <>
                        直接修改原会话文件中的服务商标记，由于没有克隆新副本，
                        切换后该会话将只在新服务商下可见。
                      </>
                    )}
                  </div>
                  <div className="flex flex-wrap items-center gap-2">
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          size="sm"
                          variant={dryRun ? "outline" : "default"}
                          onClick={() => {
                            if (dryRun) {
                              run("clone", async () => {
                                setProviderSyncProgress(null);
                                const r = await api.runProviderSync({
                                  codex_dir: codexDir,
                                  strategy,
                                  dry_run: true,
                                }, setProviderSyncProgress);
                                const ok = r.filter((x) => x.ok).length;
                                const failed = r.filter((x) => !x.ok);
                                if (r.length === 0) {
                                  toast.info("预览：当前没有需要同步的会话");
                                } else if (ok > 0) {
                                  toast.success(`预览：${ok}/${r.length} 将执行（策略：${strategy}）`);
                                }
                                if (failed.length > 0) {
                                  const detail = failed
                                    .map((item) => item.error ?? `${item.source_id} 无法同步`)
                                    .slice(0, 3)
                                    .join("\n");
                                  if (failed.length === r.length) {
                                    throw new Error(detail || "没有会话可以同步");
                                  }
                                  toast.warning(`${failed.length} 条会话无法同步`, { description: detail });
                                }
                              });
                            } else {
                              setConfirmBatch(true);
                            }
                          }}
                          disabled={!!running || !provider?.current}
                          className="gap-1.5"
                        >
                          <Wand2 className="h-3.5 w-3.5" />
                          {dryRun ? "预览批量同步" : "批量同步到当前服务商"}
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent className="max-w-sm text-xs">
                        把 provider、source 或数据库状态与当前 Codex App 不兼容的会话同步到当前
                        provider，让它们在 Codex App 里可以继续使用。切换策略见上方。
                        {!provider?.current && (
                          <div className="mt-1 text-muted-foreground">
                            需要 config.toml 里设置 model_provider 才能批量执行。
                          </div>
                        )}
                      </TooltipContent>
                    </Tooltip>
                    <span className="text-xs text-muted-foreground">
                      {providerSyncProgress
                        ? providerSyncProgress.total > 0
                          ? `正在处理 ${providerSyncProgress.completed}/${providerSyncProgress.total}`
                          : "正在扫描需要同步的会话…"
                        : providerSyncRunning
                          ? "正在准备同步…"
                        : dryRun
                          ? "当前：效果预览（不写入）"
                          : "当前：实际执行（会写入磁盘）"}
                    </span>
                  </div>
                </CardContent>
              </Card>

              <Card className="min-w-0 overflow-hidden">
                <CardHeader className="pb-3">
                  <CardTitle className="flex min-w-0 flex-wrap items-center gap-2 text-base">
                    <ShieldCheck className="h-4 w-4" />
                    会话完整性校验
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Info className="h-3.5 w-3.5 text-muted-foreground" />
                      </TooltipTrigger>
                      <TooltipContent className="max-w-sm text-xs">
                        检查 family 中各分支的 rollout 是否存在，并对已固化 sha256
                        的归档分支做 sha256 + 行数校验（未固化的分支只检查存在性，
                        不计入结果）。文件被外部改过或丢失时会在这里亮红。
                      </TooltipContent>
                    </Tooltip>
                  </CardTitle>
                </CardHeader>
                <CardContent className="min-w-0 space-y-2">
                  {integrity == null || familyPrunePreview == null ? (
                    <div className="text-xs text-muted-foreground">—</div>
                  ) : (
                    <div className="space-y-1.5">
                      <div className="flex flex-wrap items-center gap-2 text-xs">
                        {integrity.items.length === 0 ? (
                          <span className="text-muted-foreground">
                            尚无已固化校验信息（归档分支后会自动记录 sha256 + 行数）
                          </span>
                        ) : integrity.all_ok ? (
                          <>
                            <CheckCircle2 className="h-3.5 w-3.5 text-emerald-500" />
                            <span>全部 {integrity.items.length} 条校验通过</span>
                          </>
                        ) : (
                          <>
                            <ShieldAlert className="h-3.5 w-3.5 text-amber-500" />
                            <span>
                              {integrity.items.filter((x) => !x.ok).length}/
                              {integrity.items.length} 条校验失败
                            </span>
                          </>
                        )}
                        <Tooltip>
                          <TooltipTrigger asChild>
                            <Button
                              size="sm"
                              variant={dryRun ? "outline" : "destructive"}
                              onClick={() => {
                                if (dryRun) {
                                  void run("prune_family", async () => {
                                    const report = await api.pruneOrphanEntries({
                                      codex_dir: codexDir,
                                      prune_index: false,
                                      prune_threads: false,
                                      prune_family: true,
                                      dry_run: true,
                                    });
                                    setFamilyPrunePreview(report);
                                    showFamilyPruneResult(report, true);
                                  });
                                } else {
                                  setConfirmPrune("family");
                                }
                              }}
                              disabled={!!running || familyCleanupCount === 0}
                              className="ml-auto gap-1.5"
                            >
                              <Eraser className="h-3.5 w-3.5" />
                              清理 family 残留
                              {familyCleanupCount > 0 && (
                                <Badge
                                  variant="outline"
                                  className="ml-0.5 h-4 border-current/30 bg-background/20 px-1 text-[10px] font-normal"
                                >
                                  {familyCleanupCount}
                                </Badge>
                              )}
                            </Button>
                          </TooltipTrigger>
                          <TooltipContent className="max-w-sm text-xs">
                            修复 <code>session_family.json</code> 中可安全确定的残留：移除缺失分支或空
                            family，并在唯一可判断时恢复 active_id、root 与分支状态。不会改写任何现存
                            rollout JSONL；无法唯一判断的 family 只会报告。
                          </TooltipContent>
                        </Tooltip>
                      </div>
                      {(familyCleanupCount > 0 || familyPrunePreview.families_skipped.length > 0) && (
                        <div className="rounded-md border border-border/60 bg-muted/20 px-3 py-2 text-[11px] leading-5 text-muted-foreground">
                          可自动处理：{familyPrunePreview.family_branches_removed} 个缺失分支、
                          {familyPrunePreview.families_removed} 个空 family、恢复 {familyPrunePreview.families_recovered} 个
                          active_id、校正 {familyPrunePreview.families_normalized} 个状态
                          {familyPrunePreview.families_skipped.length > 0 && (
                            <>；另有 {familyPrunePreview.families_skipped.length} 个 family 无法唯一判断，需人工检查</>
                          )}
                        </div>
                      )}
                      {integrity.items.length > 0 && (
                        <div className="max-h-64 max-w-full overflow-auto rounded-md border">
                        <table className="w-full text-xs">
                          <thead className="bg-muted/40 text-left">
                            <tr>
                              <th className="px-2 py-1">对话分组(family)</th>
                              <th className="px-2 py-1">发展分支(branch)</th>
                              <th className="px-2 py-1">状态</th>
                              <th className="px-2 py-1">sha256</th>
                              <th className="px-2 py-1">行数</th>
                            </tr>
                          </thead>
                          <tbody>
                            {integrity.items.map((it) => (
                              <tr
                                key={`${it.family_id}-${it.branch_id}`}
                                className={it.ok ? "" : "bg-rose-500/5"}
                              >
                                <td className="truncate px-2 py-1 font-mono">
                                  {it.family_id.slice(0, 8)}…
                                </td>
                                <td className="truncate px-2 py-1 font-mono">
                                  {it.branch_id.slice(0, 8)}…
                                </td>
                                <td className="px-2 py-1">
                                  {it.missing ? (
                                    <Badge
                                      variant="outline"
                                      className="h-5 border-rose-500/30 px-1.5 text-rose-500"
                                    >
                                      文件缺失
                                    </Badge>
                                  ) : it.ok ? (
                                    <Badge
                                      variant="outline"
                                      className="h-5 border-emerald-500/30 px-1.5 text-emerald-600"
                                    >
                                      OK
                                    </Badge>
                                  ) : (
                                    <Badge
                                      variant="outline"
                                      className="h-5 border-amber-500/30 px-1.5 text-amber-600"
                                    >
                                      不一致
                                    </Badge>
                                  )}
                                </td>
                                <td className="px-2 py-1 font-mono">
                                  {it.actual_sha?.slice(0, 10) ?? "-"}
                                </td>
                                <td className="px-2 py-1 tabular-nums">
                                  {it.actual_lines ?? "-"}
                                </td>
                              </tr>
                            ))}
                          </tbody>
                        </table>
                      </div>
                      )}
                    </div>
                  )}
                </CardContent>
              </Card>
            </>
          )}
        </div>
      </ScrollArea>

      <AlertDialog
        open={confirmProjectConfigRepair}
        onOpenChange={setConfirmProjectConfigRepair}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>
              修复 {projectConfig?.repairable_count ?? 0} 个项目级 Codex 配置
            </AlertDialogTitle>
            <AlertDialogDescription>
              将写入对应工作目录下的 <code>.codex/config.toml</code>，只补写或改写{" "}
              <code>features.multi_agent_v2.default_wait_timeout_ms</code>，
              不自动修改 <code>min_wait_timeout_ms</code> 或{" "}
              <code>max_wait_timeout_ms</code>。这些配置会影响 Codex App 恢复该项目里的历史会话。
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>取消</AlertDialogCancel>
            <AlertDialogAction
              onClick={() => {
                setConfirmProjectConfigRepair(false);
                void run("project_config_repair", async () => {
                  const r = await api.repairProjectConfigs(codexDir, false);
                  const suffix = r.errors.length > 0 ? `，失败 ${r.errors.length} 个` : "";
                  toast.success(`已修复 ${r.repaired_count} 个项目配置${suffix}`);
                  await refresh();
                });
              }}
            >
              确认修复
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <AlertDialog open={confirmBatch} onOpenChange={setConfirmBatch}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>批量同步 {diag?.provider_mismatched_families ?? 0} 条对话记录</AlertDialogTitle>
            <AlertDialogDescription>
              目标服务商 (provider)：<b>{provider?.current}</b>；策略：
              <b>{strategy}</b>
              。操作会写入新的会话文件
              {strategy === "continuous" ? "并把旧会话归档" : ""}；
              可在 <code>archived_sessions/</code> 中找回历史记录。
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>取消</AlertDialogCancel>
            <AlertDialogAction
              onClick={() =>
                run("clone_do", async () => {
                  setProviderSyncProgress(null);
                  const r = await api.runProviderSync({
                    codex_dir: codexDir,
                    strategy,
                    dry_run: false,
                  }, setProviderSyncProgress);
                  const ok = r.filter((x) => x.ok).length;
                  const failed = r.filter((x) => !x.ok);
                  await refresh();
                  if (r.length === 0) {
                    toast.info("当前没有需要同步的会话");
                  } else if (ok > 0) {
                    toast.success(`完成：${ok}/${r.length}`);
                  }
                  if (failed.length > 0) {
                    const detail = failed
                      .map((item) => item.error ?? `${item.source_id} 未处理`)
                      .slice(0, 3)
                      .join("\n");
                    if (failed.length === r.length) {
                      throw new Error(detail || "所有会话同步均失败");
                    }
                    toast.error(`${failed.length} 条会话同步失败`, { description: detail });
                  }
                })
              }
            >
              确认执行
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <AlertDialog
        open={confirmPrune !== null}
        onOpenChange={(o) => {
          if (!o) setConfirmPrune(null);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>
              {confirmPrune === "index"
                ? "清理索引残留"
                : confirmPrune === "threads"
                  ? "清理数据库残留"
                  : "清理 family 残留"}
            </AlertDialogTitle>
            <AlertDialogDescription>
              {confirmPrune === "index" ? (
                <>
                  即将从 <code>session_index.jsonl</code> 删除{" "}
                  <b>{diag?.orphan_in_index.length ?? 0}</b> 行。这些条目指向的会话文件已经不存在，
                  删除后不可撤销。对应的历史会话文件不会因此被再次修改。
                </>
              ) : confirmPrune === "threads" ? (
                <>
                  即将从应用数据库的 <code>threads</code> 表删除{" "}
                  <b>{diag?.orphan_in_threads.length ?? 0}</b> 行。这些条目指向的会话文件已经不存在，
                  删除后不可撤销。对应的历史会话文件不会因此被再次修改。
                </>
              ) : (
                <>
                  即将按预览修复 <code>session_family.json</code>：移除{" "}
                  <b>{familyPrunePreview?.family_branches_removed ?? 0}</b> 个缺失分支和{" "}
                  <b>{familyPrunePreview?.families_removed ?? 0}</b> 个空 family，恢复{" "}
                  <b>{familyPrunePreview?.families_recovered ?? 0}</b> 个可唯一确定的 active_id，并校正{" "}
                  <b>{familyPrunePreview?.families_normalized ?? 0}</b> 个 family 的 active/root/状态元数据。
                  无法唯一判断的 family 会跳过；现存 rollout JSONL 不会被改写。
                </>
              )}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>取消</AlertDialogCancel>
            <AlertDialogAction
              className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
              onClick={() => {
                const kind = confirmPrune;
                setConfirmPrune(null);
                if (!kind) return;
                const runKey =
                  kind === "index"
                    ? "prune_index"
                    : kind === "threads"
                      ? "prune_threads"
                      : "prune_family";
                void run(runKey, async () => {
                  const r = await api.pruneOrphanEntries({
                    codex_dir: codexDir,
                    prune_index: kind === "index",
                    prune_threads: kind === "threads",
                    prune_family: kind === "family",
                    dry_run: false,
                  });
                  if (kind === "index") {
                    toast.success(`已从 session_index 删除 ${r.index_removed} 行`);
                  } else if (kind === "threads") {
                    toast.success(`已从 threads 表删除 ${r.threads_removed} 行`);
                  } else {
                    showFamilyPruneResult(r, false);
                  }
                  await refresh();
                });
              }}
            >
              确认清理
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}

function ClaudeRepairRoute() {
  const settings = useSettings((s) => s.settings);
  const claudeDir = settings?.claude_dir ?? "";
  const [report, setReport] = useState<HistoryOrphanReport | null>(null);
  const [guiReport, setGuiReport] = useState<GuiVisibilityReport | null>(null);
  const [loading, setLoading] = useState(false);
  const [running, setRunning] = useState<string | null>(null);
  const [dryRun, setDryRun] = useState(false);
  const [confirmPrune, setConfirmPrune] = useState(false);
  const [guiDryRun, setGuiDryRun] = useState(false);
  const [confirmGuiFix, setConfirmGuiFix] = useState(false);

  const refresh = useCallback(async () => {
    if (!claudeDir) return;
    setLoading(true);
    try {
      setReport(await api.diagnoseClaudeHistoryOrphans(claudeDir));
    } catch (e) {
      toast.error(`Claude history 诊断失败：${String((e as Error)?.message ?? e)}`);
    }
    try {
      setGuiReport(await api.diagnoseClaudeGuiVisibility(claudeDir));
    } catch (e) {
      toast.error(`GUI 会话可见性诊断失败：${String((e as Error)?.message ?? e)}`);
    } finally {
      setLoading(false);
    }
  }, [claudeDir]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const run = async (key: string, fn: () => Promise<void>) => {
    setRunning(key);
    try {
      await fn();
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setRunning(null);
    }
  };

  return (
    <>
      <TopBar title="Claude 修复" onRefresh={refresh} showListTools={false} />
      <ScrollArea className="flex-1">
        <div className="min-w-0 max-w-full space-y-4 p-4 sm:p-6">
          {!claudeDir ? (
            <EmptyState
              icon={<Wrench className="h-10 w-10" />}
              title="尚未配置 Claude 目录"
              description="请先在设置里填写 ~/.claude 路径"
            />
          ) : (
            <>
            <Card className="min-w-0 overflow-hidden">
              <CardHeader className="pb-3">
                <CardTitle className="flex min-w-0 flex-wrap items-center gap-2 text-base">
                  <Wrench className="h-4 w-4" />
                  history.jsonl 残留
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Info className="h-3.5 w-3.5 text-muted-foreground" />
                    </TooltipTrigger>
                    <TooltipContent className="max-w-sm text-xs">
                      将 Claude 的 <code>history.jsonl</code> 与 <code>projects</code>{" "}
                      下仍存在的会话文件比对，只清理能明确匹配到已删除会话 ID 的历史行。
                    </TooltipContent>
                  </Tooltip>
                  {loading && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                </CardTitle>
              </CardHeader>
              <CardContent className="min-w-0 space-y-3">
                <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
                  <Stat label="现存会话文件" value={report?.session_count ?? "-"} />
                  <Stat label="history 行数" value={report?.history_rows ?? "-"} />
                  <Stat
                    label="残留历史行"
                    value={report?.orphan_rows ?? "-"}
                    warn={(report?.orphan_rows ?? 0) > 0}
                  />
                  <Stat
                    label="未识别行"
                    value={report?.untracked_rows ?? "-"}
                    hint="JSON 无效或没有 sessionId/session_id/id 的行不会自动删除"
                  />
                </div>
                <div className="min-w-0 rounded-md bg-muted/30 p-3 text-xs text-muted-foreground [overflow-wrap:anywhere]">
                  文件路径：<code>{report?.history_path ?? `${claudeDir}\\history.jsonl`}</code>
                </div>
                <Separator />
                <DiffRow
                  label="history.jsonl 残留：对应 Claude 会话文件已删除"
                  ids={report?.orphan_session_ids ?? []}
                  color="muted"
                  recommendation="确认不需要恢复这些会话后，可以使用下方按钮清理对应历史行"
                />
                {(report?.orphan_session_ids.length ?? 0) === 0 && (
                  <div className="text-xs text-muted-foreground">
                    暂未发现 Claude history 残留。
                  </div>
                )}
                <div className="flex flex-wrap items-center gap-2 pt-1">
                  <div className="flex items-center gap-2 rounded-md border bg-muted/20 px-2.5 py-1">
                    <Switch
                      id="claude-history-dry-run"
                      checked={dryRun}
                      onCheckedChange={setDryRun}
                    />
                    <Label htmlFor="claude-history-dry-run" className="text-xs">
                      效果预览
                    </Label>
                  </div>
                  <Button
                    size="sm"
                    variant={dryRun ? "outline" : "destructive"}
                    onClick={() => {
                      if (dryRun) {
                        void run("claude_history_prune_preview", async () => {
                          const result = await api.pruneClaudeHistoryOrphans(claudeDir, true);
                          toast.success(
                            `预览：将从 Claude history 删除 ${result.removed_rows} 行`,
                          );
                        });
                      } else {
                        setConfirmPrune(true);
                      }
                    }}
                    disabled={!!running || (report?.orphan_rows ?? 0) === 0}
                    className="gap-1.5"
                  >
                    <Eraser className="h-3.5 w-3.5" />
                    清理 Claude history 残留
                    {(report?.orphan_rows ?? 0) > 0 && (
                      <Badge
                        variant="outline"
                        className="ml-0.5 h-4 border-current/30 bg-background/20 px-1 text-[10px] font-normal"
                      >
                        {report?.orphan_rows}
                      </Badge>
                    )}
                  </Button>
                </div>
              </CardContent>
            </Card>

            <Card className="min-w-0 overflow-hidden">
              <CardHeader className="pb-3">
                <CardTitle className="flex min-w-0 flex-wrap items-center gap-2 text-base">
                  <Wand2 className="h-4 w-4" />
                  GUI 会话列表修复
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Info className="h-3.5 w-3.5 text-muted-foreground" />
                    </TooltipTrigger>
                    <TooltipContent className="max-w-sm text-xs">
                      Claude Code 的 GUI（如 VS Code 插件）只读取会话文件头尾各 64KB
                      来推导标题，推导失败的会话会从历史列表中消失（CLI 的{" "}
                      <code>claude --resume</code> 不受影响）。走中转 provider
                      时标题生成常失败，长会话 compact 后尤其容易触发。
                      修复方式与官方"重命名会话"一致：在文件末尾补写一条{" "}
                      <code>custom-title</code> 记录，不改动任何既有内容。
                    </TooltipContent>
                  </Tooltip>
                  {loading && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                </CardTitle>
              </CardHeader>
              <CardContent className="min-w-0 space-y-3">
                <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
                  <Stat label="扫描会话" value={guiReport?.scanned_sessions ?? "-"} />
                  <Stat label="GUI 可见" value={guiReport?.visible_sessions ?? "-"} />
                  <Stat
                    label="不可见 · 可修复"
                    value={guiReport?.issues.length ?? "-"}
                    warn={(guiReport?.issues.length ?? 0) > 0}
                  />
                  <Stat
                    label="不可见 · 无法修复"
                    value={guiReport?.unfixable_sessions ?? "-"}
                    hint="没有任何用户消息可用来生成标题的会话（通常是空会话），补写标题没有意义"
                  />
                </div>
                {(guiReport?.issues.length ?? 0) > 0 ? (
                  <div className="min-w-0 space-y-1 rounded-md border bg-muted/20 p-2">
                    <div className="px-1 pb-1 text-[11px] text-muted-foreground">
                      以下会话在 GUI 历史列表中不可见，将补写的标题来自会话首条用户消息：
                    </div>
                    <div className="max-h-48 space-y-1 overflow-y-auto">
                      {guiReport?.issues.map((issue) => (
                        <div
                          key={issue.session_id}
                          className="flex min-w-0 flex-col gap-0.5 rounded bg-background/60 px-2 py-1.5"
                        >
                          <div className="truncate text-xs font-medium">
                            {issue.proposed_title}
                          </div>
                          <div className="flex min-w-0 flex-wrap items-center gap-2 text-[11px] text-muted-foreground">
                            <code className="truncate">{issue.session_id}</code>
                            <span className="truncate">{issue.project_dir}</span>
                          </div>
                        </div>
                      ))}
                    </div>
                  </div>
                ) : (
                  <div className="text-xs text-muted-foreground">
                    {guiReport
                      ? "所有会话都能在 GUI 历史列表中正常显示。"
                      : "等待诊断结果…"}
                  </div>
                )}
                <div className="flex flex-wrap items-center gap-2 pt-1">
                  <div className="flex items-center gap-2 rounded-md border bg-muted/20 px-2.5 py-1">
                    <Switch
                      id="claude-gui-dry-run"
                      checked={guiDryRun}
                      onCheckedChange={setGuiDryRun}
                    />
                    <Label htmlFor="claude-gui-dry-run" className="text-xs">
                      效果预览
                    </Label>
                  </div>
                  <Button
                    size="sm"
                    variant={guiDryRun ? "outline" : "default"}
                    onClick={() => {
                      if (guiDryRun) {
                        void run("claude_gui_fix_preview", async () => {
                          const result = await api.repairClaudeGuiVisibility(claudeDir, true);
                          toast.success(`预览：将为 ${result.fixed} 条会话补写标题`);
                        });
                      } else {
                        setConfirmGuiFix(true);
                      }
                    }}
                    disabled={!!running || (guiReport?.issues.length ?? 0) === 0}
                    className="gap-1.5"
                  >
                    <Wand2 className="h-3.5 w-3.5" />
                    补写标题，恢复 GUI 显示
                    {(guiReport?.issues.length ?? 0) > 0 && (
                      <Badge
                        variant="outline"
                        className="ml-0.5 h-4 border-current/30 bg-background/20 px-1 text-[10px] font-normal"
                      >
                        {guiReport?.issues.length}
                      </Badge>
                    )}
                  </Button>
                </div>
              </CardContent>
            </Card>
            </>
          )}
        </div>
      </ScrollArea>

      <AlertDialog open={confirmPrune} onOpenChange={setConfirmPrune}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>清理 Claude history 残留</AlertDialogTitle>
            <AlertDialogDescription>
              即将从 <code>{report?.history_path ?? "history.jsonl"}</code> 删除{" "}
              <b>{report?.orphan_rows ?? 0}</b> 行。这些行引用的 Claude 会话文件已经不存在，
              删除后不可撤销。
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>取消</AlertDialogCancel>
            <AlertDialogAction
              className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
              onClick={() => {
                setConfirmPrune(false);
                void run("claude_history_prune", async () => {
                  const result = await api.pruneClaudeHistoryOrphans(claudeDir, false);
                  toast.success(`已从 Claude history 删除 ${result.removed_rows} 行`);
                  await refresh();
                });
              }}
            >
              确认清理
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <AlertDialog open={confirmGuiFix} onOpenChange={setConfirmGuiFix}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>补写会话标题，恢复 GUI 显示</AlertDialogTitle>
            <AlertDialogDescription>
              将为 <b>{guiReport?.issues.length ?? 0}</b> 条会话在其 jsonl 文件末尾追加一条{" "}
              <code>custom-title</code> 记录（与 Claude Code 官方"重命名会话"的写入格式一致），
              不会改动既有内容。完成后这些会话会重新出现在 VS Code 等 GUI 的历史列表中。
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>取消</AlertDialogCancel>
            <AlertDialogAction
              onClick={() => {
                setConfirmGuiFix(false);
                void run("claude_gui_fix", async () => {
                  const result = await api.repairClaudeGuiVisibility(claudeDir, false);
                  if (result.errors.length > 0) {
                    toast.warning(
                      `已修复 ${result.fixed} 条，${result.errors.length} 条失败`,
                      { description: result.errors.slice(0, 3).join("\n") },
                    );
                  } else {
                    toast.success(`已为 ${result.fixed} 条会话补写标题`);
                  }
                  await refresh();
                });
              }}
            >
              确认修复
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}

function Stat({
  label,
  value,
  warn,
  hint,
}: {
  label: string;
  value: string | number;
  warn?: boolean;
  hint?: string;
}) {
  return (
    <div className="flex min-w-0 flex-col gap-0.5 rounded-md border bg-muted/20 p-3">
      <div className="flex min-w-0 items-center gap-1 text-[11px] text-muted-foreground">
        <span className="min-w-0 break-words">{label}</span>
        {hint && (
          <Tooltip>
            <TooltipTrigger asChild>
              <Info className="h-3 w-3 shrink-0 text-muted-foreground/70" />
            </TooltipTrigger>
            <TooltipContent className="max-w-sm text-xs">{hint}</TooltipContent>
          </Tooltip>
        )}
      </div>
      <div
        className={`min-w-0 break-words text-lg font-semibold tabular-nums ${
          warn ? "text-amber-600" : ""
        }`}
      >
        {value}
      </div>
    </div>
  );
}

function DiffRow({
  label,
  ids,
  color,
  recommendation,
}: {
  label: string;
  ids: string[];
  color: "amber" | "rose" | "muted";
  recommendation?: string;
}) {
  if (ids.length === 0) return null;
  const preview = ids.slice(0, 4).join("  ");
  const toneClass =
    color === "amber"
      ? "border-amber-500/30 text-amber-600"
      : color === "rose"
        ? "border-rose-500/30 text-rose-500"
        : "border-muted-foreground/20 text-muted-foreground";
  return (
    <div className="flex min-w-0 items-start gap-2 text-xs">
      <AlertCircle className={`mt-0.5 h-3.5 w-3.5 shrink-0 ${toneClass}`} />
      <div className="min-w-0 flex-1 space-y-1">
        <div className="flex min-w-0 flex-wrap items-center gap-1.5">
          <span className="min-w-0 break-words">{label}</span>
          <Badge variant="outline" className={`h-5 shrink-0 px-1.5 font-normal ${toneClass}`}>
            {ids.length}
          </Badge>
          <Dialog>
            <DialogTrigger asChild>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="h-6 shrink-0 gap-1 px-2 text-[11px]"
              >
                <List className="h-3.5 w-3.5" />
                查看明细
              </Button>
            </DialogTrigger>
            <DialogContent className="flex h-[min(85vh,40rem)] w-[calc(100vw-2rem)] max-w-2xl flex-col gap-0 overflow-hidden p-0">
              <DialogHeader className="shrink-0 space-y-1 border-b px-5 py-4 pr-10">
                <DialogTitle className="min-w-0 break-words text-base">
                  {label}
                </DialogTitle>
                <DialogDescription className="flex flex-wrap items-center gap-2">
                  <span>{ids.length} 条会话 ID · 点击可复制</span>
                  {recommendation && (
                    <span className="rounded-md border border-dashed bg-muted/30 px-2 py-0.5 text-[11px] text-foreground/80">
                      建议：{recommendation}
                    </span>
                  )}
                </DialogDescription>
              </DialogHeader>
              <ScrollArea className="min-h-0 flex-1">
                <ol className="divide-y">
                  {ids.map((id, index) => (
                    <li
                      key={`${id}-${index}`}
                      className="group flex min-w-0 items-center gap-3 px-5 py-2 hover:bg-muted/40"
                    >
                      <span className="w-6 shrink-0 text-right text-[11px] tabular-nums text-muted-foreground">
                        {index + 1}
                      </span>
                      <code className="min-w-0 flex-1 break-all font-mono text-[11px] leading-5">
                        {id}
                      </code>
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon"
                        className="h-7 w-7 shrink-0 opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100"
                        onClick={async () => {
                          try {
                            await copyText(id);
                            toast.success("已复制到剪贴板");
                          } catch (e: any) {
                            toast.error("复制失败：" + String(e?.message ?? e));
                          }
                        }}
                        aria-label="复制会话 ID"
                      >
                        <Copy className="h-3.5 w-3.5" />
                      </Button>
                    </li>
                  ))}
                </ol>
              </ScrollArea>
            </DialogContent>
          </Dialog>
        </div>
        <div className="min-w-0 truncate font-mono text-[11px] text-muted-foreground">
          {preview}
        </div>
        {recommendation && (
          <div className="min-w-0 text-[11px] text-muted-foreground">
            <span className="mr-1 rounded-sm bg-muted/60 px-1 py-0.5 text-[10px] font-medium text-foreground/70">
              建议
            </span>
            <span className="[overflow-wrap:anywhere]">{recommendation}</span>
          </div>
        )}
      </div>
    </div>
  );
}
