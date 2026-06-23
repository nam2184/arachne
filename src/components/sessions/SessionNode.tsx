import { Handle, Position, type NodeProps } from "reactflow";
import { Trash2 } from "lucide-react";
import { cn } from "@/lib/utils";
import type { AgentSession } from "@/features/sessions/sessionStore";
import type { NodeSkin } from "@/features/app/appStore";

interface SessionNodeData {
  session: AgentSession;
  skin: NodeSkin;
  onSelect: (id: string) => void;
  onOpenChat: (id: string) => void;
  onDelete?: (id: string) => void;
}

export function SessionNode({ id, selected, data }: NodeProps<SessionNodeData>) {
  const { session, skin, onSelect, onOpenChat, onDelete } = data;

  const directoryName = session.directory.split(/[\\/]/).filter(Boolean).pop() ?? session.directory;

  const handleDoubleClick = () => {
    onOpenChat(id);
  };

if (skin === "minimal") {
    return (
      <div className="group flex flex-col items-center gap-1">
        <div
          className={cn(
            "relative flex h-3 w-3 cursor-pointer items-center justify-center rounded-full bg-[#f5f5f5]",
            selected && "ring-2 ring-white ring-offset-2 ring-offset-black",
          )}
          onClick={() => onSelect(id)}
          onDoubleClick={handleDoubleClick}
        >
          <Handle type="target" position={Position.Top} className="!h-1 !w-1 !border-0 !bg-[#737373]" />
          <Handle type="source" position={Position.Bottom} className="!h-1 !w-1 !border-0 !bg-[#737373]" />
        </div>
        <span className="truncate text-[10px] text-[#737373]">{directoryName}</span>
        {onDelete && (
          <button
            type="button"
            onClick={(event) => {
              event.stopPropagation();
              onDelete(id);
            }}
            className="mt-1 flex items-center gap-1 border border-[#1f1f1f] bg-[#0a0a0a] px-1.5 py-0.5 text-[10px] text-[#a0a0a0] opacity-0 transition-opacity group-hover:opacity-100 hover:border-[#ff5f5f] hover:text-[#ff5f5f]"
            title={`Delete ${directoryName}`}
          >
            <Trash2 className="h-2.5 w-2.5" />
            Delete
          </button>
        )}
      </div>
    );
  }

  if (skin === "tui") {
    return (
      <div className="group flex flex-col items-start gap-0">
        <div
          className={cn(
            "relative flex h-10 w-10 cursor-pointer items-center justify-center border border-[#f5f5f5] bg-black",
            selected && "bg-[#f5f5f5]",
          )}
          onClick={() => onSelect(id)}
          onDoubleClick={handleDoubleClick}
        >
          <Handle type="target" position={Position.Top} className="!h-1 !w-1 !border-0 !bg-[#f5f5f5]" />
          <span className={cn("text-xs", selected ? "text-black" : "text-[#f5f5f5]")}>
            ◉
          </span>
          <Handle type="source" position={Position.Bottom} className="!h-1 !w-1 !border-0 !bg-[#f5f5f5]" />
        </div>
        <span className="truncate border-l border-r border-b border-[#f5f5f5] px-1 text-[10px] text-[#f5f5f5]">
          {directoryName}
        </span>
        {onDelete && (
          <button
            type="button"
            onClick={(event) => {
              event.stopPropagation();
              onDelete(id);
            }}
            className="mt-1 flex items-center gap-1 border border-[#f5f5f5] bg-black px-1.5 py-0.5 text-[10px] text-[#a0a0a0] opacity-0 transition-opacity group-hover:opacity-100 hover:border-[#ff5f5f] hover:text-[#ff5f5f]"
            title={`Delete ${directoryName}`}
          >
            <Trash2 className="h-2.5 w-2.5" />
            Delete
          </button>
        )}
      </div>
    );
  }

  // default: original diffused-orb design
  return (
    <div className="group flex flex-col items-center gap-1">
      <div
        className={cn(
          "relative flex h-10 w-10 cursor-pointer items-center justify-center rounded-full",
          selected && "ring-2 ring-white ring-offset-2 ring-offset-black",
        )}
        onClick={() => onSelect(id)}
        onDoubleClick={handleDoubleClick}
      >
        <Handle type="target" position={Position.Top} className="!border-black !bg-white" />
        <svg viewBox="0 0 24 24" className="h-5 w-5">
          <defs>
            <radialGradient id={`diffuse-${id}`} cx="50%" cy="50%" r="50%">
              <stop offset="0%" stopColor="#ffffff" stopOpacity="0.9" />
              <stop offset="70%" stopColor="#ffffff" stopOpacity="0.22" />
              <stop offset="100%" stopColor="#ffffff" stopOpacity="0" />
            </radialGradient>
          </defs>
          <circle cx="12" cy="12" r="10" fill={`url(#diffuse-${id})`} />
          <circle cx="12" cy="12" r="3" fill="#ffffff" />
        </svg>
<Handle type="source" position={Position.Bottom} className="!border-black !bg-white" />
      </div>
      <span className="truncate text-[10px] text-[#737373]">{directoryName}</span>
      {onDelete && (
        <button
          type="button"
          onClick={(event) => {
            event.stopPropagation();
            onDelete(id);
          }}
          className="mt-1 flex items-center gap-1 border border-[#1f1f1f] bg-[#0a0a0a] px-1.5 py-0.5 text-[10px] text-[#a0a0a0] opacity-0 transition-opacity group-hover:opacity-100 hover:border-[#ff5f5f] hover:text-[#ff5f5f]"
          title={`Delete ${directoryName}`}
        >
          <Trash2 className="h-2.5 w-2.5" />
          Delete
        </button>
      )}
    </div>
  );
}
