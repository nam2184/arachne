import { create } from "zustand";

interface EditorTab {
  id: string;
  path: string;
  name: string;
}

interface EditorState {
  tabs: EditorTab[];
  activeTabId: string | null;
  openFile: (path: string) => void;
  closeTab: (id: string) => void;
  setActiveTab: (id: string) => void;
}

export const useEditorStore = create<EditorState>((set) => ({
  tabs: [],
  activeTabId: null,
  openFile: (path) =>
    set((state) => {
      const existing = state.tabs.find((t) => t.path === path);
      if (existing) {
        return { activeTabId: existing.id };
      }
      const id = crypto.randomUUID();
      const name = path.split("/").pop() || path;
      return {
        tabs: [...state.tabs, { id, path, name }],
        activeTabId: id,
      };
    }),
  closeTab: (id) =>
    set((state) => {
      const newTabs = state.tabs.filter((t) => t.id !== id);
      let newActive = state.activeTabId;
      if (state.activeTabId === id) {
        const idx = state.tabs.findIndex((t) => t.id === id);
        newActive = newTabs[idx]?.id ?? newTabs[idx - 1]?.id ?? null;
      }
      return { tabs: newTabs, activeTabId: newActive };
    }),
  setActiveTab: (id) => set({ activeTabId: id }),
}));