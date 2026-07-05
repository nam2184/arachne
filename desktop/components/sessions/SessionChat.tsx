import { invoke } from "@tauri-apps/api/core";
import { Check, ChevronDown, ChevronLeft, ChevronRight, FolderSearch, MoreHorizontal, Plus, Search, Terminal, Wrench, X } from "lucide-react";
import { useEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import { Button } from "@/components/ui/button";
import { DiffBlock } from "@/components/ui/diff-block";
import { MarkdownContent } from "@/components/ui/markdown-content";
import { ScrollArea } from "@/components/ui/scroll-area";
import { ThemedCodeBlock } from "@/components/ui/themed-code-block";
import { cn } from "@/lib/utils";
import { getDefaultModel, getModelOptions } from "@/features/sessions/providerModels";
import type { ChatMessagePart, SessionFileDiff } from "@/features/sessions/conversationStore";
import type { AgentSession, ProviderConfig } from "@/features/sessions/sessionStore";
import { SessionDiffPane } from "@/components/sessions/SessionDiffPane";
import { ThinkBlock } from "@/components/sessions/ThinkBlock";

export interface SessionChatMessage {
  id?: string;
  role: "user" | "assistant" | "system";
  content: string;
  reasoning?: string;
  parts?: ChatMessagePart[];
  timestamp: string;
}

export type ChatMode = "plan" | "build";

interface UiCommandHint {
  name: string;
  usage: string;
  description: string;
}

interface UiCommandResult {
  status: string;
  message: string;
  conversationChanged: boolean;
}

interface ChatMenuOption {
  id: string;
  label: string;
  tone?: "default" | "danger";
  onSelect: () => void;
}

interface SessionChatProps {
  variant?: "floating" | "docked";
  session: AgentSession;
  rootSession: AgentSession;
  chats: AgentSession[];
  activeChatId: string;
  messages: SessionChatMessage[];
  isSending: boolean;
  queuedMessageCount: number;
  streamingMessageId: string | null;
  diffs?: SessionFileDiff[];
  onSendMessage: (content: string, mode: ChatMode) => void | Promise<void>;
  onStopStreaming: () => void | Promise<void>;
  onRunCommand: (input: string) => Promise<UiCommandResult>;
  onSelectChat: (sessionId: string) => void;
  onCreateChat: () => Promise<void> | void;
  onDeleteChat: (sessionId: string) => Promise<void> | void;
  onUpdateSessionProvider: (sessionId: string, provider: string, model: string) => Promise<void>;
  onUpdateSessionTitle: (sessionId: string, title: string) => Promise<void>;
  onClose?: () => void;
}

const CHAT_WIDTH = 760;
const CHAT_HEIGHT = 600;
const EDGE_PADDING = 16;
const DIFF_PANE_DEFAULT_WIDTH = 520;
const DIFF_PANE_MIN_WIDTH = 0;
const DIFF_PANE_USABLE_MIN_WIDTH = 320;
const DIFF_PANE_MAX_WIDTH = 760;
const DIFF_PANE_SNAP_THRESHOLD = 72;
const DOCK_CONTROL_CLASS =
  "h-7 min-w-0 border-0 bg-transparent px-1 text-[11px] text-[var(--text-muted)] outline-none transition-colors hover:text-[var(--foreground)] focus:text-[var(--foreground)] disabled:cursor-not-allowed disabled:opacity-40 [&>option]:bg-[var(--surface-raised)] [&>option]:text-[var(--foreground)]";

function ContextCheckpoint({ content }: { content: string }) {
  const [open, setOpen] = useState(false);
  const match = content.match(/<conversation-checkpoint>([\s\S]*?)<\/conversation-checkpoint>/);
  const summary = match ? match[1].trim() : content;
  return (
    <div className="flex justify-center" data-testid="context-checkpoint">
      <div className="w-full max-w-[80%] rounded-none border border-dashed border-[var(--border)] bg-[var(--surface)] px-4 py-2 text-xs text-[var(--text-subtle)]">
        <button
          type="button"
          onClick={() => setOpen((value) => !value)}
          className="flex w-full items-center justify-between gap-2 text-left"
        >
          <span className="flex items-center gap-2 text-[var(--foreground)]">
            {open ? <ChevronDown className="h-3 w-3" /> : <ChevronRight className="h-3 w-3" />}
            Context compacted
          </span>
          <span className="text-[10px] uppercase tracking-wide text-[var(--text-muted)]">summary</span>
        </button>
        {open && (
          <pre className="mt-2 max-h-64 overflow-y-auto whitespace-pre-wrap break-words text-[11px] leading-relaxed text-[var(--text-secondary)]">
            {summary}
          </pre>
        )}
      </div>
    </div>
  );
}

function ChatOptionsMenu({
  options,
  onClose,
}: {
  options: ChatMenuOption[];
  onClose: () => void;
}) {
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handlePointerDown = (event: globalThis.PointerEvent) => {
      const menu = menuRef.current;
      if (!menu || menu.contains(event.target as Node)) return;
      onClose();
    };
    const handleKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };

    document.addEventListener("pointerdown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("pointerdown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [onClose]);

  return (
    <div
      ref={menuRef}
      role="menu"
      className="absolute right-1 top-7 z-10 min-w-28 border border-[var(--border)] bg-[var(--surface-raised)] p-1 shadow-[0_12px_32px_rgba(0,0,0,0.22)]"
      onPointerDown={(event) => event.stopPropagation()}
    >
      {options.map((option) => (
        <button
          key={option.id}
          type="button"
          role="menuitem"
          className={cn(
            "flex h-7 w-full items-center px-2 text-left text-[11px] hover:bg-[var(--surface-soft)]",
            option.tone === "danger"
              ? "text-[#d14d4d] hover:text-[#ff5f5f]"
              : "text-[var(--text-muted)] hover:text-[var(--foreground)]",
          )}
          onClick={() => {
            option.onSelect();
            onClose();
          }}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

export function SessionChat({
  variant = "floating",
  session,
  rootSession,
  chats,
  activeChatId,
  messages,
  isSending,
  queuedMessageCount,
  streamingMessageId,
  diffs = [],
  onSendMessage,
  onStopStreaming,
  onRunCommand,
  onSelectChat,
  onCreateChat,
  onDeleteChat,
  onUpdateSessionProvider,
  onUpdateSessionTitle,
  onClose,
}: SessionChatProps) {
  const [input, setInput] = useState("");
  const [commandHints, setCommandHints] = useState<UiCommandHint[]>([]);
  const [commandStatus, setCommandStatus] = useState<string | null>(null);
  const [commandError, setCommandError] = useState<string | null>(null);
  const [isCommandRunning, setIsCommandRunning] = useState(false);
  const [position, setPosition] = useState(() => initialChatPosition());
  const [providers, setProviders] = useState<ProviderConfig[]>([]);
  const [providerDraft, setProviderDraft] = useState(session.provider);
  const [modelDraft, setModelDraft] = useState(session.model);
  const [configStatus, setConfigStatus] = useState<string | null>(null);
  const [configError, setConfigError] = useState<string | null>(null);
  const [isConfigSaving, setIsConfigSaving] = useState(false);
  const [editingTitleId, setEditingTitleId] = useState<string | null>(null);
  const [titleDraft, setTitleDraft] = useState("");
  const [titleError, setTitleError] = useState<string | null>(null);
  const [isChatSidebarOpen, setIsChatSidebarOpen] = useState(true);
  const [openChatMenuId, setOpenChatMenuId] = useState<string | null>(null);
  const [diffPaneWidth, setDiffPaneWidth] = useState(DIFF_PANE_DEFAULT_WIDTH);
  // In-memory only; not persisted to settings or anywhere else. The
  // active mode is sent to the backend on each prompt and injected into
  // the LLM's context as a synthetic user message.
  const [mode, setMode] = useState<ChatMode>("plan");
  const scrollViewportRef = useRef<HTMLDivElement>(null);
  // True when the user is "pinned" to the bottom of the chat —
  // within `SCROLL_BOTTOM_THRESHOLD_PX` of the scrollHeight.
  // Updated on every user scroll. While pinned, new streamed
  // content auto-scrolls. When the user scrolls up to read
  // history, the auto-scroll stops, so they can review past
  // turns without the view jumping.
  const [isPinnedToBottom, setIsPinnedToBottom] = useState(true);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const isFloating = variant === "floating";
  const diffResizeRef = useRef<{
    pointerId: number;
    startX: number;
    startWidth: number;
  } | null>(null);
  const chatDragRef = useRef<{
    pointerId: number;
    startX: number;
    startY: number;
    originX: number;
    originY: number;
  } | null>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  useEffect(() => {
    if (!isFloating) return;
    const handleResize = () => setPosition((current) => clampChatPosition(current));
    window.addEventListener("resize", handleResize);
    handleResize();
    return () => window.removeEventListener("resize", handleResize);
  }, [isFloating]);

  useEffect(() => {
    const el = inputRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 160)}px`;
  }, [input]);

  useEffect(() => {
    invoke<ProviderConfig[]>("get_provider_configs")
      .then((configs) => setProviders(configs))
      .catch((error) => setConfigError(formatError(error)));
  }, []);

  useEffect(() => {
    invoke<UiCommandHint[]>("list_ui_commands")
      .then((hints) => setCommandHints(hints))
      .catch((error) => setCommandError(formatError(error)));
  }, []);

  useEffect(() => {
    setProviderDraft(session.provider);
    setModelDraft(session.model);
    setConfigStatus(null);
    setConfigError(null);
  }, [session.id, session.provider, session.model]);

  // Auto-scroll on streaming content, but only when the user is
  // pinned to the bottom. A small threshold (in px) absorbs
  // sub-pixel rounding and wheel-tick imprecision — the user
  // is "pinned" as long as they're within 32 px of the bottom.
  // Once they scroll up to read history, the auto-scroll
  // pauses. A subsequent scroll back to the bottom re-arms
  // it. This matches the behavior of every chat app from Slack
  // to the GitHub issue page.
  const SCROLL_BOTTOM_THRESHOLD_PX = 32;

  // Single source of truth for the "is the user at the bottom"
  // predicate. Called on every scroll event AND on every
  // mutation of the messages array (so the threshold check
  // stays accurate as the viewport grows from a stream).
  const updatePinned = () => {
    const el = scrollViewportRef.current;
    if (!el) return;
    const distanceFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight;
    setIsPinnedToBottom(distanceFromBottom <= SCROLL_BOTTOM_THRESHOLD_PX);
  };

  useEffect(() => {
    const el = scrollViewportRef.current;
    if (!el) return;
    el.addEventListener("scroll", updatePinned, { passive: true });
    // Also re-check on resize (e.g. chat width changes): the
    // "clientHeight" changes and the pinned calculation would
    // be stale.
    const observer = new ResizeObserver(updatePinned);
    observer.observe(el);
    updatePinned();
    return () => {
      el.removeEventListener("scroll", updatePinned);
      observer.disconnect();
    };
  }, []);

  // Auto-scroll when the model is streaming new content AND
  // the user is pinned. When the user has scrolled up to read
  // history, we do nothing — the streamed content keeps
  // arriving, the chat just doesn't jump.
  useEffect(() => {
    if (!isPinnedToBottom) return;
    const el = scrollViewportRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, [isSending, messages, isPinnedToBottom]);

  const handleSend = async () => {
    if (isSending && !commandBufferActive) {
      await onStopStreaming();
      return;
    }

    const content = input.trim();
    if (!content) return;

    if (content.startsWith("/")) {
      setCommandStatus(null);
      setCommandError(null);
      try {
        setIsCommandRunning(true);
        const result = await onRunCommand(content);
        setInput("");
        setCommandStatus(result.message);
      } catch (error) {
        setCommandError(formatError(error));
      } finally {
        setIsCommandRunning(false);
      }
      return;
    }

    setCommandStatus(null);
    setCommandError(null);
    setInput("");
    await onSendMessage(content, mode);
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  };

  const handleDiffResizeStart = (event: PointerEvent<HTMLDivElement>) => {
    diffResizeRef.current = {
      pointerId: event.pointerId,
      startX: event.clientX,
      startWidth: diffPaneWidth,
    };
    event.currentTarget.setPointerCapture(event.pointerId);
  };

  const handleDiffResizeMove = (event: PointerEvent<HTMLDivElement>) => {
    const resize = diffResizeRef.current;
    if (!resize || resize.pointerId !== event.pointerId) return;
    const nextWidth = resize.startWidth - (event.clientX - resize.startX);
    setDiffPaneWidth(clampDiffPaneWidth(nextWidth));
  };

  const handleDiffResizeEnd = (event: PointerEvent<HTMLDivElement>) => {
    if (diffResizeRef.current?.pointerId === event.pointerId) {
      diffResizeRef.current = null;
    }
  };

  const handleChatDragStart = (event: PointerEvent<HTMLDivElement>) => {
    if (!isFloating || event.button !== 0 || isPopupDragExcluded(event.target)) return;
    chatDragRef.current = {
      pointerId: event.pointerId,
      startX: event.clientX,
      startY: event.clientY,
      originX: position.x,
      originY: position.y,
    };
    event.currentTarget.setPointerCapture(event.pointerId);
  };

  const handleChatDragMove = (event: PointerEvent<HTMLDivElement>) => {
    const drag = chatDragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) return;
    setPosition(clampChatPosition({
      x: drag.originX + event.clientX - drag.startX,
      y: drag.originY + event.clientY - drag.startY,
    }));
  };

  const handleChatDragEnd = (event: PointerEvent<HTMLDivElement>) => {
    if (chatDragRef.current?.pointerId === event.pointerId) {
      chatDragRef.current = null;
    }
  };

  const saveSessionConfig = async () => {
    const provider = providerDraft.trim();
    const model = modelDraft.trim();
    if (!provider || !model) {
      setConfigError("Provider and model are required.");
      return;
    }

    setIsConfigSaving(true);
    setConfigError(null);
    setConfigStatus(null);

    try {
      await onUpdateSessionProvider(session.id, provider, model);
      setConfigStatus("Saved");
    } catch (error) {
      setConfigError(formatError(error));
    } finally {
      setIsConfigSaving(false);
    }
  };

  const chatTitleIndex = (chatId: string) => {
    const index = chats.findIndex((chat) => chat.id === chatId);
    return index >= 0 ? index : 0;
  };

  const displayChatTitle = (chat: AgentSession) => chatTitle(chat, chatTitleIndex(chat.id));

  const startTitleEdit = (chat: AgentSession) => {
    setEditingTitleId(chat.id);
    setTitleDraft(displayChatTitle(chat));
    setTitleError(null);
    setOpenChatMenuId(null);
  };

  const cancelTitleEdit = () => {
    setEditingTitleId(null);
    setTitleDraft("");
    setTitleError(null);
  };

  const saveTitleEdit = async (chatId: string) => {
    const currentTitle = chats.find((chat) => chat.id === chatId)?.title?.trim() ?? "";
    const nextTitle = titleDraft.trim();
    if (nextTitle === currentTitle) {
      cancelTitleEdit();
      return;
    }

    try {
      await onUpdateSessionTitle(chatId, nextTitle);
      cancelTitleEdit();
    } catch (error) {
      setTitleError(formatError(error));
    }
  };

  const chatMenuOptions = (chat: AgentSession): ChatMenuOption[] => [
    {
      id: "rename",
      label: "Rename",
      onSelect: () => startTitleEdit(chat),
    },
    {
      id: "delete",
      label: "Delete",
      tone: "danger",
      onSelect: () => void onDeleteChat(chat.id),
    },
  ];

  const configChanged = providerDraft !== session.provider || modelDraft !== session.model;
  const providerOptions = providers.some((provider) => provider.name === providerDraft)
    ? providers
    : providerDraft
      ? [
          {
            name: providerDraft,
            model: modelDraft,
            api_key: null,
            base_url: null,
            enabled: true,
          },
          ...providers,
        ]
      : providers;
  const modelOptions = getModelOptions(providerDraft, modelDraft);
  const trimmedInput = input.trimStart();
  const commandBufferActive = trimmedInput.startsWith("/");
  const commandHasArgs = /^\/\S+\s/.test(trimmedInput);
  const commandQuery = commandBufferActive ? trimmedInput.slice(1).split(/\s+/, 1)[0].toLowerCase() : "";
  const visibleCommandHints = commandBufferActive && !commandHasArgs
    ? commandHints.filter((hint) => hint.name.startsWith(commandQuery))
    : [];

  const directoryName = rootSession.directory.split(/[\\/]/).filter(Boolean).pop() ?? rootSession.directory;
  const showChatSidebar = isFloating;
  const showDiffPane = !isFloating && diffs.length > 0;
  const isDiffPaneCollapsed = diffPaneWidth === 0;
  const activeChatIndex = chatTitleIndex(activeChatId);
  const activeChatTitle = chatTitle(session, activeChatIndex);
  const isEditingActiveTitle = editingTitleId === session.id;

  return (
    <div className={isFloating ? "pointer-events-none fixed inset-0 z-50" : "flex min-h-0 flex-1"}>
      <div
        className={cn(
          "pointer-events-auto relative flex flex-col overflow-hidden rounded-none border border-[var(--border)] bg-[var(--surface-raised)] text-[var(--foreground)] shadow-none",
          isFloating
            ? cn(
                "fixed h-[600px] max-h-[calc(100vh-32px)] max-w-[calc(100vw-32px)]",
                showDiffPane && !isDiffPaneCollapsed ? "w-[1120px]" : "w-[760px]",
              )
            : "h-full w-full border-0 border-l",
        )}
        role={isFloating ? "dialog" : "region"}
        aria-modal={isFloating ? "false" : undefined}
        aria-label={`${directoryName} chat`}
        style={isFloating ? { left: position.x, top: position.y } : undefined}
        onPointerDown={isFloating ? handleChatDragStart : undefined}
        onPointerMove={isFloating ? handleChatDragMove : undefined}
        onPointerUp={isFloating ? handleChatDragEnd : undefined}
        onPointerCancel={isFloating ? handleChatDragEnd : undefined}
      >
        {isFloating && onClose && (
          <Button
            variant="ghost"
            size="icon"
            className="absolute right-2 top-2 z-20 h-8 w-8 bg-[var(--surface-raised)]/80"
            onClick={onClose}
          >
            <X className="h-4 w-4" />
          </Button>
        )}
        <div className="flex min-h-0 min-w-0 flex-1 overflow-hidden">
          {showChatSidebar && (
            <aside
              className={cn(
                "flex shrink-0 flex-col border-r border-[var(--border)] bg-[var(--surface)] transition-[width] duration-150",
                isChatSidebarOpen ? "w-40" : "w-10",
              )}
            >
            {isChatSidebarOpen ? (
              <>
                <div className="flex items-center justify-between border-b border-[var(--border)] px-3 py-2">
                  <span className="text-[10px] uppercase tracking-[0.18em] text-[var(--text-muted)]">Sessions</span>
                  <div className="flex items-center gap-1">
                    <Button
                      variant="ghost"
                      size="icon"
                      className="h-6 w-6"
                      onClick={() => void onCreateChat()}
                      title="New chat"
                    >
                      <Plus className="h-3.5 w-3.5" />
                    </Button>
                    <Button
                      variant="ghost"
                      size="icon"
                      className="h-6 w-6"
                      onClick={() => setIsChatSidebarOpen(false)}
                      title="Collapse chats"
                    >
                      <ChevronLeft className="h-3.5 w-3.5" />
                    </Button>
                  </div>
                </div>
                <div className="min-h-0 flex-1 overflow-y-auto p-1.5">
                  {chats.map((chat, index) => {
                    const isActive = chat.id === activeChatId;
                    const isEditing = chat.id === editingTitleId;
                    return (
                      <div key={chat.id} className="group relative">
                        {isEditing ? (
                          <input
                            autoFocus
                            value={titleDraft}
                            onChange={(event) => setTitleDraft(event.target.value)}
                            onFocus={(event) => event.currentTarget.select()}
                            onBlur={() => void saveTitleEdit(chat.id)}
                            onKeyDown={(event) => {
                              if (event.key === "Enter") {
                                event.preventDefault();
                                void saveTitleEdit(chat.id);
                              } else if (event.key === "Escape") {
                                event.preventDefault();
                                cancelTitleEdit();
                              }
                            }}
                            placeholder={chatTitle(chat, index)}
                            className="mb-1 h-11 w-full border border-[var(--node-border-hover)] bg-[var(--input-bg)] px-2 text-left text-xs text-[var(--foreground)] outline-none placeholder:text-[var(--text-muted)]"
                          />
                        ) : (
                          <button
                            type="button"
                            onClick={() => onSelectChat(chat.id)}
                            onDoubleClick={() => startTitleEdit(chat)}
                            className={cn(
                              "mb-1 flex h-11 w-full min-w-0 flex-col items-start justify-center bg-transparent px-2 pr-7 text-left transition-colors",
                              isActive
                                ? "font-semibold text-[var(--foreground)]"
                                : "text-[var(--text-muted)] hover:text-[var(--foreground)]",
                            )}
                          >
                            <span className="w-full truncate text-xs font-medium">{chatTitle(chat, index)}</span>
                          </button>
                        )}
                        {!isEditing && (
                          <button
                            type="button"
                            onPointerDown={(event) => event.stopPropagation()}
                            onClick={(event) => {
                              event.stopPropagation();
                              setOpenChatMenuId((current) => current === chat.id ? null : chat.id);
                            }}
                            className={cn(
                              "absolute right-1 top-1/2 hidden h-6 w-6 -translate-y-1/2 items-center justify-center bg-transparent text-[var(--text-muted)] hover:text-[var(--foreground)] group-hover:flex",
                              openChatMenuId === chat.id && "flex",
                            )}
                            aria-haspopup="menu"
                            aria-expanded={openChatMenuId === chat.id}
                            title="Chat options"
                          >
                            <MoreHorizontal className="h-3.5 w-3.5" />
                          </button>
                        )}
                        {openChatMenuId === chat.id && (
                          <ChatOptionsMenu
                            options={chatMenuOptions(chat)}
                            onClose={() => setOpenChatMenuId(null)}
                          />
                        )}
                      </div>
                    );
                  })}
                </div>
                {titleError && <p className="border-t border-[var(--border)] px-3 py-2 text-[11px] text-[#ff5f5f]">{titleError}</p>}
              </>
            ) : (
              <div className="flex h-full flex-col items-center gap-2 py-2">
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-7 w-7"
                  onClick={() => setIsChatSidebarOpen(true)}
                  title="Expand chats"
                >
                  <ChevronRight className="h-3.5 w-3.5" />
                </Button>
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-7 w-7"
                  onClick={() => void onCreateChat()}
                  title="New chat"
                >
                  <Plus className="h-3.5 w-3.5" />
                </Button>
              </div>
            )}
            </aside>
          )}
          <div className="flex min-w-0 flex-1 flex-col overflow-hidden">
            {!isFloating && (
              <div className="flex min-h-12 items-center justify-between gap-3 border-b border-[var(--border)] bg-[var(--surface-raised)] px-4">
                <div className="min-w-0 flex-1">
                  {isEditingActiveTitle ? (
                    <input
                      autoFocus
                      value={titleDraft}
                      onChange={(event) => setTitleDraft(event.target.value)}
                      onFocus={(event) => event.currentTarget.select()}
                      onBlur={() => void saveTitleEdit(session.id)}
                      onKeyDown={(event) => {
                        if (event.key === "Enter") {
                          event.preventDefault();
                          void saveTitleEdit(session.id);
                        } else if (event.key === "Escape") {
                          event.preventDefault();
                          cancelTitleEdit();
                        }
                      }}
                      placeholder={activeChatTitle}
                      className="h-8 w-full max-w-sm border border-[var(--node-border-hover)] bg-[var(--input-bg)] px-2 text-sm font-medium text-[var(--foreground)] outline-none placeholder:text-[var(--text-muted)]"
                    />
                  ) : (
                    <button
                      type="button"
                      className="min-w-0 bg-transparent text-left"
                      onClick={() => startTitleEdit(session)}
                      title="Rename chat"
                    >
                      <span className="block truncate text-sm font-semibold text-[var(--foreground)]">{activeChatTitle}</span>
                      <span className="block truncate text-[11px] text-[var(--text-muted)]">{directoryName}</span>
                    </button>
                  )}
                </div>
                {titleError && <span className="shrink-0 text-[11px] text-[#ff5f5f]">{titleError}</span>}
              </div>
            )}
            <ScrollArea className="min-h-0 min-w-0 flex-1 px-6 py-4" viewportRef={scrollViewportRef}>
              <div className="min-w-0 max-w-full space-y-4 overflow-hidden">
                {messages.length === 0 ? (
                  <div className="flex h-full items-center justify-center">
                  </div>
                ) : (
                  messages.map((message, index) => {
                const isAssistant = message.role === "assistant";
                const reasoning = message.reasoning ?? "";
                const content = message.content ?? "";
                const parts = message.parts ?? [];
                const hasReasoningPart = parts.some((part) => part.type === "reasoning");

                if (message.role === "system" && content.includes("<conversation-checkpoint>")) {
                  return (
                    <ContextCheckpoint key={message.id ?? `${message.timestamp}-${index}`} content={content} />
                  );
                }

                return (
                  <div
                    key={message.id ?? `${message.timestamp}-${index}`}
                    className={cn("flex min-w-0 max-w-full", message.role === "user" ? "justify-end" : "justify-start")}
                  >
                    <div
                      className={cn(
                        "min-w-0 text-[12px] leading-relaxed text-[var(--foreground)]",
                        message.role === "user"
                          ? "max-w-[80%] rounded-2xl bg-[var(--foreground)] px-3 py-2 text-[var(--background)] whitespace-pre-wrap break-words"
                          : "w-full max-w-full overflow-hidden",
                      )}
                    >
                      {isAssistant && reasoning && !hasReasoningPart && (
                        <ThinkBlock
                          text={reasoning}
                          defaultOpen={streamingMessageId === message.id}
                          className="mb-2"
                        />
                      )}
                      {isAssistant && parts.length > 0 ? (
                        <AssistantMessageParts
                          parts={parts}
                          fallbackContent={content}
                          defaultThinkingOpen={streamingMessageId === message.id}
                          streaming={streamingMessageId === message.id}
                        />
                      ) : content ? (
                        isAssistant ? (
                          <MarkdownContent text={content} streaming={streamingMessageId === message.id} />
                        ) : (
                          <div className="whitespace-pre-wrap break-words">{content}</div>
                        )
                      ) : null}
                    </div>
                  </div>
                );
                  })
                )}
                {isSending && messages.every((m) => !m.content && !m.reasoning) && (
                  <div className="flex justify-start">
                    <div className="flex items-center gap-2 rounded-none border border-[var(--border)] bg-[var(--surface-raised)] px-4 py-2 text-xs text-[var(--text-muted)]">
                      <span>◌</span>
                      <span>◌</span>
                      <span>◌</span>
                      <span>thinking</span>
                    </div>
                  </div>
                )}
              </div>
            </ScrollArea>

            <div className="bg-[var(--surface-raised)] px-4 py-4">
          {visibleCommandHints.length > 0 && (
            <div className="mx-auto mb-2 max-w-[820px] overflow-hidden rounded-xl border border-[var(--border)] bg-[var(--surface)] p-1">
              {visibleCommandHints.map((hint) => (
                <button
                  key={hint.name}
                  type="button"
                  className="flex w-full items-center gap-3 rounded-lg px-2 py-1.5 text-left text-[11px] text-[var(--text-muted)] hover:bg-[var(--surface-soft)] hover:text-[var(--foreground)]"
                  onMouseDown={(event) => event.preventDefault()}
                  onClick={() => {
                    setInput(hint.usage);
                    inputRef.current?.focus();
                  }}
                >
                  <span className="w-20 shrink-0 text-[var(--foreground)]">{hint.usage}</span>
                  <span className="min-w-0 truncate">{hint.description}</span>
                </button>
              ))}
            </div>
          )}
          <div className="w-full">
            <div className="relative z-10 overflow-hidden rounded-xl border border-[var(--input-border)] bg-[var(--input-bg)] shadow-[0_1px_0_rgba(255,255,255,0.04),0_18px_42px_rgba(0,0,0,0.28)] transition-colors focus-within:border-[var(--node-border-hover)]">
              <textarea
                ref={inputRef}
                value={input}
                onChange={(e) => setInput(e.target.value)}
                onKeyDown={handleKeyDown}
                placeholder="Ask about the codebase..."
                rows={1}
                className="box-border max-h-40 min-h-[76px] w-full resize-none whitespace-pre-wrap break-words break-all bg-transparent px-4 pb-3 pt-4 font-sans text-[13px] leading-[1.55] text-[var(--foreground)] shadow-none outline-none placeholder:text-[var(--text-muted)] disabled:cursor-not-allowed disabled:opacity-50"
              />
            </div>
            <div className="flex min-w-0 items-center justify-between gap-3 px-4 pt-2">
              <div className="flex min-w-0 flex-wrap items-center gap-2 text-[11px]">
                <label className="flex min-w-0 items-center gap-1">
                  <span className="sr-only">Mode</span>
                  <select
                    value={mode}
                    onChange={(event) => setMode(event.target.value as ChatMode)}
                    className={cn(DOCK_CONTROL_CLASS, "w-[5.5rem] uppercase tracking-[0.12em]")}
                    title={
                      mode === "plan"
                        ? "Read-only: shell, write, edit, apply_patch are blocked"
                        : "All tools allowed"
                    }
                  >
                    <option value="plan">Plan</option>
                    <option value="build">Build</option>
                  </select>
                </label>
                <label className="flex min-w-0 items-center gap-1">
                  <span className="sr-only">Provider</span>
                  {providers.length > 0 ? (
                    <select
                      value={providerDraft}
                      onChange={(event) => {
                        const nextProvider = event.target.value;
                        const provider = providerOptions.find((config) => config.name === event.target.value);
                        setProviderDraft(nextProvider);
                        setModelDraft(provider?.model ?? getDefaultModel(nextProvider, modelDraft));
                        setConfigStatus(null);
                        setConfigError(null);
                      }}
                      className={cn(DOCK_CONTROL_CLASS, "w-[6.5rem]")}
                    >
                      {providerOptions.map((provider) => (
                        <option key={provider.name} value={provider.name}>
                          {provider.name}
                        </option>
                      ))}
                    </select>
                  ) : (
                    <input
                      value={providerDraft}
                      onChange={(event) => {
                        setProviderDraft(event.target.value);
                        setConfigStatus(null);
                        setConfigError(null);
                      }}
                      placeholder="anthropic"
                      className={cn(DOCK_CONTROL_CLASS, "w-[6.5rem] placeholder:text-[var(--text-muted)]")}
                    />
                  )}
                </label>
                <label className="flex min-w-0 items-center gap-1">
                  <span className="sr-only">Model</span>
                  <select
                    value={modelDraft}
                    onChange={(event) => {
                      setModelDraft(event.target.value);
                      setConfigStatus(null);
                      setConfigError(null);
                    }}
                    className={cn(DOCK_CONTROL_CLASS, "w-[8rem]")}
                    disabled={modelOptions.length === 0}
                  >
                    {modelOptions.length === 0 ? (
                      <option value="">Add models in config/provider-models.json</option>
                    ) : (
                      modelOptions.map((model) => (
                        <option key={model} value={model}>
                          {model}
                        </option>
                      ))
                    )}
                  </select>
                </label>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-7 rounded-none border-0 bg-transparent px-1 text-[11px] text-[var(--text-muted)] hover:bg-transparent hover:text-[var(--foreground)] disabled:hidden"
                  onClick={saveSessionConfig}
                  disabled={isConfigSaving || !configChanged}
                >
                  <Check className="h-3 w-3" />
                  {isConfigSaving ? "Saving" : "Save"}
                </Button>
              </div>
              <Button
                size="sm"
                className="h-8 shrink-0 rounded-md px-3 text-[11px] font-semibold"
                onClick={handleSend}
                disabled={isCommandRunning || (!isSending && !input.trim())}
              >
                {isSending && !isCommandRunning && <span className="mr-1.5 h-3 w-3 animate-spin rounded-full border border-current border-t-transparent" />}
                {isCommandRunning ? "Run" : commandBufferActive ? "Run" : isSending ? "Stop" : "Send"}
              </Button>
            </div>
          </div>
          {(configStatus || configError) && (
            <p className={cn("mt-1 px-4 text-[11px]", configError ? "text-[#ff5f5f]" : "text-[var(--text-muted)]")}>{configError ?? configStatus}</p>
          )}
          {(commandStatus || commandError) && (
            <p className={cn("mt-1 whitespace-pre-wrap px-4 text-[11px]", commandError ? "text-[#ff5f5f]" : "text-[var(--text-muted)]")}>{commandError ?? commandStatus}</p>
          )}
          {queuedMessageCount > 0 && (
            <p className="mt-2 px-4 text-xs text-[var(--text-muted)]">
              {`${queuedMessageCount} message${queuedMessageCount === 1 ? "" : "s"} queued`}
            </p>
          )}
            </div>
          </div>
          {showDiffPane && (
            <>
              <div
                data-popup-drag-exclude
                role="separator"
                aria-orientation="vertical"
                aria-label="Resize changes pane"
                className="z-10 w-1 shrink-0 cursor-col-resize border-l border-[var(--border)] bg-[var(--surface)] hover:bg-[var(--node-border-hover)] active:bg-[var(--node-border-hover)]"
                onPointerDown={handleDiffResizeStart}
                onPointerMove={handleDiffResizeMove}
                onPointerUp={handleDiffResizeEnd}
                onPointerCancel={handleDiffResizeEnd}
              />
              {!isDiffPaneCollapsed && (
                <SessionDiffPane
                  diffs={diffs}
                  width={diffPaneWidth}
                  onClose={() => setDiffPaneWidth(0)}
                />
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function chatTitle(chat: AgentSession, index: number) {
  const title = chat.title?.trim();
  return title || `Chat ${index + 1}`;
}

function AssistantMessageParts({
  parts,
  fallbackContent,
  defaultThinkingOpen,
  streaming,
}: {
  parts: ChatMessagePart[];
  fallbackContent: string;
  defaultThinkingOpen: boolean;
  streaming: boolean;
}) {
  const results = new Map<string, Extract<ChatMessagePart, { type: "tool_result" }>>();
  const resultsByName = new Map<string, Extract<ChatMessagePart, { type: "tool_result" }>>();
  for (const part of parts) {
    if (part.type === "tool_result") {
      results.set(part.id, part);
      if (part.name) resultsByName.set(part.name, part);
    }
  }

  const visibleParts = parts.filter((part) => part.type !== "tool_result");
  if (visibleParts.length === 0 && fallbackContent) {
    return <MarkdownContent text={fallbackContent} streaming={streaming} />;
  }

  return (
    <div className="min-w-0 max-w-full space-y-2 overflow-hidden">
      {visibleParts.map((part, index) => {
        if (part.type === "text") {
          return part.text ? (
            <MarkdownContent key={`text-${index}`} text={part.text} streaming={streaming} />
          ) : null;
        }
        if (part.type === "reasoning") {
          return part.text ? (
            <ThinkBlock
              key={`reasoning-${index}`}
              text={part.text}
              defaultOpen={defaultThinkingOpen}
            />
          ) : null;
        }
        if (part.type === "tool_call") {
          return <ToolCallBlock key={part.id || `tool-${index}`} call={part} result={results.get(part.id) ?? resultsByName.get(part.name)} />;
        }
        if (part.type === "tool_error") {
          return <ToolErrorBlock key={part.id || `tool-error-${index}`} error={part} />;
        }
        return null;
      })}
    </div>
  );
}

function ToolCallBlock({
  call,
  result,
}: {
  call: Extract<ChatMessagePart, { type: "tool_call" }>;
  result?: Extract<ChatMessagePart, { type: "tool_result" }>;
}) {
  const details = toolDetails(call.name, call.input);
  const resultSummary = summarizeToolResult(result);
  const metadata = toolResultMetadata(result);
  const resultLang = toolResultLang(call.name, call.input);
  const status = resultSummary?.isError ? "failed" : result ? "done" : "running";
  const Icon = details.icon;
  const showDiffs = !resultSummary?.isError && (call.name === "write" || call.name === "apply_patch");
  const readRange = call.name === "read" ? readRangeInfo(call.input, resultSummary?.text, metadata) : null;

  return (
    <div className="w-full min-w-0 max-w-full overflow-hidden rounded-none border border-[var(--node-border)] bg-[var(--input-bg)] font-mono text-xs">
      <div className="flex items-center justify-between border-b border-[var(--border)] bg-[var(--surface)] px-3 py-2">
        <div className="flex min-w-0 flex-1 items-center gap-2 overflow-hidden">
          <Icon className="h-3.5 w-3.5 shrink-0 text-[var(--text-muted)]" />
          <span className="min-w-0 shrink-0 truncate text-[var(--foreground)]">{call.name}</span>
          <span className="min-w-0 truncate text-[var(--text-muted)]">{details.label}</span>
          {metadata && (metadata.additions > 0 || metadata.deletions > 0) && (
            <span className="text-[10px] text-[var(--text-muted)]">
              <span className="text-[#7ddc8a]">+{metadata.additions}</span>{" "}
              <span className="text-[#ff5f5f]">-{metadata.deletions}</span>
            </span>
          )}
        </div>
        <span
          className={cn(
            "ml-3 shrink-0 text-[10px] uppercase tracking-[0.18em]",
            status === "failed" ? "text-[#ff5f5f]" : status === "done" ? "text-[#7ddc8a]" : "text-[#d6b85a]",
          )}
        >
          {status}
        </span>
      </div>
      <div className="min-w-0 max-w-full space-y-2 overflow-hidden px-3 py-2">
        <ThemedCodeBlock code={details.command} lang={details.lang} />
        {readRange && <ToolReadRange range={readRange} />}
        {resultSummary?.text && (
          <ThemedCodeBlock code={resultSummary.text} lang={resultSummary.isError ? "text" : resultLang} className="max-h-40" />
        )}
        {showDiffs && metadata && <ToolDiffSections metadata={metadata} />}
      </div>
    </div>
  );
}

interface ToolReadRangeInfo {
  file: string;
  label: string;
  truncated: boolean;
}

function ToolReadRange({ range }: { range: ToolReadRangeInfo }) {
  return (
    <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 border border-[var(--border)] bg-[var(--surface)] px-3 py-2 text-[10px] uppercase tracking-[0.14em] text-[var(--text-muted)]">
      <span className="min-w-0 truncate normal-case tracking-normal text-[var(--text-secondary)]">{range.file || "read result"}</span>
      <span>{range.label}</span>
      {range.truncated && <span className="text-[#d6b85a]">truncated</span>}
    </div>
  );
}

function ToolDiffSections({ metadata }: { metadata: ToolResultMetadata }) {
  const files = metadata.files.length > 0 ? metadata.files : metadata.diff ? [{
    file: metadata.file || "diff",
    diff: metadata.diff,
    additions: metadata.additions,
    deletions: metadata.deletions,
  }] : [];

  if (files.length === 0) return null;

  return (
    <div className="min-w-0 space-y-2 overflow-hidden">
      {files.map((file, index) => (
        <div key={`${file.file}-${index}`} className="min-w-0 overflow-hidden">
          <div className="flex min-w-0 items-center justify-between gap-2 border border-b-0 border-[var(--border)] bg-[var(--surface)] px-3 py-2 font-mono text-[10px] text-[var(--text-muted)]">
            <span className="min-w-0 truncate">{file.file}</span>
            {(file.additions > 0 || file.deletions > 0) && (
              <span className="shrink-0">
                <span className="text-[#7ddc8a]">+{file.additions}</span>{" "}
                <span className="text-[#ff5f5f]">-{file.deletions}</span>
              </span>
            )}
          </div>
          <DiffBlock diff={file.diff} />
        </div>
      ))}
    </div>
  );
}

function ToolErrorBlock({ error }: { error: Extract<ChatMessagePart, { type: "tool_error" }> }) {
  return (
    <div className="rounded-none border border-[#8a2f2f] bg-[var(--surface)] px-3 py-2 font-mono text-xs text-[#d14d4d]">
      <div className="mb-1 text-[10px] uppercase tracking-[0.18em] text-[#ff5f5f]">{error.name ?? "tool"} failed</div>
      <ThemedCodeBlock code={error.message} lang="text" />
    </div>
  );
}

function toolDetails(name: string, input: unknown): { icon: typeof Terminal; label: string; command: string; lang: string } {
  const args = isRecord(input) ? input : {};
  if (name === "shell") {
    const command = stringArg(args.command) || "shell";
    const workdir = stringArg(args.workdir);
    return {
      icon: Terminal,
      label: workdir ? `in ${workdir}` : "command",
      command: workdir ? `$ ${command}\n# cwd: ${workdir}` : `$ ${command}`,
      lang: "shellscript",
    };
  }
  if (name === "grep") {
    const pattern = stringArg(args.pattern) || "<pattern>";
    const path = stringArg(args.path) || ".";
    const include = stringArg(args.include);
    return {
      icon: Search,
      label: "search",
      command: `grep ${quoteArg(pattern)} ${path}${include ? ` --include ${include}` : ""}`,
      lang: "shellscript",
    };
  }
  if (name === "glob") {
    const pattern = stringArg(args.pattern) || "**/*";
    const path = stringArg(args.path) || ".";
    return {
      icon: FolderSearch,
      label: "match files",
      command: `glob ${quoteArg(pattern)} ${path}`,
      lang: "shellscript",
    };
  }
  if (name === "read") {
    return {
      icon: Wrench,
      label: "read file",
      command: stringArg(args.path) || JSON.stringify(input ?? {}, null, 2),
      lang: "text",
    };
  }
  if (name === "write") {
    return {
      icon: Wrench,
      label: "write file",
      command: stringArg(args.path) || JSON.stringify(input ?? {}, null, 2),
      lang: "text",
    };
  }
  if (name === "apply_patch") {
    return {
      icon: Wrench,
      label: "apply patch",
      command: summarizePatchText(stringArg(args.patchText) || stringArg(args.patch_text) || stringArg(args.patch)),
      lang: "text",
    };
  }
  return {
    icon: Wrench,
    label: "tool call",
    command: JSON.stringify(input ?? {}, null, 2),
    lang: "json",
  };
}

interface ToolDiffMetadata {
  file: string;
  diff: string;
  additions: number;
  deletions: number;
}

interface ToolResultMetadata {
  file: string;
  diff: string;
  additions: number;
  deletions: number;
  files: ToolDiffMetadata[];
  offset: number;
  limit: number | null;
  startLine: number;
  endLine: number;
  returnedLines: number;
  totalLines: number;
  truncated: boolean;
}

function toolResultMetadata(result?: Extract<ChatMessagePart, { type: "tool_result" }>): ToolResultMetadata | null {
  const value = unwrapResultValue(result?.result);
  if (!isRecord(value) || !isRecord(value.metadata)) return null;
  const diff = stringArg(value.metadata.diff);
  const files = Array.isArray(value.metadata.files)
    ? value.metadata.files.flatMap((item) => {
        if (!isRecord(item)) return [];
        const fileDiff = stringArg(item.diff);
        if (!fileDiff) return [];
        return [{
          file: stringArg(item.file) || "diff",
          diff: fileDiff,
          additions: numberArg(item.additions),
          deletions: numberArg(item.deletions),
        }];
      })
    : [];
  const metadata: ToolResultMetadata = {
    file: stringArg(value.metadata.file),
    diff,
    additions: numberArg(value.metadata.additions),
    deletions: numberArg(value.metadata.deletions),
    files,
    offset: numberArg(value.metadata.offset),
    limit: nullableNumberArg(value.metadata.limit),
    startLine: numberArg(value.metadata.start_line),
    endLine: numberArg(value.metadata.end_line),
    returnedLines: numberArg(value.metadata.returned_lines),
    totalLines: numberArg(value.metadata.total_lines),
    truncated: booleanArg(value.metadata.truncated),
  };
  if (!metadata.diff && metadata.files.length === 0 && !metadata.file && metadata.offset === 0) return null;
  return metadata;
}

function formatReadRange(metadata: ToolResultMetadata): string {
  if (metadata.returnedLines === 0) {
    return metadata.totalLines > 0
      ? `offset ${metadata.offset || 1} beyond ${metadata.totalLines} lines`
      : "empty file";
  }
  const range = metadata.startLine === metadata.endLine
    ? `line ${metadata.startLine}`
    : `lines ${metadata.startLine}-${metadata.endLine}`;
  const total = metadata.totalLines > 0 ? ` of ${metadata.totalLines}` : "";
  const limit = metadata.limit ? ` · limit ${metadata.limit}` : "";
  return `${range}${total}${limit}`;
}

function readRangeInfo(input: unknown, output: string | undefined, metadata: ToolResultMetadata | null): ToolReadRangeInfo | null {
  const args = isRecord(input) ? input : {};
  const file = metadata?.file || stringArg(args.path);
  if (metadata) {
    return {
      file,
      label: formatReadRange(metadata),
      truncated: metadata.truncated,
    };
  }

  const inputOffset = numberArg(args.offset) || 1;
  const inputLimit = nullableNumberArg(args.limit);
  const outputRange = output ? readRangeFromOutput(output) : null;
  if (outputRange) {
    const range = outputRange.start === outputRange.end
      ? `line ${outputRange.start}`
      : `lines ${outputRange.start}-${outputRange.end}`;
    const limit = inputLimit ? ` · limit ${inputLimit}` : "";
    return { file, label: `${range}${limit}`, truncated: false };
  }
  if (file || inputOffset !== 1 || inputLimit) {
    return {
      file,
      label: inputLimit ? `offset ${inputOffset} · limit ${inputLimit}` : `offset ${inputOffset}`,
      truncated: false,
    };
  }
  return null;
}

function readRangeFromOutput(output: string): { start: number; end: number } | null {
  const lines = output.split("\n");
  let start: number | null = null;
  let end: number | null = null;
  for (const line of lines) {
    const match = line.match(/^(\d+):/);
    if (!match) continue;
    const lineNumber = Number(match[1]);
    if (!Number.isFinite(lineNumber)) continue;
    start ??= lineNumber;
    end = lineNumber;
  }
  return start === null || end === null ? null : { start, end };
}

function nullableNumberArg(value: unknown): number | null {
  if (value === null || value === undefined) return null;
  const number = numberArg(value);
  return number === 0 ? null : number;
}

function booleanArg(value: unknown): boolean {
  return typeof value === "boolean" ? value : false;
}

function toolResultLang(name: string, input: unknown): string {
  const args = isRecord(input) ? input : {};
  if (name === "read" || name === "write") return languageForPath(stringArg(args.path));
  if (name === "shell") return "console";
  if (name === "apply_patch") return "diff";
  return "text";
}

function languageForPath(path: string): string {
  const extension = path.split(".").pop()?.toLowerCase();
  if (!extension || extension === path) return "text";
  return LANGUAGE_BY_EXTENSION[extension] ?? extension;
}

const LANGUAGE_BY_EXTENSION: Record<string, string> = {
  cjs: "javascript",
  css: "css",
  go: "go",
  h: "c",
  hpp: "cpp",
  html: "html",
  java: "java",
  js: "javascript",
  json: "json",
  jsonc: "jsonc",
  jsx: "jsx",
  md: "markdown",
  mdx: "mdx",
  mjs: "javascript",
  py: "python",
  rb: "ruby",
  rs: "rust",
  sh: "shellscript",
  toml: "toml",
  ts: "typescript",
  tsx: "tsx",
  txt: "text",
  yaml: "yaml",
  yml: "yaml",
};

function summarizeToolResult(result?: Extract<ChatMessagePart, { type: "tool_result" }>): { text: string; isError: boolean } | null {
  if (!result) return null;
  const value = unwrapResultValue(result.result);
  if (isRecord(value)) {
    const error = stringArg(value.error);
    if (error) return { text: error, isError: true };
    const text = stringArg(value.text) || stringArg(value.output);
    if (text) return { text, isError: false };
  }
  if (typeof value === "string") return { text: value, isError: false };
  if (result.output) return { text: result.output, isError: false };
  return { text: JSON.stringify(value ?? result.result ?? {}, null, 2), isError: false };
}

function unwrapResultValue(result: unknown): unknown {
  if (isRecord(result) && "value" in result && typeof result.type === "string") {
    return result.value;
  }
  return result;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function stringArg(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function numberArg(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function quoteArg(value: string): string {
  return /\s/.test(value) ? JSON.stringify(value) : value;
}

function summarizePatchText(patch: string): string {
  if (!patch.trim()) return "patch";
  const changed = patch
    .split("\n")
    .map((line) => line.match(/^\*\*\* (?:Add|Update|Delete) File: (.+)$/)?.[1])
    .filter((path): path is string => Boolean(path));
  if (changed.length === 0) return "patch";
  if (changed.length === 1) return changed[0] ?? "patch";
  return `${changed.length} files`;
}

function initialChatPosition() {
  if (typeof window === "undefined") {
    return { x: 360, y: 80 };
  }

  return clampChatPosition({
    x: window.innerWidth - CHAT_WIDTH - 24,
    y: 80,
  });
}

function clampChatPosition(position: { x: number; y: number }) {
  if (typeof window === "undefined") {
    return position;
  }

  const width = Math.min(CHAT_WIDTH, window.innerWidth - EDGE_PADDING * 2);
  const height = Math.min(CHAT_HEIGHT, window.innerHeight - EDGE_PADDING * 2);
  const maxX = Math.max(EDGE_PADDING, window.innerWidth - width - EDGE_PADDING);
  const maxY = Math.max(EDGE_PADDING, window.innerHeight - height - EDGE_PADDING);

  return {
    x: Math.min(Math.max(EDGE_PADDING, position.x), maxX),
    y: Math.min(Math.max(EDGE_PADDING, position.y), maxY),
  };
}

function clampDiffPaneWidth(width: number) {
  if (width <= DIFF_PANE_SNAP_THRESHOLD) return DIFF_PANE_MIN_WIDTH;
  return Math.min(Math.max(DIFF_PANE_USABLE_MIN_WIDTH, width), DIFF_PANE_MAX_WIDTH);
}

function isPopupDragExcluded(target: EventTarget) {
  if (!(target instanceof Element)) return false;
  return target.closest(
    [
      "button",
      "input",
      "textarea",
      "select",
      "a",
      "[role='button']",
      "[role='menu']",
      "[data-radix-scroll-area-viewport]",
      "[data-popup-drag-exclude]",
    ].join(","),
  ) !== null;
}

function formatError(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
