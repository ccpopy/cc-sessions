import { create } from "zustand";
import { api, type Settings } from "@/lib/api";

type State = {
  settings: Settings | null;
  loading: boolean;
  error: string | null;
  load: () => Promise<void>;
  save: (patch: Partial<Settings>) => Promise<void>;
};

export const useSettings = create<State>((set, get) => ({
  settings: null,
  loading: false,
  error: null,
  async load() {
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
  },
  async save(patch) {
    const cur = get().settings;
    if (!cur) throw new Error("设置尚未加载，无法保存");
    const next = { ...cur, ...patch };
    await api.saveSettings(next);
    set({ settings: next });
  },
}));
