import { isValid, parseISO } from "date-fns";

import { formatLocalDate, parseLocalDate } from "./exportDateRange";

/** 本地日期 "yyyy-MM-dd" + 时间 "HH:mm"；任一为空表示该侧不限。 */
export type LocalDateTime = { date: string; time: string };

export type MessageTimeRange = {
  /** epoch 秒，含 */
  from?: number;
  /** epoch 秒，不含（结束分钟的下一分钟起点） */
  to?: number;
  error?: string;
};

export const EMPTY_LOCAL_DATE_TIME: LocalDateTime = { date: "", time: "" };

/** ISO 时间戳 → epoch 秒；解析失败返回 null。 */
export function eventEpochSeconds(timestamp: string | null | undefined): number | null {
  if (!timestamp) return null;
  const parsed = /^\d{4}-\d{2}-\d{2}T/.test(timestamp) ? parseISO(timestamp) : new Date(timestamp);
  if (!isValid(parsed)) return null;
  return Math.floor(parsed.getTime() / 1000);
}

export function epochToLocalDateTime(seconds: number): LocalDateTime {
  const date = new Date(seconds * 1000);
  return { date: formatLocalDate(date), time: `${pad(date.getHours())}:${pad(date.getMinutes())}` };
}

/** 本地日期 + 时间 → 该分钟起点的 epoch 秒；日期或时间无效返回 undefined。 */
export function localDateTimeToEpoch(date: string, time: string): number | undefined {
  const day = parseLocalDate(date);
  const match = /^(\d{2}):(\d{2})$/.exec(time);
  if (!day || !match) return undefined;
  const hours = Number(match[1]);
  const minutes = Number(match[2]);
  if (hours > 23 || minutes > 59) return undefined;
  const at = new Date(day.getFullYear(), day.getMonth(), day.getDate(), hours, minutes);
  return Math.floor(at.getTime() / 1000);
}

/** 由消息时间戳推导默认范围：起点为最早消息所在分钟，终点为最晚消息所在分钟。 */
export function defaultMessageTimeRange(
  stamps: Array<number | null>,
): { from: LocalDateTime; to: LocalDateTime } | undefined {
  let min: number | undefined;
  let max: number | undefined;
  for (const stamp of stamps) {
    if (stamp === null) continue;
    if (min === undefined || stamp < min) min = stamp;
    if (max === undefined || stamp > max) max = stamp;
  }
  if (min === undefined || max === undefined) return undefined;
  return { from: epochToLocalDateTime(min), to: epochToLocalDateTime(max) };
}

/**
 * 把用户输入的起止时刻换算成过滤边界。
 *
 * - 日期为空表示该侧不限；时间为空时开始取当天 00:00、结束取当天 23:59；
 * - 起点含、终点所在分钟整分钟包含在内（即上界 = 终点分钟 + 60 秒，不含）。
 */
export function messageTimeRange(from: LocalDateTime, to: LocalDateTime): MessageTimeRange {
  const fromEpoch = from.date ? localDateTimeToEpoch(from.date, from.time || "00:00") : undefined;
  const toStart = to.date ? localDateTimeToEpoch(to.date, to.time || "23:59") : undefined;
  if ((from.date && fromEpoch === undefined) || (to.date && toStart === undefined)) {
    return { error: "时间范围包含无效的日期或时间" };
  }
  const toEpoch = toStart === undefined ? undefined : toStart + 60;
  if (fromEpoch !== undefined && toStart !== undefined && fromEpoch > toStart) {
    return { from: fromEpoch, to: toEpoch, error: "开始时间不能晚于结束时间" };
  }
  return { from: fromEpoch, to: toEpoch };
}

/** 无时间戳的消息无法定位，不受范围限制。 */
export function isWithinRange(ts: number | null, from?: number, to?: number): boolean {
  if (ts === null) return true;
  if (from !== undefined && ts < from) return false;
  if (to !== undefined && ts >= to) return false;
  return true;
}

export function spansMultipleDays(stamps: Array<number | null>): boolean {
  let first: string | undefined;
  for (const stamp of stamps) {
    if (stamp === null) continue;
    const day = formatLocalDate(new Date(stamp * 1000));
    if (first === undefined) first = day;
    else if (day !== first) return true;
  }
  return false;
}

/** 列表中的紧凑时间：单日会话只显示 HH:mm，跨天时带上月-日。 */
export function messageTimeLabel(ts: number | null, multiDay: boolean): string {
  if (ts === null) return "";
  const date = new Date(ts * 1000);
  const clock = `${pad(date.getHours())}:${pad(date.getMinutes())}`;
  return multiDay ? `${date.getMonth() + 1}-${date.getDate()} ${clock}` : clock;
}

export function sameLocalDateTime(a: LocalDateTime, b: LocalDateTime): boolean {
  return a.date === b.date && a.time === b.time;
}

function pad(value: number): string {
  return String(value).padStart(2, "0");
}
