/**
 * 在 React 完成下一次渲染前同步占用会话操作，避免快速连点重复派发命令。
 */
export class SessionActionRegistry {
  private readonly sessionIds = new Set<string>();

  tryBegin(sessionId: string): boolean {
    if (this.sessionIds.has(sessionId)) return false;
    this.sessionIds.add(sessionId);
    return true;
  }

  finish(sessionId: string): void {
    this.sessionIds.delete(sessionId);
  }

  snapshot(): ReadonlySet<string> {
    return new Set(this.sessionIds);
  }
}
