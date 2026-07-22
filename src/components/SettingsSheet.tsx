import { useEffect, useState, type ReactNode } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  Download,
  ExternalLink,
  FolderOpen,
  Home,
  Loader2,
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
import { pickDirectoryPath } from "@/lib/dialog";
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
  const [backup, setBackup] = useState("");
  const [codexValidation, setCodexValidation] = useState<DirValidation | null>(null);
  const [claudeValidation, setClaudeValidation] = useState<DirValidation | null>(null);
  const [updateState, setUpdateState] = useState<UpdateCheckResult>({ state: "idle" });
  const [currentVersion, setCurrentVersion] = useState("");
  const [currentVersionError, setCurrentVersionError] = useState("");

  useEffect(() => {
    if (!settings) return;
    setCodex(settings.codex_dir);
    setClaude(settings.claude_dir);
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

  const pick = async (setter: (s: string) => void, cur: string) => {
    const picked = await pickDirectoryPath({ defaultPath: cur });
    if (picked) setter(picked);
  };

  const useDefault = async () => {
    const d = await api.defaultCodexDir();
    setCodex(d);
  };

  const useDefaultClaude = async () => {
    const d = await api.defaultClaudeDir();
    setClaude(d);
  };

  const onSave = async () => {
    try {
      await save({ codex_dir: codex, claude_dir: claude, backup_dir: backup });
      toast.success("设置已保存");
      await load();
    } catch (e: any) {
      toast.error("保存失败: " + String(e?.message ?? e));
    }
  };

  const checkUpdate = async () => {
    if (updateState.state === "installing") return;
    setUpdateState({ state: "checking" });
    try {
      const update = await api.checkAppUpdate();
      setCurrentVersion(update.current_version);
      setCurrentVersionError("");
      setUpdateState({
        ...update,
        state: update.available ? "available" : "current",
      });
    } catch (e: any) {
      setUpdateState({ state: "error", message: String(e?.message ?? e) });
    }
  };

  const installUpdate = async () => {
    if (updateState.state !== "available" || !updateState.can_auto_install) return;
    const update = updateState;
    setUpdateState({ ...update, state: "installing" });
    try {
      await api.installAppUpdate();
      toast.success("更新包已下载，应用即将关闭并完成安装");
    } catch (e: any) {
      setUpdateState({ ...update, state: "available" });
      toast.error("更新失败: " + String(e?.message ?? e));
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
            本地运行，只有手动检查更新时会请求 GitHub；路径配置以当前运行环境为准。
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
                disabled={updateState.state === "checking" || updateState.state === "installing"}
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
              onInstall={installUpdate}
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
  onInstall,
}: {
  state: UpdateCheckResult;
  currentVersion: string;
  currentVersionError: string;
  onOpenRelease: () => void;
  onInstall: () => void;
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
  if (state.state === "installing") {
    return (
      <div className="rounded-md border border-sky-500/30 bg-sky-500/5 px-3 py-2.5 text-xs">
        <div className="flex items-center gap-2 font-medium text-sky-700 dark:text-sky-300">
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
          正在下载并准备安装 {state.latest_version}
        </div>
        <p className="mt-1.5 leading-5 text-muted-foreground">
          下载和校验完成后，应用会自动关闭、更新原位置并重新启动。
        </p>
      </div>
    );
  }
  return (
    <div className="space-y-2.5">
      <div className="flex flex-wrap items-center gap-2">
        <Badge variant="outline" className="gap-1 border-sky-500/40 bg-sky-500/10 text-sky-600 dark:text-sky-400">
          有新版本 {state.latest_version}，当前 {state.current_version}
        </Badge>
        <span className="text-[11px] text-muted-foreground">
          {installModeLabel(state.install_mode)}
        </span>
      </div>

      {state.can_auto_install && state.install_dir && (
        <div className="rounded-md border bg-muted/25 px-3 py-2 text-xs leading-5 text-muted-foreground">
          <div>
            更新位置：
            <span className="break-all font-mono text-[11px] text-foreground/80">
              {state.install_dir}
            </span>
          </div>
          <div>
            下载完成后会关闭当前应用，{state.install_mode === "portable" ? "原位替换便携版" : "沿用当前安装目录完成安装"}，随后自动重启。
          </div>
        </div>
      )}

      {state.message && (
        <p className="text-xs leading-5 text-amber-600 dark:text-amber-400">{state.message}</p>
      )}

      <div className="flex flex-wrap items-center gap-2">
        {state.can_auto_install && (
          <Button size="sm" className="h-8 gap-1.5" onClick={onInstall}>
            <Download className="h-3.5 w-3.5" />
            下载并安装
          </Button>
        )}
        <Button variant="secondary" size="sm" className="h-8 gap-1.5" onClick={onOpenRelease}>
          <ExternalLink className="h-3.5 w-3.5" />
          {state.can_auto_install ? "查看发布页" : "打开下载页面"}
        </Button>
      </div>
    </div>
  );
}

function installModeLabel(mode: string): string {
  switch (mode) {
    case "portable":
      return "便携版原位更新";
    case "nsis":
      return "NSIS 安装版";
    case "msi":
      return "MSI 安装版";
    case "webui":
      return "CLI WebUI";
    default:
      return "手动更新";
  }
}

function ValidationBadge({ v, provider }: { v: DirValidation | null; provider: "codex" | "claude" }) {
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
  if (!v.has_sessions) reasons.push(provider === "codex" ? "缺 sessions/" : "缺 projects/");
  return (
    <Badge variant="outline" className="gap-1 border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400">
      <AlertTriangle className="h-3 w-3" />
      {reasons.join(" · ") || "无效目录"}
    </Badge>
  );
}
