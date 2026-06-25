import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Dialog } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import type { LoopInput, SessionLoop, SessionLoopStatus } from "@/features/loops/loopStore";

interface LoopConfigDialogProps {
  open: boolean;
  loop: SessionLoop | null;
  onCancel: () => void;
  onSave: (input: LoopInput) => void;
}

const statuses: SessionLoopStatus[] = ["active", "paused", "completed"];

export function LoopConfigDialog({ open, loop, onCancel, onSave }: LoopConfigDialogProps) {
  const [title, setTitle] = useState("");
  const [goals, setGoals] = useState("");
  const [tokenLimit, setTokenLimit] = useState("48000");
  const [status, setStatus] = useState<SessionLoopStatus>("active");

  useEffect(() => {
    if (!open) return;

    setTitle(loop?.title ?? "");
    setGoals(loop?.goals.map((goal) => goal.text).join("\n") ?? "");
    setTokenLimit(loop?.token_limit ? String(loop.token_limit) : "48000");
    setStatus(loop?.status ?? "active");
  }, [loop, open]);

  const handleSubmit = () => {
    onSave({
      title,
      goals: goals.split("\n"),
      tokenLimit: Number(tokenLimit),
      status,
    });
  };

  return (
    <Dialog
      open={open}
      title={loop ? "Configure loop" : "Create loop"}
      description="Loops are containers. Append sessions by connecting a session node to a loop node."
      onClose={onCancel}
      className="max-w-lg"
      footer={
        <>
          <Button variant="ghost" onClick={onCancel}>Cancel</Button>
          <Button onClick={handleSubmit}>{loop ? "Save loop" : "Create loop"}</Button>
        </>
      }
    >
      <label className="block space-y-1.5">
        <span className="text-[10px] uppercase tracking-[0.18em] text-[var(--text-muted)]">Title</span>
        <Input value={title} onChange={(event) => setTitle(event.target.value)} placeholder="Loop title" />
      </label>

      <label className="block space-y-1.5">
        <span className="text-[10px] uppercase tracking-[0.18em] text-[var(--text-muted)]">Goals</span>
        <textarea
          value={goals}
          onChange={(event) => setGoals(event.target.value)}
          placeholder="One goal per line"
          className="min-h-32 w-full resize-none rounded-none border border-[var(--input-border)] bg-[var(--input-bg)] px-3 py-2 text-sm text-[var(--foreground)] shadow-none placeholder:text-[var(--text-muted)] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--foreground)]"
        />
      </label>

      <div className="grid grid-cols-2 gap-3">
        <label className="block space-y-1.5">
          <span className="text-[10px] uppercase tracking-[0.18em] text-[var(--text-muted)]">Token limit</span>
          <Input
            type="number"
            min={0}
            value={tokenLimit}
            onChange={(event) => setTokenLimit(event.target.value)}
            placeholder="48000"
          />
        </label>

        <label className="block space-y-1.5">
          <span className="text-[10px] uppercase tracking-[0.18em] text-[var(--text-muted)]">Status</span>
          <select
            value={status}
            onChange={(event) => setStatus(event.target.value as SessionLoopStatus)}
            className="flex h-9 w-full rounded-none border border-[var(--input-border)] bg-[var(--input-bg)] px-3 py-1 text-sm text-[var(--foreground)] shadow-none focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--foreground)]"
          >
            {statuses.map((item) => (
              <option key={item} value={item}>{item}</option>
            ))}
          </select>
        </label>
      </div>
    </Dialog>
  );
}
