import { NavLink, useLocation } from "react-router-dom";
import {
  Archive,
  BarChart3,
  MessageSquare,
  Package,
  Settings,
  Terminal,
  Wrench,
} from "lucide-react";
import { Button } from "@/components/ui/button";
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
import { SettingsSheet } from "@/components/SettingsSheet";
import { ThemeToggle } from "@/components/ThemeToggle";
import { useSettings } from "@/stores/settings";
import { cn } from "@/lib/utils";

type Accent = "codex" | "claude" | "gemini" | "global";

type NavItem = {
  to: string;
  icon: React.ComponentType<{ className?: string }>;
  label: string;
};

const codexItems: NavItem[] = [
  { to: "/codex/sessions", icon: MessageSquare, label: "会话" },
  { to: "/codex/repair", icon: Wrench, label: "修复" },
  { to: "/codex/backups", icon: Archive, label: "备份" },
  { to: "/codex/transfer", icon: Package, label: "导出 / 导入" },
];

const claudeItems: NavItem[] = [
  { to: "/claude/sessions", icon: MessageSquare, label: "会话" },
  { to: "/claude/repair", icon: Wrench, label: "修复" },
  { to: "/claude/backups", icon: Archive, label: "备份" },
  { to: "/claude/transfer", icon: Package, label: "导出 / 导入" },
];

const geminiItems: NavItem[] = [
  { to: "/gemini/sessions", icon: MessageSquare, label: "会话" },
  { to: "/gemini/repair", icon: Wrench, label: "清理" },
  { to: "/gemini/backups", icon: Archive, label: "备份" },
];

const globalItems: NavItem[] = [
  { to: "/stats", icon: BarChart3, label: "统计" },
];

const accentDot: Record<Accent, string> = {
  codex: "bg-emerald-500",
  claude: "bg-orange-500",
  gemini: "bg-sky-500",
  global: "bg-foreground/60",
};

const accentBar: Record<Accent, string> = {
  codex: "bg-emerald-500/90",
  claude: "bg-orange-500/90",
  gemini: "bg-sky-500/90",
  global: "bg-foreground/70",
};

const accentActiveBar: Record<Accent, string> = {
  codex: "bg-emerald-500 shadow-[0_0_10px_-1px_hsl(142_76%_45%/0.55)]",
  claude: "bg-orange-500 shadow-[0_0_10px_-1px_hsl(24_95%_55%/0.55)]",
  gemini: "bg-sky-500 shadow-[0_0_10px_-1px_hsl(199_89%_48%/0.55)]",
  global: "bg-foreground/80",
};

const accentActiveIcon: Record<Accent, string> = {
  codex: "text-emerald-600 dark:text-emerald-400",
  claude: "text-orange-600 dark:text-orange-400",
  gemini: "text-sky-600 dark:text-sky-400",
  global: "text-foreground",
};

const accentActiveTint: Record<Accent, string> = {
  codex:
    "bg-emerald-500/10 ring-1 ring-inset ring-emerald-500/20 dark:bg-emerald-500/12",
  claude:
    "bg-orange-500/10 ring-1 ring-inset ring-orange-500/20 dark:bg-orange-500/12",
  gemini:
    "bg-sky-500/10 ring-1 ring-inset ring-sky-500/20 dark:bg-sky-500/12",
  global: "bg-sidebar-accent ring-1 ring-inset ring-border/60",
};

