import { invoke } from "@tauri-apps/api/core";
import { Moon, Sun, Plus, Save, Settings, X } from "lucide-react";
import { useEffect, useMemo, useState, type ChangeEvent, type FormEvent } from "react";
import {
  getContextWindow,
  getDefaultModel,
  getMaxOutput,
  getModelOptions,
  getModelSpec,
} from "@/features/sessions/providerModels";
import type { ProviderAuthState, ProviderConfig, ProviderOAuthAuthorization, ProviderOAuthProfile } from "@/features/sessions/sessionStore";
import { cn } from "@/lib/utils";
import { CODE_BLOCK_THEMES, CURSOR_THEMES, useAppStore, type CodeBlockTheme, type CursorTheme, type McpServerConfig, type McpTransport, type NodeSkin, type WorkspaceMode } from "@/features/app/appStore";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { PromptCard, PromptStack } from "@/components/ui/prompt-card";

interface ProviderDraft {
  name: string;
  model: string;
  field_type: "API_KEY" | "OAUTH";
  api_key: string;
  base_url: string;
  protocol: "openai" | "anthropic";
  enabled: boolean;
}

interface McpServerDraft {
  originalName: string | null;
  name: string;
  enabled: boolean;
  transport: McpTransport;
  command: string;
  argsText: string;
  cwd: string;
  envText: string;
  url: string;
  headersText: string;
}

type OAuthProfileNamePrompt =
  | { kind: "connect"; value: string }
  | { kind: "rename"; profile: ProviderOAuthProfile; value: string };

const emptyProviderDraft: ProviderDraft = {
  name: "",
  model: "",
  field_type: "API_KEY",
  api_key: "",
  base_url: "",
  protocol: "openai",
  enabled: true,
};

const emptyMcpServerDraft: McpServerDraft = {
  originalName: null,
  name: "",
  enabled: true,
  transport: "stdio",
  command: "",
  argsText: "",
  cwd: "",
  envText: "",
  url: "",
  headersText: "",
};

const MCP_TRANSPORTS: Array<{ value: McpTransport; label: string; description: string }> = [
  { value: "stdio", label: "stdio", description: "Start a local command and speak MCP over stdin/stdout." },
  { value: "streamable_http", label: "Streamable HTTP", description: "Use the MCP Streamable HTTP endpoint directly." },
  { value: "sse", label: "SSE", description: "Use the legacy MCP SSE transport with server-provided POST endpoint." },
  { value: "polling_http", label: "Polling HTTP", description: "Use plain non-streaming JSON-RPC over HTTP POST." },
];

type SettingsTab = "appearance" | "workspace" | "nodes" | "mcp" | "providers";

const SETTINGS_TABS: Array<{ id: SettingsTab; label: string; description: string }> = [
  { id: "appearance", label: "Appearance", description: "Theme, code, cursor" },
  { id: "workspace", label: "Workspace", description: "Canvas or agent mode" },
  { id: "nodes", label: "Node Style", description: "Canvas node visuals" },
  { id: "mcp", label: "MCP", description: "Runtime tool servers" },
  { id: "providers", label: "Providers", description: "Models and auth" },
];

interface SettingsPageProps {
  open: boolean;
  onClose: () => void;
}

