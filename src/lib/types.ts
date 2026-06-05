export interface Project {
  id: string;
  path: string;
  name: string;
  techStack: string[];
}

export interface AgentSession {
  id: string;
  projectId: string;
  messages: Message[];
  provider: string;
  model: string;
}

export interface Message {
  id: string;
  role: "user" | "assistant" | "system";
  content: string;
  timestamp: number;
}

export interface Provider {
  name: string;
  model: string;
  apiKey?: string;
  baseUrl?: string;
}