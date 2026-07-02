import { MarkdownContent } from "@/components/ui/markdown-content";
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
        "text-[11px] text-[var(--text-muted)]",
        className,
      )}
    >
      <MarkdownContent text={text} streaming={defaultOpen} className="markdown-content-thinking" />
    </div>
  );
}
