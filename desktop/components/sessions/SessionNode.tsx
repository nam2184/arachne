import { Handle, Position, type NodeProps } from "reactflow";
import { Trash2 } from "lucide-react";
import { cn } from "@/lib/utils";
import type { AgentSession } from "@/features/sessions/sessionStore";
import type { NodeSkin } from "@/features/app/appStore";

interface SessionNodeData {
  session: AgentSession;
  skin: NodeSkin;
  theme: "dark" | "light";
  onSelect: (id: string) => void;
  onOpenChat: (id: string) => void;
  onDelete?: (id: string) => void;
}

const PARTICLE_ANGLES = [0, 60, 120, 180, 240, 300];
const PARTICLE_RADII = [22, 22, 22, 22, 22, 22];
const PARTICLE_DURATIONS = [2.4, 2.8, 2.2, 2.6, 3.0, 2.5];

function DeleteButton({
  onDelete,
  nodeId,
  directoryName,
}: {
  onDelete?: (id: string) => void;
  nodeId: string;
  directoryName: string;
}) {
  if (!onDelete) return null;
  return (
    <button
      type="button"
      onClick={(event) => {
        event.stopPropagation();
        onDelete(nodeId);
      }}
      style={{ pointerEvents: "auto" }}
      className={cn(
        "mt-1 flex items-center gap-1 border px-1.5 py-0.5 text-[10px] opacity-0 transition-opacity group-hover:opacity-100",
        "border-[var(--node-border)] bg-[var(--node-bg)] text-[var(--text-subtle)] hover:border-[#ff5f5f] hover:text-[#ff5f5f]",
      )}
      title={`Delete ${directoryName}`}
    >
      <Trash2 className="h-2.5 w-2.5" />
      Delete
    </button>
  );
}

function MinimalNode({ id, selected, directoryName, theme, onSelect, onOpenChat, onDelete }: {
  id: string;
  selected: boolean | undefined;
  directoryName: string;
  theme: "dark" | "light";
  onSelect: (id: string) => void;
  onOpenChat: (id: string) => void;
  onDelete?: (id: string) => void;
}) {
  const ink = theme === "light" ? "#1f1a14" : "#ffffff";

  return (
    <div className="group flex flex-col items-center gap-1">
      <div
        className={cn(
          "relative flex h-16 w-16 cursor-pointer items-center justify-center",
          selected && "[&_svg_.absorber-core]:!scale-150",
        )}
        onClick={() => onSelect(id)}
        onDoubleClick={() => onOpenChat(id)}
      >
        <Handle type="target" position={Position.Top} className="!h-1 !w-1 !border-0 !bg-[var(--node-muted)]" />
        <Handle type="source" position={Position.Bottom} className="!h-1 !w-1 !border-0 !bg-[var(--node-muted)]" />

        <svg viewBox="-32 -32 64 64" className="absolute inset-0 h-full w-full overflow-visible">
          <defs>
            <radialGradient id={`absorber-${id}`} cx="50%" cy="50%" r="50%">
              <stop offset="0%" stopColor={ink} stopOpacity="0.0" />
              <stop offset="60%" stopColor={ink} stopOpacity={theme === "light" ? "0.12" : "0.05"} />
              <stop offset="100%" stopColor={ink} stopOpacity="0" />
            </radialGradient>
            <radialGradient id={`absorber-core-${id}`} cx="50%" cy="50%" r="50%">
              <stop offset="0%" stopColor={ink} stopOpacity="1" />
              <stop offset="40%" stopColor={ink} stopOpacity="0.6" />
              <stop offset="100%" stopColor={ink} stopOpacity="0" />
            </radialGradient>
          </defs>

          <circle cx="0" cy="0" r="28" fill={`url(#absorber-${id})`} />

          <g style={{ transformOrigin: "0 0" }} className="absorber-orbit">
            {PARTICLE_ANGLES.map((angle, index) => (
              <circle
                key={index}
                cx="0"
                cy="0"
                r="1.2"
                fill={ink}
                opacity="0.85"
                style={{
                  transformOrigin: "0 0",
                  transform: `rotate(${angle}deg) translate(${PARTICLE_RADII[index]}px, 0) scale(${index % 2 === 0 ? 1 : 0.7})`,
                  animation: `absorber-orbit-${id} ${PARTICLE_DURATIONS[index]}s linear infinite`,
                }}
              />
            ))}
          </g>

          <circle
            cx="0"
            cy="0"
            r="6"
            fill={`url(#absorber-core-${id})`}
            className="absorber-core"
            style={{
              transformOrigin: "0 0",
              animation: `absorber-pulse-${id} 1.8s ease-in-out infinite`,
            }}
          />

          <circle cx="0" cy="0" r="2.5" fill={ink} />

          {selected && (
            <circle
              cx="0"
              cy="0"
              r="18"
              fill="none"
              stroke={ink}
              strokeWidth="0.5"
              strokeDasharray="2 3"
              style={{
                transformOrigin: "0 0",
                animation: `absorber-ring-${id} 4s linear infinite`,
              }}
            />
          )}
        </svg>

        <style>{`
          @keyframes absorber-orbit-${id} {
            from { transform: rotate(${PARTICLE_ANGLES[0]}deg) translate(${PARTICLE_RADII[0]}px, 0) scale(1); }
            to   { transform: rotate(${PARTICLE_ANGLES[0] + 360}deg) translate(${PARTICLE_RADII[0]}px, 0) scale(1); }
          }
          ${PARTICLE_ANGLES.map((angle, index) => `
            @keyframes absorber-orbit-${id}-${index} {
              from { transform: rotate(${angle}deg) translate(${PARTICLE_RADII[index]}px, 0) scale(${index % 2 === 0 ? 1 : 0.7}); opacity: 0.9; }
              70%  { opacity: 0.9; }
              100% { transform: rotate(${angle - 360}deg) translate(0px, 0) scale(0.1); opacity: 0; }
            }
          `).join("")}
          @keyframes absorber-pulse-${id} {
            0%, 100% { transform: scale(1); opacity: 1; }
            50%      { transform: scale(1.4); opacity: 0.7; }
          }
          @keyframes absorber-ring-${id} {
            from { transform: rotate(0deg); }
            to   { transform: rotate(360deg); }
          }
        `}</style>
      </div>

      <span className="truncate font-mono text-[10px] text-[var(--text-subtle)]">{directoryName}</span>
      <DeleteButton onDelete={onDelete} nodeId={id} directoryName={directoryName} />
    </div>
  );
}

