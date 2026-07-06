// Dev-only stub for @tauri-apps/api/core. Active when VITE_TAURI_STUB=1.
// Loaded by vite via the resolve.alias mapping in vite.config.ts (env-gated).
//
// Returns canned data so the React tree can render SettingsPage/ProviderAuth
// without a real Tauri shell. Edit the seeded profiles/credentials here to
// preview UI states.

type Canned<T> = T | Error;

const delay = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

const profilesByProvider: Record<string, any[]> = {
  openai: [
    {
      id: "prof_default",
      provider_name: "openai",
      label: "Default",
      access_token: "stub_access_default",
      refresh_token: "stub_refresh_default",
      account_id: "[email protected]",
      created_at: "2026-07-01T10:00:00.000Z",
      last_used_at: "2026-07-05T21:35:11.000Z",
      is_active: true,
    },
    {
      id: "prof_personal",
      provider_name: "openai",
      label: "Personal",
      access_token: "stub_access_personal",
      refresh_token: "stub_refresh_personal",
      account_id: "[email protected]",
      created_at: "2026-07-02T08:14:00.000Z",
      last_used_at: "2026-07-04T12:00:00.000Z",
      is_active: false,
    },
    {
      id: "prof_work",
      provider_name: "openai",
      label: "Work",
      access_token: "stub_access_work",
      refresh_token: "stub_refresh_work",
      account_id: "[email protected]",
      created_at: "2026-06-28T17:21:00.000Z",
      last_used_at: "2026-07-01T09:00:00.000Z",
      is_active: false,
    },
  ],
};

const activeProfileIdByProvider: Record<string, string> = {
  openai: "prof_default",
};

const providerConfigs = [
  {
    name: "openai",
    model: "gpt-4o",
    api_key: null,
    base_url: null,
    protocol: "openai",
    enabled: true,
  },
  {
    name: "anthropic",
    model: "claude-opus-4-20250514",
    api_key: "[email protected]",
    base_url: null,
    protocol: "anthropic",
    enabled: true,
  },
  {
    name: "local-ollama",
    model: "llama3.1:8b",
    api_key: null,
    base_url: "http://localhost:11434/v1",
    protocol: "openai",
    enabled: true,
  },
];

const providerAuthStates = [
  {
    provider_name: "openai",
    field_type: "OAUTH",
    access_token: "stub_access_default",
    refresh_token: "stub_refresh_default",
    account_id: "[email protected]",
    api_key: null,
  },
  {
    provider_name: "anthropic",
    field_type: "API_KEY",
    access_token: null,
    refresh_token: null,
    account_id: null,
    api_key: "[email protected]",
  },
];

export async function invoke<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  await delay(40); // simulate a touch of latency so loading states are visible in screenshots

  switch (cmd) {
    case "get_provider_configs":
      return providerConfigs as unknown as T;

    case "get_provider_auth_states":
      return providerAuthStates as unknown as T;

    case "list_provider_oauth_profiles": {
      const providerName = args?.providerName as string | undefined;
      if (!providerName) return [] as unknown as T;
      return (profilesByProvider[providerName] ?? []) as unknown as T;
    }

    case "set_active_provider_oauth_profile": {
      const providerName = args?.providerName as string;
      const profileId = args?.profileId as string;
      const list = profilesByProvider[providerName] ?? [];
      const target = list.find((p) => p.id === profileId);
      if (!target) throw new Error(`profile ${profileId} not found for ${providerName}`);
      activeProfileIdByProvider[providerName] = profileId;
      return list.map((p) => ({ ...p, is_active: p.id === profileId })) as unknown as T;
    }

    case "rename_provider_oauth_profile": {
      const profileId = args?.profileId as string;
      const label = (args?.label as string | undefined)?.trim();
      if (!label) throw new Error("label required");
      for (const list of Object.values(profilesByProvider)) {
        for (const p of list) {
          if (p.id === profileId) {
            if (list.some((q) => q.id !== profileId && q.label === label)) {
              throw new Error(`label "${label}" already exists`);
            }
            p.label = label;
            return p as unknown as T;
          }
        }
      }
      throw new Error(`profile ${profileId} not found`);
    }

    case "delete_provider_oauth_profile": {
      const profileId = args?.profileId as string;
      for (const [providerName, list] of Object.entries(profilesByProvider)) {
        const idx = list.findIndex((p) => p.id === profileId);
        if (idx >= 0) {
          const target = list[idx];
          if (target.is_active) throw new Error("cannot delete the active profile");
          list.splice(idx, 1);
          return target as unknown as T;
        }
      }
      throw new Error(`profile ${profileId} not found`);
    }

    case "start_provider_oauth": {
      const providerName = args?.providerName as string;
      return {
        provider_name: providerName,
        authorization_url: "https://auth.openai.example/oauth/authorize?stub=1",
        state: "stub_state",
        code_verifier: "stub_verifier",
      } as unknown as T;
    }

    case "complete_provider_oauth": {
      const providerName = args?.providerName as string;
      const profileLabel = ((args?.profileLabel as string | undefined) ?? "Default").trim() || "Default";
      const list = profilesByProvider[providerName] ?? (profilesByProvider[providerName] = []);
      if (list.some((p) => p.label === profileLabel)) {
        throw new Error(`label "${profileLabel}" already exists`);
      }
      const id = `prof_${Date.now()}`;
      const profile = {
        id,
        provider_name: providerName,
        label: profileLabel,
        access_token: `stub_access_${id}`,
        refresh_token: `stub_refresh_${id}`,
        account_id: "[email protected]",
        created_at: new Date().toISOString(),
        last_used_at: new Date().toISOString(),
        is_active: true,
      };
      for (const p of list) p.is_active = false;
      list.unshift(profile);
      activeProfileIdByProvider[providerName] = id;
      return profile as unknown as T;
    }

    case "upsert_provider_config":
    case "update_provider_auth_settings":
      return null as unknown as T;

    case "get_provider_models":
    case "list_projects":
    case "list_sessions":
    case "create_session":
    case "send_message":
      return [] as unknown as T;

    default:
      // default no-op for unknown commands in browser preview
      if (typeof process !== "undefined" && process?.env?.VITE_TAURI_STUB_DEBUG === "1") {
        // eslint-disable-next-line no-console
        console.warn("[tauri-stub] unhandled invoke:", cmd, args);
      }
      return null as unknown as T;
  }
}

// Plugins like @tauri-apps/plugin-dialog, plugin-fs, plugin-shell may also
// be imported by the UI but are not exercised by the settings page; if they
// are, these noop helpers keep them from blowing up at import time.
export const convertFileSrc = (path: string) => `file://${path}`;
export async function invokeHandler() {
  return null;
}
