import { Node, Handle, Position } from "reactflow";
import { useAgentStore } from "./agentStore";

export function AgentNodeCard({ id }: { id: string }) {
  const { sessions, setActiveSession } = useAgentStore();
  const session = sessions.get(id);

  if (!session) return null;

  return (
    <Node
      id={id}
      data={{ label: session.projectId }}
      onClick={() => setActiveSession(id)}
    >
      <Handle type="target" position={Position.Top} />
      <div className="agent-node-card">
        <div className="node-header">{session.model}</div>
        <div className="node-messages">{session.messages.length} messages</div>
      </div>
      <Handle type="source" position={Position.Bottom} />
    </Node>
  );
}