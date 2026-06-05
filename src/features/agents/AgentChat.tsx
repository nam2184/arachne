import { AgentSessionMap } from "./AgentSessionMap";
import { AgentMessageList } from "./AgentMessageList";
import { AgentComposer } from "./AgentComposer";

export function AgentChat() {
  return (
    <div className="agent-chat">
      <div className="agent-map-section">
        <AgentSessionMap />
      </div>
      <div className="agent-messages">
        <AgentMessageList />
      </div>
      <AgentComposer />
    </div>
  );
}