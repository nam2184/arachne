import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { create } from "zustand";

export interface PermissionPrompt {
  id: string;
  sessionId: string;
  permission: string;
  patterns: string[];
  tool: string;
  always: string[];
  metadata: Record<string, unknown> | null;
}

export type PermissionReply = "once" | "always" | "reject";

interface PermissionState {
  /** All currently pending prompts (across all sessions). */
  pending: PermissionPrompt[];
  /** Polling handle so we can stop it on unmount. */
  pollHandle: ReturnType<typeof setInterval> | null;
  unlistenHandle: (() => void) | null;

  /** Set up the permission event listener and polling. Call once at app start. */
  initialize: () => Promise<() => void>;
  /** Tear down the listener and stop polling. */
  shutdown: () => void;
  /** Poll once and update the pending list. */
  refresh: (sessionId: string) => Promise<void>;
  /** Reply to a pending prompt. */
  reply: (sessionId: string, requestId: string, reply: PermissionReply) => Promise<void>;
}

export const usePermissionStore = create<PermissionState>((set, get) => ({
  pending: [],
  pollHandle: null,
  unlistenHandle: null,

  initialize: async () => {
    // Listen for the backend's "permission-changed" event. The backend emits
    // this whenever a new request is added or a reply resolves one.
    const dispose = await listen<string>("permission-changed", (event) => {
      const sessionId = event.payload;
      get().refresh(sessionId);
    });
    set({ unlistenHandle: dispose });

    return () => {
      get().shutdown();
    };
  },

  shutdown: () => {
    const { pollHandle, unlistenHandle } = get();
    if (pollHandle !== null) {
      clearInterval(pollHandle);
    }
    if (unlistenHandle) {
      unlistenHandle();
    }
    set({ pollHandle: null, unlistenHandle: null, pending: [] });
  },

  refresh: async (sessionId: string) => {
    try {
      const response = await invoke<{ requests: PendingPromptWire[] }>(
        "permission_list_pending",
        { sessionId },
      );
      const prompts: PermissionPrompt[] = response.requests.map((wire) => ({
        id: typeof wire.id === "string" ? wire.id : wire.id[0],
        sessionId: wire.session_id,
        permission: wire.permission,
        patterns: wire.patterns,
        tool: wire.tool,
        always: wire.always,
        metadata: wire.metadata,
      }));
      set((state) => {
        // Keep prompts for other sessions, replace this session's prompts.
        const others = state.pending.filter((p) => p.sessionId !== sessionId);
        return { pending: [...others, ...prompts] };
      });
    } catch (error) {
      console.error("Failed to fetch pending permissions:", error);
    }
  },

  reply: async (sessionId: string, requestId: string, reply: PermissionReply) => {
    try {
      await invoke("permission_reply", {
        request: { session_id: sessionId, request_id: requestId, reply },
      });
      // Optimistically remove the prompt from local state.
      set((state) => ({
        pending: state.pending.filter((p) => p.id !== requestId),
      }));
    } catch (error) {
      console.error("Failed to reply to permission:", error);
    }
  },
}));

// Wire shape returned by the Tauri command. RequestId serializes as a string,
// but keeping the tuple fallback makes older dev builds harmless.
type PendingPromptWire = {
  id: string | { 0: string };
  session_id: string;
  permission: string;
  patterns: string[];
  tool: string;
  always: string[];
  metadata: Record<string, unknown> | null;
};