export function SettingsPage({ open, onClose }: SettingsPageProps) {
  const { settings, saveTheme, saveNodeSkin, saveWorkspaceMode, saveCodeBlockTheme, saveCursorTheme, saveMcpServers } = useAppStore();
  const [activeTab, setActiveTab] = useState<SettingsTab>("appearance");
  const [providers, setProviders] = useState<ProviderConfig[]>([]);
  const [providerAuthStates, setProviderAuthStates] = useState<Map<string, ProviderAuthState>>(() => new Map());
  const [providerOAuthProfiles, setProviderOAuthProfiles] = useState<Map<string, ProviderOAuthProfile[]>>(() => new Map());
  const [selectedProviderName, setSelectedProviderName] = useState("");
  const [providerDraft, setProviderDraft] = useState<ProviderDraft>(emptyProviderDraft);
  const [selectedMcpServerName, setSelectedMcpServerName] = useState("");
  const [mcpDraft, setMcpDraft] = useState<McpServerDraft>(emptyMcpServerDraft);
  const [mcpStatus, setMcpStatus] = useState<string | null>(null);
  const [mcpError, setMcpError] = useState<string | null>(null);
  const [isSavingMcp, setIsSavingMcp] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [isSaving, setIsSaving] = useState(false);
  const [isConnectingOAuth, setIsConnectingOAuth] = useState(false);
  const [oauthAuthorizationUrl, setOauthAuthorizationUrl] = useState<string | null>(null);
  const [oauthProfileNamePrompt, setOauthProfileNamePrompt] = useState<OAuthProfileNamePrompt | null>(null);

  useEffect(() => {
    if (!open) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  useEffect(() => {
    loadProviders().catch((loadError) => {
      setError(formatError(loadError));
    });
  }, []);

  useEffect(() => {
    const serverNames = Object.keys(settings.mcp_servers).sort();
    const selected = selectedMcpServerName && settings.mcp_servers[selectedMcpServerName]
      ? selectedMcpServerName
      : serverNames[0] ?? "";

    setSelectedMcpServerName(selected);
    if (selected) {
      setMcpDraft(mcpServerToDraft(selected, settings.mcp_servers[selected]));
    } else {
      setMcpDraft(emptyMcpServerDraft);
    }
  }, [settings.mcp_servers]);

  async function loadProviders() {
    const [configs, authStates] = await Promise.all([
      invoke<ProviderConfig[]>("get_provider_configs"),
      invoke<ProviderAuthState[]>("get_provider_auth_states"),
    ]);
    const authByProvider = new Map(authStates.map((auth) => [auth.provider_name, auth]));
    const oauthProfiles = await Promise.all(
      configs.map(async (config) => [
        config.name,
        await invoke<ProviderOAuthProfile[]>("list_provider_oauth_profiles", { providerName: config.name }),
      ] as const),
    );
    setProviders(configs);
    setProviderAuthStates(authByProvider);
    setProviderOAuthProfiles(new Map(oauthProfiles));

    const selected = configs.find((config) => config.name === selectedProviderName) ?? configs[0];
    if (selected) {
      setSelectedProviderName(selected.name);
      setProviderDraft(providerToDraft(selected, authByProvider.get(selected.name)));
    }
  }

  function selectProvider(name: string) {
    setSelectedProviderName(name);
    const provider = providers.find((config) => config.name === name);
    if (provider) {
      setProviderDraft(providerToDraft(provider, providerAuthStates.get(provider.name)));
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
      const providerName = providerDraft.name.trim();
      const config: ProviderConfig = {
        name: providerName,
        model: providerDraft.model.trim(),
        api_key: selectedAuthValue(providerDraft),
        base_url: providerDraft.base_url.trim() || null,
        protocol: providerDraft.protocol,
        enabled: providerDraft.enabled,
      };

      await invoke("upsert_provider_config", { config });
      await invoke("update_provider_auth_settings", {
        providerName,
        fieldType: providerDraft.field_type,
        apiKey: providerDraft.api_key.trim() || null,
      });
      setStatus("Provider config saved.");
      setSelectedProviderName(config.name);
      await loadProviders();
    } catch (saveError) {
      setError(formatError(saveError));
    } finally {
      setIsSaving(false);
    }
  }

  async function connectOpenAiOAuth() {
    const providerName = providerDraft.name.trim();
    if (providerName.toLowerCase() !== "openai") {
      setError("OAuth browser sign-in is currently available for OpenAI only.");
      return;
    }

    setOauthProfileNamePrompt({ kind: "connect", value: "Default" });
  }

  async function completeOpenAiOAuth(profileLabel: string) {
    const providerName = providerDraft.name.trim();

    setIsConnectingOAuth(true);
    setError(null);
    setStatus(null);
    setOauthAuthorizationUrl(null);

    try {
      const authorization = await invoke<ProviderOAuthAuthorization>("start_provider_oauth", { providerName });
      setOauthAuthorizationUrl(authorization.authorization_url);
      setStatus("Open the OAuth URL in your external browser. Waiting for the browser callback...");
      const profile = await invoke<ProviderOAuthProfile>("complete_provider_oauth", { providerName, profileLabel });
      setProviderDraft((draft) => ({
        ...draft,
        field_type: "OAUTH",
      }));
      setOauthAuthorizationUrl(null);
      setStatus(`OpenAI OAuth connected as ${profile.label}.`);
      await loadProviders();
    } catch (oauthError) {
      setError(formatError(oauthError));
    } finally {
      setIsConnectingOAuth(false);
    }
  }

  async function copyOAuthUrl() {
    if (!oauthAuthorizationUrl) return;
    await navigator.clipboard.writeText(oauthAuthorizationUrl);
    setStatus("OAuth URL copied. Paste it into your external browser.");
  }

  async function activateOAuthProfile(profileId: string) {
    const providerName = providerDraft.name.trim();
    setError(null);
    setStatus(null);
    try {
      const profile = await invoke<ProviderOAuthProfile>("set_active_provider_oauth_profile", { providerName, profileId });
      setStatus(`Switched OpenAI OAuth account to ${profile.label}.`);
      await loadProviders();
    } catch (switchError) {
      setError(formatError(switchError));
    }
  }

  async function renameOAuthProfile(profile: ProviderOAuthProfile) {
    setOauthProfileNamePrompt({ kind: "rename", profile, value: profile.label });
  }

  async function renameOAuthProfileTo(profile: ProviderOAuthProfile, label: string) {
    if (!label || label === profile.label) return;
    setError(null);
    setStatus(null);
    try {
      await invoke("rename_provider_oauth_profile", { profileId: profile.id, label });
      setStatus(`Renamed OAuth account to ${label}.`);
      await loadProviders();
    } catch (renameError) {
      setError(formatError(renameError));
    }
  }

  async function submitOAuthProfileName(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!oauthProfileNamePrompt) return;
    const label = oauthProfileNamePrompt.value.trim() || "Default";
    const prompt = oauthProfileNamePrompt;
    setOauthProfileNamePrompt(null);
    if (prompt.kind === "connect") {
      await completeOpenAiOAuth(label);
    } else {
      await renameOAuthProfileTo(prompt.profile, label);
    }
  }

  async function deleteOAuthProfile(profile: ProviderOAuthProfile) {
    if (profile.is_active) {
      setError("Switch to a different OAuth account before deleting the active one.");
      return;
    }
    if (!window.confirm(`Delete OAuth account ${profile.label}?`)) return;
    setError(null);
    setStatus(null);
    try {
      await invoke("delete_provider_oauth_profile", { profileId: profile.id });
      setStatus(`Deleted OAuth account ${profile.label}.`);
      await loadProviders();
    } catch (deleteError) {
      setError(formatError(deleteError));
    }
  }

  async function handleThemeToggle() {
    const newTheme = settings.theme === "dark" ? "light" : "dark";
    await saveTheme(newTheme);
  }

  function selectMcpServer(name: string) {
    setSelectedMcpServerName(name);
    const server = settings.mcp_servers[name];
    setMcpDraft(server ? mcpServerToDraft(name, server) : emptyMcpServerDraft);
    setMcpStatus(null);
    setMcpError(null);
  }

  async function saveMcpConfig() {
    const name = mcpDraft.name.trim();
    if (!name) {
      setMcpError("MCP server name is required.");
      return;
    }
    if (mcpDraft.transport === "stdio" && !mcpDraft.command.trim()) {
      setMcpError("MCP stdio server command is required.");
      return;
    }
    if (mcpDraft.transport !== "stdio") {
      const url = mcpDraft.url.trim();
      if (!url) {
        setMcpError("MCP URL is required for HTTP/SSE transports.");
        return;
      }
      try {
        const parsed = new URL(url);
        if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
          setMcpError("MCP URL must use http or https.");
          return;
        }
      } catch {
        setMcpError("MCP URL is not valid.");
        return;
      }
    }

    setIsSavingMcp(true);
    setMcpStatus(null);
    setMcpError(null);

    try {
      const next = { ...settings.mcp_servers };
      if (mcpDraft.originalName && mcpDraft.originalName !== name) {
        delete next[mcpDraft.originalName];
      }
      next[name] = draftToMcpServer(mcpDraft);
      await saveMcpServers(next);
      setSelectedMcpServerName(name);
      setMcpDraft((draft) => ({ ...draft, originalName: name }));
      setMcpStatus("MCP server saved. New sessions will discover its tools from runtime config.");
    } catch (saveError) {
      setMcpError(formatError(saveError));
    } finally {
      setIsSavingMcp(false);
    }
  }

  async function deleteMcpConfig() {
    const name = mcpDraft.originalName;
    if (!name) {
      setMcpDraft(emptyMcpServerDraft);
      setSelectedMcpServerName("");
      return;
    }

    setIsSavingMcp(true);
    setMcpStatus(null);
    setMcpError(null);

    try {
      const next = { ...settings.mcp_servers };
      delete next[name];
      await saveMcpServers(next);
      setMcpDraft(emptyMcpServerDraft);
      setSelectedMcpServerName("");
      setMcpStatus("MCP server removed.");
    } catch (deleteError) {
      setMcpError(formatError(deleteError));
    } finally {
      setIsSavingMcp(false);
    }
  }

  const modelOptions = getModelOptions(providerDraft.name, providerDraft.model);
  const selectedSpec = useMemo(
    () => getModelSpec(providerDraft.model),
    [providerDraft.model]
  );
  const modelContextWindow = selectedSpec?.context_window ?? getContextWindow(providerDraft.model);
  const modelMaxOutput = selectedSpec?.max_output ?? getMaxOutput(providerDraft.model);
  const selectedAuthState = providerAuthStates.get(providerDraft.name.trim());
  const selectedOAuthProfiles = providerOAuthProfiles.get(providerDraft.name.trim()) ?? [];
  const activeOAuthProfile = selectedOAuthProfiles.find((profile) => profile.is_active);
  const hasOAuthAccessToken = Boolean(selectedAuthState?.access_token);
  const hasOAuthRefreshToken = Boolean(selectedAuthState?.refresh_token);

  if (!open) return null;

  return (
    <>
    <div className="fixed inset-0 z-[100] flex items-center justify-center bg-black/55 p-3 text-[var(--foreground)] sm:p-5" role="dialog" aria-modal="true" aria-label="Settings">
      <div className="flex h-[min(760px,calc(100vh-1.5rem))] w-[min(1040px,calc(100vw-1.5rem))] flex-col overflow-hidden border border-[var(--border)] bg-[var(--background)] shadow-2xl sm:h-[min(760px,calc(100vh-2.5rem))] sm:w-[min(1040px,calc(100vw-2.5rem))]">
        <header className="flex items-center justify-between border-b border-[var(--border)] px-4 py-3 sm:px-5">
          <div className="flex min-w-0 items-center gap-3">
            <span className="flex h-8 w-8 shrink-0 items-center justify-center border border-[var(--border)] bg-[var(--surface-raised)]">
              <Settings className="h-4 w-4" />
            </span>
            <div className="min-w-0">
              <h1 className="text-sm font-semibold">Settings</h1>
              <p className="truncate text-xs text-[var(--text-subtle)]">Configure appearance, workspace behavior, runtime tools, and providers.</p>
            </div>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="flex h-8 w-8 shrink-0 items-center justify-center text-[var(--text-muted)] transition-colors hover:text-[var(--foreground)]"
            aria-label="Close settings"
          >
            <X className="h-4 w-4" />
          </button>
        </header>

        <div className="flex min-h-0 flex-1 flex-col sm:flex-row">
          <aside className="flex shrink-0 gap-1 overflow-x-auto border-b border-[var(--border)] bg-[var(--surface)] p-2 sm:w-56 sm:flex-col sm:overflow-x-visible sm:border-b-0 sm:border-r">
            {SETTINGS_TABS.map((tab) => (
              <button
                key={tab.id}
                type="button"
                onClick={() => setActiveTab(tab.id)}
                className={cn(
                  "min-w-36 rounded-none border px-3 py-2 text-left transition-colors sm:min-w-0",
                  activeTab === tab.id
                    ? "border-[var(--foreground)] bg-[var(--surface-raised)] text-[var(--foreground)]"
                    : "border-transparent text-[var(--text-secondary)] hover:border-[var(--border)] hover:bg-[var(--surface-raised)] hover:text-[var(--foreground)]",
                )}
              >
                <span className="block text-xs font-medium">{tab.label}</span>
                <span className="mt-0.5 hidden text-[10px] leading-snug text-[var(--text-subtle)] sm:block">{tab.description}</span>
              </button>
            ))}
          </aside>

          <div className="min-h-0 flex-1 overflow-y-auto">
            <div className="mx-auto max-w-3xl space-y-6 p-4 sm:p-6">
              {activeTab === "appearance" && (
          <section className="space-y-4">
            <h2 className="text-sm font-medium text-[var(--text-secondary)]">Appearance</h2>
            <div className="rounded-none border border-[var(--border)] bg-[var(--surface-raised)] p-4">
              <div className="flex items-center justify-between gap-4">
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
              <label className="mt-4 block space-y-1.5">
                <span className="text-xs font-medium text-[var(--text-secondary)]">Code block theme</span>
                <select
                  value={settings.code_block_theme}
                  onChange={(event: ChangeEvent<HTMLSelectElement>) => saveCodeBlockTheme(event.target.value as CodeBlockTheme)}
                  className="h-9 w-full rounded-none border border-[var(--input-border)] bg-[var(--input-bg)] px-3 text-sm text-[var(--foreground)] outline-none transition-colors hover:border-[var(--node-border-hover)] focus:border-[var(--foreground)]"
                >
                  {CODE_BLOCK_THEMES.map((theme) => (
                    <option key={theme.value} value={theme.value}>{theme.label}</option>
                  ))}
                </select>
              </label>
              <label className="mt-4 block space-y-1.5">
                <span className="text-xs font-medium text-[var(--text-secondary)]">Cursor design</span>
                <select
                  value={settings.cursor_theme}
                  onChange={(event: ChangeEvent<HTMLSelectElement>) => saveCursorTheme(event.target.value as CursorTheme)}
                  className="h-9 w-full rounded-none border border-[var(--input-border)] bg-[var(--input-bg)] px-3 text-sm text-[var(--foreground)] outline-none transition-colors hover:border-[var(--node-border-hover)] focus:border-[var(--foreground)]"
                >
                  {CURSOR_THEMES.map((theme) => (
                    <option key={theme.value} value={theme.value}>{theme.label}</option>
                  ))}
                </select>
              </label>
            </div>
          </section>
              )}

              {activeTab === "workspace" && (
          <section className="space-y-4">
            <h2 className="text-sm font-medium text-[var(--text-secondary)]">Workspace</h2>
            <p className="text-xs text-[var(--text-subtle)]">Choose between the node canvas and a normal docked agent chat.</p>
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <WorkspaceModeCard
                mode="canvas"
                title="Canvas"
                description="Node graph sessions with floating chats and loops"
                active={settings.workspace_mode === "canvas"}
                onSelect={saveWorkspaceMode}
              />
              <WorkspaceModeCard
                mode="agent"
                title="Agent"
                description="Sidebar sessions with full-height chat"
                active={settings.workspace_mode === "agent"}
                onSelect={saveWorkspaceMode}
              />
            </div>
          </section>
              )}

              {activeTab === "nodes" && (
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
              )}

              {activeTab === "mcp" && (
          <section className="space-y-4">
            <div>
              <h2 className="text-sm font-medium text-[var(--text-secondary)]">MCP Servers</h2>
              <p className="mt-1 text-xs text-[var(--text-subtle)]">Configure explicit MCP transports. URL transports may point at localhost or a remote host.</p>
            </div>
            <div className="grid grid-cols-1 gap-4 lg:grid-cols-[220px_minmax(0,1fr)]">
              <div className="space-y-2 rounded-none border border-[var(--border)] bg-[var(--surface-raised)] p-3">
                <Button
                  className="w-full"
                  variant="secondary"
                  onClick={() => {
                    setSelectedMcpServerName("");
                    setMcpDraft(emptyMcpServerDraft);
                    setMcpStatus(null);
                    setMcpError(null);
                  }}
                >
                  <Plus className="h-4 w-4" />
                  New MCP Server
                </Button>
                <div className="space-y-1">
                  {Object.keys(settings.mcp_servers).sort().length === 0 ? (
                    <p className="rounded-none border border-dashed border-[var(--border)] p-3 text-xs text-[var(--text-subtle)]">No MCP servers configured.</p>
                  ) : (
                    Object.keys(settings.mcp_servers).sort().map((name) => (
                      <button
                        key={name}
                        type="button"
                        onClick={() => selectMcpServer(name)}
                        className={cn(
                          "flex w-full items-center justify-between gap-2 rounded-none border px-3 py-2 text-left text-xs transition-colors",
                          selectedMcpServerName === name
                            ? "border-[var(--foreground)] bg-[var(--surface)] text-[var(--foreground)]"
                            : "border-transparent text-[var(--text-secondary)] hover:border-[var(--border)] hover:bg-[var(--surface)] hover:text-[var(--foreground)]",
                        )}
                      >
                        <span className="min-w-0 truncate">{name}</span>
                        <span className={cn("h-2 w-2 shrink-0 rounded-full", settings.mcp_servers[name]?.enabled === false ? "bg-[var(--text-muted)]" : "bg-[var(--node-focus)]")} />
                      </button>
                    ))
                  )}
                </div>
              </div>

              <div className="space-y-4 rounded-none border border-[var(--border)] bg-[var(--surface-raised)] p-4">
                <label className="block space-y-1.5">
                  <span className="text-xs font-medium text-[var(--text-secondary)]">Server name</span>
                  <Input
                    value={mcpDraft.name}
                    onChange={(event: ChangeEvent<HTMLInputElement>) => setMcpDraft((draft) => ({ ...draft, name: event.target.value }))}
                    placeholder="filesystem"
                  />
                </label>
                <label className="flex items-center gap-2 text-sm text-[var(--text-secondary)]">
                  <input
                    type="checkbox"
                    checked={mcpDraft.enabled}
                    onChange={(event) => setMcpDraft((draft) => ({ ...draft, enabled: event.target.checked }))}
                    className="h-4 w-4 rounded-none border-[var(--border)] bg-[var(--input-bg)] accent-[var(--foreground)]"
                  />
                  Enabled
                </label>
                <label className="block space-y-1.5">
                  <span className="text-xs font-medium text-[var(--text-secondary)]">Transport</span>
                  <select
                    value={mcpDraft.transport}
                    onChange={(event: ChangeEvent<HTMLSelectElement>) => setMcpDraft((draft) => ({ ...draft, transport: event.target.value as McpTransport }))}
                    className="h-9 w-full rounded-none border border-[var(--input-border)] bg-[var(--input-bg)] px-3 text-sm text-[var(--foreground)] outline-none transition-colors hover:border-[var(--node-border-hover)] focus:border-[var(--foreground)]"
                  >
                    {MCP_TRANSPORTS.map((transport) => (
                      <option key={transport.value} value={transport.value}>{transport.label}</option>
                    ))}
                  </select>
                  <span className="text-[10px] text-[var(--text-subtle)]">{MCP_TRANSPORTS.find((transport) => transport.value === mcpDraft.transport)?.description}</span>
                </label>
                {mcpDraft.transport === "stdio" ? (
                  <>
                <label className="block space-y-1.5">
                  <span className="text-xs font-medium text-[var(--text-secondary)]">Command</span>
                  <Input
                    value={mcpDraft.command}
                    onChange={(event: ChangeEvent<HTMLInputElement>) => setMcpDraft((draft) => ({ ...draft, command: event.target.value }))}
                    placeholder="npx"
                  />
                </label>
                <label className="block space-y-1.5">
                  <span className="text-xs font-medium text-[var(--text-secondary)]">Arguments</span>
                  <textarea
                    value={mcpDraft.argsText}
                    onChange={(event: ChangeEvent<HTMLTextAreaElement>) => setMcpDraft((draft) => ({ ...draft, argsText: event.target.value }))}
                    placeholder={"-y\n@modelcontextprotocol/server-filesystem\n/path/to/project"}
                    className="min-h-24 w-full resize-y rounded-none border border-[var(--input-border)] bg-[var(--input-bg)] px-3 py-2 font-mono text-xs text-[var(--foreground)] outline-none transition-colors hover:border-[var(--node-border-hover)] focus:border-[var(--foreground)]"
                  />
                  <span className="text-[10px] text-[var(--text-subtle)]">One argument per line. Values are passed directly to the stdio process.</span>
                </label>
                <label className="block space-y-1.5">
                  <span className="text-xs font-medium text-[var(--text-secondary)]">Working directory</span>
                  <Input
                    value={mcpDraft.cwd}
                    onChange={(event: ChangeEvent<HTMLInputElement>) => setMcpDraft((draft) => ({ ...draft, cwd: event.target.value }))}
                    placeholder="Optional"
                  />
                </label>
                <label className="block space-y-1.5">
                  <span className="text-xs font-medium text-[var(--text-secondary)]">Environment</span>
                  <textarea
                    value={mcpDraft.envText}
                    onChange={(event: ChangeEvent<HTMLTextAreaElement>) => setMcpDraft((draft) => ({ ...draft, envText: event.target.value }))}
                    placeholder={"TOKEN=...\nDEBUG=1"}
                    className="min-h-20 w-full resize-y rounded-none border border-[var(--input-border)] bg-[var(--input-bg)] px-3 py-2 font-mono text-xs text-[var(--foreground)] outline-none transition-colors hover:border-[var(--node-border-hover)] focus:border-[var(--foreground)]"
                  />
                  <span className="text-[10px] text-[var(--text-subtle)]">One KEY=VALUE pair per line. Stored in local settings and injected by runtime.</span>
                </label>
                  </>
                ) : (
                  <>
                <label className="block space-y-1.5">
                  <span className="text-xs font-medium text-[var(--text-secondary)]">URL</span>
                  <Input
                    value={mcpDraft.url}
                    onChange={(event: ChangeEvent<HTMLInputElement>) => setMcpDraft((draft) => ({ ...draft, url: event.target.value }))}
                    placeholder="https://mcp.example.com/mcp"
                  />
                  <span className="text-[10px] text-[var(--text-subtle)]">Use the protocol-specific endpoint. Localhost URLs are valid.</span>
                </label>
                <label className="block space-y-1.5">
                  <span className="text-xs font-medium text-[var(--text-secondary)]">Headers</span>
                  <textarea
                    value={mcpDraft.headersText}
                    onChange={(event: ChangeEvent<HTMLTextAreaElement>) => setMcpDraft((draft) => ({ ...draft, headersText: event.target.value }))}
                    placeholder={"Authorization=Bearer ...\nX-Client=arachne"}
                    className="min-h-20 w-full resize-y rounded-none border border-[var(--input-border)] bg-[var(--input-bg)] px-3 py-2 font-mono text-xs text-[var(--foreground)] outline-none transition-colors hover:border-[var(--node-border-hover)] focus:border-[var(--foreground)]"
                  />
                  <span className="text-[10px] text-[var(--text-subtle)]">One HEADER=VALUE pair per line. Stored locally and sent by runtime.</span>
                </label>
                  </>
                )}
                <div className="rounded-none border border-[var(--border)] bg-[var(--surface)] p-3 text-xs text-[var(--text-secondary)]">
                  Tools from this server are advertised as <span className="font-mono text-[var(--foreground)]">mcp__{mcpDraft.name.trim() || "server"}__tool</span> after discovery.
                </div>
                <div className="flex flex-col gap-2 sm:flex-row">
                  <Button className="flex-1" onClick={saveMcpConfig} disabled={isSavingMcp}>
                    <Save className="h-4 w-4" />
                    Save MCP Server
                  </Button>
                  <Button className="flex-1" variant="secondary" onClick={deleteMcpConfig} disabled={isSavingMcp}>
                    Delete
                  </Button>
                </div>
                {(mcpStatus || mcpError) && (
                  <p className={cn("text-xs", mcpError ? "text-[#ff5f5f]" : "text-[var(--text-secondary)]")}>{mcpError ?? mcpStatus}</p>
                )}
              </div>
            </div>
          </section>
              )}

              {activeTab === "providers" && (
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
                    <option value="">Add models in config/provider-models.json</option>
                  ) : (
                    modelOptions.map((model) => (
                      <option key={model} value={model}>{model}</option>
                    ))
                  )}
                </select>
              </label>
              <label className="block space-y-1.5">
                <span className="text-xs font-medium text-[var(--text-secondary)]">Auth Type</span>
                <select
                  value={providerDraft.field_type}
                  onChange={(event: ChangeEvent<HTMLSelectElement>) => setProviderDraft((draft) => ({ ...draft, field_type: event.target.value as ProviderDraft["field_type"] }))}
                  className="h-9 w-full rounded-none border border-[var(--input-border)] bg-[var(--input-bg)] px-3 text-sm text-[var(--foreground)] outline-none transition-colors hover:border-[var(--node-border-hover)] focus:border-[var(--foreground)]"
                >
                  <option value="API_KEY">API key</option>
                  <option value="OAUTH">OAuth</option>
                </select>
              </label>
              {providerDraft.field_type === "API_KEY" ? (
                <label className="block space-y-1.5">
                  <span className="text-xs font-medium text-[var(--text-secondary)]">API Key</span>
                  <Input
                    type="password"
                    value={providerDraft.api_key}
                    onChange={(event: ChangeEvent<HTMLInputElement>) => setProviderDraft((draft) => ({ ...draft, api_key: event.target.value }))}
                    placeholder="Stored locally"
                  />
                </label>
              ) : (
                <div className="space-y-3">
                  {providerDraft.name.trim().toLowerCase() === "openai" && (
                    <div className="space-y-2 rounded-none border border-[var(--border)] bg-[var(--surface)] p-3">
                      <Button className="w-full" variant="secondary" onClick={connectOpenAiOAuth} disabled={isConnectingOAuth}>
                        {isConnectingOAuth ? "Waiting for OpenAI..." : "Connect OpenAI OAuth"}
                      </Button>
                      {oauthAuthorizationUrl && (
                        <div className="space-y-2 text-xs text-[var(--text-secondary)]">
                          <p>Copy this URL and open it in your external browser.</p>
                          <p className="break-all rounded-none border border-[var(--border)] bg-[var(--input-bg)] p-2 text-[var(--foreground)]">
                            {oauthAuthorizationUrl}
                          </p>
                          <Button className="w-full" variant="secondary" onClick={copyOAuthUrl}>
                            Copy OAuth URL
                          </Button>
                        </div>
                      )}
                    </div>
                  )}
                  <div className="rounded-none border border-[var(--border)] bg-[var(--surface)] p-3 text-xs text-[var(--text-secondary)]">
                    <p className="text-[10px] uppercase tracking-wider text-[var(--text-muted)]">OAuth accounts</p>
                    {selectedOAuthProfiles.length === 0 ? (
                      <p className="mt-2">No Codex/OpenAI OAuth accounts saved yet.</p>
                    ) : (
                      <div className="mt-2 space-y-2">
                        {selectedOAuthProfiles.map((profile) => (
                          <div key={profile.id} className="rounded-none border border-[var(--border)] bg-[var(--input-bg)] p-2">
                            <div className="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
                              <div className="min-w-0">
                                <p className="truncate text-sm font-medium text-[var(--foreground)]">
                                  {profile.label} {profile.is_active ? "(active)" : ""}
                                </p>
                                <p className="truncate text-[10px] text-[var(--text-subtle)]">
                                  Account: {profile.account_id || "unknown"}
                                </p>
                              </div>
                              <div className="flex shrink-0 gap-2">
                                {!profile.is_active && (
                                  <Button size="sm" variant="secondary" onClick={() => activateOAuthProfile(profile.id)}>
                                    Use
                                  </Button>
                                )}
                                <Button size="sm" variant="secondary" onClick={() => renameOAuthProfile(profile)}>
                                  Rename
                                </Button>
                                <Button size="sm" variant="secondary" onClick={() => deleteOAuthProfile(profile)} disabled={profile.is_active}>
                                  Delete
                                </Button>
                              </div>
                            </div>
                          </div>
                        ))}
                      </div>
                    )}
                    <div className="mt-3 border-t border-[var(--border)] pt-3">
                      <p>
                        Active profile: <span className="text-[var(--foreground)]">{activeOAuthProfile?.label ?? "None"}</span>
                      </p>
                      <p>
                        Access token: <span className="text-[var(--foreground)]">{hasOAuthAccessToken ? "Connected" : "Not connected"}</span>
                      </p>
                      <p>
                        Refresh token: <span className="text-[var(--foreground)]">{hasOAuthRefreshToken ? "Stored" : "Not stored"}</span>
                      </p>
                      <p className="mt-2">Tokens are stored per named account and cannot be edited here.</p>
                    </div>
                  </div>
                </div>
              )}
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
              )}
        </div>
      </div>
      </div>
    </div>
    </div>
      {oauthProfileNamePrompt && (
        <PromptStack className="z-[120]">
          <form className="w-full max-w-xl" onSubmit={submitOAuthProfileName}>
            <PromptCard
              className="max-w-none items-end"
              title={oauthProfileNamePrompt.kind === "connect" ? "Name this Codex/OpenAI account" : "Rename this Codex/OpenAI account"}
              detail={oauthProfileNamePrompt.kind === "connect" ? "openai: OAuth profile" : `openai: ${oauthProfileNamePrompt.profile.label}`}
              actions={
                <>
                  <Button size="sm" type="submit">
                    {oauthProfileNamePrompt.kind === "connect" ? "Continue" : "Rename"}
                  </Button>
                  <Button size="sm" type="button" variant="ghost" onClick={() => setOauthProfileNamePrompt(null)}>
                    Cancel
                  </Button>
                </>
              }
            >
              <Input
                autoFocus
                className="mt-2 border-[#2a2a2a] bg-[#0b0b0b] text-white focus-visible:ring-white"
                value={oauthProfileNamePrompt.value}
                onChange={(event) =>
                  setOauthProfileNamePrompt((prompt) =>
                    prompt ? { ...prompt, value: event.target.value } : prompt,
                  )
                }
              />
            </PromptCard>
          </form>
        </PromptStack>
      )}
    </>
  );
}

