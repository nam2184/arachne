import providerModels from "../../../config/provider-models.json";

interface ModelSpec {
  id: string;
  context_window: number;
  max_output: number;
}

const modelsByProvider = providerModels as Record<string, ModelSpec[] | string[]>;

const specsByProvider: Record<string, ModelSpec[]> = Object.fromEntries(
  Object.entries(modelsByProvider).map(([provider, models]) => [
    provider,
    models.map((model) =>
      typeof model === "string"
        ? { id: model, context_window: 128_000, max_output: 8_000 }
        : model
    ),
  ])
);

const specIndex = new Map<string, ModelSpec>();
for (const models of Object.values(specsByProvider)) {
  for (const spec of models) {
    specIndex.set(spec.id, spec);
  }
}

export function getModelOptions(provider: string, currentModel?: string) {
  const configured = specsByProvider[provider] ?? [];
  const ids = configured.map((spec) => spec.id);
  const trimmedCurrentModel = currentModel?.trim();

  if (trimmedCurrentModel && !ids.includes(trimmedCurrentModel)) {
    return [trimmedCurrentModel, ...ids];
  }

  return ids;
}

export function getDefaultModel(provider: string, fallback = "") {
  return specsByProvider[provider]?.[0]?.id ?? fallback;
}

export function getModelSpec(modelId: string): ModelSpec | undefined {
  return specIndex.get(modelId);
}

export function getContextWindow(modelId: string, fallback = 128_000): number {
  return specIndex.get(modelId)?.context_window ?? fallback;
}

export function getMaxOutput(modelId: string, fallback = 8_000): number {
  return specIndex.get(modelId)?.max_output ?? fallback;
}
