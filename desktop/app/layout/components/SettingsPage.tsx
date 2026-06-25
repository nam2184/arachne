import { invoke } from "@tauri-apps/api/core";
import { ArrowLeft, Moon, Sun, Plus, Save, Settings } from "lucide-react";
import { useEffect, useMemo, useState, type ChangeEvent } from "react";
import {
  getContextWindow,
  getDefaultModel,
  getMaxOutput,
  getModelOptions,
  getModelSpec,
} from "@/features/sessions/providerModels";
import type { ProviderConfig } from "@/features/sessions/sessionStore";
import { cn } from "@/lib/utils";
import { useAppStore, type NodeSkin } from "@/features/app/appStore";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

interface ProviderDraft {
  name: string;
  model: string;
  api_key: string;
  base_url: string;
  protocol: "openai" | "anthropic";
  enabled: boolean;
}

const emptyProviderDraft: ProviderDraft = {
  name: "",
  model: "",
  api_key: "",
  base_url: "",
  protocol: "openai",
  enabled: true,
};

export function SettingsPage() {
  const { settings, saveTheme, saveNodeSkin, saveWebSearchSettings, setView } = useAppStore();
  const [providers, setProviders] = useState<ProviderConfig[]>([]);
  const [selectedProviderName, setSelectedProviderName] = useState("");
  const [providerDraft, setProviderDraft] = useState<ProviderDraft>(emptyProviderDraft);
  const [searxngBaseUrl, setSearxngBaseUrl] = useState(settings.searxng_base_url ?? "");
  const [websearchMaxResults, setWebsearchMaxResults] = useState(settings.websearch_max_results);
  const [websearchStatus, setWebsearchStatus] = useState<string | null>(null);
  const [websearchError, setWebsearchError] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [isSaving, setIsSaving] = useState(false);
  const [isSavingWebSearch, setIsSavingWebSearch] = useState(false);

  useEffect(() => {
    loadProviders().catch((loadError) => {
      setError(formatError(loadError));
    });
  }, []);

  useEffect(() => {
    setSearxngBaseUrl(settings.searxng_base_url ?? "");
    setWebsearchMaxResults(settings.websearch_max_results);
  }, [settings.searxng_base_url, settings.websearch_max_results]);

  async function loadProviders() {
    const configs = await invoke<ProviderConfig[]>("get_provider_configs");
    setProviders(configs);

    const selected = configs.find((config) => config.name === selectedProviderName) ?? configs[0];
    if (selected) {
      setSelectedProviderName(selected.name);
      setProviderDraft(providerToDraft(selected));
    }
  }

  function selectProvider(name: string) {
    setSelectedProviderName(name);
    const provider = providers.find((config) => config.name === name);
    if (provider) {
      setProviderDraft(providerToDraft(provider));
    }
  }

  async function saveProviderConfig() {
    if (!providerDraft.name.trim() || !providerDraft.model.trim()) {
      setError("Provider name and default model are required.");
      return;
    }

    setIsSaving(true);
    setError(null);
    setStatus(null);

    try {
      const config: ProviderConfig = {
        name: providerDraft.name.trim(),
        model: providerDraft.model.trim(),
        api_key: providerDraft.api_key.trim() || null,
        base_url: providerDraft.base_url.trim() || null,
        protocol: providerDraft.protocol,
        enabled: providerDraft.enabled,
      };

      await invoke("upsert_provider_config", { config });
      setStatus("Provider config saved.");
      setSelectedProviderName(config.name);
      await loadProviders();
    } catch (saveError) {
      setError(formatError(saveError));
    } finally {
      setIsSaving(false);
    }
  }

  async function handleThemeToggle() {
    const newTheme = settings.theme === "dark" ? "light" : "dark";
    await saveTheme(newTheme);
  }

  async function saveWebSearchConfig() {
    const trimmedBaseUrl = searxngBaseUrl.trim();
    const maxResults = Math.min(20, Math.max(1, Math.trunc(Number(websearchMaxResults) || 5)));

    if (trimmedBaseUrl) {
      try {
        const parsed = new URL(trimmedBaseUrl);
        if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
          setWebsearchError("SearXNG base URL must use http or https.");
          return;
        }
      } catch {
        setWebsearchError("SearXNG base URL is not valid.");
        return;
      }
    }

    setIsSavingWebSearch(true);
    setWebsearchError(null);
    setWebsearchStatus(null);

    try {
      await saveWebSearchSettings(trimmedBaseUrl, maxResults);
      setWebsearchMaxResults(maxResults);
      setWebsearchStatus(trimmedBaseUrl ? "Web search config saved." : "Web search disabled until a SearXNG URL is set.");
    } catch (saveError) {
      setWebsearchError(formatError(saveError));
    } finally {
      setIsSavingWebSearch(false);
    }
  }

  const modelOptions = getModelOptions(providerDraft.name, providerDraft.model);
  const selectedSpec = useMemo(
    () => getModelSpec(providerDraft.model),
    [providerDraft.model]
  );
  const modelContextWindow = selectedSpec?.context_window ?? getContextWindow(providerDraft.model);
  const modelMaxOutput = selectedSpec?.max_output ?? getMaxOutput(providerDraft.model);

  return (
    <div className="flex h-full flex-col bg-[var(--background)] text-[var(--foreground)]">
      <header className="flex items-center gap-4 border-b border-[var(--border)] px-6 py-4">
        <Button variant="ghost" size="icon" onClick={() => setView("canvas")} aria-label="Back to canvas">
          <ArrowLeft className="h-5 w-5" />
        </Button>
        <div className="flex items-center gap-2">
          <Settings className="h-5 w-5" />
          <h1 className="text-lg font-semibold">Settings</h1>
        </div>
      </header>

      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto max-w-2xl space-y-8 p-6">
          <section className="space-y-4">
            <h2 className="text-sm font-medium text-[var(--text-secondary)]">Appearance</h2>
            <div className="rounded-none border border-[var(--border)] bg-[var(--surface-raised)] p-4">
              <div className="flex items-center justify-between">
                <div>
                  <p className="text-sm font-medium">Theme</p>
                  <p className="text-xs text-[var(--text-subtle)]">Choose your preferred color scheme</p>
                </div>
                <Button variant="secondary" size="icon" onClick={handleThemeToggle}>
                  {settings.theme === "dark" ? (
                    <Moon className="h-4 w-4" />
                  ) : (
                    <Sun className="h-4 w-4" />
                  )}
                </Button>
              </div>
            </div>
          </section>

          <section className="space-y-4">
            <h2 className="text-sm font-medium text-[var(--text-secondary)]">Node Style</h2>
            <p className="text-xs text-[var(--text-subtle)]">Choose how session nodes render on the canvas.</p>
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
              <NodeSkinCard
                skin="default"
                title="Default"
                description="Soft glow orb with radial gradient"
                active={settings.node_skin === "default"}
                onSelect={saveNodeSkin}
              />
              <NodeSkinCard
                skin="minimal"
                title="Minimal"
                description="Small dot, low visual weight"
                active={settings.node_skin === "minimal"}
                onSelect={saveNodeSkin}
              />
              <NodeSkinCard
                skin="tui"
                title="TUI"
                description="Bordered, monospace feel"
                active={settings.node_skin === "tui"}
                onSelect={saveNodeSkin}
              />
            </div>
          </section>

          <section className="space-y-4">
            <h2 className="text-sm font-medium text-[var(--text-secondary)]">Web Search</h2>
            <div className="space-y-4 rounded-none border border-[var(--border)] bg-[var(--surface-raised)] p-4">
              <div>
                <p className="text-sm font-medium">SearXNG JSON API</p>
                <p className="text-xs text-[var(--text-subtle)]">Used by the agent websearch tool. Leave blank to disable web search.</p>
              </div>
              <label className="block space-y-1.5">
                <span className="text-xs font-medium text-[var(--text-secondary)]">Base URL</span>
                <Input
                  value={searxngBaseUrl}
                  onChange={(event: ChangeEvent<HTMLInputElement>) => setSearxngBaseUrl(event.target.value)}
                  placeholder="https://search.example.com"
                />
              </label>
              <label className="block space-y-1.5">
                <span className="text-xs font-medium text-[var(--text-secondary)]">Max Results</span>
                <Input
                  type="number"
                  min={1}
                  max={20}
                  value={websearchMaxResults}
                  onChange={(event: ChangeEvent<HTMLInputElement>) => setWebsearchMaxResults(Number(event.target.value))}
                />
              </label>
              <Button className="w-full" onClick={saveWebSearchConfig} disabled={isSavingWebSearch}>
                <Save className="h-4 w-4" />
                Save Web Search
              </Button>
              {(websearchStatus || websearchError) && (
                <p className={cn("text-xs", websearchError ? "text-[#ff5f5f]" : "text-[var(--text-secondary)]")}>{websearchError ?? websearchStatus}</p>
              )}
            </div>
          </section>

          <section className="space-y-4">
            <h2 className="text-sm font-medium text-[var(--text-secondary)]">AI Configuration</h2>
            <div className="space-y-4 rounded-none border border-[var(--border)] bg-[var(--surface-raised)] p-4">
              <div className="flex gap-2">
                <select
                  value={selectedProviderName}
                  onChange={(event) => selectProvider(event.target.value)}
                  className="h-9 min-w-0 flex-1 rounded-none border border-[var(--input-border)] bg-[var(--input-bg)] px-3 text-sm text-[var(--foreground)] outline-none transition-colors hover:border-[var(--node-border-hover)] focus:border-[var(--foreground)]"
                >
                  {providers.map((provider) => (
                    <option key={provider.name} value={provider.name}>{provider.name}</option>
                  ))}
                </select>
                <Button
                  variant="secondary"
                  size="icon"
                  onClick={() => {
                    setSelectedProviderName("");
                    setProviderDraft(emptyProviderDraft);
                    setStatus(null);
                    setError(null);
                  }}
                  aria-label="New provider"
                >
                  <Plus className="h-4 w-4" />
                </Button>
              </div>

              <label className="block space-y-1.5">
                <span className="text-xs font-medium text-[var(--text-secondary)]">Name</span>
                <Input
                  value={providerDraft.name}
                  onChange={(event: ChangeEvent<HTMLInputElement>) => {
                    const name = event.target.value;
                    setProviderDraft((draft) => ({
                      ...draft,
                      name,
                      model: draft.model || getDefaultModel(name),
                      protocol: inferProtocol(name),
                    }));
                  }}
                  placeholder="anthropic"
                />
              </label>
              <label className="block space-y-1.5">
                <span className="text-xs font-medium text-[var(--text-secondary)]">Protocol</span>
                <select
                  value={providerDraft.protocol}
                  onChange={(event: ChangeEvent<HTMLSelectElement>) => setProviderDraft((draft) => ({ ...draft, protocol: event.target.value as ProviderDraft["protocol"] }))}
                  className="h-9 w-full rounded-none border border-[var(--input-border)] bg-[var(--input-bg)] px-3 text-sm text-[var(--foreground)] outline-none transition-colors hover:border-[var(--node-border-hover)] focus:border-[var(--foreground)]"
                >
                  <option value="openai">OpenAI-compatible chat</option>
                  <option value="anthropic">Anthropic messages</option>
                </select>
              </label>
              <label className="block space-y-1.5">
                <span className="text-xs font-medium text-[var(--text-secondary)]">Default Model</span>
                <select
                  value={providerDraft.model}
                  onChange={(event: ChangeEvent<HTMLSelectElement>) => setProviderDraft((draft) => ({ ...draft, model: event.target.value }))}
                  className="h-9 w-full rounded-none border border-[var(--input-border)] bg-[var(--input-bg)] px-3 text-sm text-[var(--foreground)] outline-none transition-colors hover:border-[var(--node-border-hover)] focus:border-[var(--foreground)] disabled:cursor-not-allowed disabled:opacity-50"
                  disabled={modelOptions.length === 0}
                >
                  {modelOptions.length === 0 ? (
                    <option value="">Add models in src/config/provider-models.json</option>
                  ) : (
                    modelOptions.map((model) => (
                      <option key={model} value={model}>{model}</option>
                    ))
                  )}
                </select>
              </label>
              <label className="block space-y-1.5">
                <span className="text-xs font-medium text-[var(--text-secondary)]">API Key</span>
                <Input
                  type="password"
                  value={providerDraft.api_key}
                  onChange={(event: ChangeEvent<HTMLInputElement>) => setProviderDraft((draft) => ({ ...draft, api_key: event.target.value }))}
                  placeholder="Stored locally"
                />
              </label>
              <label className="block space-y-1.5">
                <span className="text-xs font-medium text-[var(--text-secondary)]">Base URL</span>
                <Input
                  value={providerDraft.base_url}
                  onChange={(event: ChangeEvent<HTMLInputElement>) => setProviderDraft((draft) => ({ ...draft, base_url: event.target.value }))}
                  placeholder="Optional provider endpoint"
                />
              </label>
              <div className="rounded-none border border-[var(--border)] bg-[var(--surface)] p-3 text-xs text-[var(--text-secondary)]">
                <p className="text-[10px] uppercase tracking-wider text-[var(--text-muted)]">Context budget (from model spec)</p>
                <p className="mt-1">
                  Window: <span className="text-[var(--foreground)]">{modelContextWindow.toLocaleString()}</span> tokens
                </p>
                <p>
                  Max output: <span className="text-[var(--foreground)]">{modelMaxOutput.toLocaleString()}</span> tokens
                </p>
                {!selectedSpec && providerDraft.model && (
                  <p className="mt-1 text-[#ff8a3d]">
                    Model not in registry — using fallback limits.
                  </p>
                )}
              </div>
              <label className="flex items-center gap-2 text-sm text-[var(--text-secondary)]">
                <input
                  type="checkbox"
                  checked={providerDraft.enabled}
                  onChange={(event) => setProviderDraft((draft) => ({ ...draft, enabled: event.target.checked }))}
                  className="h-4 w-4 rounded-none border-[var(--border)] bg-[var(--input-bg)] accent-[var(--foreground)]"
                />
                Enabled for new sessions
              </label>
              <Button className="w-full" onClick={saveProviderConfig} disabled={isSaving}>
                <Save className="h-4 w-4" />
                Save Provider
              </Button>

              {(status || error) && (
                <p className={cn("text-xs", error ? "text-[#ff5f5f]" : "text-[var(--text-secondary)]")}>{error ?? status}</p>
              )}
            </div>
          </section>
        </div>
      </div>
    </div>
  );
}

