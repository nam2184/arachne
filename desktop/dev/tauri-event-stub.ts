// Dev-only stub for @tauri-apps/api/event. Active when VITE_TAURI_STUB=1.
export async function listen(event: string, handler: (event: unknown) => void) {
  void event;
  void handler;
  // noop subscription
  return () => {};
}
export async function emit(event: string, payload?: unknown) {
  void event;
  void payload;
  return null;
}
export const TauriEvent = new Proxy({}, { get: () => "noop" });
