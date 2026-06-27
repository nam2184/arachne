import { cn } from "@/lib/utils";

interface ThinkBlockProps {
  text: string;
  className?: string;
  defaultOpen?: boolean;
}

export function ThinkBlock({ text, className, defaultOpen = false }: ThinkBlockProps) {
  if (!text) return null;
  void defaultOpen;

  return (
    <div
      className={cn(
        "text-[11px] italic text-[var(--text-muted)] whitespace-pre-wrap break-words",
        className,
      )}
    >
      <div>{text}</div>
    </div>
  );
}
