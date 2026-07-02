import DOMPurify from "dompurify";
import { marked, type Tokens } from "marked";
import { useMemo } from "react";
import { cn } from "@/lib/utils";
import { streamMarkdownBlocks } from "@/lib/markdownStream";

interface MarkdownContentProps {
  text: string;
  streaming?: boolean;
  className?: string;
}

const renderer = new marked.Renderer();

renderer.link = ({ href, title, text }: Tokens.Link) => {
  const titleAttr = title ? ` title="${escapeAttribute(title)}"` : "";
  return `<a href="${escapeAttribute(href)}"${titleAttr} target="_blank" rel="noopener noreferrer">${text}</a>`;
};

marked.use({
  gfm: true,
  breaks: false,
  renderer,
});

export function MarkdownContent({ text, streaming = false, className }: MarkdownContentProps) {
  const html = useMemo(() => renderMarkdown(text, streaming), [text, streaming]);

  if (!text) return null;

  return (
    <div
      className={cn("markdown-content min-w-0 max-w-full overflow-hidden break-words", className)}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}

function renderMarkdown(text: string, streaming: boolean) {
  const html = streamMarkdownBlocks(text, streaming)
    .map((block) => marked.parse(block.src, { async: false }))
    .join("");

  return DOMPurify.sanitize(html, {
    USE_PROFILES: { html: true },
    FORBID_TAGS: ["style"],
    FORBID_CONTENTS: ["script", "style"],
    ADD_ATTR: ["target"],
  });
}

function escapeAttribute(value: string) {
  return value
    .replace(/&/g, "&amp;")
    .replace(/"/g, "&quot;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}
