import { useCallback, useEffect, useMemo, useRef } from "react";
import {
  Background,
  ReactFlow,
  type ReactFlowInstance,
  useNodesState,
  type Edge,
  type Node,
} from "reactflow";
import "reactflow/dist/style.css";
import { LoopNode } from "@/components/loops";
import { SessionNode } from "@/components/sessions/SessionNode";
import type { SessionLoop } from "@/features/loops/loopStore";
import type { AgentSession, SessionGroup } from "@/features/sessions/sessionStore";
import { useAppStore } from "@/features/app/appStore";

const nodeTypes = { sessionCard: SessionNode, loopCard: LoopNode };
const proOptions = { hideAttribution: true };

interface SessionCanvasProps {
  sessions: Map<string, AgentSession>;
  groups: Map<string, SessionGroup>;
  loops: Map<string, SessionLoop>;
  onConnectSessions: (sourceId: string, targetId: string) => void;
  onAppendSessionToLoop: (sessionId: string, loopId: string) => void;
  onOpenSessionChat: (id: string) => void;
  onSelectSession: (id: string) => void;
  onDeleteSession?: (id: string) => void;
  onConfigureLoop: (id: string) => void;
  onDeleteLoop: (id: string) => void;
  focusRequest?: { sessionId: string; nonce: number } | null;
}

export function SessionCanvas({
  sessions,
  groups,
  loops,
  onConnectSessions,
  onAppendSessionToLoop,
  onOpenSessionChat,
  onSelectSession,
  onDeleteSession,
  onConfigureLoop,
  onDeleteLoop,
  focusRequest,
}: SessionCanvasProps) {
  const [nodes, setNodes, onNodesChange] = useNodesState([]);
  const nodeSkin = useAppStore((state) => state.settings.node_skin);
  const theme = useAppStore((state) => state.settings.theme);
  const flowRef = useRef<ReactFlowInstance | null>(null);
  const sessionNodeIds = useMemo(
    () => Array.from(sessions.keys()).sort().join("\0"),
    [sessions],
  );
  const focusSessionId = focusRequest?.sessionId ?? null;
  const focusNonce = focusRequest?.nonce ?? 0;

  useEffect(() => {
    setNodes((currentNodes) => {
      const currentPositions = new Map(currentNodes.map((node) => [node.id, node.position]));

      const sessionNodes = Array.from(sessions.values()).map<Node>((session, index) => ({
        id: session.id,
        type: "sessionCard",
        selected: session.id === focusSessionId,
        position: currentPositions.get(session.id) ?? {
          x: 96 + (index % 4) * 240,
          y: 96 + Math.floor(index / 4) * 180,
        },
        data: {
          session,
          skin: nodeSkin,
          theme,
          onOpenChat: onOpenSessionChat,
          onSelect: onSelectSession,
          onDelete: onDeleteSession,
        },
      }));

      const loopNodes = Array.from(loops.values()).map<Node>((loop, index) => ({
        id: loop.id,
        type: "loopCard",
        position: currentPositions.get(loop.id) ?? {
          x: 96 + (index % 4) * 260,
          y: 96 + Math.ceil(Math.max(sessionNodes.length, 1) / 4) * 180 + Math.floor(index / 4) * 150,
        },
        data: {
          loop,
          onConfigure: onConfigureLoop,
          onDelete: onDeleteLoop,
        },
      }));

      return [
        ...sessionNodes,
        ...loopNodes,
      ];
    });
  }, [nodeSkin, theme, onOpenSessionChat, onSelectSession, onDeleteSession, sessions, loops, setNodes, focusSessionId, focusNonce, onConfigureLoop, onDeleteLoop]);

  useEffect(() => {
    if (!focusSessionId) return;

    const frame = window.requestAnimationFrame(() => {
      flowRef.current?.fitView({ nodes: [{ id: focusSessionId }], padding: 0.35, duration: 180 });
    });
    return () => window.cancelAnimationFrame(frame);
  }, [focusSessionId, focusNonce, sessionNodeIds]);

  const edges = useMemo<Edge[]>(() => {
    const groupEdges = Array.from(groups.values()).flatMap((group) => {
      const sessionIds = group.session_ids.filter((id) => sessions.has(id));
      const edges: Edge[] = [];

      for (let i = 0; i < sessionIds.length; i += 1) {
        for (let j = i + 1; j < sessionIds.length; j += 1) {
          edges.push({
            id: `${group.id}:${sessionIds[i]}-${sessionIds[j]}`,
            source: sessionIds[i],
            target: sessionIds[j],
            animated: true,
            style: { stroke: "var(--edge-session)", strokeWidth: 1.5 },
          });
        }
      }

      return edges;
    });

    const loopEdges = Array.from(loops.values()).flatMap((loop) => (
      loop.session_ids
        .filter((sessionId) => sessions.has(sessionId))
        .map<Edge>((sessionId) => ({
          id: `${loop.id}:${sessionId}`,
          source: sessionId,
          target: loop.id,
          animated: true,
          style: { stroke: "var(--edge-loop)", strokeWidth: 1.5 },
        }))
    ));

    return [...groupEdges, ...loopEdges];
  }, [groups, loops, sessions]);

  const handleConnect = useCallback((sourceId: string, targetId: string) => {
    const sourceIsSession = sessions.has(sourceId);
    const targetIsSession = sessions.has(targetId);
    const sourceIsLoop = loops.has(sourceId);
    const targetIsLoop = loops.has(targetId);

    if (sourceIsSession && targetIsSession) {
      onConnectSessions(sourceId, targetId);
      return;
    }

    if (sourceIsSession && targetIsLoop) {
      onAppendSessionToLoop(sourceId, targetId);
      return;
    }

    if (sourceIsLoop && targetIsSession) {
      onAppendSessionToLoop(targetId, sourceId);
    }
  }, [loops, onAppendSessionToLoop, onConnectSessions, sessions]);

  return (
    <div className="h-full bg-[var(--canvas-bg)]">
      <ReactFlow
        nodes={nodes}
        edges={edges}
        onNodesChange={onNodesChange}
        onConnect={(connection) => {
          if (connection.source && connection.target) {
            handleConnect(connection.source, connection.target);
          }
        }}
        onInit={(instance) => {
          flowRef.current = instance;
        }}
        nodeTypes={nodeTypes}
        proOptions={proOptions}
        fitView
      >
        <Background color="var(--canvas-grid)" gap={24} />
      </ReactFlow>
    </div>
  );
}
