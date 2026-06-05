import { listen, emit } from "@tauri-apps/api/event";

export function onAgentMessage(callback: (payload: unknown) => void) {
  return listen("agent-message", callback);
}

export function onProjectChanged(callback: (payload: unknown) => void) {
  return listen("project-changed", callback);
}

export async function emitProjectOpened(projectId: string) {
  return emit("project-opened", { projectId });
}