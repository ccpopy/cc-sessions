import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Archive,
  CheckCircle2,
  Copy,
  Download,
  FileArchive,
  FolderOpen,
  ListFilter,
  Loader2,
  Package,
  Search,
  ShieldAlert,
  Upload,
} from "lucide-react";
import { toast } from "sonner";

import { TopBar } from "@/components/TopBar";
import { EmptyState } from "@/components/EmptyState";
import { DangerDialog } from "@/components/DangerDialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import { Switch } from "@/components/ui/switch";
import { Checkbox } from "@/components/ui/checkbox";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import {
  RadioGroup,
  RadioGroupItem,
} from "@/components/ui/radio-group";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

import { useSettings } from "@/stores/settings";
import { useSessions } from "@/hooks/useSessions";
import {
  api,
  type BundleListItem,
  type ExportReport,
  type ImportMode,
  type ProjectPathMapping,
  type SessionProvider,
  type SessionSummary,
} from "@/lib/api";
import { copyText } from "@/lib/clipboard";
import { sessionIdentity } from "@/lib/sessionIdentity";
import {
  filterSessionsByScope,
  type SessionScopeFilter,
} from "@/lib/sessionVisibility";
import { pickDirectoryPath, pickFilePath, saveFilePath } from "@/lib/dialog";
import { humanBytes, humanTokens } from "@/lib/format";
import { providerLabel } from "@/lib/providerTheme";
import { basename, dirname, joinPath } from "@/lib/cwd";

const SESSION_PAGE_SIZE = 500;

export default function TransferRoute({ provider = "codex" }: { provider?: SessionProvider }) {
  const settings = useSettings((s) => s.settings);
  const codexDir = settings?.codex_dir ?? "";
  const claudeDir = settings?.claude_dir ?? "";
  const opencodeDir = settings?.opencode_dir ?? "";
  const providerName = providerLabel(provider);
  const providerDir = provider === "codex" ? codexDir : provider === "claude" ? claudeDir : opencodeDir;

  return (
    <>
      <TopBar title={`${providerName} 导出 / 导入`} showListTools={false} />
      <ScrollArea className="flex-1">
        <div className="p-6">
          {!providerDir ? (
            <EmptyState
              icon={<Package className="h-10 w-10" />}
              title={`尚未配置 ${providerName} 目录`}
            />
          ) : (
            <Tabs defaultValue="export" className="space-y-4">
              <TabsList>
                <TabsTrigger value="export" className="gap-1.5">
                  <Upload className="h-3.5 w-3.5" /> 导出会话数据
                </TabsTrigger>
                <TabsTrigger value="import" className="gap-1.5">
                  <Download className="h-3.5 w-3.5" /> 导入会话数据
                </TabsTrigger>
              </TabsList>
              <TabsContent value="export">
                <ExportPanel
                  provider={provider}
                  codexDir={codexDir}
                  claudeDir={claudeDir}
                  opencodeDir={opencodeDir}
                />
              </TabsContent>
              <TabsContent value="import">
                <ImportPanel
                  provider={provider}
                  codexDir={codexDir}
                  claudeDir={claudeDir}
                  opencodeDir={opencodeDir}
                />
              </TabsContent>
            </Tabs>
          )}
        </div>
      </ScrollArea>
    </>
  );
}

// ========================= 导出 =========================

