import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { Plus } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { PermissionPromptBar, SessionCanvas, SessionChat } from "@/components/sessions";
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
  const [isChatSending, setIsChatSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [chatSessionId, setChatSessionId] = useState<string | null>(null);

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

  const sendChatMessage = useCallback(async (content: string, mode: "plan" | "build") => {
    if (!chatSessionId || isChatSending) return;

    setIsChatSending(true);
    setError(null);

    try {
      beginStreamingMessage(chatSessionId, content);
      await invoke<string>("send_message", {
        sessionId: chatSessionId,
        message: content,
        mode,
      });
      await loadUiConversation(chatSessionId);
    } catch (chatError) {
      const message = formatError(chatError);
      failStreamingMessage(chatSessionId, message);
      setError(message);
      console.error("Failed to send chat message:", chatError);
    } finally {
      finishStreamingMessage(chatSessionId);
      setIsChatSending(false);
    }
  }, [beginStreamingMessage, chatSessionId, failStreamingMessage, finishStreamingMessage, isChatSending, loadUiConversation]);

  const closeChat = useCallback(() => {
    setChatSessionId(null);
    clearConversation();
  }, [clearConversation]);

  const compactChat = useCallback(async () => {
    if (!chatSessionId || isChatSending || isCompacting) return;
    try {
      await compactNow(chatSessionId);
    } catch (compactError) {
      setError(formatError(compactError));
      console.error("Failed to compact conversation:", compactError);
    }
  }, [chatSessionId, compactNow, isChatSending, isCompacting]);

  const chatSession = chatSessionId ? sessions.get(chatSessionId) ?? null : null;
  const chatMessages = activeConversation?.session_id === chatSessionId
    ? activeConversation.messages
    : [];

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
          />
        </div>
      </div>
      {chatSession && (
        <SessionChat
          session={chatSession}
          messages={chatMessages}
          isSending={isChatSending}
          isCompacting={isCompacting}
          streamingMessageId={streamingMessageId}
          onSendMessage={sendChatMessage}
          onUpdateSessionProvider={updateSessionProvider}
          onCompact={compactChat}
          onClose={closeChat}
        />
      )}
      <PermissionPromptBar sessionId={chatSessionId} />
    </section>
  );
}

function formatError(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
