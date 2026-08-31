import type { DocumentReference } from "./types";

export type InlineToken =
  | { type: "text"; text: string }
  | { type: "code"; text: string; file?: InlineFileReference }
  | { type: "strong"; children: InlineToken[] }
  | { type: "link"; children: InlineToken[]; href: string | null };

export interface InlineFileReference {
  path: string;
  line?: number;
  endLine?: number;
  shortSha?: string;
}

export interface TextUnit {
  source: string;
  text: string;
}

interface BlockBase {
  key: string;
  units: TextUnit[];
}

export interface HeadingBlock extends BlockBase {
  type: "heading";
  level: number;
  source: string;
  text: string;
  path: string[];
  anchor: string;
  headingKey: string;
}

export interface ParagraphBlock extends BlockBase {
  type: "paragraph";
  source: string;
}

export interface QuoteBlock extends BlockBase {
  type: "quote";
  source: string;
}

export interface ListBlock extends BlockBase {
  type: "list";
  ordered: boolean;
  start: number;
  items: string[];
}

export interface CodeBlock extends BlockBase {
  type: "code";
  language: string;
  code: string;
}

export interface RuleBlock extends BlockBase {
  type: "rule";
}

export type MarkdownBlock =
  | HeadingBlock
  | ParagraphBlock
  | QuoteBlock
  | ListBlock
  | CodeBlock
  | RuleBlock;

export interface ParsedMarkdown {
  blocks: MarkdownBlock[];
}

export interface ResolvedDocumentReference {
  blockIndex: number;
  unitIndex: number;
  start: number;
  end: number;
}

const fencePattern = /^ {0,3}(`{3,}|~{3,})\s*([\w.+-]*)\s*$/;
const atxHeadingPattern = /^ {0,3}(#{1,6})[\t ]+(.+?)[\t ]*$/;
const listPattern = /^ {0,3}([-+*]|\d+[.)])[\t ]+(.+)$/;
const quotePattern = /^ {0,3}>[\t ]?(.*)$/;
const rulePattern = /^ {0,3}(?:(?:\*[\t ]*){3,}|(?:-[\t ]*){3,}|(?:_[\t ]*){3,})$/;

function cleanHeadingSource(source: string): string {
  return source.replace(/[\t ]+#+[\t ]*$/, "").trim();
}

function slugify(value: string): string {
  const slug = value
    .toLocaleLowerCase()
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "")
    .replace(/[^\p{L}\p{N}]+/gu, "-")
    .replace(/^-+|-+$/g, "");
  return slug || "section";
}

function isBlockStart(lines: string[], index: number): boolean {
  const line = lines[index] ?? "";
  return (
    fencePattern.test(line) ||
    atxHeadingPattern.test(line) ||
    quotePattern.test(line) ||
    listPattern.test(line) ||
    rulePattern.test(line)
  );
}

function makeInlineUnit(source: string): TextUnit {
  return { source, text: inlineText(tokenizeInline(source)) };
}

/** Removes HTML comments from rendered Markdown while preserving fenced examples. */
export function stripHtmlComments(markdown: string): string {
  const lines = markdown.replace(/\r\n?/g, "\n").split("\n");
  let fence: { character: string; length: number } | null = null;
  let insideComment = false;

  return lines
    .map((rawLine) => {
      if (fence) {
        const closing = rawLine.match(/^ {0,3}(`{3,}|~{3,})\s*$/);
        if (
          closing &&
          closing[1][0] === fence.character &&
          closing[1].length >= fence.length
        ) {
          fence = null;
        }
        return rawLine;
      }

      let line = "";
      let cursor = 0;
      while (cursor < rawLine.length) {
        if (insideComment) {
          const commentEnd = rawLine.indexOf("-->", cursor);
          if (commentEnd === -1) return line;
          insideComment = false;
          cursor = commentEnd + 3;
          continue;
        }
        const commentStart = rawLine.indexOf("<!--", cursor);
        if (commentStart === -1) {
          line += rawLine.slice(cursor);
          break;
        }
        line += rawLine.slice(cursor, commentStart);
        insideComment = true;
        cursor = commentStart + 4;
      }

      const opening = line.match(fencePattern);
      if (opening) {
        fence = { character: opening[1][0], length: opening[1].length };
      }
      return line;
    })
    .join("\n");
}

/**
 * Deliberately narrow block parser for the renderer's supported Markdown subset.
 * It does not produce HTML, making all later rendering safe by construction.
 */
