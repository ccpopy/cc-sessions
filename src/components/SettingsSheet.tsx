import { useEffect, useState, type ReactNode } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  Download,
  ExternalLink,
  FolderOpen,
  Loader2,
  RefreshCw,
  RotateCcw,
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
import { DangerDialog } from "@/components/DangerDialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { api, type DirValidation, type UpdateCheckResult } from "@/lib/api";
import { pickDirectoryPath } from "@/lib/dialog";
import { useSettings } from "@/stores/settings";
import { useMemoryDraft } from "@/stores/memoryDraft";
import { toast } from "sonner";

type Props = {
  trigger?: ReactNode;
};

export function SettingsSheet({ trigger }: Props) {
  const settings = useSettings((s) => s.settings);
  const save = useSettings((s) => s.save);
  const load = useSettings((s) => s.load);
  const memoryDirty = useMemoryDraft((state) => state.dirty);
  const [codex, setCodex] = useState("");
  const [claude, setClaude] = useState("");
  const [opencode, setOpenCode] = useState("");
  const [cursor, setCursor] = useState("");
  const [backup, setBackup] = useState("");
  const [codexValidation, setCodexValidation] = useState<DirValidation | null>(null);
  const [claudeValidation, setClaudeValidation] = useState<DirValidation | null>(null);
  const [opencodeValidation, setOpenCodeValidation] = useState<DirValidation | null>(null);
  const [cursorValidation, setCursorValidation] = useState<DirValidation | null>(null);
  const [updateState, setUpdateState] = useState<UpdateCheckResult>({ state: "idle" });
  const [currentVersion, setCurrentVersion] = useState("");
  const [currentVersionError, setCurrentVersionError] = useState("");
  const [confirmClaudeDirChange, setConfirmClaudeDirChange] = useState(false);

  useEffect(() => {
    if (!settings) return;
    setCodex(settings.codex_dir);
    setClaude(settings.claude_dir);
    setOpenCode(settings.opencode_dir);
    setCursor(settings.cursor_dir);
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
    if (!opencode) return;
    const id = window.setTimeout(async () => {
      try {
        setOpenCodeValidation(await api.validateOpenCodeDir(opencode));
      } catch {
        setOpenCodeValidation(null);
      }
    }, 200);
    return () => window.clearTimeout(id);
  }, [opencode]);

  useEffect(() => {
    if (!cursor) return;
    const id = window.setTimeout(async () => {
      try {
        setCursorValidation(await api.validateCursorDir(cursor));
      } catch {
        setCursorValidation(null);
      }
    }, 200);
    return () => window.clearTimeout(id);
  }, [cursor]);

  const pick = async (setter: (s: string) => void, cur: string) => {
    const picked = await pickDirectoryPath({ defaultPath: cur });
    if (picked) setter(picked);
  };

  const restoreCodexDefault = async () => {
    setCodex(await api.defaultCodexDir());
  };

  const restoreClaudeDefault = async () => {
    setClaude(await api.defaultClaudeDir());
  };

  const restoreOpenCodeDefault = async () => {
    setOpenCode(await api.defaultOpenCodeDir());
  };

  const restoreCursorDefault = async () => {
    setCursor(await api.defaultCursorDir());
  };

  const persistSettings = async () => {
    await save({
      codex_dir: codex,
      claude_dir: claude,
      opencode_dir: opencode,
      cursor_dir: cursor,
      backup_dir: backup,
    });
    toast.success("设置已保存");
    await load();
  };

  const onSave = () => {
    if (memoryDirty && settings && claude !== settings.claude_dir) {
      setConfirmClaudeDirChange(true);
      return;
    }
    void persistSettings().catch((e: any) => {
      toast.error("保存失败: " + String(e?.message ?? e));
    });
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
      toast.success("更新安装已开始");
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
    <>
    <Sheet>
      {trigger ? <SheetTrigger asChild>{trigger}</SheetTrigger> : defaultTrigger}
      <SheetContent
        side="right"
        className="flex h-full w-[440px] flex-col gap-0 overflow-hidden p-0 sm:max-w-[440px]"
      >
        <SheetHeader className="shrink-0 space-y-1 border-b px-6 pb-4 pt-6 pr-12">
          <SheetTitle>设置</SheetTitle>
          <SheetDescription>
            本地运行，只有手动检查更新时会请求 GitHub；路径配置以当前运行环境为准。
          </SheetDescription>
        </SheetHeader>

        <div className="thin-scrollbar min-h-0 flex-1 overflow-y-auto overflow-x-hidden overscroll-contain px-6">
          <div className="space-y-6 py-6">
          <DirField
            label="Codex 目录"
            value={codex}
            onChange={setCodex}
            placeholder={"C:\\Users\\<me>\\.codex"}
            onPick={() => pick(setCodex, codex)}
            onRestoreDefault={restoreCodexDefault}
          >
            <ValidationBadge v={codexValidation} provider="codex" />
          </DirField>

          <Separator />

          <DirField
            label="OpenCode 数据目录"
            value={opencode}
            onChange={setOpenCode}
            placeholder={"C:\\Users\\<me>\\.local\\share\\opencode"}
            onPick={() => pick(setOpenCode, opencode)}
            onRestoreDefault={restoreOpenCodeDefault}
          >
            <ValidationBadge v={opencodeValidation} provider="opencode" />
          </DirField>

          <Separator />

          <DirField
            label="Cursor 用户数据目录"
            value={cursor}
            onChange={setCursor}
            placeholder={"C:\\Users\\<me>\\AppData\\Roaming\\Cursor\\User"}
            onPick={() => pick(setCursor, cursor)}
            onRestoreDefault={restoreCursorDefault}
          >
            <ValidationBadge v={cursorValidation} provider="cursor" />
          </DirField>
          <Separator />

          <DirField
            label="Claude 目录"
            value={claude}
            onChange={setClaude}
            placeholder={"C:\\Users\\<me>\\.claude"}
            onPick={() => pick(setClaude, claude)}
            onRestoreDefault={restoreClaudeDefault}
          >
            <ValidationBadge v={claudeValidation} provider="claude" />
          </DirField>

          <Separator />

          <DirField
            label="备份目录"
            value={backup}
            onChange={setBackup}
            onPick={() => pick(setBackup, backup)}
          >
            <p className="text-xs text-muted-foreground">
              推荐放在 Codex 或 Claude 目录外，避免把备份目录再次纳入备份。
            </p>
          </DirField>

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
        </div>

        <SheetFooter className="shrink-0 border-t bg-background px-6 py-4">
          <Button onClick={onSave} className="w-full">
            保存设置
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
    <DangerDialog
      open={confirmClaudeDirChange}
      onOpenChange={setConfirmClaudeDirChange}
      title="更改 Claude 目录"
      confirmText="放弃修改并保存设置"
      onConfirm={persistSettings}
    >
      <div className="min-w-0 whitespace-normal">
        Claude Memory 当前有尚未保存的内容。更改 Claude 目录会重新载入项目并丢弃这些修改；
        取消后先保存 Memory 即可保留。
      </div>
    </DangerDialog>
    </>
  );
}

