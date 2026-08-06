import { Fragment, useMemo, useState } from "react";
import { BookMarked, Check, ChevronsUpDown, FolderTree, Search } from "lucide-react";

import {
  Breadcrumb,
  BreadcrumbEllipsis,
  BreadcrumbItem,
  BreadcrumbList,
  BreadcrumbPage,
  BreadcrumbSeparator,
  BreadcrumbText,
} from "@/components/ui/breadcrumb";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { ScrollArea } from "@/components/ui/scroll-area";
import type { ClaudeMemoryProject } from "@/lib/api";
import { cn } from "@/lib/utils";

type Props = {
  projects: ClaudeMemoryProject[];
  value: string;
  onValueChange: (projectKey: string) => void;
  disabled?: boolean;
};

export function ProjectBreadcrumbPicker({ projects, value, onValueChange, disabled }: Props) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const selected = projects.find((project) => project.project_key === value) ?? null;
  const filtered = useMemo(() => {
    const normalized = query.trim().toLocaleLowerCase();
    if (!normalized) return projects;
    return projects.filter((project) => {
      const segments = projectPathSegments(project).join(" ");
      return [project.project_path, project.project_key, segments].some((candidate) =>
        candidate.toLocaleLowerCase().includes(normalized),
      );
    });
  }, [projects, query]);
  const listHeight = Math.min(Math.max(filtered.length * 68 + 12, 76), 384);

  return (
    <Popover
      open={open}
      onOpenChange={(nextOpen) => {
        setOpen(nextOpen);
        if (!nextOpen) setQuery("");
      }}
    >
      <div className="relative">
        <PopoverTrigger asChild>
          <button
            type="button"
            disabled={disabled}
            aria-label="选择 Claude Memory 项目"
            title={selected?.project_path}
            className="peer absolute inset-0 z-10 rounded-md focus-visible:outline-none disabled:cursor-not-allowed"
          />
        </PopoverTrigger>
        <div
          className={cn(
            "flex min-h-10 w-full min-w-0 items-center gap-2 rounded-md border border-input bg-background px-2.5 py-2 text-left shadow-sm transition-colors",
            "peer-hover:bg-accent/45 peer-focus-visible:ring-2 peer-focus-visible:ring-ring peer-focus-visible:ring-offset-2",
            "peer-disabled:opacity-50",
          )}
        >
          <FolderTree className="h-4 w-4 shrink-0 text-provider-claude-fg" />
          {selected ? (
            <ProjectPathBreadcrumb
              segments={compactPathSegments(projectPathSegments(selected))}
              className="min-w-0 flex-1"
              compact
            />
          ) : (
            <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
              {projects.length === 0 ? "没有可用的 Claude 项目" : "选择 Claude 项目"}
            </span>
          )}
          {selected && (
            <span className="shrink-0 text-[10px] tabular-nums text-muted-foreground">
              {selected.file_count}
            </span>
          )}
          <ChevronsUpDown className="h-3.5 w-3.5 shrink-0 text-muted-foreground transition-colors peer-hover:text-foreground" />
        </div>
      </div>

      <PopoverContent
        align="start"
        sideOffset={6}
        className="w-[min(36rem,calc(100vw-2rem))] overflow-hidden p-0"
      >
        <div className="border-b border-border/60 p-2.5">
          <div className="relative">
            <Search className="absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input
              autoFocus
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="搜索项目路径"
              className="h-8 pl-8 text-xs"
            />
          </div>
        </div>

        <ScrollArea
          type="always"
          className="max-h-[60vh]"
          style={{ height: listHeight }}
          viewportClassName="overscroll-contain"
        >
          <div role="listbox" aria-label="Claude Memory 项目" className="space-y-0.5 p-1.5 pr-3">
            {filtered.length === 0 ? (
              <div className="px-3 py-8 text-center text-xs text-muted-foreground">
                没有匹配的项目
              </div>
            ) : (
              filtered.map((project) => {
                const active = project.project_key === value;
                return (
                  <div key={project.project_key} className="relative min-w-0">
                    <button
                      type="button"
                      role="option"
                      aria-selected={active}
                      aria-label={`${project.project_path}，${project.file_count} 个文件`}
                      onClick={() => {
                        onValueChange(project.project_key);
                        setOpen(false);
                      }}
                      className="peer absolute inset-0 z-10 rounded-md focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                    />
                    <div
                      className={cn(
                        "relative flex min-w-0 items-start gap-2.5 rounded-md px-2.5 py-2.5 transition-colors",
                        active
                          ? "bg-provider-claude/[0.08] before:absolute before:bottom-2 before:left-0 before:top-2 before:w-0.5 before:rounded-r-full before:bg-provider-claude"
                          : "peer-hover:bg-accent/55",
                      )}
                    >
                      <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center text-provider-claude-fg">
                        {active && <Check className="h-3.5 w-3.5" />}
                      </span>
                      <span className="min-w-0 flex-1">
                        <ProjectPathBreadcrumb segments={projectPathSegments(project)} />
                        <span className="mt-1.5 flex flex-wrap items-center gap-1.5 text-[10px] text-muted-foreground">
                        <span>{project.file_count} 个文件</span>
                          <span aria-hidden="true">·</span>
                          {project.has_index ? (
                            <span className="flex items-center gap-1 text-provider-claude-fg/85">
                              <BookMarked className="h-3 w-3" />
                              MEMORY.md
                            </span>
                          ) : (
                            <span>尚无索引</span>
                          )}
                        </span>
                      </span>
                    </div>
                  </div>
                );
              })
            )}
          </div>
        </ScrollArea>
      </PopoverContent>
    </Popover>
  );
}

