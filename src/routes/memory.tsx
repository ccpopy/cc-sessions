import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  BookMarked,
  FilePlus2,
  FileText,
  FolderOpen,
  Loader2,
  NotebookText,
  Save,
  Search,
  Trash2,
} from "lucide-react";
import { toast } from "sonner";
import { useBlocker } from "react-router-dom";

import { DangerDialog } from "@/components/DangerDialog";
import { EmptyState } from "@/components/EmptyState";
import {
  MemoryMarkdownEditor,
  type MemoryEditorMode,
} from "@/components/memory/MemoryMarkdownEditor";
import { ProjectBreadcrumbPicker } from "@/components/memory/ProjectBreadcrumbPicker";
import { TopBar } from "@/components/TopBar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import {
  api,
  type ClaudeMemoryDocument,
  type ClaudeMemoryFile,
  type ClaudeMemoryProject,
} from "@/lib/api";
import { absoluteTime, humanBytes } from "@/lib/format";
import { accentRow } from "@/lib/providerTheme";
import { cn } from "@/lib/utils";
import { isTauriRuntime } from "@/lib/runtime";
import { useMemoryDraft } from "@/stores/memoryDraft";
import { useSettings } from "@/stores/settings";

export default function MemoryRoute() {
  const settings = useSettings((state) => state.settings);
  const setGlobalDirty = useMemoryDraft((state) => state.setDirty);
  const claudeDir = settings?.claude_dir ?? "";
  const [projects, setProjects] = useState<ClaudeMemoryProject[]>([]);
  const [projectsScope, setProjectsScope] = useState("");
  const [projectKey, setProjectKey] = useState("");
  const [files, setFiles] = useState<ClaudeMemoryFile[]>([]);
  const [document, setDocument] = useState<ClaudeMemoryDocument | null>(null);
  const [draftName, setDraftName] = useState("");
  const [draftContent, setDraftContent] = useState("");
  const [editorMode, setEditorMode] = useState<MemoryEditorMode>("preview");
  const [query, setQuery] = useState("");
  const [loadingProjects, setLoadingProjects] = useState(false);
  const [loadingFiles, setLoadingFiles] = useState(false);
  const [saving, setSaving] = useState(false);
  const [deleteOpen, setDeleteOpen] = useState(false);
  const [closeRequested, setCloseRequested] = useState(false);
  // 有未保存改动时，把"打开 / 新建 / 切项目"挂起，交给统一的确认弹窗放行。
  const [pendingAction, setPendingAction] = useState<{ label: string; run: () => void } | null>(
    null,
  );
  const projectRequestSeq = useRef(0);
  const fileRequestSeq = useRef(0);
  const claudeDirRef = useRef(claudeDir);
  const filesScopeRef = useRef("");
  const documentRef = useRef(document);
  const dirtyRef = useRef(false);
  const allowWindowCloseRef = useRef(false);
  claudeDirRef.current = claudeDir;
  documentRef.current = document;

  const selectedProject = useMemo(
    () => projects.find((project) => project.project_key === projectKey) ?? null,
    [projectKey, projects],
  );
  const dirty = document
    ? draftName !== document.file.file_name || draftContent !== document.content
    : Boolean(draftName || draftContent);
  dirtyRef.current = dirty;
  const blocker = useBlocker(dirty);
  const filteredFiles = useMemo(() => {
    const normalized = query.trim().toLowerCase();
    if (!normalized) return files;
    return files.filter((file) =>
      [file.file_name, file.title, file.preview].some((value) =>
        value.toLowerCase().includes(normalized),
      ),
    );
  }, [files, query]);

  const guardUnsaved = (label: string, run: () => void) => {
    if (!dirty) {
      run();
      return;
    }
    setPendingAction({ label, run });
  };

  useEffect(() => {
    setGlobalDirty(dirty);
  }, [dirty, setGlobalDirty]);

  useEffect(
    () => () => {
      useMemoryDraft.getState().setDirty(false);
    },
    [],
  );

  useEffect(() => {
    if (!dirty) setCloseRequested(false);
  }, [dirty]);

  useEffect(() => {
    const onBeforeUnload = (event: BeforeUnloadEvent) => {
      if (!dirtyRef.current || allowWindowCloseRef.current) return;
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", onBeforeUnload);
    return () => window.removeEventListener("beforeunload", onBeforeUnload);
  }, []);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    const appWindow = getCurrentWindow();
    void appWindow
      .onCloseRequested((event) => {
        if (!dirtyRef.current || allowWindowCloseRef.current) return;
        event.preventDefault();
        setCloseRequested(true);
      })
      .then((stopListening) => {
        if (disposed) stopListening();
        else unlisten = stopListening;
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const loadProjects = useCallback(async () => {
    const request = ++projectRequestSeq.current;
    const scope = claudeDir;
    if (!scope) {
      setProjects([]);
      setProjectsScope("");
      setProjectKey("");
      return;
    }
    setLoadingProjects(true);
    try {
      const next = await api.listClaudeMemoryProjects(scope);
      if (request !== projectRequestSeq.current || claudeDirRef.current !== scope) return;
      setProjects(next);
      setProjectsScope(scope);
      setProjectKey((current) =>
        next.some((project) => project.project_key === current)
          ? current
          : next.find((project) => project.file_count > 0)?.project_key ?? next[0]?.project_key ?? "",
      );
    } catch (error) {
      if (request !== projectRequestSeq.current || claudeDirRef.current !== scope) return;
      toast.error("读取 Claude Memory 项目失败", {
        description: String((error as Error)?.message ?? error),
      });
    } finally {
      if (request === projectRequestSeq.current && claudeDirRef.current === scope) {
        setLoadingProjects(false);
      }
    }
  }, [claudeDir]);

  const loadFiles = useCallback(async () => {
    const scope = JSON.stringify([claudeDir, projectsScope, projectKey]);
    filesScopeRef.current = scope;
    const request = ++fileRequestSeq.current;
    if (!claudeDir || projectsScope !== claudeDir || !projectKey) {
      setFiles([]);
      setLoadingFiles(false);
      return;
    }
    setLoadingFiles(true);
    try {
      const next = await api.listClaudeMemoryFiles(claudeDir, projectKey);
      if (request !== fileRequestSeq.current || filesScopeRef.current !== scope) return;
      setFiles(next);
      const currentDocument = documentRef.current;
      if (
        currentDocument &&
        !next.some((file) => file.file_name === currentDocument.file.file_name)
      ) {
        setDocument(null);
        setDraftName("");
        setDraftContent("");
      }
    } catch (error) {
      if (request !== fileRequestSeq.current || filesScopeRef.current !== scope) return;
      toast.error("读取 Memory 文件失败", {
        description: String((error as Error)?.message ?? error),
      });
    } finally {
      if (request === fileRequestSeq.current && filesScopeRef.current === scope) {
        setLoadingFiles(false);
      }
    }
  }, [claudeDir, projectKey, projectsScope]);

  useEffect(() => {
    projectRequestSeq.current += 1;
    fileRequestSeq.current += 1;
    setProjects([]);
    setProjectsScope("");
    setProjectKey("");
    setFiles([]);
    setDocument(null);
    setDraftName("");
    setDraftContent("");
    setEditorMode("preview");
    setQuery("");
    void loadProjects();
  }, [claudeDir, loadProjects]);

  useEffect(() => {
    setDocument(null);
    setDraftName("");
    setDraftContent("");
    setEditorMode("preview");
    setQuery("");
    void loadFiles();
  }, [loadFiles, projectKey, projectsScope]);

  const openFile = (file: ClaudeMemoryFile) =>
    guardUnsaved("打开其他 Memory", () => {
      void loadDocument(file);
    });

  const loadDocument = async (file: ClaudeMemoryFile) => {
    try {
      const next = await api.readClaudeMemoryFile(claudeDir, file.project_key, file.file_name);
      setDocument(next);
      setDraftName(next.file.file_name);
      setDraftContent(next.content);
      setEditorMode("preview");
    } catch (error) {
      toast.error("打开 Memory 失败", {
        description: String((error as Error)?.message ?? error),
      });
    }
  };

  const createFile = () => {
    if (!selectedProject) return;
    guardUnsaved("新建 Memory", () => {
      const existing = new Set(files.map((file) => file.file_name.toLowerCase()));
      let index = 1;
      let name = "memory-note.md";
      while (existing.has(name.toLowerCase())) name = `memory-note-${++index}.md`;
      setDocument(null);
      setDraftName(name);
      setDraftContent("# New memory\n\n");
      setEditorMode("split");
    });
  };

  const changeProject = (nextProjectKey: string) => {
    if (nextProjectKey === projectKey) return;
    guardUnsaved("切换项目", () => setProjectKey(nextProjectKey));
  };

  const save = async () => {
    if (!selectedProject || !draftName.trim()) return;
    setSaving(true);
    try {
      let current = document;
      if (current && draftName !== current.file.file_name) {
        current = await api.renameClaudeMemoryFile({
          claude_dir: claudeDir,
          project_key: selectedProject.project_key,
          file_name: current.file.file_name,
          new_file_name: draftName,
          expected_sha256: current.file.sha256,
        });
        // 重命名已经落盘；即使后续内容保存失败，也要继续指向新文件，避免再次操作旧名称。
        setDocument(current);
      }
      if (!current || draftContent !== current.content) {
        current = await api.saveClaudeMemoryFile({
          claude_dir: claudeDir,
          project_key: selectedProject.project_key,
          file_name: draftName,
          content: draftContent,
          expected_sha256: current?.file.sha256,
        });
      }
      setDocument(current);
      setDraftName(current.file.file_name);
      setDraftContent(current.content);
      await Promise.all([loadFiles(), loadProjects()]);
      toast.success("Claude Memory 已保存");
    } catch (error) {
      toast.error("保存失败", { description: String((error as Error)?.message ?? error) });
    } finally {
      setSaving(false);
    }
  };

  const remove = async () => {
    if (!document) return;
    await api.deleteClaudeMemoryFile({
      claude_dir: claudeDir,
      project_key: document.file.project_key,
      file_name: document.file.file_name,
      expected_sha256: document.file.sha256,
    });
    setDocument(null);
    setDraftName("");
    setDraftContent("");
    setEditorMode("preview");
    await Promise.all([loadFiles(), loadProjects()]);
    toast.success("Memory 文件已删除");
  };

  const refresh = async () => {
    await loadProjects();
    await loadFiles();
    if (document && !dirty) {
      const next = await api.readClaudeMemoryFile(
        claudeDir,
        document.file.project_key,
        document.file.file_name,
      );
      setDocument(next);
      setDraftName(next.file.file_name);
      setDraftContent(next.content);
    }
  };

  return (
    <>
      <TopBar
        title="Claude Memory"
        stats={loadingProjects ? "扫描中…" : `${projects.length} 个项目 · ${projects.reduce((sum, project) => sum + project.file_count, 0)} 个文件`}
        onRefresh={refresh}
        refreshing={loadingProjects || loadingFiles}
        showListTools={false}
      />
      <div className="grid min-h-0 flex-1 grid-cols-[minmax(250px,320px)_minmax(0,1fr)] bg-muted/10">
        <aside className="flex min-h-0 flex-col border-r border-border/60 bg-background/65">
          <div className="space-y-3 border-b border-border/60 p-3">
            <ProjectBreadcrumbPicker
              projects={projects}
              value={projectKey}
              onValueChange={changeProject}
              disabled={loadingProjects || projects.length === 0}
            />
            {selectedProject && (
              <div className="flex items-center gap-2 text-[11px] text-muted-foreground">
                <Badge variant="outline" className="h-5 font-normal">
                  {selectedProject.has_index ? "含 MEMORY.md" : "尚无索引"}
                </Badge>
                <span>{humanBytes(selectedProject.total_bytes)}</span>
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      variant="ghost"
                      size="icon"
                      className="ml-auto h-6 w-6"
                      aria-label="打开 Memory 目录"
                      onClick={() => void api.revealCwd(selectedProject.memory_dir)}
                    >
                      <FolderOpen className="h-3.5 w-3.5" />
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>打开 Memory 目录</TooltipContent>
                </Tooltip>
              </div>
            )}
            <div className="flex gap-2">
              <div className="relative min-w-0 flex-1">
                <Search className="absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
                <Input
                  value={query}
                  onChange={(event) => setQuery(event.target.value)}
                  placeholder="搜索 memory"
                  className="h-8 pl-8 text-xs"
                />
              </div>
              <Button size="sm" className="h-8 gap-1.5" onClick={createFile} disabled={!selectedProject}>
                <FilePlus2 className="h-3.5 w-3.5" />
                新建
              </Button>
            </div>
          </div>
          <ScrollArea className="min-h-0 flex-1">
            <div className="space-y-1.5 p-2.5">
              {loadingFiles ? (
                <div className="flex justify-center py-10"><Loader2 className="h-5 w-5 animate-spin text-muted-foreground" /></div>
              ) : filteredFiles.length === 0 ? (
                <div className="px-3 py-10 text-center text-xs leading-relaxed text-muted-foreground">
                  {query ? "没有匹配的 Memory" : "这个项目还没有 Memory 文件"}
                </div>
              ) : (
                filteredFiles.map((file) => {
                  const active = document?.file.file_name === file.file_name;
                  return (
                    <button
                      key={file.file_name}
                      type="button"
                      onClick={() => openFile(file)}
                      className={cn(
                        "relative w-full overflow-hidden rounded-lg border px-3 py-2.5 text-left transition-colors",
                        "before:pointer-events-none before:absolute before:bottom-2.5 before:left-0 before:top-2.5 before:w-[3px] before:rounded-r-full before:opacity-0 before:transition-opacity",
                        active
                          ? cn(accentRow.claude, "before:opacity-100")
                          : "border-transparent hover:border-border/70 hover:bg-muted/45",
                      )}
                    >
                      <div className="flex items-start gap-2">
                        {file.is_index ? (
                          <BookMarked className="mt-0.5 h-3.5 w-3.5 text-provider-claude-fg" />
                        ) : (
                          <FileText className="mt-0.5 h-3.5 w-3.5 text-muted-foreground" />
                        )}
                        <div className="min-w-0 flex-1">
                          <div className="truncate text-xs font-semibold">{file.title}</div>
                          <div className="mt-0.5 truncate font-mono text-[10px] text-muted-foreground">{file.file_name}</div>
                          {file.preview && <div className="mt-1 line-clamp-2 text-[11px] leading-relaxed text-muted-foreground">{file.preview}</div>}
                        </div>
                      </div>
                    </button>
                  );
                })
              )}
            </div>
          </ScrollArea>
        </aside>

        <section className="flex min-h-0 min-w-0 flex-col">
          {!draftName && !document ? (
            <EmptyState
              title="选择或新建一个 Memory"
              description="编辑器直接对应 Claude Code 项目下的 memory/*.md；保存时会检查文件是否被外部修改。"
              icon={<NotebookText className="h-10 w-10" />}
            />
          ) : (
            <>
              <div className="flex min-w-0 items-center gap-2 border-b border-border/60 bg-background/80 px-4 py-2.5">
                <Input
                  value={draftName}
                  onChange={(event) => setDraftName(event.target.value)}
                  className="h-8 min-w-0 max-w-sm font-mono text-xs"
                  aria-label="Memory 文件名"
                />
                {document && (
                  <span className="hidden truncate text-[11px] text-muted-foreground lg:inline">
                    {humanBytes(document.file.bytes)} · 更新于 {absoluteTime(document.file.updated_at)}
                  </span>
                )}
                <div className="ml-auto flex shrink-0 gap-1.5">
                  {document && (
                    <Button variant="ghost" size="sm" className="h-8 gap-1.5 text-destructive hover:text-destructive" onClick={() => setDeleteOpen(true)}>
                      <Trash2 className="h-3.5 w-3.5" />
                      删除
                    </Button>
                  )}
                  <Button size="sm" className="h-8 gap-1.5" disabled={!dirty || saving || !draftName.trim()} onClick={() => void save()}>
                    {saving ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Save className="h-3.5 w-3.5" />}
                    {saving ? "保存中" : "保存"}
                  </Button>
                </div>
              </div>
              <MemoryMarkdownEditor
                value={draftContent}
                onChange={setDraftContent}
                mode={editorMode}
                onModeChange={setEditorMode}
              />
            </>
          )}
        </section>
      </div>

      <DangerDialog
        open={deleteOpen}
        onOpenChange={setDeleteOpen}
        title={document ? `删除 ${document.file.file_name}` : "删除 Memory"}
        confirmText="删除"
        onConfirm={remove}
      >
        <div className="min-w-0 whitespace-normal">
          将从项目 Memory 目录删除这个文件。同目录下的其他 Memory 与 MEMORY.md 索引不受影响，
          索引里指向它的链接需要你自己清理。
        </div>
        <div className="text-destructive">此操作不可撤销，也不会自动备份。</div>
      </DangerDialog>

      <DangerDialog
        open={!!pendingAction}
        onOpenChange={(open) => !open && setPendingAction(null)}
        title="放弃未保存的修改"
        confirmText="放弃并继续"
        onConfirm={() => {
          pendingAction?.run();
          setPendingAction(null);
        }}
      >
        <div className="min-w-0 whitespace-normal">
          当前 Memory 有尚未保存的改动。继续「{pendingAction?.label ?? "当前操作"}」会丢弃它们；
          先取消并保存，改动就能保留。
        </div>
      </DangerDialog>

      <DangerDialog
        open={blocker.state === "blocked"}
        onOpenChange={(open) => {
          if (!open && blocker.state === "blocked") blocker.reset();
        }}
        title="离开 Claude Memory"
        confirmText="放弃并离开"
        onConfirm={() => {
          if (blocker.state === "blocked") blocker.proceed();
        }}
      >
        <div className="min-w-0 whitespace-normal">
          当前 Memory 有尚未保存的改动。离开页面会丢弃这些内容；取消后先保存即可保留。
        </div>
      </DangerDialog>

      <DangerDialog
        open={closeRequested}
        onOpenChange={setCloseRequested}
        title="关闭 CC Sessions"
        confirmText="放弃并关闭"
        onConfirm={async () => {
          allowWindowCloseRef.current = true;
          try {
            await getCurrentWindow().close();
          } catch (error) {
            allowWindowCloseRef.current = false;
            throw error;
          }
        }}
      >
        <div className="min-w-0 whitespace-normal">
          当前 Memory 有尚未保存的改动。关闭应用会丢弃这些内容；取消后先保存即可保留。
        </div>
      </DangerDialog>
    </>
  );
}
