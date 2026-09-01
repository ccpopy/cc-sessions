import assert from "node:assert/strict";
import test from "node:test";

import {
  exportDateRange,
  formatLocalDate,
  localDateBoundary,
  parseLocalDate,
} from "./exportDateRange.ts";

const TODAY = new Date(2026, 8, 1, 12);

test("今天可以作为开始和结束日期，结束边界包含完整当天", () => {
  const range = exportDateRange("2026-09-01", "2026-09-01", TODAY);

  assert.equal(range.error, undefined);
  assert.equal(range.from, Math.floor(new Date(2026, 8, 1).getTime() / 1000));
  assert.equal(range.to, Math.floor(new Date(2026, 8, 2).getTime() / 1000));
});

test("开始日期和结束日期都不能晚于今天", () => {
  assert.equal(
    exportDateRange("2026-09-02", "", TODAY).error,
    "开始日期不能晚于今天",
  );
  assert.equal(
    exportDateRange("", "2026-09-02", TODAY).error,
    "结束日期不能晚于今天",
  );
});

test("开始日期不能晚于结束日期", () => {
  assert.equal(
    exportDateRange("2026-08-31", "2026-08-30", TODAY).error,
    "开始日期不能晚于结束日期",
  );
});

test("本地日期解析拒绝不存在的日期，并保持本地年月日", () => {
  assert.equal(parseLocalDate("2026-02-29"), undefined);
  assert.equal(localDateBoundary("not-a-date", 0), undefined);

  const leapDay = parseLocalDate("2024-02-29");
  assert.ok(leapDay);
  assert.equal(formatLocalDate(leapDay), "2024-02-29");
});