function providerToDraft(provider: ProviderConfig): ProviderDraft {
  return {
    name: provider.name,
    model: provider.model,
    api_key: provider.api_key ?? "",
    base_url: provider.base_url ?? "",
    protocol: provider.protocol ?? inferProtocol(provider.name),
    enabled: provider.enabled,
  };
}

function inferProtocol(name: string): ProviderDraft["protocol"] {
  return name.toLowerCase() === "anthropic" ? "anthropic" : "openai";
}

function formatError(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

interface NodeSkinCardProps {
  skin: NodeSkin;
  title: string;
  description: string;
  active: boolean;
  onSelect: (skin: NodeSkin) => Promise<void> | void;
}

function NodeSkinCard({ skin, title, active, onSelect }: NodeSkinCardProps) {
  return (
    <button
      type="button"
      onClick={() => onSelect(skin)}
      className={cn(
        "flex items-center justify-between rounded-none border bg-[var(--surface-raised)] p-4 text-left transition-colors",
        active ? "border-[var(--foreground)]" : "border-[var(--border)] hover:border-[var(--node-border-hover)]",
      )}
    >
      <span className="text-sm font-medium text-[var(--foreground)]">{title}</span>
      <span
        className={cn(
          "h-3 w-3 rounded-full border",
          active ? "border-[var(--foreground)] bg-[var(--foreground)]" : "border-[var(--node-border-hover)] bg-transparent",
        )}
        aria-hidden="true"
      />
    </button>
  );
}
