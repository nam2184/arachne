import { Handle, Position, type NodeProps } from "reactflow";
import type { SessionLoop, SessionLoopStatus } from "@/features/loops/loopStore";
import { cn } from "@/lib/utils";

export type LoopGoalStatus = "pending" | "in_progress" | "done" | "skipped";

export interface LoopNodeData {
  loop: SessionLoop;
  onConfigure: (id: string) => void;
  onDelete: (id: string) => void;
}

const statusTone: Record<SessionLoopStatus, string> = {
  active: "border-[var(--node-focus)] text-[var(--node-focus)]",
  paused: "border-[#facc15] text-[#facc15]",
  completed: "border-[var(--text-subtle)] text-[var(--text-subtle)]",
};

const goalTone: Record<LoopGoalStatus, string> = {
  pending: "border-[var(--node-border)] text-[var(--text-muted)]",
  in_progress: "border-[var(--node-focus)] text-[var(--node-focus)]",
  done: "border-[var(--text-subtle)] text-[var(--text-subtle)] line-through",
  skipped: "border-[var(--node-border-hover)] text-[var(--text-muted)] line-through",
};

function formatCap(value: number) {
  if (value <= 0) return "--";
  if (value >= 1000) return `${Math.round(value / 100) / 10}k`;
  return value.toString();
}

export function LoopNode({ selected, data }: NodeProps<LoopNodeData>) {
  const { loop } = data;
  const currentGoal = loop.goals.find((goal) => goal.status === "in_progress") ?? loop.goals[0];

  return (
    <div
      className={cn(
        "relative w-[220px] border bg-[var(--node-bg)] font-mono text-[var(--text-secondary)] shadow-[0_18px_60px_rgba(0,0,0,0.18)]",
        selected ? "border-[var(--node-focus)]" : "border-[var(--node-border)]",
      )}
    >
      <Handle type="target" position={Position.Left} className="!h-2 !w-2 !border-0 !bg-[var(--node-muted)]" />
      <Handle type="source" position={Position.Right} className="!h-2 !w-2 !border-0 !bg-[var(--node-focus)]" />

      <div className="space-y-2 px-3 py-2">
        <div className="flex items-start justify-between gap-2">
          <div className="min-w-0 flex-1">
            <div className="mb-1 text-[9px] uppercase tracking-[0.24em] text-[var(--text-muted)]">loop</div>
            <div className="truncate text-sm text-[var(--foreground)]">{loop.title}</div>
          </div>
          <span className={cn("shrink-0 border px-1.5 py-0.5 text-[8px] uppercase tracking-[0.14em]", statusTone[loop.status])}>
            {loop.status}
          </span>
        </div>

        <div className="grid grid-cols-3 gap-1 text-center text-[10px]">
          <div className="border border-[var(--node-border)] bg-[var(--surface)] px-1.5 py-1">
            <div className="text-[var(--text-muted)]">sessions</div>
            <div className="text-[var(--foreground)]">{loop.session_ids.length}</div>
          </div>
          <div className="border border-[var(--node-border)] bg-[var(--surface)] px-1.5 py-1">
            <div className="text-[var(--text-muted)]">goals</div>
            <div className="text-[var(--foreground)]">{loop.goals.length}</div>
          </div>
          <div className="border border-[var(--node-border)] bg-[var(--surface)] px-1.5 py-1">
            <div className="text-[var(--text-muted)]">tokens</div>
            <div className="text-[var(--foreground)]">{formatCap(loop.token_limit)}</div>
          </div>
        </div>

        <div className="grid grid-cols-[1fr_auto] items-center gap-2 border border-[var(--node-border)] bg-[var(--surface)] px-2 py-1.5">
          <div className="min-w-0 truncate text-[11px] text-[var(--text-secondary)]">
            {currentGoal?.text ?? "No goal configured"}
          </div>
          {currentGoal && (
            <span className={cn("border px-1 text-[8px] uppercase", goalTone[currentGoal.status])}>
              {currentGoal.status.replace("_", " ")}
            </span>
          )}
        </div>
      </div>

      <div className="nodrag grid grid-cols-2 border-t border-[var(--node-border)] text-[10px] uppercase tracking-[0.16em]">
        <button type="button" onClick={() => data.onConfigure(loop.id)} className="border-r border-[var(--node-border)] px-2 py-2 text-[var(--node-focus)]">Configure</button>
        <button type="button" onClick={() => data.onDelete(loop.id)} className="px-2 py-2 text-[#ff5f5f]">Delete</button>
      </div>
    </div>
  );
}