export function parseMarkdown(markdown: string): ParsedMarkdown {
  const lines = stripHtmlComments(markdown).split("\n");
  const blocks: MarkdownBlock[] = [];
  const headingStack: Array<{ level: number; text: string }> = [];
  const slugCounts = new Map<string, number>();
  const headingKeyCounts = new Map<string, number>();
  let index = 0;

  const addHeading = (level: number, source: string) => {
    const unit = makeInlineUnit(cleanHeadingSource(source));
    while (
      headingStack.length > 0 &&
      headingStack[headingStack.length - 1].level >= level
    ) {
      headingStack.pop();
    }
    headingStack.push({ level, text: unit.text });
    const path = headingStack.map((heading) => heading.text);
    const baseSlug = slugify(unit.text);
    const slugCount = slugCounts.get(baseSlug) ?? 0;
    slugCounts.set(baseSlug, slugCount + 1);
    const pathKey = JSON.stringify(path);
    const headingKeyCount = headingKeyCounts.get(pathKey) ?? 0;
    headingKeyCounts.set(pathKey, headingKeyCount + 1);
    blocks.push({
      type: "heading",
      key: `block-${blocks.length}`,
      level,
      source: unit.source,
      text: unit.text,
      path,
      anchor: slugCount === 0 ? baseSlug : `${baseSlug}-${slugCount + 1}`,
      headingKey: `${pathKey}:${headingKeyCount}`,
      units: [unit],
    });
  };

  while (index < lines.length) {
    const line = lines[index];
    if (line.trim() === "") {
      index += 1;
      continue;
    }

    const fence = line.match(fencePattern);
    if (fence) {
      const marker = fence[1];
      const markerCharacter = marker[0];
      const language = fence[2].toLocaleLowerCase();
      const codeLines: string[] = [];
      index += 1;
      while (index < lines.length) {
        const closing = lines[index].match(/^ {0,3}(`{3,}|~{3,})\s*$/);
        if (
          closing &&
          closing[1][0] === markerCharacter &&
          closing[1].length >= marker.length
        ) {
          index += 1;
          break;
        }
        codeLines.push(lines[index]);
        index += 1;
      }
      const code = codeLines.join("\n");
      blocks.push({
        type: "code",
        key: `block-${blocks.length}`,
        language,
        code,
        units: [{ source: code, text: code }],
      });
      continue;
    }

    const atxHeading = line.match(atxHeadingPattern);
    if (atxHeading) {
      addHeading(atxHeading[1].length, atxHeading[2]);
      index += 1;
      continue;
    }

    if (index + 1 < lines.length) {
      const setext = lines[index + 1].match(/^ {0,3}(=+|-+)[\t ]*$/);
      if (setext && line.trim()) {
        addHeading(setext[1][0] === "=" ? 1 : 2, line.trim());
        index += 2;
        continue;
      }
    }

    const quote = line.match(quotePattern);
    if (quote) {
      const quoteLines: string[] = [];
      while (index < lines.length) {
        const match = lines[index].match(quotePattern);
        if (!match) break;
        quoteLines.push(match[1].trim());
        index += 1;
      }
      const source = quoteLines.join(" ").trim();
      blocks.push({
        type: "quote",
        key: `block-${blocks.length}`,
        source,
        units: [makeInlineUnit(source)],
      });
      continue;
    }

    const list = line.match(listPattern);
    if (list) {
      const ordered = /^\d/.test(list[1]);
      const start = ordered ? Number.parseInt(list[1], 10) : 1;
      const items: string[] = [];
      while (index < lines.length) {
        const match = lines[index].match(listPattern);
        if (!match || /^\d/.test(match[1]) !== ordered) break;
        items.push(match[2].trim());
        index += 1;
      }
      blocks.push({
        type: "list",
        key: `block-${blocks.length}`,
        ordered,
        start,
        items,
        units: items.map(makeInlineUnit),
      });
      continue;
    }

    if (rulePattern.test(line)) {
      blocks.push({ type: "rule", key: `block-${blocks.length}`, units: [] });
      index += 1;
      continue;
    }

    const paragraphLines: string[] = [];
    while (
      index < lines.length &&
      lines[index].trim() !== "" &&
      (paragraphLines.length === 0 || !isBlockStart(lines, index))
    ) {
      if (
        paragraphLines.length > 0 &&
        index + 1 < lines.length &&
        /^ {0,3}(=+|-+)[\t ]*$/.test(lines[index + 1])
      ) {
        break;
      }
      paragraphLines.push(lines[index].trim());
      index += 1;
    }
    const source = paragraphLines.join(" ");
    blocks.push({
      type: "paragraph",
      key: `block-${blocks.length}`,
      source,
      units: [makeInlineUnit(source)],
    });
  }

  return { blocks };
}

function appendText(tokens: InlineToken[], text: string): void {
  if (!text) return;
  const previous = tokens[tokens.length - 1];
  if (previous?.type === "text") {
    previous.text += text;
  } else {
    tokens.push({ type: "text", text });
  }
}

function parseInlineFileReference(
  value: string,
  shortSha?: string,
): InlineFileReference | undefined {
  const match = value.match(/^(.+?)(?::(\d+)(?:-(\d+))?)?$/);
  if (!match) return undefined;
  const path = match[1].replace(/\\/g, "/");
  if (
    !path ||
    /\s/.test(path) ||
    path.includes("://") ||
    path.includes(":") ||
    path.startsWith("/") ||
    path === "." ||
    path.split("/").includes("..") ||
    (!shortSha &&
      !path.includes("/") &&
      !/^[^.][^/]*\.[A-Za-z0-9][A-Za-z0-9._-]*$/.test(path))
  ) {
    return undefined;
  }
  const line = match[2] ? Number.parseInt(match[2], 10) : undefined;
  const endLine = match[3] ? Number.parseInt(match[3], 10) : undefined;
  if (line !== undefined && (!Number.isSafeInteger(line) || line < 1)) return undefined;
  if (
    endLine !== undefined &&
    (!Number.isSafeInteger(endLine) || endLine < (line ?? 1))
  ) {
    return undefined;
  }
  return { path, line, endLine, shortSha };
}

function findClosingBracket(source: string, start: number, character: string): number {
  for (let index = start; index < source.length; index += 1) {
    if (source[index] === "\\") {
      index += 1;
      continue;
    }
    if (source[index] === character) return index;
  }
  return -1;
}

/** Tokenizes only bold, code spans, Markdown links, and safe autolinks. */
export function tokenizeInline(source: string): InlineToken[] {
  const tokens: InlineToken[] = [];
  let index = 0;

  while (index < source.length) {
    if (source.startsWith("<!--", index)) {
      const commentEnd = source.indexOf("-->", index + 4);
      index = commentEnd === -1 ? source.length : commentEnd + 3;
      continue;
    }

    if (source[index] === "\\" && index + 1 < source.length) {
      appendText(tokens, source[index + 1]);
      index += 2;
      continue;
    }

    if (source[index] === "`") {
      const closing = source.indexOf("`", index + 1);
      if (closing !== -1) {
        const text = source.slice(index + 1, closing);
        const shaMatch = source.slice(closing + 1).match(/^ @([0-9a-fA-F]{4,64})\b/);
        const file = parseInlineFileReference(text, shaMatch?.[1]);
        tokens.push({ type: "code", text, file });
        index = closing + 1 + (file && shaMatch ? shaMatch[0].length : 0);
        continue;
      }
    }

    const strongMarker = source.slice(index, index + 2);
    if (strongMarker === "**" || strongMarker === "__") {
      const closing = source.indexOf(strongMarker, index + 2);
      if (closing > index + 2) {
        tokens.push({
          type: "strong",
          children: tokenizeInline(source.slice(index + 2, closing)),
        });
        index = closing + 2;
        continue;
      }
    }

    if (source[index] === "[") {
      const labelEnd = findClosingBracket(source, index + 1, "]");
      if (labelEnd !== -1 && source[labelEnd + 1] === "(") {
        const hrefEnd = findClosingBracket(source, labelEnd + 2, ")");
        if (hrefEnd !== -1) {
          const href = source.slice(labelEnd + 2, hrefEnd).trim();
          tokens.push({
            type: "link",
            children: tokenizeInline(source.slice(index + 1, labelEnd)),
            href: safeLinkHref(href),
          });
          index = hrefEnd + 1;
          continue;
        }
      }
    }

    if (source[index] === "<") {
      const closing = source.indexOf(">", index + 1);
      if (closing !== -1) {
        const href = source.slice(index + 1, closing);
        const safeHref = safeLinkHref(href);
        if (safeHref) {
          tokens.push({
            type: "link",
            children: [{ type: "text", text: href }],
            href: safeHref,
          });
          index = closing + 1;
          continue;
        }
      }
    }

    appendText(tokens, source[index]);
    index += 1;
  }

  return tokens;
}

export function inlineText(tokens: InlineToken[]): string {
  return tokens
    .map((token) => {
      if (token.type === "text" || token.type === "code") return token.text;
      return inlineText(token.children);
    })
    .join("");
}

/** Only web/mail links and in-document anchors are navigable. */
export function safeLinkHref(href: string): string | null {
  const value = href.trim();
  if (value.startsWith("#")) return /^#[\w:.-]+$/.test(value) ? value : null;
  try {
    const url = new URL(value);
    return ["http:", "https:", "mailto:"].includes(url.protocol) ? value : null;
  } catch {
    return null;
  }
}

/**
 * Resolves an exact heading hierarchy, then the first exact rendered snippet
 * before the next heading at the same or a higher level.
 */
export function resolveDocumentReference(
  document: ParsedMarkdown,
  locator: DocumentReference,
): ResolvedDocumentReference | null {
  if (locator.heading.length === 0 || locator.snippet.length === 0) return null;
  const headingIndex = document.blocks.findIndex(
    (block) =>
      block.type === "heading" &&
      block.path.length === locator.heading.length &&
      block.path.every((part, index) => part === locator.heading[index]),
  );
  if (headingIndex === -1) return null;

  const heading = document.blocks[headingIndex] as HeadingBlock;
  for (let blockIndex = headingIndex + 1; blockIndex < document.blocks.length; blockIndex += 1) {
    const block = document.blocks[blockIndex];
    if (block.type === "heading" && block.level <= heading.level) break;
    for (let unitIndex = 0; unitIndex < block.units.length; unitIndex += 1) {
      const start = block.units[unitIndex].text.indexOf(locator.snippet);
      if (start !== -1) {
        return {
          blockIndex,
          unitIndex,
          start,
          end: start + locator.snippet.length,
        };
      }
    }
  }
  return null;
}

export type CodeTokenKind =
  | "plain"
  | "comment"
  | "string"
  | "number"
  | "keyword";

export interface CodeToken {
  kind: CodeTokenKind;
  text: string;
}

const keywords = new Set([
  "as", "async", "await", "break", "case", "catch", "class", "const",
  "continue", "def", "default", "do", "else", "enum", "export", "extends",
  "false", "fn", "for", "from", "function", "if", "impl", "import", "in",
  "interface", "let", "match", "mod", "new", "null", "of", "pub", "return",
  "self", "static", "struct", "super", "switch", "this", "throw", "trait",
  "true", "try", "type", "undefined", "use", "var", "while", "with", "yield",
]);

function pushCodeToken(tokens: CodeToken[], kind: CodeTokenKind, text: string): void {
  if (!text) return;
  const previous = tokens[tokens.length - 1];
  if (previous?.kind === kind) previous.text += text;
  else tokens.push({ kind, text });
}

/** Lightweight, lossless code tokenizer used only for CSS coloring. */
export function tokenizeCode(code: string, language = ""): CodeToken[] {
  const tokens: CodeToken[] = [];
  const hashComments = /^(?:py|python|sh|bash|shell|yaml|yml|toml|rb|ruby)$/.test(language);
  let index = 0;
  while (index < code.length) {
    if (code.startsWith("//", index) || (hashComments && code[index] === "#")) {
      const end = code.indexOf("\n", index);
      const stop = end === -1 ? code.length : end;
      pushCodeToken(tokens, "comment", code.slice(index, stop));
      index = stop;
      continue;
    }
    if (code.startsWith("/*", index)) {
      const end = code.indexOf("*/", index + 2);
      const stop = end === -1 ? code.length : end + 2;
      pushCodeToken(tokens, "comment", code.slice(index, stop));
      index = stop;
      continue;
    }
    if (code[index] === '"' || code[index] === "'" || code[index] === "`") {
      const quote = code[index];
      let end = index + 1;
      while (end < code.length) {
        if (code[end] === "\\") end += 2;
        else if (code[end++] === quote) break;
        else if (quote !== "`" && code[end - 1] === "\n") break;
      }
      pushCodeToken(tokens, "string", code.slice(index, end));
      index = end;
      continue;
    }
    const number = code.slice(index).match(/^\b(?:0x[\da-f]+|\d+(?:\.\d+)?)\b/i);
    if (number) {
      pushCodeToken(tokens, "number", number[0]);
      index += number[0].length;
      continue;
    }
    const word = code.slice(index).match(/^[A-Za-z_$][\w$]*/);
    if (word) {
      pushCodeToken(tokens, keywords.has(word[0]) ? "keyword" : "plain", word[0]);
      index += word[0].length;
      continue;
    }
    pushCodeToken(tokens, "plain", code[index]);
    index += 1;
  }
  return tokens;
}