function DirField({
  label,
  value,
  onChange,
  placeholder,
  onPick,
  onRestoreDefault,
  children,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  onPick: () => void;
  onRestoreDefault?: () => void;
  children?: ReactNode;
}) {
  return (
    <div className="space-y-2">
      <Label className="text-sm font-medium">{label}</Label>
      <div className="flex gap-2">
        <Input
          value={value}
          onChange={(e) => onChange(e.target.value)}
          placeholder={placeholder}
          className="font-mono text-xs"
          aria-label={label}
        />
        <Tooltip>
          <TooltipTrigger asChild>
            <Button variant="outline" size="icon" onClick={onPick} aria-label={`选择 ${label}`}>
              <FolderOpen className="h-4 w-4" />
            </Button>
          </TooltipTrigger>
          <TooltipContent>选择目录</TooltipContent>
        </Tooltip>
        {onRestoreDefault && (
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="outline"
                size="icon"
                onClick={onRestoreDefault}
                aria-label={`恢复默认 ${label}`}
              >
                <RotateCcw className="h-4 w-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>恢复默认</TooltipContent>
          </Tooltip>
        )}
      </div>
      {children}
    </div>
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
      </div>
    );
  }
  return (
    <div className="space-y-2.5">
      <div className="flex flex-wrap items-center gap-2">
        <Badge variant="outline" className="gap-1 border-sky-500/40 bg-sky-500/10 text-sky-600 dark:text-sky-400">
          有新版本 {state.latest_version}，当前 {state.current_version}
        </Badge>
      </div>

      {state.can_auto_install && state.install_dir && (
        <div className="rounded-md border bg-muted/25 px-3 py-2 text-xs leading-5 text-muted-foreground">
          <div>
            更新位置：
            <span className="break-all font-mono text-[11px] text-foreground/80">
              {state.install_dir}
            </span>
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

function ValidationBadge({
  v,
  provider,
}: {
  v: DirValidation | null;
  provider: "codex" | "claude" | "opencode" | "cursor";
}) {
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
  if (provider === "opencode" && !v.has_state_db) reasons.push("缺 opencode.db");
  if (provider === "cursor" && !v.has_state_db) reasons.push("缺 state.vscdb");
  if (!v.has_sessions) {
    reasons.push(provider === "codex" ? "缺 sessions/" : provider === "claude" ? "缺 projects/" : "数据库不可用");
  }
  return (
    <Badge variant="outline" className="gap-1 border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400">
      <AlertTriangle className="h-3 w-3" />
      {reasons.join(" · ") || "无效目录"}
    </Badge>
  );
}
