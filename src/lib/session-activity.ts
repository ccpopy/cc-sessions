export const SESSION_BUSY_EVENT = "cc-sessions:session-busy";
const marker = "[SESSION_BUSY]";

/** Reports may carry failures per item instead of rejecting the whole command. Read only error
 * fields, never transcript content, and also handle background provider-sync reports. */
export function sessionBusyMessages(value: unknown): string[] {
  const messages = new Set<string>();
  function visit(value: unknown, strings = false): void {
    if (typeof value === "string" && strings) {
      const index = value.indexOf(marker);
      if (index >= 0) messages.add(value.slice(index + marker.length).trim());
    } else if (value instanceof Error) {
      visit(value.message, true);
    } else if (Array.isArray(value)) {
      value.forEach((item) => visit(item, strings));
    } else if (value && typeof value === "object") {
      const report = value as Record<string, unknown>;
      visit(report.error, true);
      visit(report.errors, true);
      visit(report.reports);
      visit(report.results);
    }
  }
  visit(value, typeof value === "string");
  return [...messages];
}

export function notifySessionBusy(value: unknown): void {
  const messages = sessionBusyMessages(value);
  if (messages.length) window.dispatchEvent(new CustomEvent(SESSION_BUSY_EVENT, { detail: messages }));
}

/** Keep the transport marker out of notices while preserving ordinary payload strings. */
export function cleanSessionBusyErrors<T>(value: T, errorField = false): T {
  if (typeof value === "string" && errorField) return value.replaceAll(marker, "").trim() as T;
  if (Array.isArray(value)) return value.map((item) => cleanSessionBusyErrors(item, errorField)) as T;
  if (value && typeof value === "object") {
    const result = { ...value } as Record<string, unknown>;
    for (const key of ["error", "errors", "reports", "results"]) {
      if (key in result) result[key] = cleanSessionBusyErrors(result[key], key === "error" || key === "errors");
    }
    return result as T;
  }
  return value;
}
