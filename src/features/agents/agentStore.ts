import { create } from "zustand";

export interface AgentMessage {
  id: string;
  role: "user" | "assistant" | "system";
  content: string;
  timestamp: number;
}

interface AgentState {
  sessions: Map<string, AgentSession>;
  activeSessionId: string | null;
  createSession: (projectId: string) => string;
  setActiveSession: (id: string) => void;
  addMessage: (sessionId: string, message: AgentMessage) => void;
}

interface AgentSession {
  id: string;
  projectId: string;
  messages: AgentMessage[];
  provider: string;
  model: string;
}

export const useAgentStore = create<AgentState>((set, get) => ({
  sessions: new Map(),
  activeSessionId: null,
  createSession: (projectId) => {
    const id = crypto.randomUUID();
    set((state) => {
      const newSessions = new Map(state.sessions);
      newSessions.set(id, {
        id,
        projectId,
        messages: [],
        provider: "anthropic",
        model: "claude-3-5-sonnet-20241022",
      });
      return { sessions: newSessions, activeSessionId: id };
    });
    return id;
  },
  setActiveSession: (id) => set({ activeSessionId: id }),
  addMessage: (sessionId, message) =>
    set((state) => {
      const newSessions = new Map(state.sessions);
      const session = newSessions.get(sessionId);
      if (session) {
        session.messages.push(message);
        newSessions.set(sessionId, session);
      }
      return { sessions: newSessions };
    }),
}));