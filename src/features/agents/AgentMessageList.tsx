import { useAgentStore } from "./agentStore";

export function AgentMessageList() {
  const { sessions, activeSessionId } = useAgentStore();
  const session = activeSessionId ? sessions.get(activeSessionId) : null;

  if (!session) {
    return <div className="message-list empty">No active session</div>;
  }

  return (
    <div className="message-list">
      {session.messages.map((msg) => (
        <div key={msg.id} className={`message ${msg.role}`}>
          <div className="message-content">{msg.content}</div>
        </div>
      ))}
    </div>
  );
}