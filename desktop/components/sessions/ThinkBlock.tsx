import { cn } from "@/lib/utils";

interface ThinkBlockProps {
  text: string;
  className?: string;
  defaultOpen?: boolean;
}

export function ThinkBlock({ text, className, defaultOpen = false }: ThinkBlockProps) {
  if (!text) return null;

  return (
    <div
      className={cn(
        "border-l-2 border-[#2a2a2a] pl-3 py-1 text-xs italic text-[#6b6b6b] whitespace-pre-wrap break-words",
        className,
      )}
    >
      <details open={defaultOpen}>
        <summary className="cursor-pointer select-none text-[#6b6b6b] hover:text-[#8a8a8a]">
          <span className="mr-1">◌</span>Thinking
        </summary>
        <div className="mt-1 text-transparent bg-clip-text bg-gradient-to-r from-[#6b6b6b] to-[#4a4a4a]">
          {text}
        </div>
      </details>
    </div>
  );
}