function TuiNode({ id, selected, directoryName, onSelect, onOpenChat, onDelete }: {
  id: string;
  selected: boolean | undefined;
  directoryName: string;
  onSelect: (id: string) => void;
  onOpenChat: (id: string) => void;
  onDelete?: (id: string) => void;
}) {
  const cursorVisible = selected ? "▌" : "_";
  return (
    <div className="group flex flex-col items-stretch gap-0">
      <div
        className={cn(
          "relative flex h-16 w-32 cursor-pointer flex-col justify-between border px-2 py-1 font-mono text-[10px]",
          selected
            ? "border-[var(--node-focus)] bg-[var(--node-bg)] text-[var(--node-focus)]"
            : "border-[var(--node-border)] bg-[var(--node-bg)] text-[var(--node-muted)] hover:border-[var(--node-border-hover)] hover:text-[var(--node-secondary)]",
        )}
        onClick={() => onSelect(id)}
        onDoubleClick={() => onOpenChat(id)}
      >
        <Handle type="target" position={Position.Top} className="!h-1 !w-1 !border-0 !bg-[var(--node-muted)]" />
        <Handle type="source" position={Position.Bottom} className="!h-1 !w-1 !border-0 !bg-[var(--node-muted)]" />

        <div className="flex items-center justify-between text-[9px] uppercase tracking-[0.18em] opacity-70">
          <span>session</span>
          <span className={cn("h-1.5 w-1.5 rounded-full", selected ? "bg-[var(--node-focus)]" : "bg-[var(--node-muted)]")} />
        </div>

        <div className="flex items-center gap-1 truncate">
          <span className={cn(selected ? "text-[var(--node-focus)]" : "text-[var(--node-secondary)]")}>&gt;</span>
          <span className="truncate">{directoryName}</span>
          <span
            className={cn(
              "ml-auto",
              selected ? "text-[var(--node-focus)]" : "text-[var(--node-muted)]",
            )}
            style={{ animation: `tui-cursor-${id} 1s steps(1) infinite` }}
          >
            {cursorVisible}
          </span>
        </div>

        <div className="flex items-center justify-between text-[9px] opacity-60">
          <span>● live</span>
          <span>{selected ? "FOCUS" : "idle"}</span>
        </div>

        {selected && (
          <div
            className="pointer-events-none absolute inset-0 border border-[var(--node-focus)]"
            style={{ animation: `tui-scan-${id} 2.4s linear infinite` }}
          />
        )}

        <style>{`
          @keyframes tui-cursor-${id} {
            0%, 49%   { opacity: 1; }
            50%, 100% { opacity: 0; }
          }
          @keyframes tui-scan-${id} {
            0%   { box-shadow: inset 0 0 0 0 rgba(125, 220, 138, 0); }
            50%  { box-shadow: inset 0 0 12px 0 rgba(125, 220, 138, 0.35); }
            100% { box-shadow: inset 0 0 0 0 rgba(125, 220, 138, 0); }
          }
        `}</style>
      </div>

      <DeleteButton onDelete={onDelete} nodeId={id} directoryName={directoryName} />
    </div>
  );
}

