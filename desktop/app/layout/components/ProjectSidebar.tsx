import { open } from "@tauri-apps/plugin-dialog";
import { ChevronDown, ChevronRight, Folder, Plus, Settings, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { NodeWebSvg } from "@/components/node-web-svg";
import { ScrollArea } from "@/components/ui/scroll-area";
import { useAppStore } from "@/features/app/appStore";
import { cn } from "@/lib/utils";
import { useProjectStore, type Project } from "@/features/project/projectStore";
import { useSessionStore, type AgentSession } from "@/features/sessions/sessionStore";

interface ProjectSidebarProps {
  project: Project | null;
  onOpenSettings: () => void;
}

export function ProjectSidebar({ project, onOpenSettings }: ProjectSidebarProps) {
  const { createProject, initializeProjects, projects, setCurrentProject } = useProjectStore();
  const theme = useAppStore((state) => state.settings.theme);
  const workspaceMode = useAppStore((state) => state.settings.workspace_mode);
  const {
    activeSessionId,
    createSession,
    createSessionChat,
    initialize: initializeSessions,
    sessions,
    setActiveSession,
  } = useSessionStore();
  const [projectName, setProjectName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [isAddingProject, setIsAddingProject] = useState(false);
  const [isCreating, setIsCreating] = useState(false);
  const [isCreatingSession, setIsCreatingSession] = useState(false);
  const [collapsedSessionIds, setCollapsedSessionIds] = useState<Set<string>>(() => new Set());
  const projectInputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    initializeProjects().catch((initError) => {
      setError(formatError(initError));
      console.error("Failed to load projects:", initError);
    });
  }, [initializeProjects]);

  useEffect(() => {
    initializeSessions().catch((initError) => {
      setError(formatError(initError));
      console.error("Failed to load sessions:", initError);
    });
  }, [initializeSessions]);

  const sortedProjects = useMemo(
    () => [...projects].sort((a, b) => a.createdAt.localeCompare(b.createdAt)),
    [projects],
  );

  const projectSessions = useMemo(() => {
    if (!project) return [];
    return Array.from(sessions.values()).filter((session) => session.project_id === project.id);
  }, [project, sessions]);

  const rootSessions = useMemo(
    () => projectSessions
      .filter((session) => !session.parent_session_id)
      .sort((a, b) => new Date(a.created_at).getTime() - new Date(b.created_at).getTime()),
    [projectSessions],
  );

  const chatsByRoot = useMemo(() => {
    const byRoot = new Map<string, AgentSession[]>();
    for (const session of projectSessions) {
      if (!session.parent_session_id) continue;
      const chats = byRoot.get(session.parent_session_id) ?? [];
      chats.push(session);
      byRoot.set(session.parent_session_id, chats);
    }
    for (const chats of byRoot.values()) {
      chats.sort((a, b) => new Date(a.created_at).getTime() - new Date(b.created_at).getTime());
    }
    return byRoot;
  }, [projectSessions]);

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

  const chooseSessionDirectory = async () => {
    if (!project || isCreatingSession) return;
    setError(null);
    setIsCreatingSession(true);

    try {
      const selection = await open({ directory: true, multiple: false });
      const directory = Array.isArray(selection) ? selection[0] : selection;
      if (!directory) return;

      const sessionId = await createSession(project.id, directory);
      setActiveSession(sessionId);
    } catch (createError) {
      setError(formatError(createError));
      console.error("Failed to create session:", createError);
    } finally {
      setIsCreatingSession(false);
    }
  };

  const createChat = async (rootSessionId: string) => {
    try {
      const result = await createSessionChat(rootSessionId);
      setCollapsedSessionIds((current) => {
        const next = new Set(current);
        next.delete(rootSessionId);
        return next;
      });
      setActiveSession(result.chatSessionId);
    } catch (createError) {
      setError(formatError(createError));
      console.error("Failed to create chat:", createError);
    }
  };

  const toggleSessionChats = (sessionId: string) => {
    setCollapsedSessionIds((current) => {
      const next = new Set(current);
      if (next.has(sessionId)) {
        next.delete(sessionId);
      } else {
        next.add(sessionId);
      }
      return next;
    });
  };

  return (
    <aside className="flex h-full w-[200px] shrink-0 flex-col border-r border-[var(--border)] bg-[var(--surface)]">
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
          <div className="space-y-5 pr-2">
            <div className="space-y-2">
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

            {workspaceMode === "agent" && project && (
              <div className="space-y-2 border-t border-[var(--border)] pt-4">
                <div className="flex items-center justify-between">
                  <h2 className="text-xs font-semibold uppercase tracking-wide text-[var(--text-muted)]">Sessions</h2>
                  <Button
                    variant="ghost"
                    size="icon"
                    className="h-7 w-7 bg-transparent text-[var(--text-muted)] hover:bg-transparent hover:text-[var(--foreground)]"
                    onClick={chooseSessionDirectory}
                    disabled={isCreatingSession}
                    aria-label="Add session"
                    title={isCreatingSession ? "Adding session" : "Add session"}
                  >
                    <Plus className="h-4 w-4" />
                  </Button>
                </div>
                {rootSessions.length === 0 ? (
                  <p className="text-xs text-[var(--text-muted)]">No sessions yet.</p>
                ) : (
                  <div className="space-y-2">
                    {rootSessions.map((root) => {
                      const chats = chatsByRoot.get(root.id) ?? [];
                      const isCollapsed = collapsedSessionIds.has(root.id);
                      const activeRoot = activeSessionId === root.id || chats.some((chat) => chat.id === activeSessionId);
                      return (
                        <div key={root.id} className="space-y-1">
                          <div className="group flex items-center gap-1">
                            {chats.length > 0 ? (
                              <button
                                type="button"
                                className="flex h-7 w-5 shrink-0 items-center justify-center bg-transparent text-[var(--text-muted)] hover:text-[var(--foreground)]"
                                onClick={() => toggleSessionChats(root.id)}
                                aria-expanded={!isCollapsed}
                                aria-label={`${isCollapsed ? "Expand" : "Collapse"} chats for ${directoryName(root.directory)}`}
                                title={isCollapsed ? "Expand chats" : "Collapse chats"}
                              >
                                {isCollapsed ? <ChevronRight className="h-3.5 w-3.5" /> : <ChevronDown className="h-3.5 w-3.5" />}
                              </button>
                            ) : (
                              <span className="h-7 w-5 shrink-0" aria-hidden="true" />
                            )}
                            <button
                              type="button"
                              className={cn(
                                "min-w-0 flex-1 bg-transparent px-2 py-1.5 text-left text-sm transition-colors hover:bg-[var(--surface-raised)] hover:text-[var(--foreground)]",
                                activeRoot ? "text-[var(--foreground)]" : "text-[var(--text-muted)]",
                              )}
                              onClick={() => setActiveSession(root.id)}
                              title={root.directory}
                            >
                              <span className="block truncate font-medium">{directoryName(root.directory)}</span>
                            </button>
                            <button
                              type="button"
                              className="hidden h-7 w-7 shrink-0 items-center justify-center bg-transparent text-[var(--text-muted)] hover:text-[var(--foreground)] group-hover:flex"
                              onClick={() => void createChat(root.id)}
                              title="New chat"
                              aria-label={`New chat for ${directoryName(root.directory)}`}
                            >
                              <Plus className="h-3.5 w-3.5" />
                            </button>
                          </div>
                          {chats.length > 0 && !isCollapsed && (
                            <div className="space-y-1 pl-3">
                              {chats.map((chat, index) => (
                                <button
                                  key={chat.id}
                                  type="button"
                                  className={cn(
                                    "flex w-full min-w-0 items-center gap-1.5 bg-transparent px-2 py-1 text-left text-xs transition-colors hover:bg-[var(--surface-raised)] hover:text-[var(--foreground)]",
                                    activeSessionId === chat.id ? "text-[var(--foreground)]" : "text-[var(--text-muted)]",
                                  )}
                                  onClick={() => setActiveSession(chat.id)}
                                >
                                  <span className="truncate">{chatTitle(chat, index)}</span>
                                </button>
                              ))}
                            </div>
                          )}
                        </div>
                      );
                    })}
                  </div>
                )}
              </div>
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

function directoryName(directory: string) {
  return directory.split(/[\\/]/).filter(Boolean).pop() ?? directory;
}

function chatTitle(chat: AgentSession, index: number) {
  const title = chat.title?.trim();
  return title || `Chat ${index + 1}`;
}
