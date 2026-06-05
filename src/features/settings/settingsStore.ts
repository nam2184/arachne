import { create } from "zustand";

interface ProviderConfig {
  provider: string;
  model: string;
  apiKey?: string;
  baseUrl?: string;
}

interface SettingsState {
  providers: ProviderConfig[];
  activeProvider: string;
  addProvider: (config: ProviderConfig) => void;
  setActiveProvider: (name: string) => void;
  updateProvider: (name: string, config: Partial<ProviderConfig>) => void;
}

export const useSettingsStore = create<SettingsState>((set) => ({
  providers: [
    { provider: "anthropic", model: "claude-3-5-sonnet-20241022" },
    { provider: "openai", model: "gpt-4o" },
  ],
  activeProvider: "anthropic",
  addProvider: (config) =>
    set((state) => ({ providers: [...state.providers, config] })),
  setActiveProvider: (name) => set({ activeProvider: name }),
  updateProvider: (name, updates) =>
    set((state) => ({
      providers: state.providers.map((p) =>
        p.provider === name ? { ...p, ...updates } : p
      ),
    })),
}));