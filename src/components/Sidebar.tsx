import { useEffect, useState } from "react";
import { NavLink, useLocation, useNavigate } from "react-router-dom";
import {
  Archive,
  BarChart3,
  Bot,
  Braces,
  ChevronDown,
  MessageSquare,
  NotebookText,
  Package,
  Settings,
  Sparkles,
  Terminal,
  Wrench,
} from "lucide-react";

import { SettingsSheet } from "@/components/SettingsSheet";
import { ThemeToggle } from "@/components/ThemeToggle";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Sidebar as SidebarPrimitive,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
} from "@/components/ui/sidebar";
import type { SessionProvider } from "@/lib/api";
import {
  accentActiveBar,
  accentActiveIcon,
  accentActiveTint,
  accentBar,
  accentDot,
  accentMark,
  type AccentKey,
} from "@/lib/providerTheme";
import { cn } from "@/lib/utils";
import { useSettings } from "@/stores/settings";

type NavItem = {
  to: string;
  icon: React.ComponentType<{ className?: string }>;
  label: string;
};

type ProviderDefinition = {
  id: SessionProvider;
  label: string;
  icon: React.ComponentType<{ className?: string }>;
  items: NavItem[];
};

const providers: ProviderDefinition[] = [
  {
    id: "codex",
    label: "Codex",
    icon: Bot,
    items: [
      { to: "/codex/sessions", icon: MessageSquare, label: "会话" },
      { to: "/codex/repair", icon: Wrench, label: "修复" },
      { to: "/codex/backups", icon: Archive, label: "备份" },
      { to: "/codex/transfer", icon: Package, label: "导出 / 导入" },
    ],
  },
  {
    id: "claude",
    label: "Claude",
    icon: Sparkles,
    items: [
      { to: "/claude/sessions", icon: MessageSquare, label: "会话" },
      { to: "/claude/memory", icon: NotebookText, label: "Memory" },
      { to: "/claude/repair", icon: Wrench, label: "修复" },
      { to: "/claude/backups", icon: Archive, label: "备份" },
      { to: "/claude/transfer", icon: Package, label: "导出 / 导入" },
    ],
  },
  {
    id: "opencode",
    label: "OpenCode",
    icon: Braces,
    items: [{ to: "/opencode/sessions", icon: MessageSquare, label: "会话" }],
  },
];

const globalItems: NavItem[] = [{ to: "/stats", icon: BarChart3, label: "统计" }];