export function Sidebar() {
  const settings = useSettings((s) => s.settings);
  const location = useLocation();

  return (
    <SidebarPrimitive
      collapsible="none"
      className="hidden w-56 shrink-0 border-r border-border/60 md:flex"
      style={{ "--sidebar-width": "14rem" } as React.CSSProperties}
    >
      <SidebarHeader className="relative flex h-14 flex-row items-center gap-2.5 border-b border-border/60 px-3.5 py-0 after:pointer-events-none after:absolute after:inset-x-0 after:-bottom-px after:h-px after:bg-gradient-to-r after:from-transparent after:via-border/40 after:to-transparent">
        <div className="relative flex h-8 w-8 shrink-0 items-center justify-center overflow-hidden rounded-lg bg-gradient-to-br from-foreground to-foreground/80 text-background shadow-[0_1px_2px_-1px_hsl(var(--foreground)/0.4),inset_0_1px_0_0_hsl(var(--background)/0.18)]">
          <span
            aria-hidden="true"
            className="absolute inset-0 bg-[radial-gradient(circle_at_30%_20%,hsl(var(--background)/0.18),transparent_55%)]"
          />
          <Terminal className="relative h-4 w-4" />
        </div>
        <div className="min-w-0 flex-1">
          <div className="truncate text-[13px] font-semibold leading-tight tracking-tight text-foreground">
            CC Sessions
          </div>
          <div className="mt-0.5 truncate text-[9.5px] font-semibold uppercase leading-tight tracking-[0.14em] text-muted-foreground/75">
            Codex · Claude · Gemini
          </div>
        </div>
      </SidebarHeader>

      <SidebarContent className="gap-0 py-1">
        <NavGroup label="Codex" accent="codex" items={codexItems} pathname={location.pathname} />
        <NavGroup label="Claude" accent="claude" items={claudeItems} pathname={location.pathname} />
        <NavGroup label="Gemini" accent="gemini" items={geminiItems} pathname={location.pathname} />
        <NavGroup label="全局" accent="global" items={globalItems} pathname={location.pathname} />
      </SidebarContent>

      <SidebarFooter className="gap-1.5 border-t border-border/60 p-2.5">
        {settings?.codex_dir && (
          <DirCard label="Codex 目录" path={settings.codex_dir} accent="codex" />
        )}
        {settings?.claude_dir && (
          <DirCard label="Claude 目录" path={settings.claude_dir} accent="claude" />
        )}
        {settings?.gemini_dir && (
          <DirCard label="Gemini 目录" path={settings.gemini_dir} accent="gemini" />
        )}
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

function NavGroup({
  label,
  items,
  pathname,
  accent,
}: {
  label: string;
  items: NavItem[];
  pathname: string;
  accent: Accent;
}) {
  return (
    <SidebarGroup className="px-2 pb-1 pt-2.5">
      <div className="mb-1.5 flex items-center gap-2 px-2">
        <span aria-hidden="true" className={cn("h-1 w-1 shrink-0 rounded-full", accentDot[accent])} />
        <div className="text-[10px] font-semibold uppercase leading-none tracking-[0.14em] text-muted-foreground/75">
          {label}
        </div>
        <div
          aria-hidden="true"
          className="ml-1 h-px flex-1 bg-gradient-to-r from-border/60 via-border/30 to-transparent"
        />
      </div>
      <SidebarMenu className="gap-0.5">
        {items.map((it) => {
          const isActive = it.to === "/stats" ? pathname === it.to : pathname.startsWith(it.to);
          const Icon = it.icon;
          return (
            <SidebarMenuItem key={it.to}>
              <SidebarMenuButton
                asChild
                isActive={isActive}
                className={cn(
                  "relative h-auto overflow-visible px-2.5 py-2 text-[13px] font-medium",
                  "data-[active=true]:bg-transparent data-[active=true]:hover:bg-transparent",
                )}
              >
                <NavLink to={it.to} end={false}>
                  <span
                    aria-hidden="true"
                    className={cn(
                      "absolute inset-0 rounded-md transition-opacity duration-200",
                      accentActiveTint[accent],
                      isActive ? "opacity-100" : "opacity-0",
                    )}
                  />
                  <span
                    aria-hidden="true"
                    className={cn(
                      "absolute -left-[9px] top-1/2 h-5 w-[3px] -translate-y-1/2 rounded-r-full transition-all duration-200",
                      isActive
                        ? cn(accentActiveBar[accent], "scale-y-100 opacity-100")
                        : cn(accentBar[accent], "scale-y-50 opacity-0"),
                    )}
                  />
                  <Icon
                    className={cn(
                      "relative h-4 w-4 shrink-0 transition-colors",
                      isActive ? accentActiveIcon[accent] : "text-muted-foreground/80",
                    )}
                  />
                  <span
                    className={cn(
                      "relative truncate transition-colors",
                      isActive ? "font-semibold text-foreground" : "text-foreground/85",
                    )}
                  >
                    {it.label}
                  </span>
                </NavLink>
              </SidebarMenuButton>
            </SidebarMenuItem>
          );
        })}
      </SidebarMenu>
    </SidebarGroup>
  );
}

function DirCard({
  label,
  path,
  accent,
}: {
  label: string;
  path: string;
  accent: Accent;
}) {
  return (
    <div
      className="group/dir relative overflow-hidden rounded-md border border-border/60 bg-muted/30 px-2 py-1.5 transition-colors hover:bg-muted/50"
      title={path}
    >
      <span
        aria-hidden="true"
        className={cn(
          "absolute left-0 top-1/2 h-4 w-[2px] -translate-y-1/2 rounded-r-full opacity-70",
          accentBar[accent],
        )}
      />
      <div className="flex items-center gap-1.5">
        <span aria-hidden="true" className={cn("h-1 w-1 shrink-0 rounded-full", accentDot[accent])} />
        <div className="text-[9.5px] font-semibold uppercase tracking-[0.12em] text-muted-foreground/80">
          {label}
        </div>
      </div>
      <div className="mt-0.5 truncate font-mono text-[10.5px] leading-snug text-foreground/70">
        {path}
      </div>
    </div>
  );
}
