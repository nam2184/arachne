import { useEffect } from "react";
import { useProjectStore } from "@/features/project/projectStore";
import { useAppStore } from "@/features/app/appStore";
import { SessionWorkspace } from "@/pages/sessions/SessionWorkspace";
import { ProjectSidebar } from "@/app/layout/components/ProjectSidebar";
import { SettingsPage } from "@/app/layout/components/SettingsPage";

export function AppShell() {
  const currentProject = useProjectStore((state) => state.currentProject);
  const { view, loadSettings, setView } = useAppStore();

  useEffect(() => {
    loadSettings();
  }, [loadSettings]);

  return (
    <div className="flex h-screen overflow-hidden bg-[var(--background)] text-[var(--foreground)]">
      <ProjectSidebar project={currentProject} onOpenSettings={() => setView("settings")} />
      <main className="flex min-w-0 flex-1 flex-col overflow-hidden">
        {view === "settings" ? <SettingsPage /> : <SessionWorkspace />}
      </main>
    </div>
  );
}
