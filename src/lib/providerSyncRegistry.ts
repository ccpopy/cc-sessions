export type ProviderSyncSnapshot = {
  batchActive: boolean;
  sessionIds: ReadonlySet<string>;
};

/**
 * Synchronously reserves provider-sync work before React has time to re-render.
 * This prevents rapid clicks from dispatching duplicate Tauri commands.
 */
export class ProviderSyncRegistry {
  private batchActive = false;
  private readonly sessionIds = new Set<string>();

  tryBeginSession(sessionId: string): boolean {
    if (this.batchActive || this.sessionIds.has(sessionId)) return false;
    this.sessionIds.add(sessionId);
    return true;
  }

  finishSession(sessionId: string): void {
    this.sessionIds.delete(sessionId);
  }

  tryBeginBatch(): boolean {
    if (this.batchActive || this.sessionIds.size > 0) return false;
    this.batchActive = true;
    return true;
  }

  finishBatch(): void {
    this.batchActive = false;
  }

  snapshot(): ProviderSyncSnapshot {
    return {
      batchActive: this.batchActive,
      sessionIds: new Set(this.sessionIds),
    };
  }
}
