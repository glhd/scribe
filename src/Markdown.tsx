import {
  createElement,
  Fragment,
  type ReactNode,
} from "react";
import {
  tokenizeCode,
  tokenizeInline,
  type CodeToken,
  type InlineToken,
  type MarkdownBlock,
  type ParsedMarkdown,
} from "./markdown";
import type { FileReference } from "./types";

export interface HighlightRange {
  id: string;
  start: number;
  end: number;
}

export type HighlightMap = Map<string, HighlightRange[]>;

interface InlineOptions {
  files?: FileReference[];
  highlights?: HighlightRange[];
  activeHighlightId?: string | null;
  onOpenFile?: (path: string, line?: number | null) => void;
  allowUnstampedFiles?: boolean;
}

interface MarkdownDocumentProps {
  document: ParsedMarkdown;
  highlights?: HighlightMap;
  activeHighlightId?: string | null;
  onOpenFile: (path: string, line?: number | null) => void;
}

interface InlineMarkdownProps extends InlineOptions {
  source: string;
}

function unitKey(blockIndex: number, unitIndex: number): string {
  return `${blockIndex}:${unitIndex}`;
}

function markedText(
  text: string,
  absoluteStart: number,
  options: InlineOptions,
  keyPrefix: string,
): ReactNode[] {
  const absoluteEnd = absoluteStart + text.length;
  const ranges = (options.highlights ?? []).filter(
    (range) => range.start < absoluteEnd && range.end > absoluteStart,
  );
  if (ranges.length === 0) return [text];

  const boundaries = new Set([0, text.length]);
  for (const range of ranges) {
    boundaries.add(Math.max(0, range.start - absoluteStart));
    boundaries.add(Math.min(text.length, range.end - absoluteStart));
  }
  const sortedBoundaries = [...boundaries].sort((a, b) => a - b);

  return sortedBoundaries.slice(0, -1).map((start, index) => {
    const end = sortedBoundaries[index + 1];
    const segmentStart = absoluteStart + start;
    const segmentEnd = absoluteStart + end;
    let node: ReactNode = text.slice(start, end);
    const activeRanges = ranges
      .filter((range) => range.start < segmentEnd && range.end > segmentStart)
      .sort((left, right) => left.start - right.start || right.end - left.end);

    for (const range of activeRanges) {
      const startsHere = range.start === segmentStart;
      node = (
        <mark
          className={
            options.activeHighlightId === range.id
              ? "reference-highlight is-active"
              : "reference-highlight"
          }
          id={startsHere ? range.id : undefined}
          key={`${keyPrefix}-mark-${range.id}-${start}`}
        >
          {node}
        </mark>
      );
    }
    return <Fragment key={`${keyPrefix}-segment-${start}`}>{node}</Fragment>;
  });
}

function matchingFile(
  files: FileReference[] | undefined,
  displayText: string,
  parsed?: { path: string; line?: number },
): FileReference | undefined {
  return files?.find((file) => {
    const displays = [file.path];
    if (file.line) {
      displays.push(`${file.path}:${file.line}`);
      if (file.endLine) displays.push(`${file.path}:${file.line}-${file.endLine}`);
    }
    return (
      displays.includes(displayText) ||
      (parsed?.path === file.path &&
        (parsed.line === undefined || parsed.line === file.line))
    );
  });
}

function renderInlineTokens(
  tokens: InlineToken[],
  options: InlineOptions,
  offset: { value: number },
  keyPrefix: string,
): ReactNode[] {
  return tokens.map((token, index) => {
    const key = `${keyPrefix}-${index}`;
    if (token.type === "text") {
      const start = offset.value;
      offset.value += token.text.length;
      return <Fragment key={key}>{markedText(token.text, start, options, key)}</Fragment>;
    }

    if (token.type === "code") {
      const start = offset.value;
      offset.value += token.text.length;
      const content = markedText(token.text, start, options, key);
      const file = matchingFile(options.files, token.text, token.file);
      const fileLocator = token.file ??
        (file ? { path: file.path, line: file.line ?? undefined } : undefined);
      if (fileLocator) {
        const sha = file?.sha || token.file?.shortSha;
        const title = sha ? `Open file · commit ${sha}` : "Open file";
        if ((file || options.allowUnstampedFiles) && options.onOpenFile) {
          const path = file?.path ?? fileLocator.path;
          const line = fileLocator.line ?? file?.line;
          return (
            <button
              className="inline-file-reference"
              key={key}
              onClick={() => options.onOpenFile?.(path, line)}
              title={title}
              type="button"
            >
              <code>{content}</code>
            </button>
          );
        }
        return (
          <code className="inline-code inline-file-literal" key={key} title={sha ? `Commit ${sha}` : undefined}>
            {content}
          </code>
        );
      }
      return <code className="inline-code" key={key}>{content}</code>;
    }

    const children = renderInlineTokens(token.children, options, offset, `${key}-child`);
    if (token.type === "strong") return <strong key={key}>{children}</strong>;
    if (!token.href) {
      return (
        <span className="unsafe-link" key={key} title="This link uses an unsupported address">
          {children}
        </span>
      );
    }
    const external = !token.href.startsWith("#");
    return (
      <a
        href={token.href}
        key={key}
        rel={external ? "noreferrer noopener" : undefined}
        target={external ? "_blank" : undefined}
      >
        {children}
      </a>
    );
  });
}

