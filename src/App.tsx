import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  InlineMarkdown,
  MarkdownDocument,
  type HighlightMap,
} from "./Markdown";
import {
  parseMarkdown,
  resolveDocumentReference,
  type ParsedMarkdown,
  type ResolvedDocumentReference,
} from "./markdown";
import type {
  ChatMessage,
  DecisionStatus,
  DocumentReference,
  ScribeState,
} from "./types";
import "./App.css";

interface MessageReferenceState {
  locator: DocumentReference;
  resolved: boolean;
  anchorId?: string;
}

interface ReferenceIndex {
  byMessageId: Map<string, MessageReferenceState>;
  highlights: HighlightMap;
}

const staleReferencesReported = new Set<string>();
const timeFormatter = new Intl.DateTimeFormat(undefined, {
  hour: "numeric",
  minute: "2-digit",
});

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function referenceKey(messageId: string, locator: DocumentReference): string {
  return JSON.stringify([messageId, locator.heading, locator.snippet]);
}

function positionKey(position: ResolvedDocumentReference): string {
  return `${position.blockIndex}:${position.unitIndex}:${position.start}:${position.end}`;
}

function shortHash(value: string): string {
  let hash = 2166136261;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 16777619);
  }
  return (hash >>> 0).toString(36);
}

/** Builds one canonical DOM highlight for every exact resolved text range. */
export function buildReferenceIndex(
  document: ParsedMarkdown,
  messages: ChatMessage[],
): ReferenceIndex {
  const byMessageId = new Map<string, MessageReferenceState>();
  const highlights: HighlightMap = new Map();
  const anchorsByPosition = new Map<string, string>();
  const positionsByAnchor = new Map<string, string>();

  for (const message of messages) {
    if (!message.reference) continue;
    const locator = message.reference;
    const position = resolveDocumentReference(document, locator);
    if (!position) {
      byMessageId.set(message.id, { locator, resolved: false });
      continue;
    }

    const positionId = positionKey(position);
    let anchorId = anchorsByPosition.get(positionId);
    if (!anchorId) {
      const base = `note-reference-${shortHash(positionId)}`;
      anchorId = base;
      let suffix = 2;
      while (
        positionsByAnchor.has(anchorId) &&
        positionsByAnchor.get(anchorId) !== positionId
      ) {
        anchorId = `${base}-${suffix++}`;
      }
      anchorsByPosition.set(positionId, anchorId);
      positionsByAnchor.set(anchorId, positionId);
      const key = `${position.blockIndex}:${position.unitIndex}`;
      const ranges = highlights.get(key) ?? [];
      ranges.push({ id: anchorId, start: position.start, end: position.end });
      highlights.set(key, ranges);
    }
    byMessageId.set(message.id, { locator, resolved: true, anchorId });
  }

  return { byMessageId, highlights };
}

function sortedMessages(messages: ChatMessage[]): ChatMessage[] {
  return messages
    .map((message, index) => ({ message, index, time: Date.parse(message.timestamp) }))
    .sort((left, right) => {
      if (Number.isNaN(left.time) || Number.isNaN(right.time)) {
        return left.index - right.index;
      }
      return left.time - right.time || left.index - right.index;
    })
    .map(({ message }) => message);
}

function formatTime(timestamp: string): { short: string; full: string } {
  const date = new Date(timestamp);
  if (Number.isNaN(date.getTime())) return { short: timestamp, full: timestamp };
  return { short: timeFormatter.format(date), full: date.toLocaleString() };
}

function relativeNotesPath(notesPath: string, repoPath: string): string {
  const normalizedRepo = repoPath.replace(/[\\/]$/, "");
  if (
    normalizedRepo &&
    notesPath.startsWith(normalizedRepo) &&
    /[\\/]/.test(notesPath.slice(normalizedRepo.length, normalizedRepo.length + 1))
  ) {
    return notesPath.slice(normalizedRepo.length + 1);
  }
  return notesPath || "Notes";
}

type SavedMarkdownScroll =
  | { headingKey: string; offset: number; ratio: number }
  | { headingKey: null; ratio: number };

