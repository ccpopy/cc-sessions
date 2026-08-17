import assert from "node:assert/strict";
import test from "node:test";
import { archiveOriginPresentation } from "./archiveOrigin.ts";

test("高价值来源（manual/official/fork）返回 null，无需额外徽标", () => {
  // 这几种来源是用户主动归档/官方状态/回溯分支，会话卡片上的"已归档"徽标已足够表达
  for (const origin of ["manual", "official", "fork"] as const) {
    assert.equal(archiveOriginPresentation(origin), null, `${origin} 应返回 null`);
  }
});

test("provider_sync 映射为「同步分支」蓝色徽标", () => {
  assert.deepEqual(archiveOriginPresentation("provider_sync"), {
    label: "同步分支",
    className: "border-blue-500/30 text-blue-600",
  });
});

test("restore/import 映射为「迁移记录」徽标", () => {
  for (const origin of ["restore", "import"] as const) {
    assert.deepEqual(archiveOriginPresentation(origin), {
      label: "迁移记录",
      className: "border-border text-muted-foreground",
    });
  }
});

test("unknown 与 null 返回 null，视为用户主动归档，不叠加徽标", () => {
  // 无来源标识的归档（官方应用归档、旧版手动归档）归入"我的归档"组，
  // 与 mine 同策略：不显示"来源未知"徽标
  assert.equal(archiveOriginPresentation("unknown"), null);
  assert.equal(archiveOriginPresentation(null), null);
});
