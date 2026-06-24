import { forwardRef } from "react";
import { Folder, MessageSquare, Clock } from "lucide-react";
import { cn } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";

export interface SessionCardProps {
  id: string;
  directory: string;
  provider: string;
  model: string;
  messageCount?: number;
  lastActivity?: string | null;
  summary?: string | null;
  onClick?: () => void;
  onDoubleClick?: () => void;
  onDelete?: () => void;
  className?: string;
}

export const SessionCard = forwardRef<HTMLDivElement, SessionCardProps>(
  ({ directory, provider, model, messageCount, lastActivity, summary, onClick, onDoubleClick, onDelete, className }, ref) => {
    const directoryName = directory.split(/[\\/]/).filter(Boolean).pop() ?? directory;

    const formatTime = (isoString: string | null | undefined) => {
      if (!isoString) return "No activity";
      const date = new Date(isoString);
      const now = new Date();
      const diff = now.getTime() - date.getTime();
      const minutes = Math.floor(diff / 60000);
      const hours = Math.floor(diff / 3600000);
      const days = Math.floor(diff / 86400000);

      if (minutes < 1) return "Just now";
      if (minutes < 60) return `${minutes}m ago`;
      if (hours < 24) return `${hours}h ago`;
      if (days < 7) return `${days}d ago`;
      return date.toLocaleDateString();
    };

    return (
      <div
        ref={ref}
        className={cn(
          "group relative flex cursor-pointer flex-col gap-2 rounded-none border border-[#1f1f1f] bg-[#0a0a0a] p-3 transition-colors hover:border-[#2a2a2a] hover:bg-[#111111]",
          className
        )}
        onClick={onClick}
        onDoubleClick={onDoubleClick}
      >
        <div className="flex items-start justify-between gap-2">
          <div className="flex min-w-0 items-center gap-2">
            <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-none border border-[#1f1f1f] bg-[#050505]">
              <Folder className="h-3.5 w-3.5 text-[#d4d4d4]" />
            </div>
            <div className="min-w-0 flex-1">
              <h3 className="truncate text-sm font-medium text-white">{directoryName}</h3>
              <p className="truncate text-xs text-[#737373]">{directory}</p>
            </div>
          </div>
          {onDelete && (
            <Button
              variant="ghost"
              size="icon"
              className="absolute right-2 top-2 h-6 w-6 opacity-0 transition-opacity group-hover:opacity-100"
              onClick={(e) => {
                e.stopPropagation();
                onDelete();
              }}
            >
              <span className="text-xs text-[#737373]">×</span>
            </Button>
          )}
        </div>

        <div className="flex flex-wrap items-center gap-1.5">
          <Badge variant="secondary" className="text-[10px]">
            {provider}
          </Badge>
          <Badge variant="outline" className="text-[10px]">
            {model.split(/[-_]/).pop() ?? model}
          </Badge>
        </div>

        {summary && (
          <p className="line-clamp-2 text-xs text-[#737373]">{summary}</p>
        )}

        <div className="flex items-center gap-3 text-[10px] text-[#737373]">
          <span className="flex items-center gap-1">
            <MessageSquare className="h-3 w-3" />
            {messageCount ?? 0}
          </span>
          <span className="flex items-center gap-1">
            <Clock className="h-3 w-3" />
            {formatTime(lastActivity)}
          </span>
        </div>
      </div>
    );
  }
);

SessionCard.displayName = "SessionCard";