import { ReactFlow, Background, Controls } from "reactflow";
import "reactflow/dist/style.css";
import { useAgentStore } from "./agentStore";
import { AgentNodeCard } from "./AgentNodeCard";

const nodeTypes = { agentCard: AgentNodeCard };

export function AgentSessionMap() {
  const { sessions, activeSessionId } = useAgentStore();

  const nodes = Array.from(sessions.entries()).map(([id]) => ({
    id,
    type: "agentCard",
    position: { x: Math.random() * 400, y: Math.random() * 300 },
    data: { label: id },
  }));

  const edges = Array.from(sessions.keys()).map((id, _, keys) => {
    if (keys.indexOf(id) < keys.length - 1) {
      return {
        id: `${id}-${keys[keys.indexOf(id) + 1]}`,
        source: id,
        target: keys[keys.indexOf(id) + 1],
      };
    }
    return null;
  }).filter(Boolean);

  return (
    <div className="agent-session-map">
      <ReactFlow
        nodes={nodes}
        edges={edges as any}
        nodeTypes={nodeTypes}
        fitView
      >
        <Background />
        <Controls />
      </ReactFlow>
    </div>
  );
}