import { invoke } from "@tauri-apps/api/core";
import { GripHorizontal, Plus, Save, X } from "lucide-react";
import { useEffect, useRef, useState, type PointerEvent } from "react";
import { getDefaultModel, getModelOptions } from "@/features/sessions/providerModels";
import type { ProviderConfig } from "@/features/sessions/sessionStore";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

interface SessionConfigPanelProps {
  onClose: () => void;
}

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

export function SessionConfigPanel({ onClose }: SessionConfigPanelProps) {
  const [providers, setProviders] = useState<ProviderConfig[]>([]);
  const [selectedProviderName, setSelectedProviderName] = useState("");
  const [providerDraft, setProviderDraft] = useState<ProviderDraft>(emptyProviderDraft);
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [isSaving, setIsSaving] = useState(false);
  const [position, setPosition] = useState({ x: 380, y: 84 });
  const dragRef = useRef<{
    pointerId: number;
    startX: number;
    startY: number;
    originX: number;
    originY: number;
  } | null>(null);

  useEffect(() => {
    loadProviders().catch((loadError) => {
      setError(formatError(loadError));
    });
  }, []);

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

  function startDrag(event: PointerEvent<HTMLDivElement>) {
    dragRef.current = {
      pointerId: event.pointerId,
      startX: event.clientX,
      startY: event.clientY,
      originX: position.x,
      originY: position.y,
    };
    event.currentTarget.setPointerCapture(event.pointerId);
  }

  function drag(event: PointerEvent<HTMLDivElement>) {
    const activeDrag = dragRef.current;
    if (!activeDrag || activeDrag.pointerId !== event.pointerId) return;

    const maxX = Math.max(16, window.innerWidth - 440);
    const maxY = Math.max(16, window.innerHeight - 280);
    const nextX = Math.min(Math.max(16, activeDrag.originX + event.clientX - activeDrag.startX), maxX);
    const nextY = Math.min(Math.max(16, activeDrag.originY + event.clientY - activeDrag.startY), maxY);
    setPosition({ x: nextX, y: nextY });
  }

  function stopDrag(event: PointerEvent<HTMLDivElement>) {
    if (dragRef.current?.pointerId === event.pointerId) {
      dragRef.current = null;
    }
  }

  const modelOptions = getModelOptions(providerDraft.name, providerDraft.model);

  return (
    <div className="pointer-events-none fixed inset-0 z-[60]">
      <section
        role="dialog"
        aria-modal="false"
        aria-label="Provider settings"
        className="pointer-events-auto fixed flex w-[420px] max-w-[calc(100vw-32px)] flex-col overflow-hidden rounded-none border border-[#1f1f1f] bg-[#0a0a0a] text-white shadow-none"
        style={{ left: position.x, top: position.y }}
      >
        <div
          className="flex cursor-grab items-center justify-between border-b border-[#1f1f1f] px-4 py-3 active:cursor-grabbing"
          onPointerDown={startDrag}
          onPointerMove={drag}
          onPointerUp={stopDrag}
          onPointerCancel={stopDrag}
        >
          <div className="flex items-center gap-3">
            <GripHorizontal className="h-4 w-4 text-[#737373]" />
            <div>
              <h2 className="text-sm font-semibold text-white">Providers</h2>
              <p className="text-xs text-[#8a8a8a]">API keys and defaults</p>
            </div>
          </div>
          <Button variant="ghost" size="icon" onClick={onClose} aria-label="Close provider settings">
            <X className="h-4 w-4" />
          </Button>
        </div>

        <div className="space-y-4 p-4">
          <div className="flex gap-2">
            <select
              value={selectedProviderName}
              onChange={(event) => selectProvider(event.target.value)}
              className="h-9 min-w-0 flex-1 rounded-none border border-[#1f1f1f] bg-black px-3 text-sm text-white outline-none transition-colors hover:border-[#2a2a2a] focus:border-white"
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
            <span className="text-xs font-medium text-[#d4d4d4]">Name</span>
            <Input
              value={providerDraft.name}
              onChange={(event) => {
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
            <span className="text-xs font-medium text-[#d4d4d4]">Protocol</span>
            <select
              value={providerDraft.protocol}
              onChange={(event) => setProviderDraft((draft) => ({ ...draft, protocol: event.target.value as ProviderDraft["protocol"] }))}
              className="h-9 w-full rounded-none border border-[#1f1f1f] bg-black px-3 text-sm text-white outline-none transition-colors hover:border-[#2a2a2a] focus:border-white"
            >
              <option value="openai">OpenAI-compatible chat</option>
              <option value="anthropic">Anthropic messages</option>
            </select>
          </label>
          <label className="block space-y-1.5">
            <span className="text-xs font-medium text-[#d4d4d4]">Default Model</span>
            <select
              value={providerDraft.model}
              onChange={(event) => setProviderDraft((draft) => ({ ...draft, model: event.target.value }))}
              className="h-9 w-full rounded-none border border-[#1f1f1f] bg-black px-3 text-sm text-white outline-none transition-colors hover:border-[#2a2a2a] focus:border-white disabled:cursor-not-allowed disabled:opacity-50"
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
            <span className="text-xs font-medium text-[#d4d4d4]">API Key</span>
            <Input
              type="password"
              value={providerDraft.api_key}
              onChange={(event) => setProviderDraft((draft) => ({ ...draft, api_key: event.target.value }))}
              placeholder="Stored locally"
            />
          </label>
          <label className="block space-y-1.5">
            <span className="text-xs font-medium text-[#d4d4d4]">Base URL</span>
            <Input
              value={providerDraft.base_url}
              onChange={(event) => setProviderDraft((draft) => ({ ...draft, base_url: event.target.value }))}
              placeholder="Optional provider endpoint"
            />
          </label>
          <label className="flex items-center gap-2 text-sm text-[#d4d4d4]">
            <input
              type="checkbox"
              checked={providerDraft.enabled}
              onChange={(event) => setProviderDraft((draft) => ({ ...draft, enabled: event.target.checked }))}
              className="h-4 w-4 rounded border-[#2a2a2a] bg-black accent-white"
            />
            Enabled for new sessions
          </label>
          <Button className="w-full" onClick={saveProviderConfig} disabled={isSaving}>
            <Save className="h-4 w-4" />
            Save Provider
          </Button>

          {(status || error) && (
            <p className={cn("text-xs", error ? "text-[#ff5f5f]" : "text-[#bdbdbd]")}>{error ?? status}</p>
          )}
        </div>
      </section>
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
