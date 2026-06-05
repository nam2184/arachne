import { useSettingsStore } from "./settingsStore";

export function ProviderSettings() {
  const { providers, activeProvider, setActiveProvider, updateProvider } =
    useSettingsStore();

  return (
    <div className="provider-settings">
      <h3>LLM Providers</h3>
      {providers.map((p) => (
        <div key={p.provider} className={`provider ${p.provider === activeProvider ? "active" : ""}`}>
          <select
            value={p.provider}
            onChange={(e) => setActiveProvider(e.target.value)}
          >
            <option value={p.provider}>{p.provider}</option>
          </select>
          <input
            type="text"
            value={p.model}
            onChange={(e) => updateProvider(p.provider, { model: e.target.value })}
            placeholder="Model"
          />
          <input
            type="password"
            onChange={(e) => updateProvider(p.provider, { apiKey: e.target.value })}
            placeholder="API Key"
          />
        </div>
      ))}
    </div>
  );
}