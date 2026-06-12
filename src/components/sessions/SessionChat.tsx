import { invoke } from "@tauri-apps/api/core";
import { Check, ChevronDown, ChevronRight, FolderSearch, GripHorizontal, Search, Terminal, Wrench, X } from "lucide-react";
import { useEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
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

interface SessionChatProps {
  session: AgentSession;
  messages: SessionChatMessage[];
  isSending: boolean;
  isCompacting: boolean;
  streamingMessageId: string | null;
  onSendMessage: (content: string, mode: ChatMode) => void | Promise<void>;
  onUpdateSessionProvider: (sessionId: string, provider: string, model: string) => Promise<void>;
  onCompact: () => void | Promise<void>;
  onClose: () => void;
}

const CHAT_WIDTH = 700;
const CHAT_HEIGHT = 600;
const EDGE_PADDING = 16;

function ContextCheckpoint({ content }: { content: string }) {
  const [open, setOpen] = useState(false);
  const match = content.match(/<conversation-checkpoint>([\s\S]*?)<\/conversation-checkpoint>/);
  const summary = match ? match[1].trim() : content;
  return (
    <div className="flex justify-center" data-testid="context-checkpoint">
      <div className="w-full max-w-[80%] rounded-none border border-dashed border-[#3a3a3a] bg-[#0d0d0d] px-4 py-2 text-xs text-[#a0a0a0]">
        <button
          type="button"
          onClick={() => setOpen((value) => !value)}
          className="flex w-full items-center justify-between gap-2 text-left"
        >
          <span className="flex items-center gap-2 text-[#f5f5f5]">
            {open ? <ChevronDown className="h-3 w-3" /> : <ChevronRight className="h-3 w-3" />}
            Context compacted
          </span>
          <span className="text-[10px] uppercase tracking-wide text-[#737373]">summary</span>
        </button>
        {open && (
          <pre className="mt-2 max-h-64 overflow-y-auto whitespace-pre-wrap break-words text-[11px] leading-relaxed text-[#cfcfcf]">
            {summary}
          </pre>
        )}
      </div>
    </div>
  );
}

export function SessionChat({
  session,
  messages,
  isSending,
  isCompacting,
  streamingMessageId,
  onSendMessage,
  onUpdateSessionProvider,
  onCompact,
  onClose,
}: SessionChatProps) {
  const [input, setInput] = useState("");
  const [position, setPosition] = useState(() => initialChatPosition());
  const [providers, setProviders] = useState<ProviderConfig[]>([]);
  const [providerDraft, setProviderDraft] = useState(session.provider);
  const [modelDraft, setModelDraft] = useState(session.model);
  const [configStatus, setConfigStatus] = useState<string | null>(null);
  const [configError, setConfigError] = useState<string | null>(null);
  const [isConfigSaving, setIsConfigSaving] = useState(false);
  // In-memory only; not persisted to settings or anywhere else. The
  // active mode is sent to the backend on each prompt and injected into
  // the LLM's context as a synthetic user message.
  const [mode, setMode] = useState<ChatMode>("plan");
  const scrollRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
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
    invoke<ProviderConfig[]>("get_provider_configs")
      .then((configs) => setProviders(configs))
      .catch((error) => setConfigError(formatError(error)));
  }, []);

  useEffect(() => {
    setProviderDraft(session.provider);
    setModelDraft(session.model);
    setConfigStatus(null);
    setConfigError(null);
  }, [session.id, session.provider, session.model]);

  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [isSending, messages]);

  const handleSend = async () => {
    const content = input.trim();
    if (!content || isSending) return;

    setInput("");
    await onSendMessage(content, mode);
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
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

  const directoryName = session.directory.split(/[\\/]/).filter(Boolean).pop() ?? session.directory;

  return (
    <div className="pointer-events-none fixed inset-0 z-50">
      <div
        className={cn(
          "pointer-events-auto fixed flex h-[600px] max-h-[calc(100vh-32px)] w-[700px] max-w-[calc(100vw-32px)] flex-col overflow-hidden rounded-none border border-[#1f1f1f] bg-[#0a0a0a] text-white shadow-none",
        )}
        role="dialog"
        aria-modal="false"
        aria-label={`${directoryName} chat`}
        style={{ left: position.x, top: position.y }}
      >
        <div
          className="flex cursor-grab items-center justify-between border-b border-[#1f1f1f] px-6 py-4 active:cursor-grabbing"
          onPointerDown={handleDragStart}
          onPointerMove={handleDragMove}
          onPointerUp={handleDragEnd}
          onPointerCancel={handleDragEnd}
        >
          <div className="flex min-w-0 items-center gap-3">
            <GripHorizontal className="h-4 w-4 shrink-0 text-[#737373]" />
            <div className="flex min-w-0 flex-col">
              <h2 className="truncate text-sm font-semibold text-white">{directoryName}</h2>
              <p className="truncate text-xs text-[#8a8a8a]">{session.directory}</p>
            </div>
          </div>
          <Button
            variant="outline"
            size="sm"
            onPointerDown={(event) => event.stopPropagation()}
            onClick={onCompact}
            disabled={isCompacting || isSending}
            title="Summarize the conversation so the next prompt fits the model context"
          >
            {isCompacting ? "Compacting…" : "Compact"}
          </Button>
          <Button
            variant="ghost"
            size="icon"
            onPointerDown={(event) => event.stopPropagation()}
            onClick={onClose}
          >
            <X className="h-4 w-4" />
          </Button>
        </div>
        <div className="border-b border-[#1f1f1f] bg-[#0a0a0a] px-6 py-3">
          <div className="grid grid-cols-1 gap-2 md:grid-cols-[minmax(140px,0.8fr)_minmax(220px,1fr)_auto_auto]">
            <label className="space-y-1">
              <span className="text-[10px] font-medium uppercase tracking-[0.18em] text-[#737373]">Provider</span>
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
                  className="h-9 w-full rounded-none border border-[#1f1f1f] bg-black px-3 text-sm text-white outline-none transition-colors hover:border-[#2a2a2a] focus:border-white"
                >
                  {providerOptions.map((provider) => (
                    <option key={provider.name} value={provider.name}>{provider.name}</option>
                  ))}
                </select>
              ) : (
                <Input
                  value={providerDraft}
                  onChange={(event) => setProviderDraft(event.target.value)}
                  placeholder="anthropic"
                />
              )}
            </label>
            <label className="space-y-1">
              <span className="text-[10px] font-medium uppercase tracking-[0.18em] text-[#737373]">Model</span>
              <select
                value={modelDraft}
                onChange={(event) => {
                  setModelDraft(event.target.value);
                  setConfigStatus(null);
                  setConfigError(null);
                }}
                className="h-9 w-full rounded-none border border-[#1f1f1f] bg-black px-3 text-sm text-white outline-none transition-colors hover:border-[#2a2a2a] focus:border-white disabled:cursor-not-allowed disabled:opacity-50"
                disabled={modelOptions.length === 0}
              >
                {modelOptions.length === 0 ? (
                  <option value="">Add models in src/config/provider-models.json</option>
                ) : (
                  modelOptions.map((model) => (
                    <option key={model} value={model}>{model}</option>
                  ))
                )}
              </select>
            </label>
            <div className="space-y-1">
              <span className="text-[10px] font-medium uppercase tracking-[0.18em] text-[#737373]">Mode</span>
              <div
                className="flex h-9 w-full rounded-none border border-[#1f1f1f] bg-black text-sm"
                role="radiogroup"
                aria-label="Permission mode"
              >
                {(["plan", "build"] as const).map((option) => {
                  const active = mode === option;
                  return (
                    <button
                      key={option}
                      type="button"
                      role="radio"
                      aria-checked={active}
                      onClick={() => setMode(option)}
                      className={cn(
                        "flex-1 select-none px-3 uppercase tracking-[0.18em] text-[11px] transition-colors",
                        active
                          ? option === "plan"
                            ? "bg-[#1a1a1a] text-white"
                            : "bg-[#2a2a2a] text-white"
                          : "text-[#737373] hover:text-[#bdbdbd]",
                      )}
                      title={
                        option === "plan"
                          ? "Read-only: shell, write, edit, apply_patch are blocked"
                          : "All tools allowed"
                      }
                    >
                      {option}
                    </button>
                  );
                })}
              </div>
            </div>
            <div className="flex items-end gap-2">
              <Button
                variant="secondary"
                className="h-9 border border-[#2a2a2a]"
                onClick={saveSessionConfig}
                disabled={isConfigSaving || !configChanged}
              >
                <Check className="h-4 w-4" />
                {isConfigSaving ? "Saving" : "Save"}
              </Button>
            </div>
          </div>
          {(configStatus || configError) && (
            <p className={cn("mt-2 text-xs", configError ? "text-[#ff5f5f]" : "text-[#bdbdbd]")}>{configError ?? configStatus}</p>
          )}
        </div>
        <ScrollArea className="flex-1 px-6 py-4" ref={scrollRef}>
          <div className="space-y-4">
            {messages.length === 0 ? (
              <div className="flex h-full items-center justify-center">
                <p className="text-sm text-[#737373]">
                  Chat with {directoryName} session. Ask about the codebase or files in this directory.
                </p>
              </div>
            ) : (
              messages.map((message, index) => {
                const isAssistant = message.role === "assistant";
                const reasoning = message.reasoning ?? "";
                const content = message.content ?? "";
                const parts = message.parts ?? [];

                if (message.role === "system" && content.includes("<conversation-checkpoint>")) {
                  return (
                    <ContextCheckpoint key={message.id ?? `${message.timestamp}-${index}`} content={content} />
                  );
                }

                return (
                  <div
                    key={message.id ?? `${message.timestamp}-${index}`}
                    className={cn(
                      "flex",
                      message.role === "user" ? "justify-end" : "justify-start",
                    )}
                  >
                    <div
                      className={cn(
                        "max-w-[80%] rounded-none px-4 py-2 text-sm",
                        message.role === "user"
                          ? "whitespace-pre-wrap break-words bg-white text-black"
                          : "border border-[#2a2a2a] bg-[#111111] text-[#f5f5f5]",
                      )}
                    >
                      {isAssistant && reasoning && (
                        <ThinkBlock
                          text={reasoning}
                          defaultOpen={streamingMessageId === message.id}
                          className="mb-2"
                        />
                      )}
                      {isAssistant && parts.length > 0 ? (
                        <AssistantMessageParts parts={parts} fallbackContent={content} />
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
                <div className="flex items-center gap-2 rounded-none border border-[#1f1f1f] bg-[#0a0a0a] px-4 py-2 text-xs text-[#737373]">
                  <span>◌</span>
                  <span>◌</span>
                  <span>◌</span>
                  <span>thinking</span>
                </div>
              </div>
            )}
            {isSending && streamingMessageId && (
              <div className="flex justify-start">
                <div className="rounded-none border border-[#2a2a2a] bg-[#111111] px-4 py-2 text-xs text-[#737373]">
                  <span className="mr-1">◌</span>Thinking
                </div>
              </div>
            )}
          </div>
        </ScrollArea>

        <div className="border-t border-[#1f1f1f] px-6 py-4">
          <div className="flex gap-2">
            <Input
              ref={inputRef}
              value={input}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={handleKeyDown}
              placeholder="Ask about the codebase..."
              className="flex-1"
              disabled={isSending}
            />
            <Button onClick={handleSend} disabled={isSending || !input.trim()}>
              Send
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
}

function AssistantMessageParts({ parts, fallbackContent }: { parts: ChatMessagePart[]; fallbackContent: string }) {
  const results = new Map<string, Extract<ChatMessagePart, { type: "tool_result" }>>();
  for (const part of parts) {
    if (part.type === "tool_result") {
      results.set(part.id, part);
    }
  }

  const visibleParts = parts.filter((part) => part.type !== "reasoning" && part.type !== "tool_result");
  if (visibleParts.length === 0 && fallbackContent) {
    return <div className="whitespace-pre-wrap break-words">{fallbackContent}</div>;
  }

  return (
    <div className="space-y-2">
      {visibleParts.map((part, index) => {
        if (part.type === "text") {
          return part.text ? (
            <div key={`text-${index}`} className="whitespace-pre-wrap break-words">
              {part.text}
            </div>
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
    <div className="overflow-hidden rounded-none border border-[#2a2a2a] bg-black font-mono text-xs">
      <div className="flex items-center justify-between border-b border-[#1f1f1f] bg-[#080808] px-3 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <Icon className="h-3.5 w-3.5 shrink-0 text-[#8a8a8a]" />
          <span className="truncate text-[#f5f5f5]">{call.name}</span>
          <span className="text-[#4a4a4a]">{details.label}</span>
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
        <pre className="whitespace-pre-wrap break-words text-[#d4d4d4]">{details.command}</pre>
        {resultSummary?.text && (
          <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-words border-t border-[#1f1f1f] pt-2 text-[#8a8a8a]">
            {resultSummary.text}
          </pre>
        )}
      </div>
    </div>
  );
}

function ToolErrorBlock({ error }: { error: Extract<ChatMessagePart, { type: "tool_error" }> }) {
  return (
    <div className="rounded-none border border-[#3a1f1f] bg-[#120808] px-3 py-2 font-mono text-xs text-[#ff8a8a]">
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
