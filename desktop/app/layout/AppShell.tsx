import { useEffect, useState } from "react";
import { useProjectStore } from "@/features/project/projectStore";
import { useAppStore } from "@/features/app/appStore";
import { SessionWorkspace } from "@/pages/sessions/SessionWorkspace";
import { ProjectSidebar } from "@/app/layout/components/ProjectSidebar";
import { SettingsPage } from "@/app/layout/components/SettingsPage";
import { AppTitleBar } from "@/app/layout/components/AppTitleBar";
import { cn } from "@/lib/utils";

export function AppShell() {
  const currentProject = useProjectStore((state) => state.currentProject);
  const { loadSettings } = useAppStore();
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);

  useEffect(() => {
    loadSettings();
  }, [loadSettings]);

  return (
    <div className="flex h-screen flex-col overflow-hidden bg-[var(--background)] text-[var(--foreground)]">
      <AppTitleBar
        sidebarCollapsed={sidebarCollapsed}
        onToggleSidebar={() => setSidebarCollapsed((collapsed) => !collapsed)}
      />
      <div className="flex min-h-0 flex-1 overflow-hidden">
        <div
          className={cn(
            "h-full min-h-0 shrink-0 overflow-hidden transition-[width] duration-150 ease-out",
            sidebarCollapsed ? "w-0" : "w-[200px]",
          )}
          aria-hidden={sidebarCollapsed}
        >
          <ProjectSidebar project={currentProject} onOpenSettings={() => setSettingsOpen(true)} />
        </div>
        <main className="flex min-w-0 flex-1 flex-col overflow-hidden">
          <SessionWorkspace />
        </main>
      </div>
      <SettingsPage open={settingsOpen} onClose={() => setSettingsOpen(false)} />
    </div>
  );
}
