import { create } from "zustand";

type State = {
  dirty: boolean;
  setDirty: (dirty: boolean) => void;
};

export const useMemoryDraft = create<State>((set) => ({
  dirty: false,
  setDirty: (dirty) => set({ dirty }),
}));