function providerToDraft(provider: ProviderConfig, auth?: ProviderAuthState): ProviderDraft {
  return {
    name: provider.name,
    model: provider.model,
    field_type: auth?.field_type ?? "API_KEY",
    api_key: auth?.api_key ?? provider.api_key ?? "",
    base_url: provider.base_url ?? "",
    protocol: provider.protocol ?? inferProtocol(provider.name),
    enabled: provider.enabled,
  };
}

function mcpServerToDraft(name: string, server: McpServerConfig): McpServerDraft {
  return {
    originalName: name,
    name,
    enabled: server.enabled !== false,
    transport: server.transport,
    command: server.command ?? "",
    argsText: server.args.join("\n"),
    cwd: server.cwd ?? "",
    envText: serializeStringRecord(server.env),
    url: server.url ?? "",
    headersText: serializeStringRecord(server.headers),
  };
}

function draftToMcpServer(draft: McpServerDraft): McpServerConfig {
  return {
    enabled: draft.enabled,
    transport: draft.transport,
    command: draft.transport === "stdio" ? draft.command.trim() || null : null,
    args: draft.transport === "stdio" ? splitLines(draft.argsText) : [],
    env: draft.transport === "stdio" ? parseStringRecord(draft.envText) : {},
    cwd: draft.transport === "stdio" ? draft.cwd.trim() || null : null,
    url: draft.transport === "stdio" ? null : draft.url.trim() || null,
    headers: draft.transport === "stdio" ? {} : parseStringRecord(draft.headersText),
  };
}

