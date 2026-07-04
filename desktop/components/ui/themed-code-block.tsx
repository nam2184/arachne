import DOMPurify from "dompurify";
import { useEffect, useState } from "react";
import { codeToHtml } from "shiki";
import { getCodeBlockThemePair, useAppStore } from "@/features/app/appStore";
import { cn } from "@/lib/utils";

interface ThemedCodeBlockProps {
  code: string;
  lang?: string;
  className?: string;
}

export function ThemedCodeBlock({ code, lang = "text", className }: ThemedCodeBlockProps) {
  const codeBlockTheme = useAppStore((state) => state.settings.code_block_theme);
  const [html, setHtml] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    codeToHtml(code, { lang, themes: getCodeBlockThemePair(codeBlockTheme) })
      .then((nextHtml) => {
        if (!cancelled) setHtml(sanitize(nextHtml));
      })
      .catch(() => {
        if (!cancelled) setHtml(null);
      });
    return () => {
      cancelled = true;
    };
  }, [code, lang, codeBlockTheme]);

  if (html) {
    return (
      <div
        className={cn("themed-code-block", className)}
        dangerouslySetInnerHTML={{ __html: html }}
      />
    );
  }

  return (
    <pre className={cn("themed-code-block-fallback", className)}>
      <code>{code}</code>
    </pre>
  );
}

function sanitize(html: string) {
  return DOMPurify.sanitize(html, {
    USE_PROFILES: { html: true },
    FORBID_TAGS: ["script"],
    FORBID_CONTENTS: ["script"],
  });
}
