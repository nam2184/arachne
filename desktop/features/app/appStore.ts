import { invoke } from "@tauri-apps/api/core";
import { create } from "zustand";

type View = "canvas" | "settings";

export type NodeSkin = "default" | "minimal" | "tui";

export const NODE_SKINS: NodeSkin[] = ["default", "minimal", "tui"];

export interface AppSettings {
  theme: "dark" | "light";
  editor_font_size: number;
  editor_tab_size: number;
  node_skin: NodeSkin;
  searxng_base_url: string | null;
  websearch_max_results: number;
}

interface AppState {
  view: View;
  settings: AppSettings;
  setView: (view: View) => void;
  loadSettings: () => Promise<void>;
  saveTheme: (theme: "dark" | "light") => Promise<void>;
  saveNodeSkin: (skin: NodeSkin) => Promise<void>;
  saveWebSearchSettings: (baseUrl: string | null, maxResults: number) => Promise<void>;
}

function applyTheme(theme: "dark" | "light") {
  if (theme === "light") {
    document.documentElement.classList.add("light");
  } else {
    document.documentElement.classList.remove("light");
  }
}

const DEFAULT_SETTINGS: AppSettings = {
  theme: "dark",
  editor_font_size: 14,
  editor_tab_size: 2,
  node_skin: "default",
  searxng_base_url: null,
  websearch_max_results: 5,
};

function normalize(settings: Partial<AppSettings> | null | undefined): AppSettings {
  return {
    ...DEFAULT_SETTINGS,
    ...(settings ?? {}),
    theme: settings?.theme === "light" ? "light" : "dark",
    node_skin: NODE_SKINS.includes(settings?.node_skin as NodeSkin)
      ? (settings!.node_skin as NodeSkin)
      : "default",
    searxng_base_url: settings?.searxng_base_url?.trim() || null,
    websearch_max_results: clampWebSearchMax(settings?.websearch_max_results),
  };
}

function clampWebSearchMax(value: unknown): number {
  const parsed = typeof value === "number" ? value : Number(value);
  if (!Number.isFinite(parsed)) return DEFAULT_SETTINGS.websearch_max_results;
  return Math.min(20, Math.max(1, Math.trunc(parsed)));
}

export const useAppStore = create<AppState>((set, get) => ({
  view: "canvas",
  settings: DEFAULT_SETTINGS,

  setView: (view) => set({ view }),

  loadSettings: async () => {
    try {
      const raw = await invoke<AppSettings>("get_settings");
      const settings = normalize(raw);
      set({ settings });
      applyTheme(settings.theme);
    } catch {
      set({ settings: DEFAULT_SETTINGS });
      applyTheme("dark");
    }
  },

  saveTheme: async (theme) => {
    const next = { ...get().settings, theme };
    set({ settings: next });
    applyTheme(theme);
    await invoke("save_settings", { settings: next });
  },

  saveNodeSkin: async (node_skin) => {
    const next = { ...get().settings, node_skin };
    set({ settings: next });
    await invoke("save_settings", { settings: next });
  },

  saveWebSearchSettings: async (baseUrl, maxResults) => {
    const next = {
      ...get().settings,
      searxng_base_url: baseUrl === null ? null : baseUrl.trim(),
      websearch_max_results: clampWebSearchMax(maxResults),
    };
    set({ settings: next });
    await invoke("save_settings", { settings: next });
  },
}));
