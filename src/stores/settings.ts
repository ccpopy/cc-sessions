import { create } from "zustand";
import { api, type Settings } from "@/lib/api";

type State = {
  settings: Settings | null;
  loading: boolean;
  error: string | null;
  load: () => Promise<void>;
  save: (patch: Partial<Settings>) => Promise<void>;
};

// One queue for every component and page using this store, including reloads.
let pending: Promise<void> = Promise.resolve();
function enqueue(operation: () => Promise<void>): Promise<void> {
  const result = pending.then(operation);
  pending = result.catch(() => {});
  return result;
}

export const useSettings = create<State>((set, get) => ({
  settings: null,
  loading: false,
  error: null,
  load: () => enqueue(async () => {
    set({ loading: true, error: null });
    try {
      const s = await api.getSettings();
      set({ settings: s, error: null });
    } catch (error) {
      set({ error: String((error as Error)?.message ?? error) });
      throw error;
    } finally {
      set({ loading: false });
    }
  }),
  save(patch) {
    const queuedPatch = { ...patch };
    return enqueue(async () => {
      const cur = get().settings;
      if (!cur) throw new Error("设置尚未加载，无法保存");
      const next = { ...cur, ...queuedPatch };
      await api.saveSettings(next);
      set({ settings: next });
    });
  },
}));
