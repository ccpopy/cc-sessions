import { create } from "zustand";
import {
  DEFAULT_SIZE_SORT_DIRECTION,
  oppositeSortDirection,
  type SortDirection,
} from "@/lib/sessionSort";

export type View = "time" | "project" | "size";

// issue #10：记住上次选择的视图模式，下次启动直接以该视图打开
const VIEW_STORAGE_KEY = "cc-sessions:view";
const SIZE_SORT_DIRECTION_STORAGE_KEY = "cc-sessions:size-sort-direction";

function readStoredView(): View {
  if (typeof window === "undefined") return "time";
  try {
    const raw = window.localStorage.getItem(VIEW_STORAGE_KEY);
    if (raw === "time" || raw === "project" || raw === "size") return raw;
  } catch {
    /* localStorage 不可用时回退到 time */
  }
  return "time";
}

function readStoredSizeSortDirection(): SortDirection {
  if (typeof window === "undefined") return DEFAULT_SIZE_SORT_DIRECTION;
  try {
    const raw = window.localStorage.getItem(SIZE_SORT_DIRECTION_STORAGE_KEY);
    if (raw === "asc" || raw === "desc") return raw;
  } catch {
    /* localStorage 不可用时回退到降序 */
  }
  return DEFAULT_SIZE_SORT_DIRECTION;
}

type State = {
  view: View;
  sizeSortDirection: SortDirection;
  query: string;
  showSubagentSessions: boolean;
  showArchivedSessions: boolean;
  setView: (v: View) => void;
  setQuery: (q: string) => void;
  setShowSubagentSessions: (v: boolean) => void;
  setShowArchivedSessions: (v: boolean) => void;
  prefillCwd: string | null;
  setPrefillCwd: (cwd: string | null) => void;
  toggleSizeSortDirection: () => void;
};

export const useView = create<State>((set) => ({
  view: readStoredView(),
  sizeSortDirection: readStoredSizeSortDirection(),
  query: "",
  showSubagentSessions: false,
  showArchivedSessions: false,
  prefillCwd: null,
  setView: (v) => {
    try {
      window.localStorage.setItem(VIEW_STORAGE_KEY, v);
    } catch {
      /* ignore */
    }
    set({ view: v });
  },
  setQuery: (q) => set({ query: q }),
  setShowSubagentSessions: (v) => set({ showSubagentSessions: v }),
  setShowArchivedSessions: (v) => set({ showArchivedSessions: v }),
  setPrefillCwd: (cwd) => set({ prefillCwd: cwd }),
  toggleSizeSortDirection: () =>
    set((state) => {
      const next = oppositeSortDirection(state.sizeSortDirection);
      try {
        window.localStorage.setItem(SIZE_SORT_DIRECTION_STORAGE_KEY, next);
      } catch {
        /* ignore */
      }
      return { sizeSortDirection: next };
    }),
}));
