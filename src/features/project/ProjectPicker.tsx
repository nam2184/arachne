import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { useProjectStore } from "./projectStore";

export function ProjectPicker() {
  const { setCurrentProject } = useProjectStore();

  const openDirectory = async () => {
    const selected = await open({ directory: true });
    if (selected) {
      const project = await invoke("open_project", { path: selected });
      setCurrentProject(project as any);
    }
  };

  return (
    <div className="project-picker">
      <button onClick={openDirectory}>Open Project</button>
    </div>
  );
}