function captureMarkdownScroll(scroller: HTMLDivElement): SavedMarkdownScroll {
  const range = Math.max(0, scroller.scrollHeight - scroller.clientHeight);
  const ratio = range === 0 ? 0 : scroller.scrollTop / range;
  const viewportTop = scroller.getBoundingClientRect().top;
  let anchor: HTMLElement | undefined;
  for (const heading of scroller.querySelectorAll<HTMLElement>("[data-heading-key]")) {
    if (heading.getBoundingClientRect().top <= viewportTop + 88) anchor = heading;
    else break;
  }
  return anchor?.dataset.headingKey
    ? {
        headingKey: anchor.dataset.headingKey,
        offset: anchor.getBoundingClientRect().top - viewportTop,
        ratio,
      }
    : { headingKey: null, ratio };
}

function restoreMarkdownScroll(
  scroller: HTMLDivElement,
  saved: SavedMarkdownScroll,
): void {
  const matchingHeading = saved.headingKey
    ? [...scroller.querySelectorAll<HTMLElement>("[data-heading-key]")].find(
        (heading) => heading.dataset.headingKey === saved.headingKey,
      )
    : undefined;
  if (matchingHeading && "offset" in saved) {
    const currentOffset =
      matchingHeading.getBoundingClientRect().top - scroller.getBoundingClientRect().top;
    scroller.scrollTop += currentOffset - saved.offset;
    return;
  }
  const range = Math.max(0, scroller.scrollHeight - scroller.clientHeight);
  scroller.scrollTop = range * saved.ratio;
}

function ScribeMark() {
  return (
    <svg aria-hidden="true" className="scribe-mark" viewBox="0 0 32 32">
      <path d="M8 6.5h13.5a4 4 0 0 1 4 4V24H12a4 4 0 0 1-4-4V6.5Z" />
      <path d="M12 11h9M12 15h9M12 19h5.5" />
      <path d="m5.5 23 3.2-1.2L6.6 19l-1.1 4Z" />
    </svg>
  );
}

function ArrowIcon() {
  return (
    <svg aria-hidden="true" viewBox="0 0 16 16">
      <path d="M3 13 13 3m0 0H6m7 0v7" />
    </svg>
  );
}

function CheckIcon() {
  return (
    <svg aria-hidden="true" viewBox="0 0 16 16">
      <path d="m3 8.5 3.1 3L13 4.8" />
    </svg>
  );
}

function MessageReference({
  reference,
  onOpen,
}: {
  reference: MessageReferenceState;
  onOpen: (anchorId: string) => void;
}) {
  const heading =
    reference.locator.heading[reference.locator.heading.length - 1] ||
    "Document reference";
  const label = `${reference.locator.heading.join(" › ")}: ${reference.locator.snippet}`;
  if (!reference.resolved || !reference.anchorId) {
    return (
      <span
        aria-label={`Stale note reference: ${label}`}
        className="document-reference is-stale"
        role="status"
        title="This passage no longer exists at the exact location"
      >
        <span className="reference-copy">
          <span className="reference-heading">{heading}</span>
          <span className="reference-snippet">{reference.locator.snippet}</span>
        </span>
        <span aria-hidden="true" className="stale-label">stale</span>
      </span>
    );
  }
  return (
    <button
      aria-label={`Show referenced note: ${label}`}
      className="document-reference"
      onClick={() => onOpen(reference.anchorId as string)}
      type="button"
    >
      <span className="reference-copy">
        <span className="reference-heading">{heading}</span>
        <span className="reference-snippet">{reference.locator.snippet}</span>
      </span>
      <ArrowIcon />
    </button>
  );
}

function Timestamp({ timestamp }: { timestamp: string }) {
  const formatted = formatTime(timestamp);
  return <time dateTime={timestamp} title={formatted.full}>{formatted.short}</time>;
}

interface MessageCardProps {
  message: ChatMessage;
  reference?: MessageReferenceState;
  pendingDecision?: DecisionStatus;
  onDecision: (id: string, status: "approved" | "rejected") => void;
  onOpenFile: (path: string, line?: number | null) => void;
  onOpenReference: (anchorId: string) => void;
}

