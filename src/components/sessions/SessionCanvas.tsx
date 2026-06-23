import { useEffect, useMemo } from "react";
import {
  Background,
  ReactFlow,
  useNodesState,
  type Edge,
  type Node,
} from "reactflow";
import "reactflow/dist/style.css";
import { SessionNode } from "@/components/sessions/SessionNode";
import type { AgentSession, SessionGroup } from "@/features/sessions/sessionStore";
import { useAppStore } from "@/features/app/appStore";

const nodeTypes = { sessionCard: SessionNode };
const proOptions = { hideAttribution: true };

interface SessionCanvasProps {
  sessions: Map<string, AgentSession>;
  groups: Map<string, SessionGroup>;
  onConnectSessions: (sourceId: string, targetId: string) => void;
  onOpenSessionChat: (id: string) => void;
  onSelectSession: (id: string) => void;
  onDeleteSession?: (id: string) => void;
}

export function SessionCanvas({
  sessions,
  groups,
  onConnectSessions,
  onOpenSessionChat,
  onSelectSession,
  onDeleteSession,
}: SessionCanvasProps) {
  const [nodes, setNodes, onNodesChange] = useNodesState([]);
  const nodeSkin = useAppStore((state) => state.settings.node_skin);

  useEffect(() => {
    setNodes((currentNodes) => {
      const currentPositions = new Map(currentNodes.map((node) => [node.id, node.position]));

      return Array.from(sessions.values()).map<Node>((session, index) => ({
        id: session.id,
        type: "sessionCard",
        position: currentPositions.get(session.id) ?? {
          x: 96 + (index % 4) * 240,
          y: 96 + Math.floor(index / 4) * 180,
        },
        data: {
          session,
          skin: nodeSkin,
          onOpenChat: onOpenSessionChat,
          onSelect: onSelectSession,
          onDelete: onDeleteSession,
        },
      }));
    });
  }, [nodeSkin, onOpenSessionChat, onSelectSession, onDeleteSession, sessions, setNodes]);

  const edges = useMemo<Edge[]>(() => {
    return Array.from(groups.values()).flatMap((group) => {
      const sessionIds = group.session_ids.filter((id) => sessions.has(id));
      const groupEdges: Edge[] = [];

      for (let i = 0; i < sessionIds.length; i += 1) {
        for (let j = i + 1; j < sessionIds.length; j += 1) {
          groupEdges.push({
            id: `${group.id}:${sessionIds[i]}-${sessionIds[j]}`,
            source: sessionIds[i],
            target: sessionIds[j],
            animated: true,
            style: { stroke: "#f5f5f5", strokeWidth: 1.5 },
          });
        }
      }

      return groupEdges;
    });
  }, [groups, sessions]);

  return (
    <div className="h-full bg-black">
      <ReactFlow
        nodes={nodes}
        edges={edges}
        onNodesChange={onNodesChange}
        onConnect={(connection) => {
          if (connection.source && connection.target) {
            onConnectSessions(connection.source, connection.target);
          }
        }}
        nodeTypes={nodeTypes}
        proOptions={proOptions}
        fitView
      >
        <Background color="#1f1f1f" gap={24} />
      </ReactFlow>
    </div>
  );
}
