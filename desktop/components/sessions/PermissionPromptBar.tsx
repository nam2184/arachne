import { useMemo } from "react";
import {
  type PermissionPrompt,
  usePermissionStore,
} from "@/features/permissions/permissionStore";
import { Button } from "@/components/ui/button";
import { PromptCard, PromptStack } from "@/components/ui/prompt-card";

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
    <PromptStack>
      {prompts.map((prompt) => (
        <PermissionPromptCard
          key={prompt.id}
          prompt={prompt}
          sessionName={sessionName}
          onReply={(replyKind) => reply(prompt.sessionId, prompt.id, replyKind)}
        />
      ))}
    </PromptStack>
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
    <PromptCard
      title={label}
      detail={`${prompt.tool}: ${prompt.patterns[0] ?? prompt.permission}`}
      actions={
        <>
        <Button size="sm" onClick={() => onReply("once")}>
          Yes
        </Button>
        <Button size="sm" variant="ghost" onClick={() => onReply("reject")}>
          No
        </Button>
        </>
      }
    />
  );
}

function directoryName(path: string | null | undefined): string {
  if (!path) {
    return "this";
  }
  const trimmed = path.replace(/[\\/]+$/, "");
  return trimmed.split(/[\\/]/).pop() || "this";
}