function MessageCard({
  message,
  reference,
  pendingDecision,
  onDecision,
  onOpenFile,
  onOpenReference,
}: MessageCardProps) {
  if (message.kind === "ack") {
    return (
      <div className={`acknowledgement ${message.read ? "" : "is-unread"}`}>
        <span aria-hidden="true" className="ack-icon">i</span>
        <span><InlineMarkdown source={message.text} files={message.files} onOpenFile={onOpenFile} /></span>
      </div>
    );
  }

  const status = message.decisionStatus ?? "unreviewed";
  if (message.kind === "decision") {
    const reviewed = status !== "unreviewed";
    return (
      <article className={`decision-card ${message.read ? "" : "is-unread"}`}>
        <header className="decision-heading">
          <div>
            <span aria-hidden="true" className="decision-glyph">◆</span>
            <span>Decision requested</span>
          </div>
          <Timestamp timestamp={message.timestamp} />
        </header>
        <div className="message-copy">
          <InlineMarkdown source={message.text} files={message.files} onOpenFile={onOpenFile} />
        </div>
        {reference && <MessageReference reference={reference} onOpen={onOpenReference} />}
        <footer className="decision-footer">
          {reviewed ? (
            <span className={`decision-result is-${status}`}>
              {status === "approved" ? <CheckIcon /> : <span aria-hidden="true">×</span>}
              {status === "approved" ? "Approved" : "Rejected"}
            </span>
          ) : (
            <>
              <button
                aria-label="Approve this decision"
                className="decision-button approve"
                disabled={Boolean(pendingDecision)}
                onClick={() => onDecision(message.id, "approved")}
                type="button"
              >
                <CheckIcon />
                {pendingDecision === "approved" ? "Approving…" : "Approve"}
              </button>
              <button
                aria-label="Reject this decision"
                className="decision-button reject"
                disabled={Boolean(pendingDecision)}
                onClick={() => onDecision(message.id, "rejected")}
                type="button"
              >
                <span aria-hidden="true">×</span>
                {pendingDecision === "rejected" ? "Rejecting…" : "Reject"}
              </button>
            </>
          )}
        </footer>
      </article>
    );
  }

  return (
    <article className={`message-bubble ${message.read ? "" : "is-unread"}`}>
      <div className="message-copy">
        <InlineMarkdown source={message.text} files={message.files} onOpenFile={onOpenFile} />
      </div>
      {reference && <MessageReference reference={reference} onOpen={onOpenReference} />}
      <footer className="message-meta">
        {!message.read && <span className="unread-label">New</span>}
        <Timestamp timestamp={message.timestamp} />
      </footer>
    </article>
  );
}

function LoadingShell() {
  return (
    <div aria-busy="true" aria-label="Loading Scribe" className="loading-state">
      <span className="loading-mark"><ScribeMark /></span>
      <strong>Opening Scribe</strong>
      <span>Loading notes and messages…</span>
    </div>
  );
}

