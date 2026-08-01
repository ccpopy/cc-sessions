import { useLocation } from "react-router-dom";
import { Archive, ArrowDown10, Clock, FileSearch, FolderKanban, RefreshCw, Trash2, X } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { SearchInput } from "@/components/SearchInput";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { Separator } from "@/components/ui/separator";
import { useSelection } from "@/stores/selection";
import { useView, type View } from "@/stores/view";

type Props = {
  title?: string;
  /** 次级展示，如"12 条"，会放在右侧刷新按钮前（不再置于标题下方）。 */
  stats?: string;
  onRefresh?: () => void;
  refreshing?: boolean;
  onBulkBackup?: () => void;
  onBulkDelete?: () => void;
  onContentSearch?: () => void;
  showListTools?: boolean;
  children?: React.ReactNode;
};

export function TopBar({
  title,
  stats,
  onRefresh,
  refreshing = false,
  onBulkBackup,
  onBulkDelete,
  onContentSearch,
  showListTools,
  children,
}: Props) {
  const loc = useLocation();
  const view = useView((s) => s.view);
  const setView = useView((s) => s.setView);
  const query = useView((s) => s.query);
  const setQuery = useView((s) => s.setQuery);
  const selected = useSelection((s) => s.selected);
  const clearSel = useSelection((s) => s.clear);

  const autoShowTools =
    showListTools ??
    (loc.pathname.startsWith("/sessions") ||
      loc.pathname.startsWith("/codex/sessions") ||
      loc.pathname.startsWith("/claude/sessions") ||
      loc.pathname.startsWith("/backups/") ||
      loc.pathname.startsWith("/codex/backups/") ||
      loc.pathname.startsWith("/claude/backups/"));

  const hasSelection = selected.size > 0;

  return (
    <header className="relative shrink-0 border-b border-border/60 bg-background/95 after:pointer-events-none after:absolute after:inset-x-0 after:-bottom-px after:h-px after:bg-gradient-to-r after:from-transparent after:via-border/40 after:to-transparent">
      <div className="flex h-14 min-w-0 items-center gap-2.5 px-4 sm:gap-3 sm:px-6">
        {title && (
          <div className="flex shrink-0 items-center gap-2.5">
            <span
              aria-hidden="true"
              className="h-4 w-[3px] rounded-full bg-gradient-to-b from-foreground/70 via-foreground/40 to-foreground/10"
            />
            <h1 className="whitespace-nowrap text-[15px] font-semibold tracking-tight text-foreground">
              {title}
            </h1>
          </div>
        )}

        {autoShowTools && (
          <>
            {title && <Separator orientation="vertical" className="h-5 bg-border/60" />}
            <SearchInput
              value={query}
              onChange={setQuery}
              className="min-w-0 flex-1 basis-64 sm:max-w-96 lg:min-w-48 xl:max-w-[30rem]"
            />
            {onContentSearch && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="outline"
                    size="icon"
                    onClick={onContentSearch}
                    className="h-9 w-9 shrink-0 text-muted-foreground hover:text-foreground active:scale-95"
                    aria-label="搜索对话全文"
                  >
                    <FileSearch className="h-4 w-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>搜索对话全文</TooltipContent>
              </Tooltip>
            )}
            <Tabs value={view} onValueChange={(v) => setView(v as View)} className="shrink-0">
              <TabsList className="h-9">
                <TabsTrigger value="time" className="gap-1.5 px-2.5 text-xs">
                  <Clock className="h-3.5 w-3.5" />
                  时间
                </TabsTrigger>
                <TabsTrigger value="project" className="gap-1.5 px-2.5 text-xs">
                  <FolderKanban className="h-3.5 w-3.5" />
                  项目
                </TabsTrigger>
                <TabsTrigger value="size" className="gap-1.5 px-2.5 text-xs">
                  <ArrowDown10 className="h-3.5 w-3.5" />
                  大小降序
                </TabsTrigger>
              </TabsList>
            </Tabs>
          </>
        )}

        <div className="ml-auto flex shrink-0 items-center gap-2">
          {hasSelection && (
            <div className="flex h-8 items-center gap-0.5 rounded-md border border-emerald-500/30 bg-emerald-500/8 pl-2.5 pr-1 shadow-[inset_0_1px_0_0_hsl(var(--background))] dark:bg-emerald-500/10">
              <span className="flex items-center gap-1.5 text-xs font-medium text-emerald-700 dark:text-emerald-300">
                <span aria-hidden="true" className="relative flex h-1.5 w-1.5">
                  <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-emerald-400/60" />
                  <span className="relative inline-flex h-1.5 w-1.5 rounded-full bg-emerald-500" />
                </span>
                已选 <span className="tabular-nums">{selected.size}</span>
              </span>
              <Separator orientation="vertical" className="mx-1 h-4 bg-emerald-500/30" />
              {onBulkBackup && (
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={onBulkBackup}
                  className="h-7 gap-1.5 px-2 text-emerald-700 hover:bg-emerald-500/15 hover:text-emerald-800 dark:text-emerald-300 dark:hover:text-emerald-200"
                >
                  <Archive className="h-3.5 w-3.5" />
                  备份
                </Button>
              )}
              {onBulkDelete && (
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={onBulkDelete}
                  className="h-7 gap-1.5 px-2 text-destructive hover:bg-destructive/10 hover:text-destructive"
                >
                  <Trash2 className="h-3.5 w-3.5" />
                  删除
                </Button>
              )}
              <Button
                variant="ghost"
                size="icon"
                onClick={clearSel}
                className="h-7 w-7 text-muted-foreground hover:bg-emerald-500/15 hover:text-foreground"
                aria-label="清除选择"
              >
                <X className="h-3.5 w-3.5" />
              </Button>
            </div>
          )}

          {children}

          {stats && (
            <Badge
              variant="outline"
              className="h-7 gap-1.5 border-border/70 bg-muted/40 px-2 text-[11px] font-medium tabular-nums text-muted-foreground"
            >
              <span aria-hidden="true" className="h-1 w-1 rounded-full bg-muted-foreground/50" />
              {stats}
            </Badge>
          )}

          {onRefresh && (
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="icon"
                  onClick={onRefresh}
                  disabled={refreshing}
                  className="h-8 w-8 shrink-0 text-muted-foreground hover:text-foreground"
                  aria-label="刷新"
                >
                  <RefreshCw className={refreshing ? "h-4 w-4 animate-spin" : "h-4 w-4"} />
                </Button>
              </TooltipTrigger>
              <TooltipContent>{refreshing ? "正在刷新…" : "刷新"}</TooltipContent>
            </Tooltip>
          )}
        </div>
      </div>
    </header>
  );
}
