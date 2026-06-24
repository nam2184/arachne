import { useMemo } from "react";
import { SessionCard } from "@/components/sessions/SessionCard";
import { ScrollArea } from "@/components/ui/scroll-area";

export interface SessionListProps {
  sessions: Array<{
    id: string;
    directory: string;
    provider: string;
    model: string;
    messageCount?: number;
    lastActivity?: string | null;
    summary?: string | null;
  }>;
  onSessionClick?: (id: string) => void;
  onSessionDoubleClick?: (id: string) => void;
  onSessionDelete?: (id: string) => void;
  emptyMessage?: string;
}

export function SessionList({
  sessions,
  onSessionClick,
  onSessionDoubleClick,
  onSessionDelete,
  emptyMessage = "No sessions yet",
}: SessionListProps) {
  const sessionCards = useMemo(
    () =>
      sessions.map((session) => ({
        ...session,
        onClick: () => onSessionClick?.(session.id),
        onDoubleClick: () => onSessionDoubleClick?.(session.id),
        onDelete: onSessionDelete ? () => onSessionDelete(session.id) : undefined,
      })),
    [sessions, onSessionClick, onSessionDoubleClick, onSessionDelete]
  );

  return (
    <ScrollArea className="h-full">
      <div className="flex flex-col gap-3 p-4">
        {sessionCards.length === 0 ? (
          <div className="flex h-40 items-center justify-center">
            <p className="text-sm text-[#6c7086]">{emptyMessage}</p>
          </div>
        ) : (
          sessionCards.map((session) => (
            <SessionCard key={session.id} {...session} />
          ))
        )}
      </div>
    </ScrollArea>
  );
}