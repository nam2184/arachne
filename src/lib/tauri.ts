import { invoke } from "@tauri-apps/api/core";

export async function openProject(path: string) {
  return invoke("open_project", { path });
}

export async function readFile(path: string) {
  return invoke("read_file", { path });
}

export async function writeFile(path: string, content: string) {
  return invoke("write_file", { path, content });
}

export async function sendMessage(sessionId: string, message: string) {
  return invoke("send_message", { sessionId, message });
}

export async function createSession(projectId: string) {
  return invoke("create_agent_session", { projectId });
}

export async function listDirectory(path: string) {
  return invoke("list_directory", { path });
}