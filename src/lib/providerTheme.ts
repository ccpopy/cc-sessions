import type { SessionProvider } from "@/lib/api";

/**
 * provider 视觉标识的唯一来源。
 *
 * 应用主色（emerald）表示"选中 / 主操作"，与这里的 Agent 品牌色互不重叠：
 * 侧栏、会话卡片徽章、Memory 页只要涉及"这是哪个 Agent"，都从这里取类名，
 * 避免同一含义在不同页面出现三套绿色。
 */
export type AccentKey = SessionProvider | "global";

const LABELS: Record<SessionProvider, string> = {
  codex: "Codex",
  claude: "Claude",
  opencode: "OpenCode",
  cursor: "Cursor",
};

/** 把 provider 标识转成界面文案；未知取值原样返回，便于排查后端新增来源。 */
export function providerLabel(provider: string | null | undefined): string {
  if (!provider) return "未知";
  return LABELS[provider as SessionProvider] ?? provider;
}

/** 小圆点：分组标题、目录卡、下拉当前项。 */
export const accentDot: Record<AccentKey, string> = {
  codex: "bg-provider-codex",
  claude: "bg-provider-claude",
  opencode: "bg-provider-opencode",
  cursor: "bg-provider-cursor",
  global: "bg-foreground/60",
};

/** 静置状态的竖条。 */
export const accentBar: Record<AccentKey, string> = {
  codex: "bg-provider-codex/90",
  claude: "bg-provider-claude/90",
  opencode: "bg-provider-opencode/90",
  cursor: "bg-provider-cursor/90",
  global: "bg-foreground/70",
};

/** 选中状态的竖条，带一层同色辉光。 */
export const accentActiveBar: Record<AccentKey, string> = {
  codex: "bg-provider-codex shadow-[0_0_10px_-1px_hsl(var(--provider-codex)/0.55)]",
  claude: "bg-provider-claude shadow-[0_0_10px_-1px_hsl(var(--provider-claude)/0.55)]",
  opencode: "bg-provider-opencode shadow-[0_0_10px_-1px_hsl(var(--provider-opencode)/0.5)]",
  cursor: "bg-provider-cursor shadow-[0_0_10px_-1px_hsl(var(--provider-cursor)/0.5)]",
  global: "bg-foreground/80",
};

/** 选中状态的图标色。 */
export const accentActiveIcon: Record<AccentKey, string> = {
  codex: "text-provider-codex-fg",
  claude: "text-provider-claude-fg",
  opencode: "text-provider-opencode-fg",
  cursor: "text-provider-cursor-fg",
  global: "text-foreground",
};

/** 选中状态的底色 + 内描边。 */
export const accentActiveTint: Record<AccentKey, string> = {
  codex: "bg-provider-codex/10 ring-1 ring-inset ring-provider-codex/20",
  claude: "bg-provider-claude/10 ring-1 ring-inset ring-provider-claude/20",
  opencode: "bg-provider-opencode/10 ring-1 ring-inset ring-provider-opencode/20",
  cursor: "bg-provider-cursor/10 ring-1 ring-inset ring-provider-cursor/20",
  global: "bg-sidebar-accent ring-1 ring-inset ring-border/60",
};

/** 方形 Agent 标记（侧栏切换器）：品牌色淡底 + 内描边 + 同色图标。 */
export const accentMark: Record<SessionProvider, string> = {
  codex: "bg-provider-codex/10 text-provider-codex-fg ring-1 ring-inset ring-provider-codex/25",
  claude: "bg-provider-claude/10 text-provider-claude-fg ring-1 ring-inset ring-provider-claude/25",
  opencode: "bg-provider-opencode/10 text-provider-opencode-fg ring-1 ring-inset ring-provider-opencode/25",
  cursor: "bg-provider-cursor/10 text-provider-cursor-fg ring-1 ring-inset ring-provider-cursor/25",
};

/** 会话卡片上的 provider 徽章。 */
export const accentBadge: Record<SessionProvider, string> = {
  codex: "border-provider-codex/35 bg-provider-codex/10 text-provider-codex-fg",
  claude: "border-provider-claude/40 bg-provider-claude/10 text-provider-claude-fg",
  opencode: "border-provider-opencode/35 bg-provider-opencode/10 text-provider-opencode-fg",
  cursor: "border-provider-cursor/35 bg-provider-cursor/10 text-provider-cursor-fg",
};

/** 列表项选中态：与会话卡片一致的"左侧色条 + 淡底"写法。 */
export const accentRow: Record<SessionProvider, string> = {
  codex: "border-provider-codex/30 bg-provider-codex/[0.07] before:bg-provider-codex",
  claude: "border-provider-claude/30 bg-provider-claude/[0.07] before:bg-provider-claude",
  opencode: "border-provider-opencode/30 bg-provider-opencode/[0.07] before:bg-provider-opencode",
  cursor: "border-provider-cursor/30 bg-provider-cursor/[0.07] before:bg-provider-cursor",
};

export function accentBadgeFor(provider: string): { label: string; className: string } {
  const known = provider as SessionProvider;
  if (known in accentBadge) {
    return { label: LABELS[known], className: accentBadge[known] };
  }
  return { label: providerLabel(provider), className: "border-border text-muted-foreground" };
}
