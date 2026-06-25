import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { Plus } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { LoopConfigDialog } from "@/components/loops";
import { PermissionPromptBar, SessionCanvas, SessionChat, DeleteSessionDialog } from "@/components/sessions";
import { Button } from "@/components/ui/button";
import { useLoopStore, type LoopInput } from "@/features/loops/loopStore";
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
    appendSessionToLoop,
    createLoop,
    deleteLoop,
    initialize: initializeLoops,
    loops,
    updateLoop,
  } = useLoopStore();
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
  const [isAddMenuOpen, setIsAddMenuOpen] = useState(false);
  const [isLoopDialogOpen, setIsLoopDialogOpen] = useState(false);
  const [configuringLoopId, setConfiguringLoopId] = useState<string | null>(null);
  const [focusRequest, setFocusRequest] = useState<{ sessionId: string; nonce: number } | null>(null);
  const sendQueuesRef = useRef(new Map<string, Array<{ content: string; mode: "plan" | "build" }>>());
  const processingSessionsRef = useRef(new Set<string>());
  const creatingSessionRef = useRef(false);
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
    initializeLoops();
  }, [initializeLoops]);

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

  const projectLoops = useMemo(() => {
    if (!currentProject) return new Map();

    return new Map(
      Array.from(loops.entries()).filter(([, loop]) => loop.project_id === currentProject.id),
    );
  }, [currentProject, loops]);

  const createSessionNode = async () => {
    if (creatingSessionRef.current) return;

    if (!currentProject) {
      setError("Create or select a project before adding sessions.");
      return;
    }

    creatingSessionRef.current = true;
    setError(null);

    try {
      const selection = await open({ directory: true, multiple: false });
      const directory = Array.isArray(selection) ? selection[0] : selection;
      if (!directory) return;

      setIsCreating(true);
      const sessionId = await createSession(currentProject.id, directory);
      setFocusRequest((current) => ({
        sessionId,
        nonce: (current?.nonce ?? 0) + 1,
      }));
    } catch (createError) {
      setError(formatError(createError));
      console.error("Failed to create session:", createError);
    } finally {
      creatingSessionRef.current = false;
      setIsCreating(false);
    }
  };

  const toggleAddMenu = useCallback(() => {
    setIsAddMenuOpen((open) => !open);
  }, []);

  const chooseSession = useCallback(() => {
    setIsAddMenuOpen(false);
    void createSessionNode();
  }, [currentProject, createSession, setFocusRequest]);

  const chooseLoop = useCallback(() => {
    setIsAddMenuOpen(false);
    setConfiguringLoopId(null);
    setIsLoopDialogOpen(true);
  }, []);

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

  const openConfigureLoopDialog = useCallback((id: string) => {
    setConfiguringLoopId(id);
    setIsLoopDialogOpen(true);
  }, []);

  const closeLoopDialog = useCallback(() => {
    setIsLoopDialogOpen(false);
    setConfiguringLoopId(null);
  }, []);

  const saveLoop = useCallback((input: LoopInput) => {
    if (configuringLoopId) {
      updateLoop(configuringLoopId, input);
    } else if (currentProject) {
      createLoop(currentProject.id, input);
    }
    closeLoopDialog();
  }, [closeLoopDialog, configuringLoopId, createLoop, currentProject, updateLoop]);

  const removeLoop = useCallback((id: string) => {
    deleteLoop(id);
    if (configuringLoopId === id) {
      closeLoopDialog();
    }
  }, [closeLoopDialog, configuringLoopId, deleteLoop]);

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
  const configuringLoop = configuringLoopId ? loops.get(configuringLoopId) ?? null : null;

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
    <section className="flex h-screen min-w-0 flex-1 flex-col bg-[var(--background)]">
      <div className="relative flex min-h-0 flex-1">
        <div className="pointer-events-none absolute right-4 top-4 z-20 flex flex-col items-end gap-2">
          <Button
            size="icon"
            className="pointer-events-auto h-9 w-9 rounded-none border border-black bg-black text-white shadow-none hover:border-black hover:bg-black"
            onClick={toggleAddMenu}
            disabled={!currentProject}
            aria-label="Add to canvas"
            aria-expanded={isAddMenuOpen}
            title={isCreating ? "Adding session" : "Add session or loop"}
          >
            <Plus className="h-5 w-5" />
          </Button>
          {isAddMenuOpen && (
            <div className="pointer-events-auto grid w-40 gap-1 border border-[var(--border)] bg-[var(--surface-raised)] p-1 shadow-[0_18px_60px_rgba(0,0,0,0.18)]" role="menu">
              <button
                type="button"
                onClick={chooseSession}
                disabled={isCreating}
                className="h-8 px-2 text-left text-xs text-[var(--foreground)] hover:bg-[var(--surface-soft)] disabled:cursor-not-allowed disabled:opacity-50"
                role="menuitem"
              >
                Session
              </button>
              <button
                type="button"
                onClick={chooseLoop}
                className="h-8 px-2 text-left text-xs text-[var(--foreground)] hover:bg-[var(--surface-soft)]"
                role="menuitem"
              >
                Loop container
              </button>
            </div>
          )}
          {error && <span className="pointer-events-auto max-w-[320px] rounded-none border border-[var(--border)] bg-[var(--surface-raised)] px-3 py-2 text-xs text-[#ff5f5f] shadow-none">{error}</span>}
        </div>
        <div className="min-w-0 flex-1 overflow-hidden">
          <SessionCanvas
            sessions={projectSessions}
            groups={groups}
            loops={projectLoops}
            onConnectSessions={connectSessions}
            onAppendSessionToLoop={appendSessionToLoop}
            onOpenSessionChat={openSessionChat}
            onSelectSession={selectSession}
            onDeleteSession={requestDeleteSession}
            onConfigureLoop={openConfigureLoopDialog}
            onDeleteLoop={removeLoop}
            focusRequest={focusRequest}
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
      <LoopConfigDialog
        open={isLoopDialogOpen}
        loop={configuringLoop}
        onCancel={closeLoopDialog}
        onSave={saveLoop}
      />
    </section>
  );
}

function formatError(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
