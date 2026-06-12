import { invoke } from "@tauri-apps/api/core";
import { create } from "zustand";

export interface ConversationMessage {
  id: string;
  role: "user" | "assistant" | "system";
  content: string;
  reasoning?: string;
  parts?: ChatMessagePart[];
  timestamp: string;
}

export type ChatMessagePart =
  | { type: "text"; text: string }
  | { type: "reasoning"; text: string }
  | { type: "tool_call"; id: string; name: string; input?: unknown }
  | { type: "tool_result"; id: string; name?: string; result?: unknown; output?: string | null }
  | { type: "tool_error"; id?: string; name?: string; message: string };

export interface ConversationFile {
  session_id: string;
  messages: ConversationMessage[];
  summary: string | null;
}

type AgentLlmEvent = {
  type: string;
  id?: string;
  text?: string;
  name?: string;
  message?: string;
  input?: unknown;
  result?: unknown;
  output?: string | null;
  provider_executed?: boolean | null;
};

export type AgentStreamEvent =
  | { type: "started"; session_id: string }
  | { type: "llm_event"; session_id: string; step: number; event: AgentLlmEvent }
  | { type: "finished"; session_id: string; response: string }
  | { type: "error"; session_id: string; message: string };

interface ConversationState {
  activeConversation: ConversationFile | null;
  streamingMessageId: string | null;
  isCompacting: boolean;
  loadConversation: (sessionId: string) => Promise<void>;
  loadUiConversation: (sessionId: string) => Promise<void>;
  appendMessage: (sessionId: string, role: "user" | "assistant" | "system", content: string) => Promise<string>;
  beginStreamingMessage: (sessionId: string, content: string) => void;
  applyAgentEvent: (event: AgentStreamEvent) => void;
  failStreamingMessage: (sessionId: string, message: string) => void;
  finishStreamingMessage: (sessionId: string) => void;
  compactConversation: (sessionId: string, summary: string) => Promise<void>;
  compactNow: (sessionId: string) => Promise<{ status: string; summary: string }>;
  clearConversation: () => void;
}

interface ParsedAssistantContent {
  content: string;
  reasoning: string;
  parts: ChatMessagePart[];
}

interface ContentPart {
  type?: string;
  text?: string;
  name?: string;
  id?: string;
  input?: unknown;
  result?: unknown;
  output?: string | null;
}

const THINK_OPEN = "<think>";
const THINK_CLOSE = "</think>";

/**
 * Splits a stored assistant message (which is a JSON array of
 * `ContentPart`s) into the visible `content` and the `reasoning` that
 * the UI renders in `ThinkBlock`.
 */
export function parseAssistantParts(raw: string): ParsedAssistantContent {
  let content = raw;
  let reasoning = "";
  let parsedParts: ChatMessagePart[] = raw ? [{ type: "text", text: raw }] : [];

  try {
    const parts = JSON.parse(raw) as ContentPart[];
    if (Array.isArray(parts)) {
      const textChunks: string[] = [];
      const reasoningChunks: string[] = [];
      parsedParts = [];
      for (const part of parts) {
        if (part.type === "text" && typeof part.text === "string") {
          textChunks.push(part.text);
          parsedParts.push({ type: "text", text: part.text });
        } else if (part.type === "reasoning" && typeof part.text === "string") {
          reasoningChunks.push(part.text);
          parsedParts.push({ type: "reasoning", text: part.text });
        } else if (part.type === "tool_call" && typeof part.name === "string") {
          parsedParts.push({
            type: "tool_call",
            id: part.id ?? `tool-${parsedParts.length}`,
            name: part.name,
            input: part.input,
          });
        } else if (part.type === "tool_result" && typeof part.id === "string") {
          parsedParts.push({
            type: "tool_result",
            id: part.id,
            name: part.name,
            result: part.result,
            output: part.output,
          });
        }
      }
      content = textChunks.join("\n").trim();
      reasoning = reasoningChunks.join("\n").trim();
    }
  } catch {
    // raw was not JSON; treat it as plain text content.
  }

  return { content, reasoning, parts: parsedParts };
}