export function Sidebar() {
  const settings = useSettings((state) => state.settings);
  const location = useLocation();
  const navigate = useNavigate();
  const pathProvider = providerFromPath(location.pathname);
  const [lastProvider, setLastProvider] = useState<SessionProvider>(pathProvider ?? "codex");
  const activeProvider = pathProvider ?? lastProvider;
  const provider = providers.find((item) => item.id === activeProvider) ?? providers[0];
  const providerPath = providerDirectory(settings, provider.id);

  useEffect(() => {
    if (pathProvider) setLastProvider(pathProvider);
  }, [pathProvider]);

  const switchProvider = (nextProvider: ProviderDefinition) => {
    const currentFeature = location.pathname.split("/").filter(Boolean)[1] ?? "sessions";
    const matchingFeature = nextProvider.items.find(
      (item) => item.to.split("/").filter(Boolean)[1] === currentFeature,
    );
    navigate(matchingFeature?.to ?? nextProvider.items[0].to);
  };

  return (
    <SidebarPrimitive
      collapsible="none"
      className="hidden w-56 shrink-0 border-r border-border/60 md:flex"
      style={{ "--sidebar-width": "14rem" } as React.CSSProperties}
    >
      <SidebarHeader className="relative flex h-14 flex-row items-center gap-2.5 border-b border-border/60 px-3.5 py-0 after:pointer-events-none after:absolute after:inset-x-0 after:-bottom-px after:h-px after:bg-gradient-to-r after:from-transparent after:via-border/40 after:to-transparent">
        <div className="relative flex h-8 w-8 shrink-0 items-center justify-center overflow-hidden rounded-lg bg-gradient-to-br from-foreground to-foreground/80 text-background shadow-[0_1px_2px_-1px_hsl(var(--foreground)/0.4),inset_0_1px_0_0_hsl(var(--background)/0.18)]">
          <span aria-hidden="true" className="absolute inset-0 bg-[radial-gradient(circle_at_30%_20%,hsl(var(--background)/0.18),transparent_55%)]" />
          <Terminal className="relative h-4 w-4" />
        </div>
        <div className="min-w-0 flex-1">
          <div className="truncate text-[13px] font-semibold leading-tight tracking-tight text-foreground">CC Sessions</div>
          <div className="mt-0.5 truncate text-[9.5px] font-semibold uppercase leading-tight tracking-[0.14em] text-muted-foreground/75">
            Multi-agent workspace
          </div>
        </div>
      </SidebarHeader>

      <SidebarContent className="gap-0 py-1">
        <div className="px-2.5 pb-1 pt-2.5">
          <div className="mb-1.5 px-1.5 text-[9.5px] font-semibold uppercase tracking-[0.14em] text-muted-foreground/70">
            当前 Agent
          </div>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button
                variant="outline"
                className="group/provider h-11 w-full justify-start gap-2.5 rounded-lg border-border/70 bg-muted/30 px-2.5 text-left shadow-[inset_0_1px_0_0_hsl(var(--background)/0.55)] hover:bg-muted/50"
              >
                <ProviderMark provider={provider} />
                <span className="min-w-0 flex-1 truncate text-xs font-semibold text-foreground">
                  {provider.label}
                </span>
                <ChevronDown className="h-3.5 w-3.5 shrink-0 text-muted-foreground transition-transform group-data-[state=open]/provider:rotate-180" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="start" className="w-[calc(14rem-1.25rem)]">
              <DropdownMenuLabel className="text-[10px] uppercase tracking-[0.12em] text-muted-foreground">切换 Agent</DropdownMenuLabel>
              <DropdownMenuSeparator />
              {providers.map((item) => (
                <DropdownMenuItem
                  key={item.id}
                  onSelect={() => switchProvider(item)}
                  className="gap-2.5 py-2"
                >
                  <ProviderMark provider={item} compact />
                  <span className="min-w-0 flex-1 truncate text-xs font-medium">{item.label}</span>
                  {item.id === provider.id && <span className={cn("h-1.5 w-1.5 rounded-full", accentDot[item.id])} />}
                </DropdownMenuItem>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
        </div>

        <NavGroup label="工具" accent={provider.id} items={provider.items} pathname={location.pathname} />
        <NavGroup label="全局" accent="global" items={globalItems} pathname={location.pathname} />
      </SidebarContent>

      <SidebarFooter className="gap-1.5 border-t border-border/60 p-2.5">
        {providerPath && <DirCard label={`${provider.label} 目录`} path={providerPath} accent={provider.id} />}
        <div className="mt-0.5 flex items-center gap-1">
          <ThemeToggle className="flex-1" />
          <SettingsSheet
            trigger={
              <Button
                variant="ghost"
                size="icon"
                aria-label="设置"
                className="group/settings h-9 w-9 shrink-0 rounded-lg border border-border/70 bg-muted/40 text-muted-foreground shadow-[inset_0_1px_0_0_hsl(var(--background)/0.6)] hover:border-border hover:bg-muted/60 hover:text-foreground dark:bg-muted/30 dark:shadow-[inset_0_1px_0_0_hsl(var(--background)/0.2)]"
              >
                <Settings className="h-4 w-4 transition-transform duration-500 ease-out group-hover/settings:rotate-90" />
              </Button>
            }
          />
        </div>
      </SidebarFooter>
    </SidebarPrimitive>
  );
}

function ProviderMark({ provider, compact = false }: { provider: ProviderDefinition; compact?: boolean }) {
  const Icon = provider.icon;
  return (
    <span
      className={cn(
        "relative flex shrink-0 items-center justify-center rounded-md border bg-background/80",
        compact ? "h-7 w-7" : "h-8 w-8",
        accentMark[provider.id],
      )}
    >
      <span className={cn("absolute bottom-0.5 right-0.5 h-1 w-1 rounded-full", accentDot[provider.id])} />
      <Icon className={compact ? "h-3.5 w-3.5" : "h-4 w-4"} />
    </span>
  );
}

function NavGroup({
  label,
  items,
  pathname,
  accent,
}: {
  label: string;
  items: NavItem[];
  pathname: string;
  accent: AccentKey;
}) {
  return (
    <SidebarGroup className="px-2 pb-1 pt-2.5">
      <div className="mb-1.5 flex items-center gap-2 px-2">
        <span aria-hidden="true" className={cn("h-1 w-1 shrink-0 rounded-full", accentDot[accent])} />
        <div className="text-[10px] font-semibold uppercase leading-none tracking-[0.14em] text-muted-foreground/75">{label}</div>
        <div aria-hidden="true" className="ml-1 h-px flex-1 bg-gradient-to-r from-border/60 via-border/30 to-transparent" />
      </div>
      <SidebarMenu className="gap-0.5">
        {items.map((item) => {
          const isActive = item.to === "/stats" ? pathname === item.to : pathname.startsWith(item.to);
          const Icon = item.icon;
          return (
            <SidebarMenuItem key={item.to}>
              <SidebarMenuButton
                asChild
                isActive={isActive}
                className={cn(
                  "relative h-auto overflow-visible px-2.5 py-2 text-[13px] font-medium",
                  "data-[active=true]:bg-transparent data-[active=true]:hover:bg-transparent",
                )}
              >
                <NavLink to={item.to} end={false}>
                  <span aria-hidden="true" className={cn("absolute inset-0 rounded-md transition-opacity duration-200", accentActiveTint[accent], isActive ? "opacity-100" : "opacity-0")} />
                  <span
                    aria-hidden="true"
                    className={cn(
                      "absolute -left-[9px] top-1/2 h-5 w-[3px] -translate-y-1/2 rounded-r-full transition-all duration-200",
                      isActive ? cn(accentActiveBar[accent], "scale-y-100 opacity-100") : cn(accentBar[accent], "scale-y-50 opacity-0"),
                    )}
                  />
                  <Icon className={cn("relative h-4 w-4 shrink-0 transition-colors", isActive ? accentActiveIcon[accent] : "text-muted-foreground/80")} />
                  <span className={cn("relative truncate transition-colors", isActive ? "font-semibold text-foreground" : "text-foreground/85")}>{item.label}</span>
                </NavLink>
              </SidebarMenuButton>
            </SidebarMenuItem>
          );
        })}
      </SidebarMenu>
    </SidebarGroup>
  );
}

function DirCard({ label, path, accent }: { label: string; path: string; accent: AccentKey }) {
  return (
    <div className="group/dir relative overflow-hidden rounded-md border border-border/60 bg-muted/30 px-2 py-1.5 transition-colors hover:bg-muted/50" title={path}>
      <span aria-hidden="true" className={cn("absolute left-0 top-1/2 h-4 w-[2px] -translate-y-1/2 rounded-r-full opacity-70", accentBar[accent])} />
      <div className="flex items-center gap-1.5">
        <span aria-hidden="true" className={cn("h-1 w-1 shrink-0 rounded-full", accentDot[accent])} />
        <div className="text-[9.5px] font-semibold uppercase tracking-[0.12em] text-muted-foreground/80">{label}</div>
      </div>
      <div className="mt-0.5 truncate font-mono text-[10.5px] leading-snug text-foreground/70">{path}</div>
    </div>
  );
}

function providerFromPath(pathname: string): SessionProvider | null {
  const segment = pathname.split("/").filter(Boolean)[0];
  if (segment === "codex" || segment === "claude" || segment === "opencode") return segment;
  return null;
}

function providerDirectory(
  settings: ReturnType<typeof useSettings.getState>["settings"],
  provider: SessionProvider,
): string {
  if (!settings) return "";
  if (provider === "claude") return settings.claude_dir;
  if (provider === "opencode") return settings.opencode_dir;
  return settings.codex_dir;
}
