import { Link2, Trash2 } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";
import type { SessionGroup } from "@/features/sessions/sessionStore";

interface SessionGroupsPanelProps {
  groups: SessionGroup[];
  onRenameGroup: (groupId: string, currentName: string | null | undefined, nextName: string) => void;
  onDeleteGroup: (groupId: string) => void;
}

export function SessionGroupsPanel({
  groups,
  onRenameGroup,
  onDeleteGroup,
}: SessionGroupsPanelProps) {
  return (
    <aside className="w-[260px] shrink-0 border-r border-[#313244] bg-[#181825] p-3">
      <Card className="h-full overflow-hidden bg-[#1e1e2e]/40">
        <CardHeader className="border-b border-[#313244]">
          <CardTitle className="flex items-center gap-2">
            <Link2 className="h-4 w-4 text-[#89b4fa]" />
            Session Groups
          </CardTitle>
        </CardHeader>
        <CardContent className="h-[calc(100%-57px)] p-0">
          <ScrollArea className="h-full">
            <div className="space-y-2 p-3">
              {groups.length === 0 ? (
                <p className="rounded-lg border border-dashed border-[#313244] p-3 text-xs text-[#6c7086]">
                  Connect two session nodes to create an untitled group.
                </p>
              ) : (
                groups.map((group) => (
                  <div
                    key={group.id}
                    className="group rounded-lg border border-[#313244] bg-[#181825] p-2 transition-colors hover:border-[#45475a]"
                  >
                    <div className="flex items-center gap-2">
                      <Input
                        className="h-8 border-transparent bg-transparent px-2 text-xs placeholder:text-[#cdd6f4] focus-visible:bg-[#11111b]"
                        defaultValue={group.name ?? ""}
                        placeholder="Untitled group"
                        onBlur={(event) => onRenameGroup(group.id, group.name, event.target.value)}
                        onKeyDown={(event) => {
                          if (event.key === "Enter") {
                            event.currentTarget.blur();
                          }
                        }}
                      />
                      <Button
                        variant="ghost"
                        size="icon"
                        className="h-8 w-8 opacity-0 transition-opacity group-hover:opacity-100"
                        onClick={() => onDeleteGroup(group.id)}
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                      </Button>
                    </div>
                    <Badge variant="secondary" className="mt-2">
                      {group.session_ids.length} sessions
                    </Badge>
                  </div>
                ))
              )}
            </div>
          </ScrollArea>
        </CardContent>
      </Card>
    </aside>
  );
}
