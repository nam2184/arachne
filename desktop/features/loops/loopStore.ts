import { create } from "zustand";
import type { LoopGoalStatus } from "@/components/loops";

const STORAGE_KEY = "arachne.session-loops.v1";

export type SessionLoopStatus = "active" | "paused" | "completed";

export interface SessionLoopGoal {
  id: string;
  text: string;
  status: LoopGoalStatus;
}

export interface SessionLoop {
  id: string;
  project_id: string;
  title: string;
  goals: SessionLoopGoal[];
  token_limit: number;
  status: SessionLoopStatus;
  session_ids: string[];
  created_at: string;
}

export interface LoopInput {
  title: string;
  goals: string[];
  tokenLimit: number;
  status?: SessionLoopStatus;
}

interface LoopState {
  loops: Map<string, SessionLoop>;
  initialize: () => void;
  createLoop: (projectId: string, input: LoopInput) => string;
  updateLoop: (id: string, input: LoopInput) => void;
  deleteLoop: (id: string) => void;
  appendSessionToLoop: (sessionId: string, loopId: string) => void;
  removeSessionFromLoop: (sessionId: string, loopId: string) => void;
}

function readLoops() {
  if (typeof window === "undefined") return new Map<string, SessionLoop>();

  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) return new Map<string, SessionLoop>();
    const parsed = JSON.parse(raw) as SessionLoop[];
    if (!Array.isArray(parsed)) return new Map<string, SessionLoop>();
    return new Map(parsed.map((loop) => [loop.id, loop]));
  } catch {
    return new Map<string, SessionLoop>();
  }
}

function writeLoops(loops: Map<string, SessionLoop>) {
  if (typeof window === "undefined") return;
  window.localStorage.setItem(STORAGE_KEY, JSON.stringify(Array.from(loops.values())));
}

function makeGoal(text: string, index: number, existing?: SessionLoopGoal): SessionLoopGoal {
  return {
    id: existing?.text === text ? existing.id : crypto.randomUUID(),
    text,
    status: index === 0 ? "in_progress" : "pending",
  };
}

function normalizeInput(input: LoopInput, existingGoals: SessionLoopGoal[] = []) {
  const title = input.title.trim() || "Untitled loop";
  const goals = input.goals
    .map((goal) => goal.trim())
    .filter(Boolean)
    .map((goal, index) => makeGoal(goal, index, existingGoals[index]));

  return {
    title,
    goals,
    token_limit: Math.max(0, Math.floor(input.tokenLimit || 0)),
    status: input.status ?? "active",
  };
}

export const useLoopStore = create<LoopState>((set) => ({
  loops: new Map(),

  initialize: () => {
    set({ loops: readLoops() });
  },

  createLoop: (projectId, input) => {
    const id = crypto.randomUUID();
    set((state) => {
      const loops = new Map(state.loops);
      loops.set(id, {
        id,
        project_id: projectId,
        session_ids: [],
        created_at: new Date().toISOString(),
        ...normalizeInput(input),
      });
      writeLoops(loops);
      return { loops };
    });
    return id;
  },

  updateLoop: (id, input) => {
    set((state) => {
      const current = state.loops.get(id);
      if (!current) return state;

      const loops = new Map(state.loops);
      loops.set(id, {
        ...current,
        ...normalizeInput(input, current.goals),
      });
      writeLoops(loops);
      return { loops };
    });
  },

  deleteLoop: (id) => {
    set((state) => {
      const loops = new Map(state.loops);
      loops.delete(id);
      writeLoops(loops);
      return { loops };
    });
  },

  appendSessionToLoop: (sessionId, loopId) => {
    set((state) => {
      const loops = new Map<string, SessionLoop>();
      for (const loop of state.loops.values()) {
        const sessionIds = loop.session_ids.filter((id) => id !== sessionId);
        loops.set(loop.id, {
          ...loop,
          session_ids: loop.id === loopId ? [...sessionIds, sessionId] : sessionIds,
        });
      }
      writeLoops(loops);
      return { loops };
    });
  },

  removeSessionFromLoop: (sessionId, loopId) => {
    set((state) => {
      const current = state.loops.get(loopId);
      if (!current) return state;

      const loops = new Map(state.loops);
      loops.set(loopId, {
        ...current,
        session_ids: current.session_ids.filter((id) => id !== sessionId),
      });
      writeLoops(loops);
      return { loops };
    });
  },
}));
