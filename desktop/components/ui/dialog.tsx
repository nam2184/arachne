import { useEffect, type ReactNode } from "react";
import { X } from "lucide-react";
import { cn } from "@/lib/utils";

export interface DialogProps {
  open: boolean;
  title: string;
  description?: ReactNode;
  children?: ReactNode;
  footer?: ReactNode;
  onClose: () => void;
  className?: string;
}

export function Dialog({ open, title, description, children, footer, onClose, className }: DialogProps) {
  useEffect(() => {
    if (!open) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-[100] flex items-center justify-center bg-black/70 p-4" role="dialog" aria-modal="true" aria-label={title}>
      <div className={cn("pointer-events-auto flex w-full max-w-md flex-col border border-[#1f1f1f] bg-[#0a0a0a] text-white shadow-none", className)}>
        <div className="flex items-center justify-between border-b border-[#1f1f1f] px-5 py-3">
          <h2 className="text-sm font-semibold">{title}</h2>
          <button
            type="button"
            onClick={onClose}
            className="flex h-6 w-6 items-center justify-center text-[#737373] transition-colors hover:text-white"
            aria-label="Close dialog"
          >
            <X className="h-4 w-4" />
          </button>
        </div>
        <div className="space-y-3 px-5 py-4 text-sm text-[#d4d4d4]">
          {description && <p className="text-xs text-[#a0a0a0]">{description}</p>}
          {children}
        </div>
        {footer && (
          <div className="flex items-center justify-end gap-2 border-t border-[#1f1f1f] px-5 py-3">
            {footer}
          </div>
        )}
      </div>
    </div>
  );
}