import { FileDiff, X } from "lucide-react";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import type { SessionFileDiff } from "@/features/sessions/conversationStore";
import { cn } from "@/lib/utils";

interface SessionDiffPaneProps {
  diffs: SessionFileDiff[];
  width: number;
  onClose: () => void;
}

interface PatchLine {
  id: string;
  text: string;
  kind: "add" | "delete" | "context" | "hunk" | "meta";
}

export function SessionDiffPane({ diffs, width, onClose }: SessionDiffPaneProps) {
  const [activeFile, setActiveFile] = useState(() => diffs[0]?.file ?? "");

  useEffect(() => {
    if (diffs.length === 0) {
      setActiveFile("");
      return;
    }
    if (!diffs.some((diff) => diff.file === activeFile)) {
      setActiveFile(diffs[0].file);
    }
  }, [activeFile, diffs]);

  const selectedDiff = diffs.find((diff) => diff.file === activeFile) ?? diffs[0] ?? null;
  const totals = summarizeDiffs(diffs);
  const lines = selectedDiff ? parsePatch(selectedDiff.patch) : [];

  return (
    <aside
      className="flex min-w-0 max-w-[760px] shrink-0 flex-col bg-[var(--background)]"
      style={{ width }}
    >
      <div className="flex h-14 items-center justify-between border-b border-[var(--border)] px-4">
        <div className="min-w-0">
          <div className="flex items-center gap-2 text-sm font-semibold text-[var(--foreground)]">
            <FileDiff className="h-4 w-4 text-[var(--text-muted)]" />
            Changes
          </div>
          <p className="truncate text-[11px] text-[var(--text-muted)]">
            {diffs.length} file{diffs.length === 1 ? "" : "s"} changed  +{totals.additions} -{totals.deletions}
          </p>
        </div>
        <Button variant="ghost" size="icon" className="h-8 w-8 shrink-0" onClick={onClose} title="Close changes">
          <X className="h-4 w-4" />
        </Button>
      </div>

      {diffs.length === 0 ? (
        <div className="flex min-h-0 flex-1 items-center justify-center px-6 text-center text-xs text-[var(--text-muted)]">
          No file changes for this turn yet.
        </div>
      ) : (
        <div className="grid min-h-0 flex-1 grid-cols-[minmax(160px,0.36fr)_minmax(0,1fr)]">
          <ScrollArea className="border-r border-[var(--border)] bg-[var(--surface)]">
            <div className="py-1">
              {diffs.map((diff) => {
                const active = diff.file === selectedDiff?.file;
                return (
                  <button
                    key={diff.file}
                    type="button"
                    onClick={() => setActiveFile(diff.file)}
                    className={cn(
                      "flex w-full min-w-0 flex-col border-l-2 px-3 py-2 text-left transition-colors",
                      active
                        ? "border-[var(--node-border-hover)] bg-[var(--surface-soft)] text-[var(--foreground)]"
                        : "border-transparent text-[var(--text-muted)] hover:bg-[var(--surface-soft)] hover:text-[var(--foreground)]",
                    )}
                  >
                    <span className="flex w-full min-w-0 items-center gap-2">
                      <span className={cn("h-1.5 w-1.5 shrink-0 rounded-full", statusDotClass(diff.status))} />
                      <span className="truncate font-mono text-[11px]">{diff.file}</span>
                    </span>
                    <span className="mt-1 flex gap-2 pl-3.5 text-[10px] uppercase tracking-[0.12em]">
                      <span className={statusTextClass(diff.status)}>{diff.status}</span>
                      <span className="text-[#6fdc8c]">+{diff.additions}</span>
                      <span className="text-[#ff7a7a]">-{diff.deletions}</span>
                    </span>
                  </button>
                );
              })}
            </div>
          </ScrollArea>

          <div className="flex min-w-0 flex-col">
            <div className="flex min-h-10 items-center justify-between gap-3 border-b border-[var(--border)] bg-[var(--surface-raised)] px-3">
              <div className="min-w-0 truncate font-mono text-[11px] text-[var(--foreground)]">
                {selectedDiff?.file}
              </div>
              {selectedDiff && (
                <div className="flex shrink-0 gap-2 font-mono text-[11px]">
                  <span className="text-[#6fdc8c]">+{selectedDiff.additions}</span>
                  <span className="text-[#ff7a7a]">-{selectedDiff.deletions}</span>
                </div>
              )}
            </div>
            <ScrollArea className="min-h-0 flex-1 bg-[#050505]">
              {selectedDiff?.patch ? (
                <div className="min-w-max py-2 font-mono text-[11px] leading-5">
                  {lines.map((line) => (
                    <div key={line.id} className={cn("grid grid-cols-[3.5rem_minmax(0,1fr)] border-l-2", patchLineClass(line.kind))}>
                      <span className="select-none border-r border-white/5 pr-2 text-right text-[var(--text-muted)]">
                        {line.kind === "hunk" || line.kind === "meta" ? "" : line.id}
                      </span>
                      <span className="whitespace-pre px-3">{line.text || " "}</span>
                    </div>
                  ))}
                </div>
              ) : (
                <div className="flex h-full min-h-40 items-center justify-center px-6 text-center text-xs text-[var(--text-muted)]">
                  Patch content is unavailable for this file.
                </div>
              )}
            </ScrollArea>
          </div>
        </div>
      )}
    </aside>
  );
}

function summarizeDiffs(diffs: SessionFileDiff[]) {
  return diffs.reduce(
    (total, diff) => ({
      additions: total.additions + diff.additions,
      deletions: total.deletions + diff.deletions,
    }),
    { additions: 0, deletions: 0 },
  );
}

function parsePatch(patch?: string): PatchLine[] {
  if (!patch) return [];
  return patch.split(/\r?\n/).map((text, index) => ({
    id: String(index + 1),
    text,
    kind: patchLineKind(text),
  }));
}

function patchLineKind(text: string): PatchLine["kind"] {
  if (text.startsWith("@@")) return "hunk";
  if (text.startsWith("diff --git") || text.startsWith("index ") || text.startsWith("--- ") || text.startsWith("+++ ")) {
    return "meta";
  }
  if (text.startsWith("+")) return "add";
  if (text.startsWith("-")) return "delete";
  return "context";
}

function patchLineClass(kind: PatchLine["kind"]) {
  switch (kind) {
    case "add":
      return "border-[#2f8f4f] bg-[#0b2715] text-[#c6f5d2]";
    case "delete":
      return "border-[#9b3737] bg-[#321111] text-[#ffd0d0]";
    case "hunk":
      return "border-[#4c65b8] bg-[#111827] text-[#b7c7ff]";
    case "meta":
      return "border-transparent bg-[#080808] text-[var(--text-muted)]";
    default:
      return "border-transparent text-[var(--text-secondary)]";
  }
}

function statusDotClass(status: SessionFileDiff["status"]) {
  if (status === "added") return "bg-[#6fdc8c]";
  if (status === "deleted") return "bg-[#ff7a7a]";
  return "bg-[#d6b85a]";
}

function statusTextClass(status: SessionFileDiff["status"]) {
  if (status === "added") return "text-[#6fdc8c]";
  if (status === "deleted") return "text-[#ff7a7a]";
  return "text-[#d6b85a]";
}
