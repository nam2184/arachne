import DOMPurify from "dompurify";
import { Marked, marked, type Tokens } from "marked";
import markedShiki from "marked-shiki";
import { useEffect, useRef, useState } from "react";
import { codeToHtml } from "shiki";
import { getCodeBlockThemePair, useAppStore, type CodeBlockTheme } from "@/features/app/appStore";
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

export function MarkdownContent({ text, streaming = false, className }: MarkdownContentProps) {
  const codeBlockTheme = useAppStore((state) => state.settings.code_block_theme);
  const rootRef = useRef<HTMLDivElement>(null);
  const [html, setHtml] = useState(() => renderFallbackMarkdown(text, streaming));

  useEffect(() => {
    let cancelled = false;
    setHtml(renderFallbackMarkdown(text, streaming));
    renderMarkdown(text, streaming, codeBlockTheme)
      .then((nextHtml) => {
        if (!cancelled) setHtml(nextHtml);
      })
      .catch(() => {
        if (!cancelled) setHtml(renderFallbackMarkdown(text, streaming));
      });
    return () => {
      cancelled = true;
    };
  }, [text, streaming, codeBlockTheme]);

  useEffect(() => decorateCodeBlocks(rootRef.current), [html]);

  useEffect(() => setupCodeCopy(rootRef.current), []);

  if (!text) return null;

  return (
    <div
      ref={rootRef}
      className={cn("markdown-content min-w-0 max-w-full overflow-hidden break-words", className)}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}

async function renderMarkdown(text: string, streaming: boolean, codeBlockTheme: CodeBlockTheme) {
  const markedWithShiki = createMarkedWithShiki(codeBlockTheme);
  const html = (
    await Promise.all(
      streamMarkdownBlocks(text, streaming).map((block) => markedWithShiki.parse(block.src, { async: true })),
    )
  ).join("");

  return sanitize(html);
}

function createMarkedWithShiki(codeBlockTheme: CodeBlockTheme) {
  const themes = getCodeBlockThemePair(codeBlockTheme);
  return new Marked(
    {
      gfm: true,
      breaks: false,
      renderer,
    },
    markedShiki({
      container: '<div data-component="markdown-code">%s</div>',
      highlight(code, lang) {
        return codeToHtml(code, {
          lang: lang || "text",
          themes,
        });
      },
    }),
  );
}

function renderFallbackMarkdown(text: string, streaming: boolean) {
  return sanitize(
    streamMarkdownBlocks(text, streaming)
      .map((block) => marked.parse(block.src, { async: false, gfm: true, breaks: false, renderer }))
      .join(""),
  );
}

function sanitize(html: string) {
  return DOMPurify.sanitize(html, {
    USE_PROFILES: { html: true },
    FORBID_TAGS: ["style"],
    FORBID_CONTENTS: ["script", "style"],
    ADD_ATTR: ["target"],
  });
}

function decorateCodeBlocks(root: HTMLDivElement | null) {
  if (!root) return;
  for (const block of root.querySelectorAll('[data-component="markdown-code"]')) {
    if (block.querySelector('[data-slot="markdown-copy-button"]')) continue;
    const button = document.createElement("button");
    button.type = "button";
    button.setAttribute("data-slot", "markdown-copy-button");
    button.setAttribute("aria-label", "Copy code");
    button.textContent = "copy";
    block.appendChild(button);
  }
}

function setupCodeCopy(root: HTMLDivElement | null) {
  if (!root) return undefined;

  const handleClick = async (event: MouseEvent) => {
    const target = event.target;
    if (!(target instanceof Element)) return;
    const button = target.closest('[data-slot="markdown-copy-button"]');
    if (!(button instanceof HTMLButtonElement)) return;
    const content = button.closest('[data-component="markdown-code"]')?.querySelector("code")?.textContent;
    if (!content) return;
    await navigator.clipboard?.writeText(content);
    button.textContent = "copied";
    window.setTimeout(() => {
      button.textContent = "copy";
    }, 1500);
  };

  root.addEventListener("click", handleClick);
  return () => root.removeEventListener("click", handleClick);
}

function escapeAttribute(value: string) {
  return value
    .replace(/&/g, "&amp;")
    .replace(/"/g, "&quot;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}
