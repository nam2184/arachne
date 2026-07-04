import { invoke } from "@tauri-apps/api/core";
import { create } from "zustand";

type View = "canvas" | "settings";

export type NodeSkin = "default" | "minimal" | "tui";
export type WorkspaceMode = "canvas" | "agent";
export type CursorTheme = "react-flow" | "pixel-arrow" | "terminal" | "crosshair" | "blade";

export const NODE_SKINS: NodeSkin[] = ["default", "minimal", "tui"];
export const WORKSPACE_MODES: WorkspaceMode[] = ["canvas", "agent"];
export const CURSOR_THEMES: Array<{ value: CursorTheme; label: string; description: string }> = [
  { value: "react-flow", label: "React Flow", description: "Native grab/pointer cursors" },
  { value: "pixel-arrow", label: "Pixel Arrow", description: "Blocky white arrow with green accent" },
  { value: "terminal", label: "Terminal", description: "TUI-style square pointer" },
  { value: "crosshair", label: "Crosshair", description: "Precise canvas cursor with center dot" },
  { value: "blade", label: "Blade", description: "Thin angular pointer with sharp edge" },
];
export const CODE_BLOCK_THEMES = [
  { value: "github", label: "GitHub", light: "github-light", dark: "github-dark" },
  { value: "tokyo-night", label: "Tokyo Night", light: "github-light", dark: "tokyo-night" },
  { value: "catppuccin", label: "Catppuccin", light: "catppuccin-latte", dark: "catppuccin-mocha" },
  { value: "monokai", label: "Monokai", light: "github-light", dark: "monokai" },
  { value: "nord", label: "Nord", light: "github-light", dark: "nord" },
  { value: "everforest", label: "Everforest", light: "everforest-light", dark: "everforest-dark" },
  { value: "kanagawa", label: "Kanagawa", light: "kanagawa-lotus", dark: "kanagawa-wave" },
  { value: "rose-pine", label: "Rose Pine", light: "rose-pine-dawn", dark: "rose-pine" },
  { value: "solarized", label: "Solarized", light: "solarized-light", dark: "solarized-dark" },
] as const;

export type CodeBlockTheme = (typeof CODE_BLOCK_THEMES)[number]["value"];

export interface AppSettings {
  theme: "dark" | "light";
  editor_font_size: number;
  editor_tab_size: number;
  node_skin: NodeSkin;
  workspace_mode: WorkspaceMode;
  code_block_theme: CodeBlockTheme;
  cursor_theme: CursorTheme;
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
  saveWorkspaceMode: (mode: WorkspaceMode) => Promise<void>;
  saveCodeBlockTheme: (theme: CodeBlockTheme) => Promise<void>;
  saveCursorTheme: (theme: CursorTheme) => Promise<void>;
  saveWebSearchSettings: (baseUrl: string | null, maxResults: number) => Promise<void>;
}

function applyTheme(theme: "dark" | "light") {
  if (theme === "light") {
    document.documentElement.classList.add("light");
  } else {
    document.documentElement.classList.remove("light");
  }
}

function applyCursorTheme(theme: CursorTheme) {
  for (const item of CURSOR_THEMES) {
    document.documentElement.classList.toggle(`cursor-${item.value}`, item.value === theme);
  }
}

const DEFAULT_SETTINGS: AppSettings = {
  theme: "dark",
  editor_font_size: 14,
  editor_tab_size: 2,
  node_skin: "default",
  workspace_mode: "canvas",
  code_block_theme: "github",
  cursor_theme: "react-flow",
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
    workspace_mode: WORKSPACE_MODES.includes(settings?.workspace_mode as WorkspaceMode)
      ? (settings!.workspace_mode as WorkspaceMode)
      : "canvas",
    code_block_theme: normalizeCodeBlockTheme(settings?.code_block_theme),
    cursor_theme: normalizeCursorTheme(settings?.cursor_theme),
    searxng_base_url: settings?.searxng_base_url?.trim() || null,
    websearch_max_results: clampWebSearchMax(settings?.websearch_max_results),
  };
}

function isCursorTheme(value: unknown): value is CursorTheme {
  return CURSOR_THEMES.some((theme) => theme.value === value);
}

function normalizeCursorTheme(value: unknown): CursorTheme {
  return isCursorTheme(value) ? value : "react-flow";
}

function isCodeBlockTheme(value: unknown): value is CodeBlockTheme {
  return CODE_BLOCK_THEMES.some((theme) => theme.value === value);
}

function normalizeCodeBlockTheme(value: unknown): CodeBlockTheme {
  if (isCodeBlockTheme(value)) return value;
  if (value === "github-dark" || value === "github-light") return "github";
  if (value === "catppuccin-mocha" || value === "catppuccin-latte") return "catppuccin";
  if (value === "everforest-dark" || value === "everforest-light") return "everforest";
  if (value === "kanagawa-wave") return "kanagawa";
  if (value === "solarized-dark" || value === "solarized-light") return "solarized";
  return "github";
}

export function getCodeBlockThemePair(theme: CodeBlockTheme) {
  const preset = CODE_BLOCK_THEMES.find((item) => item.value === theme) ?? CODE_BLOCK_THEMES[0];
  return { light: preset.light, dark: preset.dark };
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
      applyCursorTheme(settings.cursor_theme);
    } catch {
      set({ settings: DEFAULT_SETTINGS });
      applyTheme("dark");
      applyCursorTheme(DEFAULT_SETTINGS.cursor_theme);
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

  saveWorkspaceMode: async (workspace_mode) => {
    const next = { ...get().settings, workspace_mode };
    set({ settings: next });
    await invoke("save_settings", { settings: next });
  },

  saveCodeBlockTheme: async (code_block_theme) => {
    const next = { ...get().settings, code_block_theme };
    set({ settings: next });
    await invoke("save_settings", { settings: next });
  },

  saveCursorTheme: async (cursor_theme) => {
    const next = { ...get().settings, cursor_theme };
    set({ settings: next });
    applyCursorTheme(cursor_theme);
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
