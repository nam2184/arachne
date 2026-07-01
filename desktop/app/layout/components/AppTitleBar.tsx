import { getCurrentWindow } from "@tauri-apps/api/window";
import { Minus, PanelLeftClose, PanelLeftOpen, Square, X } from "lucide-react";
import type { MouseEvent } from "react";

interface AppTitleBarProps {
  sidebarCollapsed: boolean;
  onToggleSidebar: () => void;
}

export function AppTitleBar({ sidebarCollapsed, onToggleSidebar }: AppTitleBarProps) {
  const appWindow = getCurrentWindow();

  const minimize = () => {
    void appWindow.minimize();
  };

  const toggleMaximize = () => {
    void (async () => {
      if (await appWindow.isMaximized()) {
        await appWindow.unmaximize();
      } else {
        await appWindow.maximize();
      }
    })();
  };

  const close = () => {
    void appWindow.close();
  };

  const startDragging = (event: MouseEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    void appWindow.startDragging();
  };

  const handleDoubleClick = () => {
    toggleMaximize();
  };

  return (
    <header
      className="flex h-9 shrink-0 select-none items-center justify-between border-b border-[var(--border)] bg-[var(--surface)] text-[var(--foreground)]"
    >
      <div className="flex h-full min-w-0 items-center">
        <button
          type="button"
          data-titlebar-control
          className="flex h-full w-11 items-center justify-center text-[var(--text-muted)] transition-colors hover:bg-[var(--surface-soft)] hover:text-[var(--foreground)]"
          onClick={onToggleSidebar}
          aria-label={sidebarCollapsed ? "Expand sidebar" : "Collapse sidebar"}
          title={sidebarCollapsed ? "Expand sidebar" : "Collapse sidebar"}
        >
          {sidebarCollapsed ? <PanelLeftOpen className="h-4 w-4" /> : <PanelLeftClose className="h-4 w-4" />}
        </button>
      </div>
      <div
        data-tauri-drag-region
        className="h-full min-w-0 flex-1"
        onMouseDown={startDragging}
        onDoubleClick={handleDoubleClick}
      />
      <div className="flex h-full">
        <button
          type="button"
          data-titlebar-control
          className="flex h-full w-11 items-center justify-center text-[var(--text-muted)] transition-colors hover:bg-[var(--surface-soft)] hover:text-[var(--foreground)]"
          onClick={minimize}
          aria-label="Minimize window"
        >
          <Minus className="h-3.5 w-3.5" />
        </button>
        <button
          type="button"
          data-titlebar-control
          className="flex h-full w-11 items-center justify-center text-[var(--text-muted)] transition-colors hover:bg-[var(--surface-soft)] hover:text-[var(--foreground)]"
          onClick={toggleMaximize}
          aria-label="Maximize window"
        >
          <Square className="h-3 w-3" />
        </button>
        <button
          type="button"
          data-titlebar-control
          className="flex h-full w-11 items-center justify-center text-[var(--text-muted)] transition-colors hover:bg-[#7f1d1d] hover:text-white"
          onClick={close}
          aria-label="Close window"
        >
          <X className="h-3.5 w-3.5" />
        </button>
      </div>
    </header>
  );
}
