import { invoke } from "@tauri-apps/api/core";
import { create } from "zustand";

interface RustProject {
  id: string;
  path: string;
  name: string;
  tech_stack?: string[];
  techStack?: string[];
  created_at: string;
}

export interface Project {
  id: string;
  path: string;
  name: string;
  techStack: string[];
  createdAt: string;
}

interface ProjectState {
  currentProject: Project | null;
  projects: Project[];
  initializeProjects: () => Promise<void>;
  createProject: (name: string) => Promise<Project>;
  setCurrentProject: (project: Project | null) => void;
}

export const useProjectStore = create<ProjectState>((set) => ({
  currentProject: null,
  projects: [],

  initializeProjects: async () => {
    const projects = await invoke<RustProject[]>("list_projects");
    const normalizedProjects = projects.map(normalizeProject);
    set((state) => ({
      projects: normalizedProjects,
      currentProject:
        state.currentProject && normalizedProjects.some((project) => project.id === state.currentProject?.id)
          ? state.currentProject
          : normalizedProjects[0] ?? null,
    }));
  },

  createProject: async (name) => {
    const project = normalizeProject(await invoke<RustProject>("create_project", { name }));
    set((state) => ({
      projects: [...state.projects, project],
      currentProject: project,
    }));
    return project;
  },

  setCurrentProject: (project) => set({ currentProject: project }),
}));

function normalizeProject(project: RustProject): Project {
  return {
    id: project.id,
    path: project.path,
    name: project.name,
    techStack: project.tech_stack ?? project.techStack ?? [],
    createdAt: project.created_at,
  };
}