function App() {
  const [state, setState] = useState<ScribeState | null>(null);
  const [loading, setLoading] = useState(true);
  const [connectionError, setConnectionError] = useState<string | null>(null);
  const [liveWarning, setLiveWarning] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [retryNonce, setRetryNonce] = useState(0);
  const [markingRead, setMarkingRead] = useState(false);
  const [pendingDecisions, setPendingDecisions] = useState<Record<string, DecisionStatus>>({});
  const [activeReferenceId, setActiveReferenceId] = useState<string | null>(null);
  const feedRef = useRef<HTMLDivElement>(null);
  const notesRef = useRef<HTMLDivElement>(null);
  const feedIsNearBottom = useRef(true);
  const savedNotesScroll = useRef<SavedMarkdownScroll | null>(null);

  const acceptBackendState = useCallback((nextState: ScribeState) => {
    const notesScroller = notesRef.current;
    if (notesScroller) savedNotesScroll.current = captureMarkdownScroll(notesScroller);
    setState(nextState);
  }, []);

  useEffect(() => {
    document.title = "Scribe";
  }, []);

  useEffect(() => {
    let active = true;
    let unlisten: UnlistenFn | undefined;
    let eventVersion = 0;

    const connect = async () => {
      setLoading(true);
      setConnectionError(null);
      setLiveWarning(null);
      try {
        unlisten = await listen<ScribeState>("state_changed", (event) => {
          if (!active) return;
          eventVersion += 1;
          acceptBackendState(event.payload);
          setLoading(false);
          setConnectionError(null);
        });
        if (!active) {
          unlisten();
          return;
        }
      } catch (error) {
        if (active) setLiveWarning(`Live updates unavailable: ${errorMessage(error)}`);
      }

      const versionAtRequest = eventVersion;
      try {
        const initialState = await invoke<ScribeState>("get_state");
        if (active && eventVersion === versionAtRequest) acceptBackendState(initialState);
      } catch (error) {
        if (active) setConnectionError(errorMessage(error));
      } finally {
        if (active) setLoading(false);
      }
    };

    void connect();
    return () => {
      active = false;
      unlisten?.();
    };
  }, [acceptBackendState, retryNonce]);

  const messages = useMemo(() => sortedMessages(state?.messages ?? []), [state?.messages]);
  const parsedDocument = useMemo(
    () => parseMarkdown(state?.markdown ?? ""),
    [state?.markdown],
  );
  const references = useMemo(
    () => buildReferenceIndex(parsedDocument, messages),
    [parsedDocument, messages],
  );

  useEffect(() => {
    for (const message of messages) {
      const reference = references.byMessageId.get(message.id);
      if (!reference || reference.resolved) continue;
      const key = referenceKey(message.id, reference.locator);
      if (staleReferencesReported.has(key)) continue;
      staleReferencesReported.add(key);
      void invoke("report_stale_reference", {
        messageId: message.id,
        locator: reference.locator,
      }).catch(() => {
        // A later document or chat update will retry a transient IPC failure.
        staleReferencesReported.delete(key);
      });
    }
  }, [messages, references]);

  useLayoutEffect(() => {
    const notesScroller = notesRef.current;
    const saved = savedNotesScroll.current;
    if (!notesScroller || !saved) return;
    restoreMarkdownScroll(notesScroller, saved);
    savedNotesScroll.current = null;
  }, [state?.markdown]);

  useLayoutEffect(() => {
    if (!feedIsNearBottom.current) return;
    const frame = requestAnimationFrame(() => {
      const feed = feedRef.current;
      if (feed) feed.scrollTop = feed.scrollHeight;
    });
    return () => cancelAnimationFrame(frame);
  }, [messages]);

  const handleFeedScroll = useCallback(() => {
    const feed = feedRef.current;
    if (!feed) return;
    feedIsNearBottom.current =
      feed.scrollHeight - feed.scrollTop - feed.clientHeight < 96;
  }, []);

  const openFile = useCallback(async (path: string, line?: number | null) => {
    setActionError(null);
    try {
      await invoke("open_file_reference", { path, line: line ?? null });
    } catch (error) {
      setActionError(`Could not open ${path}: ${errorMessage(error)}`);
    }
  }, []);

  const openReference = useCallback((anchorId: string) => {
    setActiveReferenceId(anchorId);
    requestAnimationFrame(() => {
      document.getElementById(anchorId)?.scrollIntoView({
        behavior: "smooth",
        block: "center",
      });
    });
  }, []);

  const reviewDecision = useCallback(
    async (id: string, status: "approved" | "rejected") => {
      if (pendingDecisions[id]) return;
      setActionError(null);
      setPendingDecisions((pending) => ({ ...pending, [id]: status }));
      try {
        await invoke("review_decision", { id, status });
        setState((current) =>
          current
            ? {
                ...current,
                messages: current.messages.map((message) =>
                  message.id === id ? { ...message, decisionStatus: status } : message,
                ),
              }
            : current,
        );
      } catch (error) {
        setActionError(`Decision was not saved: ${errorMessage(error)}`);
      } finally {
        setPendingDecisions((pending) => {
          const next = { ...pending };
          delete next[id];
          return next;
        });
      }
    },
    [pendingDecisions],
  );

  const unreadCount = messages.filter(
    (message) => message.kind !== "ack" && !message.read,
  ).length;
  const markRead = useCallback(async () => {
    const throughId = messages[messages.length - 1]?.id ?? null;
    if (!throughId || markingRead || unreadCount === 0) return;
    setActionError(null);
    setMarkingRead(true);
    try {
      await invoke("mark_read", { throughId });
      setState((current) =>
        current
          ? (() => {
              const throughIndex = current.messages.findIndex(
                (message) => message.id === throughId,
              );
              if (throughIndex === -1) return current;
              return {
                ...current,
                messages: current.messages.map((message, index) =>
                  index <= throughIndex && message.kind !== "ack"
                    ? { ...message, read: true }
                    : message,
                ),
              };
            })()
          : current,
      );
    } catch (error) {
      setActionError(`Messages were not marked as read: ${errorMessage(error)}`);
    } finally {
      setMarkingRead(false);
    }
  }, [markingRead, messages, unreadCount]);

  if (loading && !state) {
    return <main className="app-shell app-centered"><LoadingShell /></main>;
  }

  if (!state) {
    return (
      <main className="app-shell app-centered">
        <div className="fatal-error" role="alert">
          <span aria-hidden="true">!</span>
          <h1>Scribe couldn’t open</h1>
          <p>{connectionError || "The backend did not return an initial state."}</p>
          <button onClick={() => setRetryNonce((value) => value + 1)} type="button">Try again</button>
        </div>
      </main>
    );
  }

  const notesPath = relativeNotesPath(state.notesPath, state.repoPath);
  const notesPathParts = notesPath.split(/[\\/]/);
  const notesFileName = notesPathParts[notesPathParts.length - 1] || "Notes";

  return (
    <main className="app-shell">
      <section aria-label="Scribe messages" className="chat-pane">
        <header className="app-header">
          <div className="brand-lockup">
            <ScribeMark />
            <div>
              <h1>Scribe</h1>
              <span>Review stream</span>
            </div>
          </div>
          {unreadCount > 0 && (
            <span aria-label={`${unreadCount} unread messages`} className="unread-count">
              {unreadCount > 99 ? "99+" : unreadCount}
            </span>
          )}
        </header>

        {(liveWarning || actionError) && (
          <div className="warning-banner" role="alert">
            <span>{actionError || liveWarning}</span>
            {actionError && (
              <button aria-label="Dismiss error" onClick={() => setActionError(null)} type="button">×</button>
            )}
          </div>
        )}

        <div className="message-feed" onScroll={handleFeedScroll} ref={feedRef}>
          {messages.length === 0 ? (
            <div className="empty-state">
              <span aria-hidden="true" className="empty-icon"><CheckIcon /></span>
              <strong>You’re all caught up</strong>
              <p>New review notes and decisions will appear here.</p>
            </div>
          ) : (
            <div className="message-stack">
              {messages.map((message) => (
                <MessageCard
                  key={message.id}
                  message={message}
                  onDecision={reviewDecision}
                  onOpenFile={openFile}
                  onOpenReference={openReference}
                  pendingDecision={pendingDecisions[message.id]}
                  reference={references.byMessageId.get(message.id)}
                />
              ))}
            </div>
          )}
        </div>

        <footer className="read-toolbar">
          <button
            aria-label={
              unreadCount > 0
                ? `Mark ${unreadCount} message${unreadCount === 1 ? "" : "s"} as read`
                : "All messages are read"
            }
            className="mark-read-button"
            disabled={unreadCount === 0 || markingRead}
            onClick={markRead}
            type="button"
          >
            <CheckIcon />
            <span>{markingRead ? "Marking…" : "Mark as read"}</span>
            {unreadCount > 0 && <span className="button-count">{unreadCount}</span>}
          </button>
        </footer>
      </section>

      <section aria-label="Live notes" className="notes-pane">
        <header className="notes-header">
          <div>
            <span className="section-label">Live notes</span>
            <h2>{notesFileName}</h2>
          </div>
          <span className="notes-path" title={state.notesPath}>{notesPath}</span>
        </header>
        <div className="notes-scroll" ref={notesRef}>
          {state.markdown.trim() ? (
            <article className="markdown-body">
              <MarkdownDocument
                activeHighlightId={activeReferenceId}
                document={parsedDocument}
                highlights={references.highlights}
                onOpenFile={openFile}
              />
            </article>
          ) : (
            <div className="empty-state notes-empty">
              <span aria-hidden="true" className="empty-page">◇</span>
              <strong>No notes yet</strong>
              <p>Changes to {notesPath} will appear automatically.</p>
            </div>
          )}
        </div>
      </section>
    </main>
  );
}

export default App;
