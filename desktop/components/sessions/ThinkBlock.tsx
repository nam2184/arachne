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
        "text-[11px] italic text-[#6b6b6b] whitespace-pre-wrap break-words",
        className,
      )}
    >
      <div className="bg-gradient-to-r from-[#6b6b6b] to-[#4a4a4a] bg-clip-text text-transparent">
        {text}
      </div>
    </div>
  );
}
