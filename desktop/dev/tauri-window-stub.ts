// Dev-only stub for @tauri-apps/api/window. Active when VITE_TAURI_STUB=1.
// Loaded via vite alias for non-tauri browser preview.

export function getCurrentWindow() {
  return {
    label: "main",
    minimize: async () => {},
    unminimize: async () => {},
    maximize: async () => {},
    unmaximize: async () => {},
    isMaximized: async () => false,
    close: async () => {},
    startDragging: async () => {},
    onMoved: () => () => {},
    onResized: () => () => {},
    onCloseRequested: () => () => {},
    onFocusChanged: () => () => {},
  };
}

export function getAllWindows() {
  return [getCurrentWindow()];
}

export class WebviewWindow {
  constructor() {
    throw new Error("[tauri-stub] WebviewWindow not available in browser preview");
  }
}

export const appWindow = getCurrentWindow();
export const currentWindow = getCurrentWindow();