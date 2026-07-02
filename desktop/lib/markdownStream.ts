import { marked, type Tokens } from "marked";
import remend from "remend";

export interface MarkdownBlock {
  raw: string;
  src: string;
}

function hasReferenceDefinitions(text: string) {
  return /^\[[^\]]+\]:\s+\S+/m.test(text) || /^\[\^[^\]]+\]:\s+/m.test(text);
}

function isOpenFence(raw: string) {
  const match = raw.match(/^[ \t]{0,3}(`{3,}|~{3,})/);
  if (!match) return false;

  const mark = match[1];
  if (!mark) return false;

  const lines = raw.trimEnd().split("\n");
  const lastLine = lines[lines.length - 1]?.trim() ?? "";
  return !new RegExp(`^[\\t ]{0,3}${mark[0]}{${mark.length},}[\\t ]*$`).test(lastLine);
}

function heal(text: string) {
  return remend(text, { linkMode: "text-only" });
}

export function streamMarkdownBlocks(text: string, streaming: boolean): MarkdownBlock[] {
  if (!streaming) return [{ raw: text, src: text }];

  const healed = heal(text);
  if (hasReferenceDefinitions(text)) return [{ raw: text, src: healed }];

  const tokens = marked.lexer(text);
  let tail = -1;
  for (let index = tokens.length - 1; index >= 0; index -= 1) {
    if (tokens[index]?.type !== "space") {
      tail = index;
      break;
    }
  }
  if (tail < 0) return [{ raw: text, src: healed }];

  const last = tokens[tail];
  if (!last || last.type !== "code") return [{ raw: text, src: healed }];

  const code = last as Tokens.Code;
  if (!isOpenFence(code.raw)) return [{ raw: text, src: healed }];

  const head = tokens
    .slice(0, tail)
    .map((token) => token.raw)
    .join("");

  if (!head) return [{ raw: code.raw, src: code.raw }];

  return [
    { raw: head, src: heal(head) },
    { raw: code.raw, src: code.raw },
  ];
}
