import { invoke } from "@tauri-apps/api/core";
import { Check, ChevronDown, ChevronLeft, ChevronRight, FolderSearch, GripHorizontal, MoreHorizontal, Plus, Search, Terminal, Wrench, X } from "lucide-react";
import { useEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import { cn } from "@/lib/utils";
import { getDefaultModel, getModelOptions } from "@/features/sessions/providerModels";
import type { ChatMessagePart } from "@/features/sessions/conversationStore";
import type { AgentSession, ProviderConfig } from "@/features/sessions/sessionStore";
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
  onSendMessage: (content: string, mode: ChatMode) => void | Promise<void>;
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
const CONTROL_LABEL_CLASS = "text-[10px] uppercase tracking-[0.18em] text-[var(--text-muted)]";
const TRANSPARENT_CONTROL_CLASS =
  "min-w-0 bg-transparent py-1 text-[11px] text-[var(--text-muted)] outline-none transition-colors hover:text-[var(--foreground)] focus:text-[var(--foreground)] disabled:cursor-not-allowed disabled:opacity-40 [&>option]:bg-[var(--surface-raised)] [&>option]:text-[var(--foreground)]";
const COMPOSER_SELECT_CLASS = "w-[5.75rem]";

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
  onSendMessage,
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
  const dragRef = useRef<{
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

  const handleDragStart = (event: PointerEvent<HTMLDivElement>) => {
    dragRef.current = {
      pointerId: event.pointerId,
      startX: event.clientX,
      startY: event.clientY,
      originX: position.x,
      originY: position.y,
    };
    event.currentTarget.setPointerCapture(event.pointerId);
  };

  const handleDragMove = (event: PointerEvent<HTMLDivElement>) => {
    const drag = dragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) return;

    setPosition(clampChatPosition({
      x: drag.originX + event.clientX - drag.startX,
      y: drag.originY + event.clientY - drag.startY,
    }));
  };

  const handleDragEnd = (event: PointerEvent<HTMLDivElement>) => {
    if (dragRef.current?.pointerId === event.pointerId) {
      dragRef.current = null;
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

  const startTitleEdit = (chat: AgentSession) => {
    setEditingTitleId(chat.id);
    setTitleDraft(chat.title?.trim() ?? "");
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
  const isFloating = variant === "floating";
  const showChatSidebar = isFloating;

  return (
    <div className={isFloating ? "pointer-events-none fixed inset-0 z-50" : "flex min-h-0 flex-1"}>
      <div
        className={cn(
          "pointer-events-auto flex flex-col overflow-hidden rounded-none border border-[var(--border)] bg-[var(--surface-raised)] text-[var(--foreground)] shadow-none",
          isFloating
            ? "fixed h-[600px] max-h-[calc(100vh-32px)] w-[760px] max-w-[calc(100vw-32px)]"
            : "h-full w-full border-0 border-l",
        )}
        role={isFloating ? "dialog" : "region"}
        aria-modal={isFloating ? "false" : undefined}
        aria-label={`${directoryName} chat`}
        style={isFloating ? { left: position.x, top: position.y } : undefined}
      >
        <div
          className={cn(
            "flex items-center justify-between border-b border-[var(--border)] px-6 py-4",
            isFloating && "cursor-grab active:cursor-grabbing",
          )}
          onPointerDown={isFloating ? handleDragStart : undefined}
          onPointerMove={isFloating ? handleDragMove : undefined}
          onPointerUp={isFloating ? handleDragEnd : undefined}
          onPointerCancel={isFloating ? handleDragEnd : undefined}
        >
          <div className="flex min-w-0 items-center gap-3">
            {isFloating && <GripHorizontal className="h-4 w-4 shrink-0 text-[var(--text-muted)]" />}
            <div className="flex min-w-0 flex-col">
              <h2 className="truncate text-sm font-semibold text-[var(--foreground)]">{directoryName}</h2>
              <p className="truncate text-xs text-[var(--text-muted)]">{rootSession.directory}</p>
            </div>
          </div>
          {isFloating && onClose && (
            <div className="flex shrink-0 items-center gap-2">
              <Button
                variant="ghost"
                size="icon"
                onPointerDown={(event) => event.stopPropagation()}
                onClick={onClose}
              >
                <X className="h-4 w-4" />
              </Button>
            </div>
          )}
        </div>
        <div className="flex min-h-0 flex-1">
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
                  {chats.map((chat) => {
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
                            placeholder="Unknown"
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
                            <span className="w-full truncate text-xs font-medium">{chatTitle(chat)}</span>
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
          <div className="flex min-w-0 flex-1 flex-col">
            <ScrollArea className="flex-1 px-6 py-4" viewportRef={scrollViewportRef}>
              <div className="space-y-4">
                {messages.length === 0 ? (
                  <div className="flex h-full items-center justify-center">
                    <p className="text-sm text-[var(--text-muted)]">
                      Chat with {directoryName} session. Ask about the codebase or files in this directory.
                    </p>
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
                    className={cn("flex", message.role === "user" ? "justify-end" : "justify-start")}
                  >
                    <div
                      className={cn(
                        "min-w-0 max-w-[80%] text-[12px] leading-relaxed text-[var(--foreground)]",
                        message.role === "user"
                          ? "rounded-2xl bg-[var(--foreground)] px-3 py-2 text-[var(--background)] whitespace-pre-wrap break-words"
                          : "overflow-hidden",
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
                        />
                      ) : content ? (
                        <div className="whitespace-pre-wrap break-words">{content}</div>
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

            <div className="border-t border-[var(--border)] px-6 py-4">
          {visibleCommandHints.length > 0 && (
            <div className="mb-2 overflow-hidden rounded-xl border border-[var(--border)] bg-[var(--surface)] p-1">
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
          <div className="rounded-2xl border border-[var(--input-border)] bg-[var(--input-bg)] px-3 py-2 transition-colors focus-within:border-[var(--node-border-hover)]">
            <textarea
              ref={inputRef}
              value={input}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={handleKeyDown}
              placeholder="Ask about the codebase..."
              rows={1}
              className="box-border max-h-32 min-h-[42px] w-full resize-none whitespace-pre-wrap break-words break-all bg-transparent py-1 font-sans text-[12px] leading-[1.5] text-[var(--foreground)] shadow-none outline-none placeholder:text-[var(--text-muted)] disabled:cursor-not-allowed disabled:opacity-50"
            />
            <div className="flex min-w-0 items-center justify-between gap-2">
              <div className="flex min-w-0 flex-wrap items-center gap-x-1 gap-y-1 text-[11px]">
                <label className="flex min-w-0 items-center gap-1">
                  <span className={CONTROL_LABEL_CLASS}>Mode</span>
                  <select
                    value={mode}
                    onChange={(event) => setMode(event.target.value as ChatMode)}
                    className={cn(TRANSPARENT_CONTROL_CLASS, COMPOSER_SELECT_CLASS, "uppercase tracking-[0.12em]")}
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
                  <span className={CONTROL_LABEL_CLASS}>Provider</span>
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
                      className={cn(TRANSPARENT_CONTROL_CLASS, COMPOSER_SELECT_CLASS)}
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
                      className={cn(TRANSPARENT_CONTROL_CLASS, COMPOSER_SELECT_CLASS, "placeholder:text-[var(--text-muted)]")}
                    />
                  )}
                </label>
                <label className="flex min-w-0 items-center gap-1">
                  <span className={CONTROL_LABEL_CLASS}>Model</span>
                  <select
                    value={modelDraft}
                    onChange={(event) => {
                      setModelDraft(event.target.value);
                      setConfigStatus(null);
                      setConfigError(null);
                    }}
                    className={cn(TRANSPARENT_CONTROL_CLASS, COMPOSER_SELECT_CLASS)}
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
                  className="h-6 px-1 text-[11px] text-[var(--text-muted)] hover:bg-transparent hover:text-[var(--foreground)] disabled:hidden"
                  onClick={saveSessionConfig}
                  disabled={isConfigSaving || !configChanged}
                >
                  <Check className="h-3 w-3" />
                  {isConfigSaving ? "Saving" : "Save"}
                </Button>
              </div>
              <Button
                size="sm"
                className="h-7 shrink-0 rounded-full px-3 text-[11px]"
                onClick={handleSend}
                disabled={!input.trim() || isCommandRunning}
              >
                {isSending && !isCommandRunning && <span className="mr-1.5 h-3 w-3 animate-spin rounded-full border border-current border-t-transparent" />}
                {isCommandRunning ? "Run" : commandBufferActive ? "Run" : isSending ? "Queue" : "Send"}
              </Button>
            </div>
          </div>
          {(configStatus || configError) && (
            <p className={cn("mt-1 text-[11px]", configError ? "text-[#ff5f5f]" : "text-[var(--text-muted)]")}>{configError ?? configStatus}</p>
          )}
          {(commandStatus || commandError) && (
            <p className={cn("mt-1 whitespace-pre-wrap text-[11px]", commandError ? "text-[#ff5f5f]" : "text-[var(--text-muted)]")}>{commandError ?? commandStatus}</p>
          )}
          {queuedMessageCount > 0 && (
            <p className="mt-2 text-xs text-[var(--text-muted)]">
              {`${queuedMessageCount} message${queuedMessageCount === 1 ? "" : "s"} queued`}
            </p>
          )}
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}

function chatTitle(chat: AgentSession) {
  const title = chat.title?.trim();
  return title || "Unknown";
}

function AssistantMessageParts({
  parts,
  fallbackContent,
  defaultThinkingOpen,
}: {
  parts: ChatMessagePart[];
  fallbackContent: string;
  defaultThinkingOpen: boolean;
}) {
  const results = new Map<string, Extract<ChatMessagePart, { type: "tool_result" }>>();
  for (const part of parts) {
    if (part.type === "tool_result") {
      results.set(part.id, part);
    }
  }

  const visibleParts = parts.filter((part) => part.type !== "tool_result");
  if (visibleParts.length === 0 && fallbackContent) {
    return <div className="whitespace-pre-wrap break-words">{fallbackContent}</div>;
  }

  return (
    <div className="min-w-0 max-w-full space-y-2 overflow-hidden">
      {visibleParts.map((part, index) => {
        if (part.type === "text") {
          return part.text ? (
            <div key={`text-${index}`} className="whitespace-pre-wrap break-words">
              {part.text}
            </div>
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
          return <ToolCallBlock key={part.id || `tool-${index}`} call={part} result={results.get(part.id)} />;
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
  const status = resultSummary?.isError ? "failed" : result ? "done" : "running";
  const Icon = details.icon;

  return (
    <div className="min-w-0 max-w-full overflow-hidden rounded-none border border-[var(--node-border)] bg-[var(--input-bg)] font-mono text-xs">
      <div className="flex items-center justify-between border-b border-[var(--border)] bg-[var(--surface)] px-3 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <Icon className="h-3.5 w-3.5 shrink-0 text-[var(--text-muted)]" />
          <span className="truncate text-[var(--foreground)]">{call.name}</span>
          <span className="text-[var(--text-muted)]">{details.label}</span>
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
      <div className="space-y-2 px-3 py-2">
        <pre className="max-w-full whitespace-pre-wrap break-all text-[var(--text-secondary)]">{details.command}</pre>
        {resultSummary?.text && (
          <pre className="max-h-40 max-w-full overflow-auto whitespace-pre-wrap break-all border-t border-[var(--border)] pt-2 text-[var(--text-muted)]">
            {resultSummary.text}
          </pre>
        )}
      </div>
    </div>
  );
}

function ToolErrorBlock({ error }: { error: Extract<ChatMessagePart, { type: "tool_error" }> }) {
  return (
    <div className="rounded-none border border-[#8a2f2f] bg-[var(--surface)] px-3 py-2 font-mono text-xs text-[#d14d4d]">
      <div className="mb-1 text-[10px] uppercase tracking-[0.18em] text-[#ff5f5f]">{error.name ?? "tool"} failed</div>
      <pre className="whitespace-pre-wrap break-words">{error.message}</pre>
    </div>
  );
}

function toolDetails(name: string, input: unknown): { icon: typeof Terminal; label: string; command: string } {
  const args = isRecord(input) ? input : {};
  if (name === "shell") {
    const command = stringArg(args.command) || "shell";
    const workdir = stringArg(args.workdir);
    return {
      icon: Terminal,
      label: workdir ? `in ${workdir}` : "command",
      command: workdir ? `$ ${command}\n# cwd: ${workdir}` : `$ ${command}`,
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
    };
  }
  if (name === "glob") {
    const pattern = stringArg(args.pattern) || "**/*";
    const path = stringArg(args.path) || ".";
    return {
      icon: FolderSearch,
      label: "match files",
      command: `glob ${quoteArg(pattern)} ${path}`,
    };
  }
  if (name === "read") {
    return {
      icon: Wrench,
      label: "read file",
      command: stringArg(args.path) || JSON.stringify(input ?? {}, null, 2),
    };
  }
  return {
    icon: Wrench,
    label: "tool call",
    command: JSON.stringify(input ?? {}, null, 2),
  };
}

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

function quoteArg(value: string): string {
  return /\s/.test(value) ? JSON.stringify(value) : value;
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

function formatError(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
