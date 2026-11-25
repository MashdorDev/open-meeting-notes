export interface ModelWithStatus {
  status?: unknown;
  quantization?: unknown;
}

interface ProviderCommands {
  initialize: string;
  hasAvailableModels: string;
  getAvailableModels: string;
}

const PROVIDER_COMMANDS: Record<string, ProviderCommands> = {
  localWhisper: {
    initialize: 'whisper_init',
    hasAvailableModels: 'whisper_has_available_models',
    getAvailableModels: 'whisper_get_available_models',
  },
  parakeet: {
    initialize: 'parakeet_init',
    hasAvailableModels: 'parakeet_has_available_models',
    getAvailableModels: 'parakeet_get_available_models',
  },
};

export function getProviderCommands(provider: string): ProviderCommands | null {
  return PROVIDER_COMMANDS[provider] ?? null;
}

// Counts only downloads that will make recording ready. The full-precision
// Parakeet model is used after each recording, never live.
export function hasDownloadingModel(models: ModelWithStatus[]): boolean {
  return models.some(({ status, quantization }) => quantization !== 'FP32' && (
    status === 'Downloading'
    || (status !== null && typeof status === 'object' && 'Downloading' in status)
  ));
}