/**
 * Maintains a running buffer of `text_delta` chunks. As soon as a
 * complete `<think>...</think>` block is observed, the inner text is
 * moved to the reasoning stream; anything before/after stays in the
 * visible content stream. Unterminated `<think>` is held in a buffer
 * until the close tag arrives.
 */
class ThinkSplitter {
  private buffer = "";
  private visible = "";
  private reasoning = "";

  feed(chunk: string): { content: string; reasoning: string } {
    this.buffer += chunk;
    this.drainComplete();
    return this.snapshot();
  }

  snapshot(): { content: string; reasoning: string } {
    return { content: this.visible, reasoning: this.reasoning };
  }

  private drainComplete() {
    while (this.buffer.length > 0) {
      const closeIdx = this.buffer.indexOf(THINK_CLOSE);
      if (closeIdx === -1) {
        // No close yet. Look for an open: anything before it is visible,
        // anything from the open onward stays buffered (and will be
        // flushed as reasoning when/if the close arrives, or as visible
        // text if no close ever arrives).
        const openIdx = this.buffer.indexOf(THINK_OPEN);
        if (openIdx === -1) {
          this.visible += this.buffer;
          this.buffer = "";
        } else if (openIdx > 0) {
          this.visible += this.buffer.slice(0, openIdx);
          this.buffer = this.buffer.slice(openIdx);
        }
        return;
      }

      // We have a close. Find the most recent open before it.
      const openIdx = this.buffer.lastIndexOf(THINK_OPEN, closeIdx);
      if (openIdx === -1) {
        // Stray close (no matching open). Treat as visible text.
        this.visible += this.buffer.slice(0, closeIdx + THINK_CLOSE.length);
        this.buffer = this.buffer.slice(closeIdx + THINK_CLOSE.length);
        continue;
      }

      const before = this.buffer.slice(0, openIdx);
      const think = this.buffer.slice(openIdx + THINK_OPEN.length, closeIdx);
      const after = this.buffer.slice(closeIdx + THINK_CLOSE.length);

      if (before) this.visible += before;
      if (think) this.reasoning += think;
      this.buffer = after;
    }
  }
}

function createTempMessage(role: ConversationMessage["role"], content: string): ConversationMessage {
  return {
    id: `temp-${role}-${Date.now()}-${Math.random().toString(16).slice(2)}`,
    role,
    content,
    timestamp: new Date().toISOString(),
  };
}

function ensureConversation(sessionId: string, current: ConversationFile | null): ConversationFile {
  if (current?.session_id === sessionId) {
    return current;
  }

  return {
    session_id: sessionId,
    messages: [],
    summary: null,
  };
}

/**
 * Normalizes a freshly-loaded assistant message so the UI sees a flat
 * `content` / `reasoning` pair rather than the stored JSON array.
 */
function normalizeAssistantMessage(message: ConversationMessage): ConversationMessage {
  if (message.role !== "assistant") return message;
  const parsed = parseAssistantParts(message.content);
  return {
    ...message,
    content: parsed.content,
    reasoning: parsed.reasoning || message.reasoning,
    parts: parsed.parts,
  };
}

function appendPart(parts: ChatMessagePart[] | undefined, part: ChatMessagePart): ChatMessagePart[] {
  const current = parts ?? [];
  if ((part.type === "tool_call" || part.type === "tool_result") && part.id) {
    const existingIndex = current.findIndex((existing) =>
      existing.type === part.type && "id" in existing && existing.id === part.id,
    );
    if (existingIndex !== -1) {
      return current.map((existing, index) => (index === existingIndex ? part : existing));
    }
  }
  return [...current, part];
}

function setTextPart(parts: ChatMessagePart[] | undefined, text: string): ChatMessagePart[] | undefined {
  if (!text) return parts;
  const current = parts ?? [];
  const textIndex = current.findIndex((part) => part.type === "text");
  if (textIndex === -1) {
    return [{ type: "text", text }, ...current];
  }
  return current.map((part, index) => (index === textIndex ? { type: "text", text } : part));
}

function normalizeConversation(conv: ConversationFile): ConversationFile {
  return {
    ...conv,
    messages: conv.messages.map(normalizeAssistantMessage),
  };
}

