import { useEffect, useState, type ReactNode } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import {
  AlertTriangle,
  CheckCircle2,
  ExternalLink,
  FolderOpen,
  Home,
  RefreshCw,
  Settings as SettingsIcon,
} from "lucide-react";
import {
  Sheet,
  SheetContent,
  SheetHeader,
  SheetTitle,
  SheetTrigger,
  SheetDescription,
  SheetFooter,
} from "@/components/ui/sheet";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { api, type DirValidation, type UpdateCheckResult } from "@/lib/api";
import { useSettings } from "@/stores/settings";
import { toast } from "sonner";

type Props = {
  trigger?: ReactNode;
};

export function SettingsSheet({ trigger }: Props) {
  const settings = useSettings((s) => s.settings);
  const save = useSettings((s) => s.save);
  const load = useSettings((s) => s.load);
  const [codex, setCodex] = useState("");
  const [claude, setClaude] = useState("");
  const [gemini, setGemini] = useState("");
  const [backup, setBackup] = useState("");
  const [codexValidation, setCodexValidation] = useState<DirValidation | null>(null);
  const [claudeValidation, setClaudeValidation] = useState<DirValidation | null>(null);
  const [geminiValidation, setGeminiValidation] = useState<DirValidation | null>(null);
  const [updateState, setUpdateState] = useState<UpdateCheckResult>({ state: "idle" });
  const [currentVersion, setCurrentVersion] = useState("");
  const [currentVersionError, setCurrentVersionError] = useState("");

  useEffect(() => {
    if (!settings) return;
    setCodex(settings.codex_dir);
    setClaude(settings.claude_dir);
    setGemini(settings.gemini_dir);
    setBackup(settings.backup_dir);
  }, [settings]);

  useEffect(() => {
    api.appVersion()
      .then((version) => {
        setCurrentVersion(version);
        setCurrentVersionError("");
      })
      .catch((e: any) => {
        setCurrentVersion("");
        setCurrentVersionError(String(e?.message ?? e));
      });
  }, []);

  useEffect(() => {
    if (!codex) return;
    const id = window.setTimeout(async () => {
      try {
        const v = await api.validateCodexDir(codex);
        setCodexValidation(v);
      } catch {
        setCodexValidation(null);
      }
    }, 200);
    return () => window.clearTimeout(id);
  }, [codex]);

  useEffect(() => {
    if (!claude) return;
    const id = window.setTimeout(async () => {
      try {
        const v = await api.validateClaudeDir(claude);
        setClaudeValidation(v);
      } catch {
        setClaudeValidation(null);
      }
    }, 200);
    return () => window.clearTimeout(id);
  }, [claude]);

  useEffect(() => {
    if (!gemini) return;
    const id = window.setTimeout(async () => {
      try {
        const v = await api.validateGeminiDir(gemini);
        setGeminiValidation(v);
      } catch {
        setGeminiValidation(null);
      }
    }, 200);
    return () => window.clearTimeout(id);
  }, [gemini]);

  const pick = async (setter: (s: string) => void, cur: string) => {
    const picked = await openDialog({ directory: true, defaultPath: cur });
    if (typeof picked === "string") setter(picked);
  };

  const useDefault = async () => {
    const d = await api.defaultCodexDir();
    setCodex(d);
  };

  const useDefaultClaude = async () => {
    const d = await api.defaultClaudeDir();
    setClaude(d);
  };

  const useDefaultGemini = async () => {
    const d = await api.defaultGeminiDir();
    setGemini(d);
  };

  const onSave = async () => {
    try {
      await save({ codex_dir: codex, claude_dir: claude, gemini_dir: gemini, backup_dir: backup });
      toast.success("设置已保存");
      await load();
    } catch (e: any) {
      toast.error("保存失败: " + String(e?.message ?? e));
    }
  };

  const checkUpdate = async () => {
    setUpdateState({ state: "checking" });
    try {
      const [currentVersion, latest] = await Promise.all([
        api.appVersion(),
        fetch("https://api.github.com/repos/ccpopy/cc-sessions/releases/latest", {
          headers: { Accept: "application/vnd.github+json" },
        }),
      ]);
      if (!latest.ok) {
        throw new Error(`GitHub 返回 ${latest.status}`);
      }
      const payload = await latest.json() as { tag_name?: unknown; html_url?: unknown };
      const latestTag = typeof payload.tag_name === "string" ? payload.tag_name : "";
      const htmlUrl = typeof payload.html_url === "string" ? payload.html_url : "";
      if (!latestTag || !htmlUrl) {
        throw new Error("GitHub Release 响应缺少 tag_name 或 html_url");
      }
      const latestVersion = normalizeVersion(latestTag);
      const next: UpdateCheckResult = compareVersions(latestVersion, currentVersion) > 0
        ? {
            state: "available",
            current_version: currentVersion,
            latest_version: latestVersion,
            html_url: htmlUrl,
          }
        : {
            state: "current",
            current_version: currentVersion,
            latest_version: latestVersion,
            html_url: htmlUrl,
          };
      setUpdateState(next);
    } catch (e: any) {
      setUpdateState({ state: "error", message: String(e?.message ?? e) });
    }
  };

  const openReleasePage = async () => {
    try {
      await api.openLatestReleasePage();
    } catch (e: any) {
      toast.error("打开 Release 页面失败: " + String(e?.message ?? e));
    }
  };

  const defaultTrigger = (
    <Tooltip>
      <TooltipTrigger asChild>
        <SheetTrigger asChild>
          <Button variant="ghost" size="icon" aria-label="设置">
            <SettingsIcon className="h-4 w-4" />
          </Button>
        </SheetTrigger>
      </TooltipTrigger>
      <TooltipContent>设置 (Ctrl + ,)</TooltipContent>
    </Tooltip>
  );

  return (
    <Sheet>
      {trigger ? <SheetTrigger asChild>{trigger}</SheetTrigger> : defaultTrigger}
      <SheetContent side="right" className="w-[440px] sm:max-w-[440px]">
        <SheetHeader className="space-y-1">
          <SheetTitle>设置</SheetTitle>
          <SheetDescription>
            本地运行，只有手动检查更新时会请求 GitHub；配置写入本机 config 目录。
          </SheetDescription>
        </SheetHeader>

        <div className="mt-6 space-y-6">
          <div className="space-y-2">
            <Label className="text-sm font-medium">Codex 目录</Label>
            <div className="flex gap-2">
              <Input
                value={codex}
                onChange={(e) => setCodex(e.target.value)}
                placeholder={"C:\\Users\\<me>\\.codex"}
                className="font-mono text-xs"
              />
              <Button variant="outline" size="icon" onClick={() => pick(setCodex, codex)} title="选择目录">
                <FolderOpen className="h-4 w-4" />
              </Button>
              <Button variant="outline" size="icon" onClick={useDefault} title="使用默认">
                <Home className="h-4 w-4" />
              </Button>
            </div>
            <ValidationBadge v={codexValidation} provider="codex" />
          </div>

          <Separator />

          <div className="space-y-2">
            <Label className="text-sm font-medium">Claude 目录</Label>
            <div className="flex gap-2">
              <Input
                value={claude}
                onChange={(e) => setClaude(e.target.value)}
                placeholder={"C:\\Users\\<me>\\.claude"}
                className="font-mono text-xs"
              />
              <Button variant="outline" size="icon" onClick={() => pick(setClaude, claude)} title="选择目录">
                <FolderOpen className="h-4 w-4" />
              </Button>
              <Button variant="outline" size="icon" onClick={useDefaultClaude} title="使用默认">
                <Home className="h-4 w-4" />
              </Button>
            </div>
            <ValidationBadge v={claudeValidation} provider="claude" />
          </div>

          <Separator />

          <div className="space-y-2">
            <Label className="text-sm font-medium">Gemini / Antigravity 目录</Label>
            <div className="flex gap-2">
              <Input
                value={gemini}
                onChange={(e) => setGemini(e.target.value)}
                placeholder={"C:\\Users\\<me>\\.gemini"}
                className="font-mono text-xs"
              />
              <Button variant="outline" size="icon" onClick={() => pick(setGemini, gemini)} title="选择目录">
                <FolderOpen className="h-4 w-4" />
              </Button>
              <Button variant="outline" size="icon" onClick={useDefaultGemini} title="使用默认">
                <Home className="h-4 w-4" />
              </Button>
            </div>
            <ValidationBadge v={geminiValidation} provider="gemini" />
          </div>

          <Separator />

          <div className="space-y-2">
            <Label className="text-sm font-medium">备份目录</Label>
            <div className="flex gap-2">
              <Input
                value={backup}
                onChange={(e) => setBackup(e.target.value)}
                className="font-mono text-xs"
              />
              <Button variant="outline" size="icon" onClick={() => pick(setBackup, backup)} title="选择目录">
                <FolderOpen className="h-4 w-4" />
              </Button>
            </div>
            <p className="text-xs text-muted-foreground">
              推荐放在 Codex 或 Claude 目录外，避免把备份目录再次纳入备份。
            </p>
          </div>

          <Separator />

          <div className="space-y-3">
            <div className="flex items-center justify-between gap-3">
              <div className="min-w-0">
                <Label className="text-sm font-medium">版本更新</Label>
                <p className="mt-1 text-xs text-muted-foreground">
                  检查 GitHub Release 更新。
                </p>
              </div>
              <Button
                variant="outline"
                size="sm"
                className="h-8 shrink-0 gap-1.5"
                disabled={updateState.state === "checking"}
                onClick={checkUpdate}
              >
                <RefreshCw className={updateState.state === "checking" ? "h-3.5 w-3.5 animate-spin" : "h-3.5 w-3.5"} />
                检查更新
              </Button>
            </div>
            <UpdateStatus
              state={updateState}
              currentVersion={currentVersion}
              currentVersionError={currentVersionError}
              onOpenRelease={openReleasePage}
            />
          </div>
        </div>

        <SheetFooter className="mt-6">
          <Button onClick={onSave} className="w-full">
            保存设置
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}

function UpdateStatus({
  state,
  currentVersion,
  currentVersionError,
  onOpenRelease,
}: {
  state: UpdateCheckResult;
  currentVersion: string;
  currentVersionError: string;
  onOpenRelease: () => void;
}) {
  if (state.state === "idle") {
    return (
      <div className="text-xs text-muted-foreground">
        {currentVersionError
          ? `当前版本读取失败：${currentVersionError}`
          : `当前版本：${currentVersion || "读取中…"}`}
      </div>
    );
  }
  if (state.state === "checking") {
    return <div className="text-xs text-muted-foreground">正在检查 GitHub 最新版本…</div>;
  }
  if (state.state === "error") {
    return (
      <Badge variant="outline" className="gap-1 border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400">
        <AlertTriangle className="h-3 w-3" />
        检查失败：{state.message}
      </Badge>
    );
  }
  if (state.state === "current") {
    return (
      <Badge variant="outline" className="gap-1 border-emerald-500/40 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400">
        <CheckCircle2 className="h-3 w-3" />
        已是最新版本 {state.current_version}
      </Badge>
    );
  }
  return (
    <div className="flex flex-wrap items-center gap-2">
      <Badge variant="outline" className="gap-1 border-sky-500/40 bg-sky-500/10 text-sky-600 dark:text-sky-400">
        有新版本 {state.latest_version}，当前 {state.current_version}
      </Badge>
      <Button variant="secondary" size="sm" className="h-7 gap-1.5" onClick={onOpenRelease}>
        <ExternalLink className="h-3.5 w-3.5" />
        打开下载页面
      </Button>
    </div>
  );
}

function normalizeVersion(raw: string): string {
  return raw.trim().replace(/^v/i, "");
}

function compareVersions(a: string, b: string): number {
  const pa = parseVersion(a);
  const pb = parseVersion(b);
  for (let i = 0; i < Math.max(pa.length, pb.length); i += 1) {
    const da = pa[i] ?? 0;
    const db = pb[i] ?? 0;
    if (da !== db) return da > db ? 1 : -1;
  }
  return 0;
}

function parseVersion(version: string): number[] {
  return normalizeVersion(version)
    .split(/[.-]/)
    .map((part) => Number.parseInt(part, 10))
    .filter((part) => Number.isFinite(part));
}

function ValidationBadge({ v, provider }: { v: DirValidation | null; provider: "codex" | "claude" | "gemini" }) {
  if (!v) return null;
  if (v.valid) {
    return (
      <Badge variant="outline" className="gap-1 border-emerald-500/40 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400">
        <CheckCircle2 className="h-3 w-3" />
        有效 · {v.threads_count} 个会话
      </Badge>
    );
  }
  const reasons: string[] = [];
  if (provider === "codex" && !v.has_state_db) reasons.push("缺 state_5.sqlite");
  if (!v.has_sessions) {
    reasons.push(
      provider === "codex"
        ? "缺 sessions/"
        : provider === "claude"
          ? "缺 projects/"
          : "缺 tmp/ 或 antigravity*/conversations/",
    );
  }
  return (
    <Badge variant="outline" className="gap-1 border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400">
      <AlertTriangle className="h-3 w-3" />
      {reasons.join(" · ") || "无效目录"}
    </Badge>
  );
}