function fileAppearsInline(tokens: InlineToken[], file: FileReference): boolean {
  return tokens.some((token) => {
    if (token.type === "code") {
      return matchingFile([file], token.text, token.file) !== undefined;
    }
    if (token.type === "strong" || token.type === "link") {
      return fileAppearsInline(token.children, file);
    }
    return false;
  });
}

function fileLabel(file: FileReference): string {
  if (!file.line) return file.path;
  return `${file.path}:${file.line}${file.endLine ? `-${file.endLine}` : ""}`;
}

export function InlineMarkdown({ source, ...options }: InlineMarkdownProps) {
  const tokens = tokenizeInline(source);
  const unmatchedFiles = options.files?.filter(
    (file) => !fileAppearsInline(tokens, file),
  );
  return (
    <>
      {renderInlineTokens(tokens, options, { value: 0 }, "inline")}
      {unmatchedFiles?.map((file) => (
        <button
          className="inline-file-reference inline-file-attachment"
          key={`${file.path}:${file.line ?? ""}:${file.endLine ?? ""}`}
          onClick={() => options.onOpenFile?.(file.path, file.line)}
          title={`Open file · commit ${file.sha}`}
          type="button"
        >
          <code>{fileLabel(file)}</code>
        </button>
      ))}
    </>
  );
}

function renderCodeTokens(
  tokens: CodeToken[],
  options: InlineOptions,
): ReactNode[] {
  let offset = 0;
  return tokens.map((token, index) => {
    const start = offset;
    offset += token.text.length;
    const content = markedText(token.text, start, options, `code-${index}`);
    return token.kind === "plain" ? (
      <Fragment key={`code-${index}`}>{content}</Fragment>
    ) : (
      <span className={`syntax-${token.kind}`} key={`code-${index}`}>
        {content}
      </span>
    );
  });
}

function renderBlock(
  block: MarkdownBlock,
  blockIndex: number,
  highlights: HighlightMap | undefined,
  activeHighlightId: string | null | undefined,
  onOpenFile: (path: string, line?: number | null) => void,
): ReactNode {
  const inlineOptions = (unitIndex: number): InlineOptions => ({
    highlights: highlights?.get(unitKey(blockIndex, unitIndex)),
    activeHighlightId,
    onOpenFile,
    allowUnstampedFiles: true,
  });

  if (block.type === "heading") {
    return createElement(
      `h${block.level}`,
      {
        id: block.anchor,
        key: block.key,
        "data-heading-key": block.headingKey,
      },
      <InlineMarkdown source={block.source} {...inlineOptions(0)} />,
    );
  }
  if (block.type === "paragraph") {
    return (
      <p key={block.key}>
        <InlineMarkdown source={block.source} {...inlineOptions(0)} />
      </p>
    );
  }
  if (block.type === "quote") {
    return (
      <blockquote key={block.key}>
        <InlineMarkdown source={block.source} {...inlineOptions(0)} />
      </blockquote>
    );
  }
  if (block.type === "list") {
    const items = block.items.map((item, index) => (
      <li key={`${block.key}-item-${index}`}>
        <InlineMarkdown source={item} {...inlineOptions(index)} />
      </li>
    ));
    return block.ordered ? (
      <ol key={block.key} start={block.start}>{items}</ol>
    ) : (
      <ul key={block.key}>{items}</ul>
    );
  }
  if (block.type === "code") {
    const options = inlineOptions(0);
    return (
      <div className="code-block" key={block.key}>
        {block.language && <span className="code-language">{block.language}</span>}
        <pre>
          <code className={block.language ? `language-${block.language}` : undefined}>
            {renderCodeTokens(tokenizeCode(block.code, block.language), options)}
          </code>
        </pre>
      </div>
    );
  }
  return <hr key={block.key} />;
}

export function MarkdownDocument({
  document,
  highlights,
  activeHighlightId,
  onOpenFile,
}: MarkdownDocumentProps) {
  return (
    <>
      {document.blocks.map((block, index) =>
        renderBlock(block, index, highlights, activeHighlightId, onOpenFile),
      )}
    </>
  );
}