function splitLines(value: string): string[] {
  return value
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean);
}

function parseStringRecord(value: string): Record<string, string> {
  const record: Record<string, string> = {};
  for (const line of value.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const separator = trimmed.indexOf("=");
    if (separator <= 0) continue;
    const key = trimmed.slice(0, separator).trim();
    const itemValue = trimmed.slice(separator + 1).trim();
    if (key) record[key] = itemValue;
  }
  return record;
}

function serializeStringRecord(record: Record<string, string>) {
  return Object.entries(record)
    .map(([key, value]) => `${key}=${value}`)
    .join("\n");
}

function selectedAuthValue(provider: ProviderDraft) {
  return provider.field_type === "API_KEY"
    ? provider.api_key.trim() || null
    : null;
}

function inferProtocol(name: string): ProviderDraft["protocol"] {
  return name.toLowerCase() === "anthropic" ? "anthropic" : "openai";
}

function formatError(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

interface WorkspaceModeCardProps {
  mode: WorkspaceMode;
  title: string;
  description: string;
  active: boolean;
  onSelect: (mode: WorkspaceMode) => Promise<void> | void;
}

function WorkspaceModeCard({ mode, title, description, active, onSelect }: WorkspaceModeCardProps) {
  return (
    <button
      type="button"
      onClick={() => onSelect(mode)}
      className={cn(
        "flex flex-col gap-2 rounded-none border bg-[var(--surface-raised)] p-4 text-left transition-colors",
        active ? "border-[var(--foreground)]" : "border-[var(--border)] hover:border-[var(--node-border-hover)]",
      )}
    >
      <span className="flex items-center justify-between gap-3">
        <span className="text-sm font-medium text-[var(--foreground)]">{title}</span>
        <span
          className={cn(
            "h-3 w-3 rounded-full border",
            active ? "border-[var(--foreground)] bg-[var(--foreground)]" : "border-[var(--node-border-hover)] bg-transparent",
          )}
          aria-hidden="true"
        />
      </span>
      <span className="text-xs text-[var(--text-subtle)]">{description}</span>
    </button>
  );
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