function ProjectPathBreadcrumb({
  segments,
  className,
  compact = false,
}: {
  segments: string[];
  className?: string;
  compact?: boolean;
}) {
  return (
    <Breadcrumb className={cn("min-w-0", className)}>
      <BreadcrumbList
        className={cn(
          "gap-1 text-[11px]",
          compact ? "flex-nowrap overflow-hidden" : "gap-y-1",
        )}
      >
        {segments.map((segment, index) => {
          const last = index === segments.length - 1;
          return (
            <Fragment key={`${segment}-${index}`}>
              {index > 0 && <BreadcrumbSeparator />}
              <BreadcrumbItem className={cn(compact && last && "min-w-0 flex-1")}>
                {segment === "…" ? (
                  <BreadcrumbEllipsis className="h-4 w-4 text-muted-foreground/75" />
                ) : last ? (
                  <BreadcrumbPage
                    title={segment}
                    className={cn(
                      "text-[11px] font-medium text-foreground",
                      compact && "truncate",
                      !compact && "break-all",
                    )}
                  >
                    {segment}
                  </BreadcrumbPage>
                ) : (
                  <BreadcrumbText
                    title={segment}
                    className={cn(
                      "text-[11px] text-muted-foreground",
                      !compact && "break-all",
                    )}
                  >
                    {segment}
                  </BreadcrumbText>
                )}
              </BreadcrumbItem>
            </Fragment>
          );
        })}
      </BreadcrumbList>
    </Breadcrumb>
  );
}

function projectPathSegments(project: ClaudeMemoryProject): string[] {
  const path = project.project_path.trim() || project.project_key.trim();
  if (!path) return ["未知项目"];

  const windowsPath = path.match(/^([a-zA-Z]):[\\/]*(.*)$/);
  if (windowsPath) {
    return [`${windowsPath[1].toUpperCase()}:`, ...splitPath(windowsPath[2])];
  }

  if (path.startsWith("/")) return ["/", ...splitPath(path.slice(1))];

  if (!path.includes("/") && !path.includes("\\")) {
    const encodedWindowsPath = path.match(/^([a-zA-Z])--(.+)$/);
    if (encodedWindowsPath) {
      return [
        `${encodedWindowsPath[1].toUpperCase()}:`,
        ...encodedProjectPathSegments(encodedWindowsPath[2]),
      ];
    }
  }

  return splitPath(path);
}

function encodedProjectPathSegments(path: string): string[] {
  const segments: string[] = [];
  for (const token of path.split(/(-+)/).filter(Boolean)) {
    if (!token.startsWith("-")) {
      segments.push(token);
      continue;
    }
    // Claude 把路径分隔符和非 ASCII 字符都编码为 `-`。单个 `-` 视为分隔符，
    // 更长的连续段保留未知字符数量，避免多个中文项目都塌缩成同一条面包屑。
    const unknownCharacters = token.length - 1;
    if (unknownCharacters > 0) segments.push(`未知字符×${unknownCharacters}`);
  }
  return segments.length > 0 ? segments : [path];
}

function splitPath(path: string): string[] {
  const segments = path.split(/[\\/]+/).filter(Boolean);
  return segments.length > 0 ? segments : [path];
}

function compactPathSegments(segments: string[]): string[] {
  if (segments.length <= 3) return segments;
  return [segments[0], "…", segments.at(-1) ?? "未知项目"];
}
