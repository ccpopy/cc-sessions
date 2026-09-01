export type ExportDateRange = {
  from?: number;
  to?: number;
  error?: string;
};

export function exportDateRange(
  fromDate: string,
  toDate: string,
  today = new Date(),
): ExportDateRange {
  const from = fromDate ? localDateBoundary(fromDate, 0) : undefined;
  const toStart = toDate ? localDateBoundary(toDate, 0) : undefined;
  const to = toDate ? localDateBoundary(toDate, 1) : undefined;
  if (
    (fromDate && from === undefined) ||
    (toDate && (toStart === undefined || to === undefined))
  ) {
    return { error: "时间范围包含无效日期" };
  }

  const todayStart = localDateBoundary(formatLocalDate(today), 0);
  if (from !== undefined && todayStart !== undefined && from > todayStart) {
    return { from, to, error: "开始日期不能晚于今天" };
  }
  if (toStart !== undefined && todayStart !== undefined && toStart > todayStart) {
    return { from, to, error: "结束日期不能晚于今天" };
  }
  if (from !== undefined && to !== undefined && from >= to) {
    return { from, to, error: "开始日期不能晚于结束日期" };
  }
  return { from, to };
}

export function parseLocalDate(value: string): Date | undefined {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value);
  if (!match) return undefined;

  const year = Number(match[1]);
  const month = Number(match[2]);
  const day = Number(match[3]);
  const date = new Date(year, month - 1, day);
  if (
    date.getFullYear() !== year ||
    date.getMonth() !== month - 1 ||
    date.getDate() !== day
  ) {
    return undefined;
  }
  return date;
}

export function formatLocalDate(date: Date): string {
  const year = String(date.getFullYear()).padStart(4, "0");
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

export function localDateBoundary(value: string, dayOffset: number): number | undefined {
  const date = parseLocalDate(value);
  if (!date) return undefined;

  const boundary = new Date(
    date.getFullYear(),
    date.getMonth(),
    date.getDate() + dayOffset,
  );
  return Math.floor(boundary.getTime() / 1000);
}