function findOrCreateStreamingMessage(
  conversation: ConversationFile,
  streamingMessageId: string | null,
): { messageId: string; index: number; isNew: boolean; existing?: ConversationMessage } {
  const messageId = streamingMessageId ?? createTempMessage("assistant", "").id;
  const index = conversation.messages.findIndex((m) => m.id === messageId);
  if (index === -1) {
    return { messageId, index: -1, isNew: true };
  }
  return { messageId, index, isNew: false, existing: conversation.messages[index] };
}

function upsertAssistant(
  conversation: ConversationFile,
  streamingMessageId: string | null,
  update: (current: ConversationMessage) => ConversationMessage,
): { conversation: ConversationFile; streamingMessageId: string } {
  const { messageId, index, isNew, existing } = findOrCreateStreamingMessage(
    conversation,
    streamingMessageId,
  );

  const base: ConversationMessage = isNew
    ? { ...createTempMessage("assistant", ""), id: messageId }
    : existing!;
  const updated = update(base);

  const messages = isNew
    ? [...conversation.messages, updated]
    : conversation.messages.map((m, i) => (i === index ? updated : m));

  return {
    conversation: { ...conversation, messages },
    streamingMessageId: messageId,
  };
}

const streamingSplitters = new Map<string, ThinkSplitter>();

function getOrCreateSplitter(messageId: string): ThinkSplitter {
  let splitter = streamingSplitters.get(messageId);
  if (!splitter) {
    splitter = new ThinkSplitter();
    streamingSplitters.set(messageId, splitter);
  }
  return splitter;
}

function clearSplitter(messageId: string | null) {
  if (messageId) streamingSplitters.delete(messageId);
}

