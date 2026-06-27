import { useEffect, useState } from "react";
import { Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Dialog } from "@/components/ui/dialog";

export interface DeleteSessionDialogProps {
  open: boolean;
  sessionId: string | null;
  sessionLabel?: string | null;
  kind?: "session" | "chat";
  onCancel: () => void;
  onConfirm: (id: string) => Promise<void> | void;
}

export function DeleteSessionDialog({ open, sessionId, sessionLabel, kind = "session", onCancel, onConfirm }: DeleteSessionDialogProps) {
  const [isDeleting, setIsDeleting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (open) {
      setError(null);
      setIsDeleting(false);
    }
  }, [open, sessionId]);

  const handleConfirm = async () => {
    if (!sessionId || isDeleting) return;
    setIsDeleting(true);
    setError(null);
    try {
      await onConfirm(sessionId);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setIsDeleting(false);
    }
  };

  return (
    <Dialog
      open={open}
      title={kind === "chat" ? "Delete chat" : "Delete session"}
      description={
        sessionLabel
          ? `This will permanently delete “${sessionLabel}” and its conversation history. This action cannot be undone.`
          : `This will permanently delete the selected ${kind} and its conversation history. This action cannot be undone.`
      }
      onClose={isDeleting ? () => undefined : onCancel}
      footer={
        <>
          <Button variant="ghost" onClick={onCancel} disabled={isDeleting}>
            Cancel
          </Button>
          <Button
            variant="destructive"
            onClick={handleConfirm}
            disabled={isDeleting || !sessionId}
            className="gap-2"
          >
            <Trash2 className="h-4 w-4" />
            {isDeleting ? "Deleting…" : "Delete"}
          </Button>
        </>
      }
    >
      {error && (
        <p className="border border-[#3a1f1f] bg-[#1a0808] px-3 py-2 text-xs text-[#ff8a8a]">
          {error}
        </p>
      )}
    </Dialog>
  );
}
