import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useAgentStore } from "./agentStore";

export function AgentComposer() {
  const [input, setInput] = useState("");
  const { activeSessionId, addMessage } = useAgentStore();

  const send = async () => {
    if (!input.trim() || !activeSessionId) return;

    const userMessage = {
      id: crypto.randomUUID(),
      role: "user" as const,
      content: input,
      timestamp: Date.now(),
    };

    addMessage(activeSessionId, userMessage);
    setInput("");

    try {
      const response = await invoke("send_message", {
        sessionId: activeSessionId,
        message: input,
      });
      const assistantMessage = {
        id: crypto.randomUUID(),
        role: "assistant" as const,
        content: response as string,
        timestamp: Date.now(),
      };
      addMessage(activeSessionId, assistantMessage);
    } catch (err) {
      console.error("Failed to send message:", err);
    }
  };

  return (
    <div className="agent-composer">
      <textarea
        value={input}
        onChange={(e) => setInput(e.target.value)}
        placeholder="Ask the agent..."
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            send();
          }
        }}
      />
      <button onClick={send}>Send</button>
    </div>
  );
}