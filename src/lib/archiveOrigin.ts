import type { ArchiveOrigin } from "@/lib/api";

/**
 * 归档来源的视觉呈现：徽标文案 + 配色类名。
 *
 * 为什么抽成独立纯函数：SessionCard（归档徽标）与 FamilyHistorySheet（分支徽标）
 * 共用同一映射，集中在一个模块可以保证两处视图对同一来源永远显示一致的解释；
 * 同时把"未知来源"兜底收口在这里，后端新增来源时界面不会因枚举不匹配而异常。
 */
export type ArchiveOriginPresentation = {
  label: string;
  className: string;
};

/**
 * 把归档来源映射为徽标展示；返回 null 表示无需额外徽标。
 *
 * 映射规则：
 * - manual/official/fork → 高价值来源（用户主动归档 / 官方状态 / 回溯分支），
 *   会话卡片上已有"已归档"徽标表达，不再叠加来源徽标。
 * - provider_sync → "同步分支"：切换模型服务配置时由工具自动归档。
 * - restore/import → "迁移记录"：备份恢复或会话包导入产生的归档。
 * - unknown / null / 未知字符串 → null：无来源标识的归档视为用户主动归档
 *   （官方应用归档、旧版手动归档都不写来源标识），与 mine 组同样不叠加徽标。
 */
export function archiveOriginPresentation(
  origin: ArchiveOrigin | null,
): ArchiveOriginPresentation | null {
  switch (origin) {
    case "manual":
    case "official":
    case "fork":
    case "unknown":
      return null;
    case "provider_sync":
      return { label: "同步分支", className: "border-blue-500/30 text-blue-600" };
    case "restore":
    case "import":
      return { label: "迁移记录", className: "border-border text-muted-foreground" };
    default:
      // null 或未来新增的枚举值：与"无标识即用户主动归档"语义一致，不叠加徽标
      return null;
  }
}
