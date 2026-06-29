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
import { useSessionStore, type AgentSession } from "@/features/sessions/sessionStore";

interface UiCommandResult {
  status: string;
  message: string;
  conversationChanged: boolean;
}

export function SessionWorkspace() {
  const currentProject = useProjectStore((state) => state.currentProject);
  const {
    addSessionToGroup,
    createGroup,
    createSession,
    createSessionChat,
    deleteSession,
    groups,
    initialize,
    sessions,
    setActiveSession,
    updateSessionProvider,
    updateSessionTitle,
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
  const pendingPromptKeysRef = useRef(new Map<string, Set<string>>());

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

  const rootProjectSessions = useMemo(() => (
    new Map(Array.from(projectSessions.entries()).filter(([, session]) => !session.parent_session_id))
  ), [projectSessions]);

  const chatsByRoot = useMemo(() => {
    const byRoot = new Map<string, AgentSession[]>();
    for (const session of projectSessions.values()) {
      if (!session.parent_session_id) continue;
      const chats = byRoot.get(session.parent_session_id) ?? [];
      chats.push(session);
      byRoot.set(session.parent_session_id, chats);
    }
    for (const chats of byRoot.values()) {
      chats.sort((a, b) => new Date(a.created_at).getTime() - new Date(b.created_at).getTime());
    }
    return byRoot;
  }, [projectSessions]);

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
    const chats = chatsByRoot.get(id) ?? [];
    const chatId = chats[0]?.id ?? id;
    setActiveSession(id);
    setChatSessionId(chatId);
  }, [chatsByRoot, sessions, setActiveSession]);

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
        const promptKey = promptQueueKey(next.content, next.mode);

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
          pendingPromptKeysRef.current.get(sessionId)?.delete(promptKey);
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
    const promptKey = promptQueueKey(content, mode);
    const pendingKeys = pendingPromptKeysRef.current.get(chatSessionId) ?? new Set<string>();
    if (pendingKeys.has(promptKey)) return;
    pendingKeys.add(promptKey);
    pendingPromptKeysRef.current.set(chatSessionId, pendingKeys);

    const queue = sendQueuesRef.current.get(chatSessionId) ?? [];
    queue.push({ content, mode });
    sendQueuesRef.current.set(chatSessionId, queue);
    setSessionQueuedCount(chatSessionId, queue.length);
    void drainSendQueue(chatSessionId);
  }, [chatSessionId, drainSendQueue, setSessionQueuedCount]);

  const createChatForCurrentSession = useCallback(async () => {
    if (!chatSessionId) return;
    const result = await createSessionChat(chatSessionId);
    setChatSessionId(result.chatSessionId);
    setFocusRequest((current) => ({
      sessionId: result.rootSessionId,
      nonce: (current?.nonce ?? 0) + 1,
    }));
  }, [chatSessionId, createSessionChat]);

  const closeChat = useCallback(() => {
    setChatSessionId(null);
    clearConversation();
  }, [clearConversation]);

  const runUiCommand = useCallback(async (input: string): Promise<UiCommandResult> => {
    if (!chatSessionId) {
      throw new Error("No active chat session.");
    }
    if (runningSessionIds.has(chatSessionId)) {
      throw new Error("Wait for the current run to finish before running a UI command.");
    }

    const result = await invoke<UiCommandResult>("execute_ui_command", {
      sessionId: chatSessionId,
      input,
    });
    if (result.conversationChanged) {
      await loadUiConversation(chatSessionId);
    }
    return result;
  }, [chatSessionId, loadUiConversation, runningSessionIds]);

  const chatSession = chatSessionId ? sessions.get(chatSessionId) ?? null : null;
  const chatRootId = chatSession?.parent_session_id ?? chatSession?.id ?? null;
  const chatRoot = chatRootId ? sessions.get(chatRootId) ?? chatSession : null;
  const chatSiblings = chatRootId ? chatsByRoot.get(chatRootId) ?? [] : [];
  const chatOptions = chatRoot && chatSiblings.length > 0 ? chatSiblings : chatRoot ? [chatRoot] : [];
  const isChatSending = chatSessionId ? runningSessionIds.has(chatSessionId) : false;
  const queuedMessageCount = chatSessionId ? queuedCounts.get(chatSessionId) ?? 0 : 0;
  const chatMessages = activeConversation?.session_id === chatSessionId
    ? activeConversation.messages
    : [];

  const pendingDeleteSession = sessionPendingDelete ? sessions.get(sessionPendingDelete) ?? null : null;
  const sessionPendingDeleteKind: "session" | "chat" = pendingDeleteSession?.parent_session_id ? "chat" : "session";
  const sessionPendingDeleteLabel = pendingDeleteSession
    ? pendingDeleteSession.parent_session_id
      ? chatDisplayTitle(pendingDeleteSession)
      : pendingDeleteSession.directory
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
      const deletedSession = sessions.get(id) ?? null;
      const deletedRootId = deletedSession?.parent_session_id ?? deletedSession?.id ?? id;
      const activeChat = chatSessionId ? sessions.get(chatSessionId) ?? null : null;
      const activeRootId = activeChat?.parent_session_id ?? activeChat?.id ?? null;
      const isDeletingActiveChat = chatSessionId === id;
      const isDeletingRoot = deletedSession ? !deletedSession.parent_session_id : false;
      const isDeletingActiveRoot = isDeletingRoot && activeRootId === id;
      const nextChatId = isDeletingActiveChat && deletedSession?.parent_session_id
        ? (chatsByRoot.get(deletedSession.parent_session_id) ?? []).find((chat) => chat.id !== id)?.id ?? null
        : null;

      sendQueuesRef.current.delete(id);
      pendingPromptKeysRef.current.delete(id);
      setSessionQueuedCount(id, 0);
      setSessionRunning(id, false);
      await deleteSession(id);
      if (isDeletingActiveRoot) {
        setChatSessionId(null);
        clearConversation();
        setActiveSession("");
      } else if (isDeletingActiveChat) {
        if (nextChatId) {
          setChatSessionId(nextChatId);
          setActiveSession(deletedRootId);
        } else {
          setChatSessionId(null);
          clearConversation();
          setActiveSession("");
        }
      } else if (isDeletingRoot) {
        setActiveSession("");
      }
      setSessionPendingDelete(null);
    } catch (deleteError) {
      throw deleteError;
    }
  }, [chatSessionId, chatsByRoot, clearConversation, deleteSession, sessions, setActiveSession, setSessionQueuedCount, setSessionRunning]);

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
            sessions={rootProjectSessions}
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
          rootSession={chatRoot ?? chatSession}
          chats={chatOptions}
          activeChatId={chatSession.id}
          messages={chatMessages}
          isSending={isChatSending}
          queuedMessageCount={queuedMessageCount}
          streamingMessageId={streamingMessageId}
          onSendMessage={sendChatMessage}
          onRunCommand={runUiCommand}
          onSelectChat={setChatSessionId}
          onCreateChat={createChatForCurrentSession}
          onDeleteChat={requestDeleteSession}
          onUpdateSessionProvider={updateSessionProvider}
          onUpdateSessionTitle={updateSessionTitle}
          onClose={closeChat}
        />
      )}
      <PermissionPromptBar sessionId={chatSessionId} sessionDirectory={chatRoot?.directory ?? chatSession?.directory} />
      <DeleteSessionDialog
        open={sessionPendingDelete !== null}
        sessionId={sessionPendingDelete}
        sessionLabel={sessionPendingDeleteLabel}
        kind={sessionPendingDeleteKind}
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

function chatDisplayTitle(session: AgentSession) {
  const title = session.title?.trim();
  return title || "Unknown";
}

function promptQueueKey(content: string, mode: "plan" | "build") {
  return `${mode}\0${content.trim()}`;
}
