import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { Plus } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { PermissionPromptBar, SessionCanvas, SessionChat, DeleteSessionDialog } from "@/components/sessions";
import { Button } from "@/components/ui/button";
import { useProjectStore } from "@/features/project/projectStore";
import { usePermissionStore } from "@/features/permissions/permissionStore";
import { useConversationStore, type AgentStreamEvent } from "@/features/sessions/conversationStore";
import { useSessionStore } from "@/features/sessions/sessionStore";

export function SessionWorkspace() {
  const currentProject = useProjectStore((state) => state.currentProject);
  const {
    addSessionToGroup,
    createGroup,
    createSession,
    deleteSession,
    groups,
    initialize,
    sessions,
    setActiveSession,
    updateSessionProvider,
  } = useSessionStore();
  const {
    activeConversation,
    applyAgentEvent,
    beginStreamingMessage,
    loadUiConversation,
    clearConversation,
    failStreamingMessage,
    finishStreamingMessage,
    streamingMessageId,
  } = useConversationStore();
  const compactNow = useConversationStore((state) => state.compactNow);
  const isCompacting = useConversationStore((state) => state.isCompacting);
  const initializePermissions = usePermissionStore((state) => state.initialize);
  const [isCreating, setIsCreating] = useState(false);
  const [runningSessionIds, setRunningSessionIds] = useState<Set<string>>(() => new Set());
  const [queuedCounts, setQueuedCounts] = useState<Map<string, number>>(() => new Map());
  const [error, setError] = useState<string | null>(null);
  const [chatSessionId, setChatSessionId] = useState<string | null>(null);
  const [sessionPendingDelete, setSessionPendingDelete] = useState<string | null>(null);
  const sendQueuesRef = useRef(new Map<string, Array<{ content: string; mode: "plan" | "build" }>>());
  const processingSessionsRef = useRef(new Set<string>());
  const chatSessionIdRef = useRef<string | null>(null);

  useEffect(() => {
    chatSessionIdRef.current = chatSessionId;
  }, [chatSessionId]);

  useEffect(() => {
    initialize().catch((initError) => {
      setError(formatError(initError));
      console.error("Failed to initialize sessions:", initError);
    });
  }, [initialize]);

  useEffect(() => {
    let dispose: (() => void) | undefined;
    initializePermissions()
      .then((d) => {
        dispose = d;
      })
      .catch((err) => {
        console.error("Failed to initialize permission store:", err);
      });
    return () => {
      dispose?.();
    };
  }, [initializePermissions]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let mounted = true;

    listen<AgentStreamEvent>("agent:event", (event) => {
      applyAgentEvent(event.payload);
    })
      .then((dispose) => {
        if (mounted) {
          unlisten = dispose;
        } else {
          dispose();
        }
      })
      .catch((listenError) => {
        setError(formatError(listenError));
        console.error("Failed to subscribe to agent events:", listenError);
      });

    return () => {
      mounted = false;
      unlisten?.();
    };
  }, [applyAgentEvent]);

  useEffect(() => {
    if (!chatSessionId) {
      clearConversation();
      return;
    }

    loadUiConversation(chatSessionId).catch((loadError) => {
      setError(formatError(loadError));
      console.error("Failed to load conversation:", loadError);
    });
  }, [chatSessionId, clearConversation, loadUiConversation]);

  const projectSessions = useMemo(() => {
    if (!currentProject) return new Map();

    return new Map(
      Array.from(sessions.entries()).filter(([, session]) => session.project_id === currentProject.id),
    );
  }, [currentProject, sessions]);

  const createSessionNode = async () => {
    if (!currentProject) {
      setError("Create or select a project before adding sessions.");
      return;
    }

    const directory = await open({ directory: true });
    if (!directory) return;

    setIsCreating(true);
    setError(null);

    try {
      await createSession(currentProject.id, directory);
    } catch (createError) {
      setError(formatError(createError));
      console.error("Failed to create session:", createError);
    } finally {
      setIsCreating(false);
    }
  };

  const selectSession = useCallback((id: string) => {
    setActiveSession(id);
  }, [setActiveSession]);

  const openSessionChat = useCallback((id: string) => {
    setActiveSession(id);
    setChatSessionId(id);
  }, [setActiveSession]);

  const connectSessions = useCallback((sourceId: string, targetId: string) => {
    const sourceGroupId = sessions.get(sourceId)?.group_id;
    const targetGroupId = sessions.get(targetId)?.group_id;

    let action: Promise<unknown>;
    if (sourceGroupId && sourceGroupId === targetGroupId) {
      return;
    }

    if (sourceGroupId && !targetGroupId) {
      action = addSessionToGroup(targetId, sourceGroupId);
    } else if (!sourceGroupId && targetGroupId) {
      action = addSessionToGroup(sourceId, targetGroupId);
    } else if (sourceGroupId && targetGroupId) {
      const sourceGroup = groups.get(sourceGroupId);
      const targetGroup = groups.get(targetGroupId);
      action = createGroup(
        Array.from(new Set([
          ...(sourceGroup?.session_ids ?? []),
          ...(targetGroup?.session_ids ?? []),
          sourceId,
          targetId,
        ])),
      );
    } else {
      action = createGroup([sourceId, targetId]);
    }

    action.catch((groupError) => {
      setError(formatError(groupError));
      console.error("Failed to update session group:", groupError);
    });
  }, [addSessionToGroup, createGroup, groups, sessions]);

  const setSessionRunning = useCallback((sessionId: string, running: boolean) => {
    setRunningSessionIds((current) => {
      const next = new Set(current);
      if (running) {
        next.add(sessionId);
      } else {
        next.delete(sessionId);
      }
      return next;
    });
  }, []);

  const setSessionQueuedCount = useCallback((sessionId: string, count: number) => {
    setQueuedCounts((current) => {
      const next = new Map(current);
      if (count > 0) {
        next.set(sessionId, count);
      } else {
        next.delete(sessionId);
      }
      return next;
    });
  }, []);

  const drainSendQueue = useCallback(async (sessionId: string) => {
    if (processingSessionsRef.current.has(sessionId)) return;
    processingSessionsRef.current.add(sessionId);
    setSessionRunning(sessionId, true);

    try {
      while (true) {
        const queue = sendQueuesRef.current.get(sessionId) ?? [];
        const next = queue.shift();
        if (queue.length === 0) {
          sendQueuesRef.current.delete(sessionId);
        } else {
          sendQueuesRef.current.set(sessionId, queue);
        }
        setSessionQueuedCount(sessionId, queue.length);
        if (!next) break;

        setError(null);
        try {
          const isVisibleSession = chatSessionIdRef.current === sessionId;
          if (isVisibleSession) {
            beginStreamingMessage(sessionId, next.content);
          }
          await invoke<string>("send_message", {
            sessionId,
            message: next.content,
            mode: next.mode,
          });
          if (chatSessionIdRef.current === sessionId) {
            await loadUiConversation(sessionId);
          }
        } catch (chatError) {
          const message = formatError(chatError);
          if (chatSessionIdRef.current === sessionId) {
            failStreamingMessage(sessionId, message);
          }
          setError(message);
          console.error("Failed to send chat message:", chatError);
        } finally {
          if (chatSessionIdRef.current === sessionId) {
            finishStreamingMessage(sessionId);
          }
        }
      }
    } finally {
      processingSessionsRef.current.delete(sessionId);
      setSessionRunning(sessionId, false);
    }
  }, [beginStreamingMessage, failStreamingMessage, finishStreamingMessage, loadUiConversation, setSessionQueuedCount, setSessionRunning]);

  const sendChatMessage = useCallback((content: string, mode: "plan" | "build") => {
    if (!chatSessionId) return;
    const queue = sendQueuesRef.current.get(chatSessionId) ?? [];
    queue.push({ content, mode });
    sendQueuesRef.current.set(chatSessionId, queue);
    setSessionQueuedCount(chatSessionId, queue.length);
    void drainSendQueue(chatSessionId);
  }, [chatSessionId, drainSendQueue, setSessionQueuedCount]);

  const closeChat = useCallback(() => {
    setChatSessionId(null);
    clearConversation();
  }, [clearConversation]);

  const compactChat = useCallback(async () => {
    if (!chatSessionId || runningSessionIds.has(chatSessionId) || isCompacting) return;
    try {
      await compactNow(chatSessionId);
    } catch (compactError) {
      setError(formatError(compactError));
      console.error("Failed to compact conversation:", compactError);
    }
  }, [chatSessionId, compactNow, runningSessionIds, isCompacting]);

  const chatSession = chatSessionId ? sessions.get(chatSessionId) ?? null : null;
  const isChatSending = chatSessionId ? runningSessionIds.has(chatSessionId) : false;
  const queuedMessageCount = chatSessionId ? queuedCounts.get(chatSessionId) ?? 0 : 0;
  const chatMessages = activeConversation?.session_id === chatSessionId
    ? activeConversation.messages
    : [];

  const sessionPendingDeleteLabel = sessionPendingDelete
    ? (sessions.get(sessionPendingDelete)?.directory ?? null)
    : null;

  const requestDeleteSession = useCallback((id: string) => {
    setSessionPendingDelete(id);
  }, []);

  const cancelDeleteSession = useCallback(() => {
    setSessionPendingDelete(null);
  }, []);

  const confirmDeleteSession = useCallback(async (id: string) => {
    try {
      if (chatSessionId === id) {
        setChatSessionId(null);
        clearConversation();
      }
      sendQueuesRef.current.delete(id);
      setSessionQueuedCount(id, 0);
      setSessionRunning(id, false);
      setActiveSession("");
      await deleteSession(id);
      setSessionPendingDelete(null);
    } catch (deleteError) {
      throw deleteError;
    }
  }, [chatSessionId, clearConversation, deleteSession, setActiveSession, setSessionQueuedCount, setSessionRunning]);

  return (
    <section className="flex h-screen min-w-0 flex-1 flex-col bg-black">
      <div className="relative flex min-h-0 flex-1">
        <div className="pointer-events-none absolute right-4 top-4 z-20 flex flex-col items-end gap-2">
          <Button
            size="icon"
            className="pointer-events-auto h-9 w-9 rounded-none border border-[#1f1f1f] bg-[#0a0a0a] text-white shadow-none hover:border-[#2a2a2a] hover:bg-[#111111]"
            onClick={createSessionNode}
            disabled={isCreating || !currentProject}
            aria-label="Add session"
            title="Add session"
          >
            <Plus className="h-5 w-5" />
          </Button>
          {error && <span className="pointer-events-auto max-w-[320px] rounded-none border border-[#1f1f1f] bg-black px-3 py-2 text-xs text-[#ff5f5f] shadow-none">{error}</span>}
        </div>
        <div className="min-w-0 flex-1 overflow-hidden">
          <SessionCanvas
            sessions={projectSessions}
            groups={groups}
            onConnectSessions={connectSessions}
            onOpenSessionChat={openSessionChat}
            onSelectSession={selectSession}
            onDeleteSession={requestDeleteSession}
          />
        </div>
      </div>
      {chatSession && (
        <SessionChat
          session={chatSession}
          messages={chatMessages}
          isSending={isChatSending}
          queuedMessageCount={queuedMessageCount}
          isCompacting={isCompacting}
          streamingMessageId={streamingMessageId}
          onSendMessage={sendChatMessage}
          onUpdateSessionProvider={updateSessionProvider}
          onCompact={compactChat}
          onClose={closeChat}
        />
      )}
      <PermissionPromptBar sessionId={chatSessionId} />
      <DeleteSessionDialog
        open={sessionPendingDelete !== null}
        sessionId={sessionPendingDelete}
        sessionLabel={sessionPendingDeleteLabel}
        onCancel={cancelDeleteSession}
        onConfirm={confirmDeleteSession}
      />
    </section>
  );
}

function formatError(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
