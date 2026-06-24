import { Shield } from "lucide-react";
import { useMemo } from "react";
import {
  type PermissionPrompt,
  usePermissionStore,
} from "@/features/permissions/permissionStore";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";

interface PermissionPromptBarProps {
  /** The session whose prompts to show. If null, no bar is rendered. */
  sessionId: string | null;
}

export function PermissionPromptBar({ sessionId }: PermissionPromptBarProps) {
  const pending = usePermissionStore((state) => state.pending);
  const reply = usePermissionStore((state) => state.reply);

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
          onReply={(replyKind) => reply(prompt.sessionId, prompt.id, replyKind)}
        />
      ))}
    </div>
  );
}

function PermissionPromptCard({
  prompt,
  onReply,
}: {
  prompt: PermissionPrompt;
  onReply: (reply: "once" | "always" | "reject") => void;
}) {
  const suggestedPatterns = prompt.always.length > 0 ? prompt.always : prompt.patterns;

  return (
    <Card className="pointer-events-auto w-full max-w-2xl border-[#1f1f1f] bg-[#0a0a0a] p-4 shadow-none">
      <div className="flex items-start gap-3">
        <div className="mt-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-none border border-[#1f1f1f] bg-[#050505]">
          <Shield className="h-4 w-4 text-white" />
        </div>
        <div className="min-w-0 flex-1 space-y-2">
          <div>
            <p className="text-sm font-medium text-white">
              Allow <span className="text-[#bdbdbd]">{prompt.tool}</span> to run?
            </p>
            <ul className="mt-1 space-y-0.5 text-xs text-[#8a8a8a]">
              {prompt.patterns.map((pattern) => (
                <li key={pattern} className="truncate font-mono">
                  {prompt.permission} {pattern}
                </li>
              ))}
            </ul>
          </div>

          <div className="flex flex-wrap items-center gap-2 pt-1">
            <Button size="sm" onClick={() => onReply("once")}>
              Allow once
            </Button>
            {suggestedPatterns.length > 0 && (
              <Button size="sm" variant="secondary" onClick={() => onReply("always")}>
                Always allow {suggestedPatterns[0]}
              </Button>
            )}
            <Button size="sm" variant="ghost" onClick={() => onReply("reject")}>
              Reject
            </Button>
          </div>

          {suggestedPatterns.length > 1 && (
            <p className="text-[10px] text-[#737373]">
              +{suggestedPatterns.length - 1} more pattern
              {suggestedPatterns.length - 1 === 1 ? "" : "s"} will be remembered
            </p>
          )}
        </div>
      </div>
    </Card>
  );
}
