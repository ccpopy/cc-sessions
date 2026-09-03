import assert from "node:assert/strict";
import test from "node:test";

import {
  defaultMessageTimeRange,
  eventEpochSeconds,
  isWithinRange,
  localDateTimeToEpoch,
  messageTimeLabel,
  messageTimeRange,
  spansMultipleDays,
} from "./exportTimeRange.ts";

const at = (y: number, m: number, d: number, h: number, min: number, s = 0) =>
  Math.floor(new Date(y, m - 1, d, h, min, s).getTime() / 1000);

test("默认范围取首尾消息所在分钟，忽略无时间戳的消息", () => {
  const range = defaultMessageTimeRange([at(2026, 9, 2, 10, 10, 36), null, at(2026, 9, 2, 12, 30, 5)]);

  assert.deepEqual(range, {
    from: { date: "2026-09-02", time: "10:10" },
    to: { date: "2026-09-02", time: "12:30" },
  });
  assert.equal(defaultMessageTimeRange([null]), undefined);
});

test("结束分钟整分钟包含在内，起点含终点不含", () => {
  const range = messageTimeRange(
    { date: "2026-09-02", time: "10:10" },
    { date: "2026-09-02", time: "12:30" },
  );

  assert.equal(range.error, undefined);
  assert.equal(range.from, at(2026, 9, 2, 10, 10));
  assert.equal(range.to, at(2026, 9, 2, 12, 31));
  assert.equal(isWithinRange(at(2026, 9, 2, 12, 30, 59), range.from, range.to), true);
  assert.equal(isWithinRange(at(2026, 9, 2, 12, 31), range.from, range.to), false);
  assert.equal(isWithinRange(at(2026, 9, 2, 10, 9, 59), range.from, range.to), false);
  assert.equal(isWithinRange(null, range.from, range.to), true);
});

test("同一分钟的起止时刻合法，反向范围报错", () => {
  const same = messageTimeRange(
    { date: "2026-09-02", time: "10:10" },
    { date: "2026-09-02", time: "10:10" },
  );
  assert.equal(same.error, undefined);
  assert.equal(same.to, (same.from as number) + 60);

  const reversed = messageTimeRange(
    { date: "2026-09-02", time: "12:00" },
    { date: "2026-09-02", time: "11:59" },
  );
  assert.equal(reversed.error, "开始时间不能晚于结束时间");
});

test("日期为空表示该侧不限，时间为空按整天处理", () => {
  const openStart = messageTimeRange({ date: "", time: "" }, { date: "2026-09-02", time: "" });
  assert.equal(openStart.from, undefined);
  assert.equal(openStart.to, at(2026, 9, 3, 0, 0));

  const openEnd = messageTimeRange({ date: "2026-09-02", time: "" }, { date: "", time: "" });
  assert.equal(openEnd.from, at(2026, 9, 2, 0, 0));
  assert.equal(openEnd.to, undefined);
});

test("无效日期或时间被拒绝", () => {
  assert.equal(localDateTimeToEpoch("2026-02-30", "10:00"), undefined);
  assert.equal(localDateTimeToEpoch("2026-09-02", "24:00"), undefined);
  assert.equal(
    messageTimeRange({ date: "2026-02-30", time: "10:00" }, { date: "", time: "" }).error,
    "时间范围包含无效的日期或时间",
  );
});

test("时间戳解析与列表标签", () => {
  const epoch = eventEpochSeconds("2026-09-02T02:10:36.500Z");
  assert.equal(epoch, Math.floor(Date.parse("2026-09-02T02:10:36.500Z") / 1000));
  assert.equal(eventEpochSeconds("not a time"), null);
  assert.equal(eventEpochSeconds(""), null);

  const stamp = at(2026, 9, 2, 9, 5);
  assert.equal(messageTimeLabel(stamp, false), "09:05");
  assert.equal(messageTimeLabel(stamp, true), "9-2 09:05");
  assert.equal(messageTimeLabel(null, true), "");
  assert.equal(spansMultipleDays([stamp, at(2026, 9, 2, 23, 59)]), false);
  assert.equal(spansMultipleDays([stamp, at(2026, 9, 3, 0, 0)]), true);
});