function DefaultNode({ id, selected, directoryName, theme, onSelect, onOpenChat, onDelete }: {
  id: string;
  selected: boolean | undefined;
  directoryName: string;
  theme: "dark" | "light";
  onSelect: (id: string) => void;
  onOpenChat: (id: string) => void;
  onDelete?: (id: string) => void;
}) {
  const ink = theme === "light" ? "#1f1a14" : "#ffffff";

  return (
    <div className="group flex flex-col items-center gap-1">
      <div
        className={cn(
          "relative flex h-10 w-10 cursor-pointer items-center justify-center rounded-full",
          selected && "ring-2 ring-[var(--node-ink)] ring-offset-2 ring-offset-[var(--canvas-bg)]",
        )}
        onClick={() => onSelect(id)}
        onDoubleClick={() => onOpenChat(id)}
      >
        <Handle type="target" position={Position.Top} className="!border-[var(--canvas-bg)] !bg-[var(--node-ink)]" />
        <svg viewBox="0 0 24 24" className="h-5 w-5">
          <defs>
            <radialGradient id={`diffuse-${id}`} cx="50%" cy="50%" r="50%">
              <stop offset="0%" stopColor={ink} stopOpacity="0.9" />
              <stop offset="70%" stopColor={ink} stopOpacity={theme === "light" ? "0.3" : "0.22"} />
              <stop offset="100%" stopColor={ink} stopOpacity="0" />
            </radialGradient>
          </defs>
          <circle cx="12" cy="12" r="10" fill={`url(#diffuse-${id})`} />
          <circle cx="12" cy="12" r="3" fill={ink} />
        </svg>
        <Handle type="source" position={Position.Bottom} className="!border-[var(--canvas-bg)] !bg-[var(--node-ink)]" />
      </div>
      <span className="truncate text-[10px] text-[var(--text-muted)]">{directoryName}</span>
      <DeleteButton onDelete={onDelete} nodeId={id} directoryName={directoryName} />
    </div>
  );
}

export function SessionNode({ id, selected, data }: NodeProps<SessionNodeData>) {
  const { session, skin, theme, onSelect, onOpenChat, onDelete } = data;
  const directoryName = session.directory.split(/[\\/]/).filter(Boolean).pop() ?? session.directory;

  if (skin === "minimal") {
    return (
      <MinimalNode
        id={id}
        selected={selected}
        directoryName={directoryName}
        theme={theme}
        onSelect={onSelect}
        onOpenChat={onOpenChat}
        onDelete={onDelete}
      />
    );
  }

  if (skin === "tui") {
    return (
      <TuiNode
        id={id}
        selected={selected}
        directoryName={directoryName}
        onSelect={onSelect}
        onOpenChat={onOpenChat}
        onDelete={onDelete}
      />
    );
  }

  return (
    <DefaultNode
      id={id}
      selected={selected}
      directoryName={directoryName}
      theme={theme}
      onSelect={onSelect}
      onOpenChat={onOpenChat}
      onDelete={onDelete}
    />
  );
}
