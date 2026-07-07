import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

interface PromptStackProps {
  children: ReactNode;
  className?: string;
}

export function PromptStack({ children, className }: PromptStackProps) {
  return (
    <div className={cn("pointer-events-none fixed inset-x-0 bottom-0 z-50 flex flex-col items-center gap-2 p-4", className)}>
      {children}
    </div>
  );
}

interface PromptCardProps {
  title: ReactNode;
  detail?: ReactNode;
  actions?: ReactNode;
  children?: ReactNode;
  className?: string;
}

export function PromptCard({ title, detail, actions, children, className }: PromptCardProps) {
  return (
    <div className={cn("pointer-events-auto flex w-full max-w-xl items-center gap-3 border border-[#2a2a2a] bg-[#050505] px-3 py-2 shadow-none", className)}>
      <div className="min-w-0 flex-1">
        <p className="truncate text-sm font-medium text-white">{title}</p>
        {detail && <p className="truncate font-mono text-xs text-[#8a8a8a]">{detail}</p>}
        {children}
      </div>
      {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
    </div>
  );
}