function ExportPanel({
  provider,
  codexDir,
  claudeDir,
  opencodeDir,
}: {
  provider: SessionProvider;
  codexDir: string;
  claudeDir: string;
  opencodeDir: string;
}) {
  const { sessions, loading: loadingSessions } = useSessions(provider, "");
  const [outDir, setOutDir] = useState("");
  const [machineLabel, setMachineLabel] = useState("");
  const [exportGroup, setExportGroup] = useState("default");
  const [activeOnly, setActiveOnly] = useState(true);
  const [sessionSearch, setSessionSearch] = useState("");
  // issue #30：默认只列正常会话，子代理/归档会话按需从下拉里切换
  const [sessionScope, setSessionScope] = useState<SessionScopeFilter>("main");
  const [visibleSessionLimit, setVisibleSessionLimit] = useState(SESSION_PAGE_SIZE);
  const [selectedSessionKeys, setSelectedSessionKeys] = useState<Set<string>>(new Set());
  const [running, setRunning] = useState(false);
  const [lastZipSource, setLastZipSource] = useState<string | null>(null);

  const pickDir = async () => {
    const picked = await pickDirectoryPath({ defaultPath: outDir || undefined });
    if (picked) {
      setOutDir(picked);
      setLastZipSource(null);
    }
  };

  useEffect(() => {
    setSelectedSessionKeys(new Set());
    setLastZipSource(null);
    setSessionScope("main");
    setVisibleSessionLimit(SESSION_PAGE_SIZE);
  }, [provider]);

  useEffect(() => {
    setVisibleSessionLimit(SESSION_PAGE_SIZE);
  }, [sessionScope, sessionSearch]);

  const toggle = (session: SessionSummary) => {
    const key = sessionIdentity(session);
    setSelectedSessionKeys((prev) => {
      const n = new Set(prev);
      if (n.has(key)) n.delete(key);
      else n.add(key);
      return n;
    });
  };

  const scopeOptions = useMemo(() => {
    const scopes: SessionScopeFilter[] =
      provider === "claude"
        ? ["main", "subagent", "all"]
        : ["main", "subagent", "archived", "all"];
    return scopes.map((scope) => ({
      value: scope,
      label:
        scope === "main"
          ? "会话"
          : scope === "subagent"
            ? provider === "opencode"
              ? "子会话"
              : "子代理"
            : scope === "archived"
              ? "归档会话"
              : "全部",
      count: filterSessionsByScope(sessions, scope).length,
    }));
  }, [provider, sessions]);
  const activeScopeLabel =
    scopeOptions.find((option) => option.value === sessionScope)?.label ?? "全部";

  const scopedSessions = useMemo(
    () => filterSessionsByScope(sessions, sessionScope),
    [sessions, sessionScope],
  );

  const filteredSessions = useMemo(() => {
    const terms = sessionSearch
      .trim()
      .toLowerCase()
      .split(/\s+/)
      .filter(Boolean);
    if (terms.length === 0) return scopedSessions;

    return scopedSessions.filter((s) => {
      const haystack = [
        s.id,
        s.title,
        s.first_user_message,
        s.cwd,
        s.cwd_display,
        s.source,
        s.git_branch,
        s.reasoning_effort,
      ]
        .filter(Boolean)
        .join("\n")
        .toLowerCase();
      return terms.every((term) => haystack.includes(term));
    });
  }, [scopedSessions, sessionSearch]);
  const filteredArchivedCount = useMemo(
    () => filteredSessions.filter((session) => session.archived).length,
    [filteredSessions],
  );
  const visibleSessions = useMemo(
    () => filteredSessions.slice(0, visibleSessionLimit),
    [filteredSessions, visibleSessionLimit],
  );
  const selectedSessions = useMemo(
    () => sessions.filter((session) => selectedSessionKeys.has(sessionIdentity(session))),
    [sessions, selectedSessionKeys],
  );
  const visibleSelectedCount = visibleSessions.filter((s) =>
    selectedSessionKeys.has(sessionIdentity(s)),
  ).length;
  const allSelected =
    visibleSessions.length > 0 &&
    visibleSessions.every((s) => selectedSessionKeys.has(sessionIdentity(s)));
  const someSelected =
    !allSelected && visibleSessions.some((s) => selectedSessionKeys.has(sessionIdentity(s)));
  const toggleAll = () => {
    setSelectedSessionKeys((prev) => {
      if (allSelected) {
        const n = new Set(prev);
        for (const s of visibleSessions) n.delete(sessionIdentity(s));
        return n;
      }
      const n = new Set(prev);
      for (const s of visibleSessions) n.add(sessionIdentity(s));
      return n;
    });
  };
  const copyId = async (id: string) => {
    try {
      await copyText(id);
      toast.success("已复制到剪贴板");
    } catch (e: any) {
      toast.error("复制失败：" + String(e?.message ?? e));
    }
  };

  const exportSessions = async (selected: SessionSummary[]) => {
    const r = await api.exportSessionBundles({
      provider,
      codex_dir: codexDir,
      claude_dir: claudeDir,
      opencode_dir: opencodeDir,
      out_dir: outDir,
      ids: selected.map((session) => session.id),
      targets: selected.map((session) => ({
        id: session.id,
        rollout_path: session.rollout_path,
      })),
      machine_label: machineLabel || undefined,
      export_group: exportGroup || undefined,
    });
    setLastZipSource(zipSourceFromReports(r));
    return r;
  };

  const exportSelected = async () => {
    if (!outDir) return toast.error("请先选择导出目录");
    if (selectedSessions.length === 0) return toast.error("请先勾选要导出的会话");
    setRunning(true);
    try {
      const r = await exportSessions(selectedSessions);
      const ok = r.filter((x) => x.ok).length;
      notifyExportResult(r, `已导出 ${ok}/${r.length} 条会话数据`);
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setRunning(false);
    }
  };

  const exportAll = async () => {
    if (!outDir) return toast.error("请先选择导出目录");
    setRunning(true);
    try {
      const r = await api.exportAllBundles({
        provider,
        codex_dir: codexDir,
        claude_dir: claudeDir,
        opencode_dir: opencodeDir,
        out_dir: outDir,
        machine_label: machineLabel || undefined,
        export_group: exportGroup || undefined,
        active_only: activeOnly,
      });
      const ok = r.filter((x) => x.ok).length;
      notifyExportResult(
        r,
        `已导出 ${ok}/${r.length} 条会话数据${activeOnly ? "（仅包含活跃记录）" : ""}`,
      );
      setLastZipSource(zipSourceFromReports(r));
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setRunning(false);
    }
  };

  const packZip = async () => {
    if (!outDir) return toast.error("请先选择导出目录");
    if (selectedSessions.length === 0 && !lastZipSource) {
      return toast.error("请先勾选要导出的会话，或先完成一次导出");
    }
    const zipPath = await saveFilePath({
      defaultPath: joinPath(outDir, `${provider}-bundles-${Date.now()}.zip`),
      filters: [{ name: "Zip", extensions: ["zip"] }],
      webPrompt: "请输入要写入的 zip 文件路径。该路径必须能被运行 cc-sessions webui 的环境访问。",
    });
    if (!zipPath) return;
    setRunning(true);
    try {
      let zipSource = lastZipSource;
      if (selectedSessions.length > 0) {
        const reports = await exportSessions(selectedSessions);
        zipSource = zipSourceFromReports(reports);
        const ok = reports.filter((x) => x.ok).length;
        if (!zipSource) {
          throw new Error(`导出成功 ${ok}/${reports.length} 条，但没有可打包的会话目录`);
        }
      }
      if (!zipSource) {
        throw new Error("没有可打包的会话目录");
      }
      const r = await api.packBundlesZip(zipSource, zipPath);
      toast.success(`打包完成：${r.files} 个文件 · ${humanBytes(r.bytes)}`);
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setRunning(false);
    }
  };

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="flex items-center gap-2 text-base">
            <FolderOpen className="h-4 w-4" />
            导出目标
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
            <div className="space-y-1.5">
              <Label className="text-xs">导出目录</Label>
              <div className="flex gap-1.5">
                <Input
                  value={outDir}
                  onChange={(e) => {
                    setOutDir(e.target.value);
                    setLastZipSource(null);
                  }}
                  placeholder="选一个本地目录"
                  className="font-mono text-xs"
                />
                <Button
                  variant="outline"
                  size="sm"
                  onClick={pickDir}
                  className="shrink-0"
                >
                  浏览
                </Button>
              </div>
            </div>
            <div className="grid grid-cols-2 gap-3">
              <div className="space-y-1.5">
                <Label className="text-xs">机器标签</Label>
                <Input
                  value={machineLabel}
                  onChange={(e) => setMachineLabel(e.target.value)}
                  placeholder="默认取主机名"
                />
              </div>
              <div className="space-y-1.5">
                <Label className="text-xs">export group</Label>
                <Input
                  value={exportGroup}
                  onChange={(e) => setExportGroup(e.target.value)}
                  placeholder="default"
                />
              </div>
            </div>
          </div>
          <Separator />
          <div className="flex flex-wrap items-center gap-3">
            {provider !== "claude" && (
              <div className="flex items-center gap-2">
                <Switch
                  id="active-only"
                  checked={activeOnly}
                  onCheckedChange={setActiveOnly}
                />
                <Label htmlFor="active-only" className="text-xs">
                  {provider === "codex"
                    ? "导出全部时仅导出当前分支（分支记录）"
                    : "导出全部时仅导出未归档会话"}
                </Label>
              </div>
            )}
            <div className="ml-auto flex gap-1.5">
              <Button
                size="sm"
                variant="outline"
                onClick={exportSelected}
                disabled={running || selectedSessions.length === 0}
                className="gap-1.5"
              >
                <Upload className="h-3.5 w-3.5" />
                导出勾选（{selectedSessions.length}）
              </Button>
              <Button
                size="sm"
                onClick={exportAll}
                disabled={running}
                className="gap-1.5"
                title="导出该服务商的全部会话，不受下方会话列表的筛选与搜索影响"
              >
                <Upload className="h-3.5 w-3.5" />
                导出全部
              </Button>
              <Button
                size="sm"
                variant="outline"
                onClick={packZip}
                disabled={running || (selectedSessions.length === 0 && !lastZipSource)}
                className="gap-1.5"
              >
                <FileArchive className="h-3.5 w-3.5" />
                打包成 zip
              </Button>
            </div>
          </div>
          {running && (
            <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <Loader2 className="h-3.5 w-3.5 animate-spin" /> 执行中…
            </div>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="flex items-center gap-2 text-base">
            <Archive className="h-4 w-4" />
            会话（勾选即可逐条导出）
            <Badge variant="secondary" className="h-5 px-1.5 font-normal">
              {filteredSessions.length === sessions.length
                ? sessions.length
                : `${filteredSessions.length}/${sessions.length}`}
            </Badge>
            {sessionScope !== "archived" && filteredArchivedCount > 0 && (
              <Badge variant="outline" className="h-5 gap-1 px-1.5 font-normal text-muted-foreground">
                <Archive className="h-3 w-3" />
                含已归档 {filteredArchivedCount}
              </Badge>
            )}
            {visibleSessions.length > 0 && (
              <span className="ml-auto text-xs font-normal text-muted-foreground">
                已选 {visibleSelectedCount}/{visibleSessions.length}
                {filteredSessions.length > visibleSessions.length
                  ? ` · 显示 ${visibleSessions.length}/${filteredSessions.length}`
                  : ""}
              </span>
            )}
          </CardTitle>
        </CardHeader>
        <CardContent>
          {loadingSessions ? (
            <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <Loader2 className="h-3.5 w-3.5 animate-spin" /> 加载中…
            </div>
          ) : sessions.length === 0 ? (
            <EmptyState title="没有会话可导出" />
          ) : (
            <div className="space-y-3">
              <div className="flex flex-wrap items-center gap-2">
                <div className="relative min-w-[12rem] flex-1 sm:max-w-md">
                  <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
                  <Input
                    value={sessionSearch}
                    onChange={(e) => setSessionSearch(e.target.value)}
                    placeholder="搜索 id / 标题 / 首条消息 / 目录"
                    className="h-9 pl-8 text-xs"
                  />
                </div>
                <Select
                  value={sessionScope}
                  onValueChange={(value) => setSessionScope(value as SessionScopeFilter)}
                >
                  <SelectTrigger
                    className="h-9 w-32 shrink-0 gap-1.5 text-xs"
                    aria-label="按会话范围筛选"
                  >
                    <ListFilter className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                    <SelectValue className="flex-1 text-left">{activeScopeLabel}</SelectValue>
                  </SelectTrigger>
                  <SelectContent>
                    {scopeOptions.map((option) => (
                      <SelectItem key={option.value} value={option.value} className="text-xs">
                        {option.label}{" "}
                        <span className="ml-1 tabular-nums text-muted-foreground">
                          {option.count}
                        </span>
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
              {filteredSessions.length === 0 ? (
                <EmptyState
                  title={
                    sessionSearch.trim() || sessionScope === "all"
                      ? "没有匹配的会话"
                      : `没有${activeScopeLabel}`
                  }
                />
              ) : (
                <div className="rounded-md border">
                  <div className="grid grid-cols-[2rem_8rem_minmax(0,1fr)_9rem_5rem] items-center gap-2 border-b bg-muted/40 px-3 py-2 text-[11px] font-medium text-muted-foreground">
                    <Checkbox
                      checked={allSelected ? true : someSelected ? "indeterminate" : false}
                      onCheckedChange={toggleAll}
                      aria-label="全选当前列表"
                    />
                    <span>id</span>
                    <span>标题</span>
                    <span>model</span>
                    <span className="text-right">tokens</span>
                  </div>
                  <ScrollArea className="h-96">
                    <ul className="divide-y text-xs">
                      {visibleSessions.map((s) => (
                        <li
                          key={sessionIdentity(s)}
                          className={`grid grid-cols-[2rem_8rem_minmax(0,1fr)_9rem_5rem] items-center gap-2 px-3 py-2 ${
                            selectedSessionKeys.has(sessionIdentity(s))
                              ? "bg-primary/5"
                              : "hover:bg-muted/30"
                          }`}
                        >
                          <Checkbox
                            checked={selectedSessionKeys.has(sessionIdentity(s))}
                            onCheckedChange={() => toggle(s)}
                            aria-label="选择该会话"
                          />
                          <Tooltip>
                            <TooltipTrigger asChild>
                              <button
                                type="button"
                                onClick={() => copyId(s.id)}
                                className="flex min-w-0 items-center gap-1 truncate text-left font-mono text-[11px] text-muted-foreground hover:text-foreground"
                                aria-label="复制会话 ID"
                              >
                                <span className="truncate">{s.id.slice(0, 8)}…</span>
                                <Copy className="h-3 w-3 shrink-0 opacity-60" />
                              </button>
                            </TooltipTrigger>
                            <TooltipContent className="font-mono text-[11px]">
                              {s.id} · 点击复制
                            </TooltipContent>
                          </Tooltip>
                          <div className="flex min-w-0 items-center gap-1.5">
                            <span className="min-w-0 truncate" title={s.title || s.first_user_message || ""}>
                              {s.title || s.first_user_message || "—"}
                            </span>
                            {s.archived && (
                              <Badge variant="outline" className="h-5 shrink-0 px-1.5 text-[11px] font-normal">
                                已归档
                              </Badge>
                            )}
                          </div>
                          <span className="truncate text-muted-foreground" title={s.model ?? ""}>
                            {s.model ?? "—"}
                          </span>
                          <span className="text-right tabular-nums text-muted-foreground">
                            {humanTokens(s.tokens_used)}
                          </span>
                        </li>
                      ))}
                    </ul>
                    {visibleSessions.length < filteredSessions.length && (
                      <div className="border-t p-3 text-center">
                        <Button
                          type="button"
                          variant="ghost"
                          size="sm"
                          onClick={() =>
                            setVisibleSessionLimit((limit) => limit + SESSION_PAGE_SIZE)
                          }
                        >
                          继续加载（剩余 {filteredSessions.length - visibleSessions.length} 条）
                        </Button>
                      </div>
                    )}
                  </ScrollArea>
                </div>
              )}
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

function zipSourceFromReports(reports: ExportReport[]): string | null {
  const bundlePaths = reports
    .filter((report) => report.ok && report.bundle_path)
    .map((report) => report.bundle_path as string);
  if (bundlePaths.length === 0) return null;
  if (bundlePaths.length === 1) return bundlePaths[0];

  const parentDirs = Array.from(new Set(bundlePaths.map(dirname)));
  return parentDirs.length === 1 ? parentDirs[0] : null;
}

function notifyExportResult(reports: ExportReport[], summary: string) {
  const failed = reports.filter((report) => !report.ok);
  const description = failed
    .map((report) => `${report.session_id.slice(0, 8)}：${report.error ?? report.skipped_reason ?? "导出失败"}`)
    .slice(0, 3)
    .join("\n");
  if (reports.length === 0 || failed.length === reports.length) {
    toast.error(reports.length === 0 ? "没有找到可导出的会话" : summary, {
      description: description || undefined,
    });
  } else if (failed.length > 0) {
    toast.warning(summary, { description: description || undefined });
  } else {
    toast.success(summary);
  }
}

// ========================= 导入 =========================

function ImportPanel({
  provider,
  codexDir,
  claudeDir,
  opencodeDir,
}: {
  provider: SessionProvider;
  codexDir: string;
  claudeDir: string;
  opencodeDir: string;
}) {
  const [srcDir, setSrcDir] = useState("");
  const [items, setItems] = useState<BundleListItem[]>([]);
  const [mode, setMode] = useState<ImportMode>("skip");
  const [makeVisible, setMakeVisible] = useState(true);
  const [strict, setStrict] = useState(true);
  const [running, setRunning] = useState(false);
  const [scanLoading, setScanLoading] = useState(false);
  const [projectTargets, setProjectTargets] = useState<Record<string, string>>({});
  const [confirmImport, setConfirmImport] = useState(false);
  const [verifiedSource, setVerifiedSource] = useState<{
    dir: string;
    provider: SessionProvider;
  } | null>(null);
  const scanGeneration = useRef(0);
  const latestSource = useRef(srcDir);
  const latestProvider = useRef(provider);
  latestSource.current = srcDir;
  latestProvider.current = provider;

  const pickDir = async () => {
    const picked = await pickDirectoryPath();
    if (picked) {
      setSrcDir(picked);
    }
  };

  const pickZip = async () => {
    const picked = await pickFilePath({
      filters: [{ name: "Zip", extensions: ["zip"] }],
      webPrompt: "请输入要导入的 zip 文件路径。该路径必须能被运行 cc-sessions webui 的环境读取。",
    });
    if (picked) {
      setRunning(true);
      try {
        const r = await api.unpackZipToTemp(picked);
        toast.success(`已读取 zip：${r.files} 文件 · ${humanBytes(r.bytes)}`);
        setSrcDir(r.path);
      } catch (e) {
        toast.error(String((e as Error)?.message ?? e));
      } finally {
        setRunning(false);
      }
    }
  };

  const rescan = useCallback(
    async (dir: string) => {
      const generation = ++scanGeneration.current;
      setItems([]);
      setProjectTargets({});
      setVerifiedSource(null);
      if (!dir) {
        setScanLoading(false);
        return;
      }
      setScanLoading(true);
      try {
        const r = await api.verifyBundlesCmd(dir, provider);
        if (
          generation !== scanGeneration.current ||
          latestSource.current !== dir ||
          latestProvider.current !== provider
        ) {
          return;
        }
        setItems(r);
        setVerifiedSource({ dir, provider });
      } catch (e) {
        if (generation === scanGeneration.current) {
          toast.error(String((e as Error)?.message ?? e));
        }
      } finally {
        if (generation === scanGeneration.current) {
          setScanLoading(false);
        }
      }
    },
    [provider],
  );

  useEffect(() => {
    if (srcDir) void rescan(srcDir);
  }, [srcDir, rescan]);

  const sourceProjects = useMemo(() => {
    return Array.from(
      new Set(
        items
          .map((item) => item.manifest.session_cwd.trim())
          .filter((cwd) => cwd.length > 0),
      ),
    ).sort((a, b) => a.localeCompare(b));
  }, [items]);

  useEffect(() => {
    setProjectTargets((prev) => {
      const next: Record<string, string> = {};
      for (const source of sourceProjects) {
        next[source] = prev[source] ?? source;
      }
      return next;
    });
  }, [sourceProjects]);

  const pickProjectTarget = async (source: string) => {
    const picked = await pickDirectoryPath({
      defaultPath: projectTargets[source] || undefined,
      title: `选择 ${basename(source)} 的目标项目目录`,
      webPrompt: `请输入 ${basename(source)} 的目标项目目录。该路径必须能被运行 cc-sessions webui 的环境访问。`,
    });
    if (picked) {
      setProjectTargets((prev) => ({ ...prev, [source]: picked }));
    }
  };

  const buildProjectMappings = (): ProjectPathMapping[] => {
    const mappings: ProjectPathMapping[] = [];
    for (const source of sourceProjects) {
      const target = (projectTargets[source] ?? "").trim();
      if (!target) {
        throw new Error(`请为源项目 ${source} 选择目标项目目录`);
      }
      if (target !== source) {
        mappings.push({ source_cwd: source, target_cwd: target });
      }
    }
    return mappings;
  };

  const runImport = async () => {
    if (!srcDir) {
      toast.error("请先选择数据所在的目录或解压好的 zip 文件夹");
      return;
    }
    if (
      scanLoading ||
      verifiedSource?.dir !== srcDir ||
      verifiedSource?.provider !== provider
    ) {
      toast.error("数据源尚未完成当前服务商的校验，请等待扫描完成");
      return;
    }
    const sourceAtStart = srcDir;
    const providerAtStart = provider;
    const reviewedScan = JSON.stringify(items);
    setRunning(true);
    try {
      const currentScan = await api.verifyBundlesCmd(sourceAtStart, providerAtStart);
      if (
        latestSource.current !== sourceAtStart ||
        latestProvider.current !== providerAtStart
      ) {
        throw new Error("导入期间数据源或服务商已改变，已取消本次导入");
      }
      setItems(currentScan);
      setVerifiedSource({ dir: sourceAtStart, provider: providerAtStart });
      if (JSON.stringify(currentScan) !== reviewedScan) {
        throw new Error("数据源内容在确认后发生变化，已刷新列表，请核对后重新导入");
      }
      const projectMappings = buildProjectMappings();
      const r = await api.importSessionBundles({
        provider: providerAtStart,
        src_dir: sourceAtStart,
        codex_dir: codexDir,
        claude_dir: claudeDir,
        opencode_dir: opencodeDir,
        mode,
        make_visible: providerAtStart === "codex" ? makeVisible : false,
        strict,
        project_mappings: projectMappings,
      });
      const ok = r.filter((x) => x.ok).length;
      const skipped = r.filter((x) => x.skipped_reason).length;
      const fail = r.filter((x) => x.error).length;
      const shaBad = r.filter((x) => x.sha_mismatch).length;
      const summary = `导入完成：${ok}/${r.length}${skipped ? ` · ${skipped} 条跳过` : ""}${fail ? ` · ${fail} 条失败` : ""}${shaBad ? ` · ${shaBad} 条 sha 不一致` : ""}`;
      const failedDetails = r
        .filter((item) => !item.ok || item.error)
        .map((item) => `${item.session_id.slice(0, 8)}：${item.error ?? "未完整导入"}`)
        .slice(0, 3)
        .join("\n");
      if (r.length === 0 || ok === 0) {
        toast.error(r.length === 0 ? "没有找到可导入的会话数据" : summary, {
          description: failedDetails || undefined,
        });
      } else if (fail > 0 || shaBad > 0 || ok < r.length) {
        toast.warning(summary, { description: failedDetails || undefined });
      } else {
        toast.success(summary);
      }
      await rescan(sourceAtStart);
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setRunning(false);
    }
  };

  const verified = items.filter((x) => x.verified === true).length;
  const corrupt = items.filter((x) => x.verified === false).length;

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="flex items-center gap-2 text-base">
            <FolderOpen className="h-4 w-4" />
            数据源
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="space-y-1.5">
            <Label className="text-xs">数据来源</Label>
            <div className="flex gap-1.5">
              <Input
                value={srcDir}
                onChange={(e) => setSrcDir(e.target.value)}
                placeholder="选择包含 manifest.json 的文件夹，或直接导入 zip"
                className="font-mono text-xs"
              />
              <Button variant="outline" size="sm" onClick={pickDir} className="shrink-0">
                浏览目录
              </Button>
              <Button variant="outline" size="sm" onClick={pickZip} className="shrink-0 gap-1.5">
                <FileArchive className="h-3.5 w-3.5" /> 导入 zip
              </Button>
            </div>
          </div>
          <Separator />
          {sourceProjects.length > 0 && (
            <>
              <div className="space-y-2">
                <div className="flex items-center justify-between gap-3">
                  <Label className="text-xs">项目归属映射</Label>
                  <Badge variant="secondary" className="h-5 px-1.5 font-normal">
                    {sourceProjects.length}
                  </Badge>
                </div>
                <div className="rounded-md border">
                  <div className="grid grid-cols-[minmax(0,1fr)_minmax(0,1fr)_5rem] items-center gap-2 border-b bg-muted/40 px-3 py-2 text-[11px] font-medium text-muted-foreground">
                    <span>源项目路径</span>
                    <span>目标项目路径</span>
                    <span>操作</span>
                  </div>
                  <div className="max-h-56 divide-y overflow-auto">
                    {sourceProjects.map((source) => (
                      <div
                        key={source}
                        className="grid grid-cols-[minmax(0,1fr)_minmax(0,1fr)_5rem] items-center gap-2 px-3 py-2"
                      >
                        <span className="truncate font-mono text-[11px] text-muted-foreground" title={source}>
                          {source}
                        </span>
                        <Input
                          value={projectTargets[source] ?? ""}
                          onChange={(e) =>
                            setProjectTargets((prev) => ({ ...prev, [source]: e.target.value }))
                          }
                          className="h-8 font-mono text-[11px]"
                        />
                        <Button
                          type="button"
                          variant="outline"
                          size="sm"
                          onClick={() => pickProjectTarget(source)}
                        >
                          浏览
                        </Button>
                      </div>
                    ))}
                  </div>
                </div>
              </div>
              <Separator />
            </>
          )}
          <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
            <div className="space-y-1.5">
              <Label className="text-xs">冲突策略</Label>
              <RadioGroup
                value={mode}
                onValueChange={(v) => setMode(v as ImportMode)}
                className="flex flex-col gap-1"
              >
                <div className="flex items-center gap-2">
                  <RadioGroupItem value="skip" id="m-skip" />
                  <Label htmlFor="m-skip" className="text-xs font-normal">
                    跳过已存在的记录（如果本地已有同名会话则不导入，推荐使用）
                  </Label>
                </div>
                <div className="flex items-center gap-2">
                  <RadioGroupItem value="keep_local" id="m-keep" />
                  <Label htmlFor="m-keep" className="text-xs font-normal">
                    保留较新版本（如果本地已有且更新时间较晚，则保留本地，否则用导入的版本覆盖）
                  </Label>
                </div>
                <div className="flex items-center gap-2">
                  <RadioGroupItem value="overwrite" id="m-over" />
                  <Label htmlFor="m-over" className="text-xs font-normal">
                    强制覆盖（不管本地记录的新旧，直接用导入的数据予以替换）
                  </Label>
                </div>
              </RadioGroup>
            </div>
            <div className="space-y-3">
              {provider === "codex" && (
                <div className="space-y-1.5">
                  <div className="flex items-center gap-2">
                    <Switch
                      id="visible"
                      checked={makeVisible}
                      onCheckedChange={setMakeVisible}
                    />
                    <Label htmlFor="visible" className="cursor-pointer text-xs">
                      导入完成后，在应用内立刻显示这些会话内容
                    </Label>
                  </div>
                  <div className="pl-11 text-[11px] text-muted-foreground">
                    开启后会自动更新数据库和索引，保证记录刷新。如果关闭，仅将文件放入本地目录中，应用内可能暂时不可见。
                  </div>
                </div>
              )}
              <div className="space-y-1.5">
                <div className="flex items-center gap-2">
                  <Switch id="strict" checked={strict} onCheckedChange={setStrict} />
                  <Label htmlFor="strict" className="cursor-pointer text-xs">
                    严格 sha256 校验（推荐）
                  </Label>
                </div>
                <div className="pl-11 text-[11px] text-muted-foreground">
                  开启后：损坏的数据会被跳过。关闭后：即使校验失败也强制导入（除非你确定自己修改过内容，否则不建议关闭此项）。
                </div>
              </div>
            </div>
          </div>
          <div className="flex flex-wrap items-center gap-3">
            {items.length > 0 && (
              <div className="flex items-center gap-2 text-xs">
                {corrupt === 0 ? (
                  <CheckCircle2 className="h-3.5 w-3.5 text-emerald-500" />
                ) : (
                  <ShieldAlert className="h-3.5 w-3.5 text-amber-500" />
                )}
                <span>
                  {items.length} 条数据 · 校验 {verified} 通过
                  {corrupt ? ` · ${corrupt} 失败` : ""}
                </span>
              </div>
            )}
            <div className="ml-auto flex gap-1.5">
              <Button
                variant="outline"
                size="sm"
                onClick={() => rescan(srcDir)}
                disabled={!srcDir || scanLoading}
                className="gap-1.5"
              >
                重新扫描
              </Button>
              <Button
                size="sm"
                onClick={() => {
                  if (mode === "skip") {
                    void runImport();
                  } else {
                    setConfirmImport(true);
                  }
                }}
                disabled={
                  running ||
                  scanLoading ||
                  items.length === 0 ||
                  verifiedSource?.dir !== srcDir ||
                  verifiedSource?.provider !== provider
                }
                className="gap-1.5"
              >
                <Download className="h-3.5 w-3.5" />
                执行导入
              </Button>
            </div>
          </div>
          {(running || scanLoading) && (
            <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              {running ? "执行中…" : "扫描中…"}
            </div>
          )}
        </CardContent>
      </Card>

      {items.length > 0 && (
        <Card>
          <CardHeader className="pb-3">
            <CardTitle className="flex items-center gap-2 text-base">
              待导入列表
              <Badge variant="secondary" className="h-5 px-1.5 font-normal">
                {items.length}
              </Badge>
            </CardTitle>
          </CardHeader>
          <CardContent>
            <div className="rounded-md border">
              <div className="grid grid-cols-[8rem_minmax(0,1fr)_8rem_7rem_9rem_4rem] items-center gap-2 border-b bg-muted/40 px-3 py-2 text-[11px] font-medium text-muted-foreground">
                <span>id</span>
                <span>标题</span>
                <span>源设备(machine)</span>
                <span>服务商(provider)</span>
                <span>导出时间</span>
                <span>校验</span>
              </div>
              <ScrollArea className="h-96">
                <ul className="divide-y text-xs">
                  {items.map((it) => (
                    <li
                      key={it.bundle_dir}
                      className="grid grid-cols-[8rem_minmax(0,1fr)_8rem_7rem_9rem_4rem] items-center gap-2 px-3 py-2 hover:bg-muted/20"
                    >
                      <Tooltip>
                        <TooltipTrigger asChild>
                          <button
                            type="button"
                            onClick={async () => {
                              try {
                                await copyText(it.manifest.session_id);
                                toast.success("已复制到剪贴板");
                              } catch (e: any) {
                                toast.error("复制失败：" + String(e?.message ?? e));
                              }
                            }}
                            className="flex min-w-0 items-center gap-1 truncate text-left font-mono text-[11px] text-muted-foreground hover:text-foreground"
                            aria-label="复制会话 ID"
                          >
                            <span className="truncate">
                              {it.manifest.session_id.slice(0, 8)}…
                            </span>
                            <Copy className="h-3 w-3 shrink-0 opacity-60" />
                          </button>
                        </TooltipTrigger>
                        <TooltipContent className="font-mono text-[11px]">
                          {it.manifest.session_id} · 点击复制
                        </TooltipContent>
                      </Tooltip>
                      <span
                        className="truncate"
                        title={it.manifest.thread_name || ""}
                      >
                        {it.manifest.thread_name || "—"}
                      </span>
                      <span
                        className="truncate text-muted-foreground"
                        title={it.manifest.export_machine}
                      >
                        {it.manifest.export_machine}
                      </span>
                      <span
                        className="truncate text-muted-foreground"
                        title={it.manifest.model_provider ?? ""}
                      >
                        {it.manifest.model_provider ?? "—"}
                      </span>
                      <span
                        className="truncate text-muted-foreground"
                        title={it.manifest.exported_at}
                      >
                        {it.manifest.exported_at}
                      </span>
                      <span>
                        {it.verified === true ? (
                          <Badge
                            variant="outline"
                            className="h-5 border-emerald-500/30 px-1.5 text-emerald-600"
                          >
                            OK
                          </Badge>
                        ) : it.verified === false ? (
                          <Badge
                            variant="outline"
                            className="h-5 border-rose-500/30 px-1.5 text-rose-500"
                          >
                            损坏
                          </Badge>
                        ) : (
                          <Badge
                            variant="outline"
                            className="h-5 px-1.5 text-muted-foreground"
                          >
                            ?
                          </Badge>
                        )}
                      </span>
                    </li>
                  ))}
                </ul>
              </ScrollArea>
            </div>
          </CardContent>
        </Card>
      )}

      <DangerDialog
        open={confirmImport}
        onOpenChange={setConfirmImport}
        title={mode === "overwrite" ? "确认强制覆盖本地会话" : "确认按更新时间替换本地会话"}
        confirmText={mode === "overwrite" ? "强制覆盖并导入" : "比较后导入"}
        onConfirm={runImport}
      >
        <p>
          将处理 <b>{items.length}</b> 条 {providerLabel(provider)} 会话，目标目录：
          <code>{provider === "codex" ? codexDir : provider === "claude" ? claudeDir : opencodeDir}</code>。
        </p>
        <p>
          {mode === "overwrite"
            ? "同 ID 的本地会话会被导入数据直接替换。"
            : "只有 bundle 的会话更新时间明确晚于本地时才会替换；无法可靠比较时保留本地。"}
        </p>
        <p className="text-destructive">覆盖前不会自动创建备份，请确认重要会话已有可用备份。</p>
      </DangerDialog>
    </div>
  );
}