export const useConversationStore = create<ConversationState>((set, get) => ({
  activeConversation: null,
  streamingMessageId: null,
  isCompacting: false,

  loadConversation: async (sessionId) => {
    const conv = await invoke<ConversationFile>("get_ai_conversation", { sessionId });
    set({ activeConversation: normalizeConversation(conv), streamingMessageId: null });
  },

  loadUiConversation: async (sessionId) => {
    const conv = await invoke<ConversationFile>("get_ui_conversation", { sessionId });
    set({ activeConversation: normalizeConversation(conv), streamingMessageId: null });
  },

  appendMessage: async (sessionId, role, content) => {
    const messageId = await invoke<string>("append_message", {
      sessionId,
      role,
      content,
    });
    const conv = await invoke<ConversationFile>("get_ui_conversation", { sessionId });
    set({ activeConversation: normalizeConversation(conv), streamingMessageId: null });
    return messageId;
  },

  beginStreamingMessage: (sessionId, content) => {
    set((state) => {
      const conversation = ensureConversation(sessionId, state.activeConversation);
      const assistant = createTempMessage("assistant", "");

      return {
        activeConversation: {
          ...conversation,
          messages: [...conversation.messages, createTempMessage("user", content), assistant],
        },
        streamingMessageId: assistant.id,
      };
    });
  },

  applyAgentEvent: (event) => {
    set((state) => {
      if (!state.activeConversation || state.activeConversation.session_id !== event.session_id) {
        return state;
      }

      if (event.type === "error") {
        const { conversation, streamingMessageId } = upsertAssistant(
          state.activeConversation,
          state.streamingMessageId,
          (current) => ({
            ...current,
            content: appendErrorToDraft(current.content, event.message),
          }),
        );
        return { activeConversation: conversation, streamingMessageId };
      }

      if (event.type !== "llm_event") {
        return state;
      }

      if (event.event.type === "text_delta") {
        const chunk = event.event.text ?? "";
        const { conversation, streamingMessageId } = upsertAssistant(
          state.activeConversation,
          state.streamingMessageId,
          (current) => {
            const splitter = getOrCreateSplitter(current.id);
            const snapshot = splitter.feed(chunk);
            return {
              ...current,
              content: snapshot.content,
              reasoning: snapshot.reasoning,
              parts: setTextPart(current.parts, snapshot.content),
            };
          },
        );
        return { activeConversation: conversation, streamingMessageId };
      }

      if (event.event.type === "reasoning_delta") {
        const { conversation, streamingMessageId } = upsertAssistant(
          state.activeConversation,
          state.streamingMessageId,
          (current) => ({
            ...current,
            reasoning: (current.reasoning ?? "") + (event.event.text ?? ""),
            parts: appendPart(current.parts, { type: "reasoning", text: event.event.text ?? "" }),
          }),
        );
        return { activeConversation: conversation, streamingMessageId };
      }

      if (event.event.type === "tool_call") {
        const { conversation, streamingMessageId } = upsertAssistant(
          state.activeConversation,
          state.streamingMessageId,
          (current) => ({
            ...current,
            parts: appendPart(current.parts, {
              type: "tool_call",
              id: event.event.id ?? `tool-${Date.now()}`,
              name: event.event.name ?? "tool",
              input: event.event.input,
            }),
          }),
        );
        return { activeConversation: conversation, streamingMessageId };
      }

      if (event.event.type === "tool_result") {
        const { conversation, streamingMessageId } = upsertAssistant(
          state.activeConversation,
          state.streamingMessageId,
          (current) => ({
            ...current,
            parts: appendPart(current.parts, {
              type: "tool_result",
              id: event.event.id ?? `tool-result-${Date.now()}`,
              name: event.event.name,
              result: event.event.result,
              output: event.event.output,
            }),
          }),
        );
        return { activeConversation: conversation, streamingMessageId };
      }

      if (event.event.type === "tool_error") {
        const errorLine = `${event.event.name ?? "tool"} failed: ${event.event.message ?? "Unknown error"}`;
        const { conversation, streamingMessageId } = upsertAssistant(
          state.activeConversation,
          state.streamingMessageId,
          (current) => ({
            ...current,
            content: current.content,
            parts: appendPart(current.parts, {
              type: "tool_error",
              id: event.event.id,
              name: event.event.name,
              message: event.event.message ?? errorLine,
            }),
          }),
        );
        return { activeConversation: conversation, streamingMessageId };
      }

      if (event.event.type === "provider_error") {
        const { conversation, streamingMessageId } = upsertAssistant(
          state.activeConversation,
          state.streamingMessageId,
          (current) => ({
            ...current,
            content: appendErrorToDraft(current.content, event.event.message ?? "Unknown LLM provider error"),
          }),
        );
        return { activeConversation: conversation, streamingMessageId };
      }

      return state;
    });
  },

  failStreamingMessage: (sessionId, message) => {
    set((state) => {
      const conversation = ensureConversation(sessionId, state.activeConversation);
      const { conversation: updatedConversation, streamingMessageId } = upsertAssistant(
        conversation,
        state.streamingMessageId,
        (current) => ({
          ...current,
          content: appendErrorToDraft(current.content, message),
        }),
      );
      return { activeConversation: updatedConversation, streamingMessageId };
    });
  },

  finishStreamingMessage: (sessionId) => {
    const state = get();
    if (state.activeConversation?.session_id !== sessionId) {
      return;
    }
    clearSplitter(state.streamingMessageId);
    set({ streamingMessageId: null });
  },

  compactConversation: async (sessionId, summary) => {
    await invoke("compact_conversation", { sessionId, summary });
    const conv = await invoke<ConversationFile>("get_ai_conversation", { sessionId });
    set({ activeConversation: normalizeConversation(conv), streamingMessageId: null });
  },

  compactNow: async (sessionId) => {
    set({ isCompacting: true });
    try {
      const result = await invoke<{ status: string; summary: string }>("compact_now", { sessionId });
      const conv = await invoke<ConversationFile>("get_ai_conversation", { sessionId });
      set({ activeConversation: normalizeConversation(conv), streamingMessageId: null });
      return result;
    } finally {
      set({ isCompacting: false });
    }
  },

  clearConversation: () => set({ activeConversation: null, streamingMessageId: null, isCompacting: false }),
}));

function appendErrorToDraft(current: string, message: string) {
  const errorText = `Error: ${message}`;
  if (!current.trim()) {
    return errorText;
  }
  if (current.includes(errorText) || current.includes(message)) {
    return current;
  }
  return `${current.trim()}\n\n${errorText}`;
}
