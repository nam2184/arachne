import { Folder, Plus, Settings, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { NodeWebSvg } from "@/components/node-web-svg";
import { ScrollArea } from "@/components/ui/scroll-area";
import { useAppStore } from "@/features/app/appStore";
import { cn } from "@/lib/utils";
import { useProjectStore, type Project } from "@/features/project/projectStore";

interface ProjectSidebarProps {
  project: Project | null;
  onOpenSettings: () => void;
}

export function ProjectSidebar({ project, onOpenSettings }: ProjectSidebarProps) {
  const { createProject, initializeProjects, projects, setCurrentProject } = useProjectStore();
  const theme = useAppStore((state) => state.settings.theme);
  const [projectName, setProjectName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [isAddingProject, setIsAddingProject] = useState(false);
  const [isCreating, setIsCreating] = useState(false);
  const projectInputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    initializeProjects().catch((initError) => {
      setError(formatError(initError));
      console.error("Failed to load projects:", initError);
    });
  }, [initializeProjects]);

  const sortedProjects = useMemo(
    () => [...projects].sort((a, b) => a.createdAt.localeCompare(b.createdAt)),
    [projects],
  );

  useEffect(() => {
    if (isAddingProject) {
      projectInputRef.current?.focus();
    }
  }, [isAddingProject]);

  const submitProject = async () => {
    if (!projectName.trim()) return;

    setError(null);
    setIsCreating(true);

    try {
      await createProject(projectName);
      setProjectName("");
      setIsAddingProject(false);
    } catch (createError) {
      setError(formatError(createError));
      console.error("Failed to create project:", createError);
    } finally {
      setIsCreating(false);
    }
  };

  return (
    <aside className="flex w-[200px] shrink-0 flex-col border-r border-[var(--border)] bg-[var(--surface)]">
      <div className="p-4 pb-2">
        <div className="mb-5 flex items-center">
          <a href="#top" className="flex items-center gap-3" aria-label="Arachne home">
            <span className={cn(
              "flex h-14 w-14 items-center justify-center overflow-hidden border-2 p-1.5",
              theme === "light" ? "border-black bg-transparent" : "border-white bg-transparent",
            )}>
              <NodeWebSvg nodeTone={theme === "light" ? "black" : "white"} className="h-full w-full" />
            </span>
            <span className="font-mono text-sm font-semibold uppercase tracking-[0.28em] text-[var(--foreground)]">
              ARACHNE
            </span>
          </a>
        </div>
        <div className="flex items-center justify-between">
          <h2 className="text-xs font-semibold uppercase tracking-wide text-[var(--text-muted)]">Projects</h2>
          <Button
            variant="ghost"
            size="icon"
            className="h-7 w-7 bg-transparent text-[var(--text-muted)] hover:bg-transparent hover:text-[var(--foreground)]"
            onClick={() => setIsAddingProject((value) => !value)}
            aria-label="Add project"
          >
            {isAddingProject ? <X className="h-4 w-4" /> : <Plus className="h-4 w-4" />}
          </Button>
        </div>

        {isAddingProject && (
          <div className="mt-3 flex gap-2">
            <Input
              ref={projectInputRef}
              value={projectName}
              placeholder="Project name"
              onChange={(event) => setProjectName(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  submitProject();
                } else if (event.key === "Escape") {
                  setProjectName("");
                  setIsAddingProject(false);
                }
              }}
              className="h-8"
            />
            <Button size="sm" onClick={submitProject} disabled={isCreating || !projectName.trim()}>
              <Plus className="h-4 w-4" />
              {isCreating ? "Adding" : "Add"}
            </Button>
          </div>
        )}

        {error && <p className="mt-2 text-xs text-[#ff5f5f]">{error}</p>}
      </div>

      <div className="min-h-0 flex-1 px-4 pb-4">
        <ScrollArea className="h-full">
          <div className="space-y-2 pr-2">
            {sortedProjects.length === 0 ? (
              <p className="text-xs text-[var(--text-muted)]">
                No projects yet. Create a project before adding sessions.
              </p>
            ) : (
              sortedProjects.map((item) => (
                <button
                  key={item.id}
                  className={cn(
                    "flex w-full items-center gap-2 bg-transparent p-2 text-left transition-colors hover:bg-[var(--surface-raised)] hover:text-[var(--foreground)]",
                    project?.id === item.id ? "text-[var(--foreground)]" : "text-[var(--text-muted)]",
                  )}
                  onClick={() => setCurrentProject(item)}
                >
                  <Folder className="h-3.5 w-3.5 shrink-0" />
                  <span className="truncate text-sm font-medium">{item.name}</span>
                </button>
              ))
            )}
          </div>
        </ScrollArea>
      </div>
      <div className="p-4">
        <Button variant="ghost" className="w-full justify-start gap-2 bg-transparent p-2 text-[var(--text-muted)] hover:bg-[var(--surface-raised)] hover:text-[var(--foreground)]" onClick={onOpenSettings}>
          <Settings className="h-3.5 w-3.5" />
          <span className="text-sm font-medium">Settings</span>
        </Button>
      </div>
    </aside>
  );
}

function formatError(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
