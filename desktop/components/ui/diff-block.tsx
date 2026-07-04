import { parsePatch, type StructuredPatch } from "diff";
import { useMemo } from "react";
import { ThemedCodeBlock } from "@/components/ui/themed-code-block";
import { cn } from "@/lib/utils";

type DiffRowType = "added" | "removed" | "unchanged" | "hunk";

interface DiffRow {
  id: string;
  type: DiffRowType;
  oldNumber: number | null;
  newNumber: number | null;
  text: string;
}

interface DiffBlockProps {
  diff: string;
  themed?: boolean;
  className?: string;
}

export function DiffBlock({ diff, themed = false, className }: DiffBlockProps) {
  const patches = useMemo(() => parseDiff(diff), [diff]);

  if (!diff.trim()) return null;

  if (themed) {
    return <ThemedCodeBlock code={diff} lang="diff" className={className} />;
  }

  if (patches.length === 0) {
    return (
      <pre className={cn("max-h-72 overflow-auto whitespace-pre-wrap break-words border border-[var(--border)] bg-[var(--surface-raised)] p-3 font-mono text-[11px] text-[var(--text-secondary)]", className)}>
        {diff}
      </pre>
    );
  }

  return (
    <div className={cn("overflow-hidden border border-[var(--border)] bg-[var(--surface-raised)] font-mono text-[11px]", className)}>
      {patches.map((patch, patchIndex) => (
        <div key={`${patch.oldFileName ?? "old"}-${patch.newFileName ?? "new"}-${patchIndex}`} className="min-w-0">
          <div className="border-b border-[var(--border)] bg-[var(--surface)] px-3 py-2 text-[10px] text-[var(--text-muted)]">
            <span className="truncate">{formatPatchName(patch)}</span>
          </div>
          <div className="max-h-80 overflow-auto">
            {rowsForPatch(patch).map((row) => (
              <div
                key={row.id}
                className={cn(
                  "grid min-w-max grid-cols-[3.5rem_3.5rem_minmax(0,1fr)] border-b border-[rgba(255,255,255,0.03)] last:border-b-0",
                  row.type === "added" && "bg-[rgba(125,220,138,0.09)]",
                  row.type === "removed" && "bg-[rgba(255,95,95,0.09)]",
                  row.type === "hunk" && "bg-[rgba(255,255,255,0.04)] text-[var(--text-muted)]",
                )}
              >
                <span className="select-none px-2 py-0.5 text-right text-[var(--text-muted)]">{row.oldNumber ?? ""}</span>
                <span className="select-none border-l border-[var(--border)] px-2 py-0.5 text-right text-[var(--text-muted)]">{row.newNumber ?? ""}</span>
                <code className="min-w-0 whitespace-pre px-2 py-0.5 text-[var(--text-secondary)]">
                  <span className={cn(row.type === "added" && "text-[#7ddc8a]", row.type === "removed" && "text-[#ff5f5f]")}>{prefixForRow(row.type)}</span>
                  {row.text || " "}
                </code>
              </div>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}

function parseDiff(diff: string): StructuredPatch[] {
  try {
    return parsePatch(diff);
  } catch {
    return [];
  }
}

function rowsForPatch(patch: StructuredPatch): DiffRow[] {
  return patch.hunks.flatMap((hunk, hunkIndex) => {
    let oldNumber = hunk.oldStart;
    let newNumber = hunk.newStart;
    const rows: DiffRow[] = [{
      id: `${hunkIndex}:header`,
      type: "hunk",
      oldNumber: null,
      newNumber: null,
      text: `@@ -${hunk.oldStart},${hunk.oldLines} +${hunk.newStart},${hunk.newLines} @@`,
    }];

    hunk.lines.forEach((line, lineIndex) => {
      const marker = line[0] ?? " ";
      const text = line.slice(1);
      if (marker === "+") {
        rows.push({ id: `${hunkIndex}:${lineIndex}`, type: "added", oldNumber: null, newNumber, text });
        newNumber += 1;
        return;
      }
      if (marker === "-") {
        rows.push({ id: `${hunkIndex}:${lineIndex}`, type: "removed", oldNumber, newNumber: null, text });
        oldNumber += 1;
        return;
      }
      rows.push({ id: `${hunkIndex}:${lineIndex}`, type: "unchanged", oldNumber, newNumber, text });
      oldNumber += 1;
      newNumber += 1;
    });

    return rows;
  });
}

function formatPatchName(patch: StructuredPatch) {
  const name = patch.newFileName || patch.oldFileName || "patch";
  return name.replace(/^[ab]\//, "");
}

function prefixForRow(type: DiffRowType) {
  if (type === "added") return "+";
  if (type === "removed") return "-";
  return " ";
}
