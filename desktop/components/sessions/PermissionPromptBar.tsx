import { useMemo } from "react";
import {
  type PermissionPrompt,
  usePermissionStore,
} from "@/features/permissions/permissionStore";
import { Button } from "@/components/ui/button";

interface PermissionPromptBarProps {
  /** The session whose prompts to show. If null, no bar is rendered. */
  sessionId: string | null;
  sessionDirectory?: string | null;
}

export function PermissionPromptBar({ sessionId, sessionDirectory }: PermissionPromptBarProps) {
  const pending = usePermissionStore((state) => state.pending);
  const reply = usePermissionStore((state) => state.reply);
  const sessionName = useMemo(
    () => directoryName(sessionDirectory),
    [sessionDirectory],
  );

  const prompts = useMemo(
    () => pending.filter((p) => p.sessionId === sessionId),
    [pending, sessionId],
  );

  if (!sessionId || prompts.length === 0) {
    return null;
  }

  return (
    <div className="pointer-events-none fixed inset-x-0 bottom-0 z-50 flex flex-col items-center gap-2 p-4">
      {prompts.map((prompt) => (
        <PermissionPromptCard
          key={prompt.id}
          prompt={prompt}
          sessionName={sessionName}
          onReply={(replyKind) => reply(prompt.sessionId, prompt.id, replyKind)}
        />
      ))}
    </div>
  );
}

function PermissionPromptCard({
  prompt,
  sessionName,
  onReply,
}: {
  prompt: PermissionPrompt;
  sessionName: string;
  onReply: (reply: "once" | "reject") => void;
}) {
  const label =
    prompt.permission === "external_directory"
      ? `Allow session ${sessionName} agent to access external directory?`
      : "Allow agent to do this?";

  return (
    <div className="pointer-events-auto flex w-full max-w-xl items-center gap-3 border border-[#2a2a2a] bg-[#050505] px-3 py-2 shadow-none">
      <div className="min-w-0 flex-1">
        <p className="truncate text-sm font-medium text-white">{label}</p>
        <p className="truncate font-mono text-xs text-[#8a8a8a]">
          {prompt.tool}: {prompt.patterns[0] ?? prompt.permission}
        </p>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <Button size="sm" onClick={() => onReply("once")}>
          Yes
        </Button>
        <Button size="sm" variant="ghost" onClick={() => onReply("reject")}>
          No
        </Button>
      </div>
    </div>
  );
}

function directoryName(path: string | null | undefined): string {
  if (!path) {
    return "this";
  }
  const trimmed = path.replace(/[\\/]+$/, "");
  return trimmed.split(/[\\/]/).pop() || "this";
}
