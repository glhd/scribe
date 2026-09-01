import {
  type CSSProperties,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";
import {
  InlineMarkdown,
  MarkdownDocument,
  type HighlightMap,
} from "./MarkdownView";
import {
  parseMarkdown,
  resolveDocumentReference,
  type ParsedMarkdown,
  type ResolvedDocumentReference,
} from "./markdown";
import scribeIcon from "../src-tauri/icons/128x128.png";
import type {
  ChatMessage,
  DecisionStatus,
  DocumentReference,
  SessionSummary,
  ScribeState,
  SourceHealth,
  UpdateState,
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
const MIN_CHAT_PANE_WIDTH = 320;
const MIN_NOTES_PANE_WIDTH = 320;
const PANE_DIVIDER_LAYOUT_WIDTH = 1;
const PANE_RESIZE_STEP = 16;

function maximumChatPaneWidth(containerWidth: number): number {
  return Math.max(
    MIN_CHAT_PANE_WIDTH,
    containerWidth - MIN_NOTES_PANE_WIDTH - PANE_DIVIDER_LAYOUT_WIDTH,
  );
}

function constrainChatPaneWidth(width: number, containerWidth: number): number {
  return Math.min(
    maximumChatPaneWidth(containerWidth),
    Math.max(MIN_CHAT_PANE_WIDTH, width),
  );
}

function defaultChatPaneWidth(viewportWidth: number): number {
  if (viewportWidth <= 980) return MIN_CHAT_PANE_WIDTH;
  return Math.min(420, Math.max(MIN_CHAT_PANE_WIDTH, viewportWidth * 0.34));
}

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

function AppIcon({ className }: { className: string }) {
  return (
    <img alt="" aria-hidden="true" className={className} src={scribeIcon} />
  );
}

function WindowTitlebar() {
  return (
    <div aria-hidden="true" className="window-titlebar" data-tauri-drag-region>
      <span data-tauri-drag-region>Scribe</span>
    </div>
  );
}

function PaneResizer({
  width,
  onResize,
}: {
  width: number;
  onResize: (width: number) => void;
}) {
  const [resizing, setResizing] = useState(false);

  const resizeFromPointer = (clientX: number, divider: HTMLDivElement) => {
    const bounds = divider.parentElement?.getBoundingClientRect();
    if (bounds) onResize(constrainChatPaneWidth(clientX - bounds.left, bounds.width));
  };

  return (
    <div
      aria-controls="scribe-messages planning-handoff-pane"
      aria-label="Resize review and planning panes"
      aria-orientation="vertical"
      aria-valuemax={maximumChatPaneWidth(window.innerWidth)}
      aria-valuemin={MIN_CHAT_PANE_WIDTH}
      aria-valuenow={Math.round(width)}
      aria-valuetext={`${Math.round(width)} pixels for Review`}
      className={`pane-resizer${resizing ? " is-resizing" : ""}`}
      onKeyDown={(event) => {
        const containerWidth = event.currentTarget.parentElement?.clientWidth ?? window.innerWidth;
        let nextWidth: number | undefined;
        if (event.key === "ArrowLeft") nextWidth = width - PANE_RESIZE_STEP;
        if (event.key === "ArrowRight") nextWidth = width + PANE_RESIZE_STEP;
        if (event.key === "Home") nextWidth = MIN_CHAT_PANE_WIDTH;
        if (event.key === "End") nextWidth = maximumChatPaneWidth(containerWidth);
        if (nextWidth === undefined) return;
        event.preventDefault();
        onResize(constrainChatPaneWidth(nextWidth, containerWidth));
      }}
      onLostPointerCapture={() => setResizing(false)}
      onPointerCancel={(event) => {
        if (event.currentTarget.hasPointerCapture(event.pointerId)) {
          event.currentTarget.releasePointerCapture(event.pointerId);
        }
        setResizing(false);
      }}
      onPointerDown={(event) => {
        if (event.button !== 0) return;
        event.preventDefault();
        event.currentTarget.setPointerCapture(event.pointerId);
        setResizing(true);
        resizeFromPointer(event.clientX, event.currentTarget);
      }}
      onPointerMove={(event) => {
        if (event.currentTarget.hasPointerCapture(event.pointerId)) {
          resizeFromPointer(event.clientX, event.currentTarget);
        }
      }}
      onPointerUp={(event) => {
        if (event.currentTarget.hasPointerCapture(event.pointerId)) {
          event.currentTarget.releasePointerCapture(event.pointerId);
        }
        setResizing(false);
      }}
      role="separator"
      tabIndex={0}
    />
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
      <AppIcon className="loading-mark" />
      <strong>Opening Scribe</strong>
      <span>Loading notes and messages…</span>
    </div>
  );
}

function SourceStrip({ sources }: { sources: SourceHealth[] }) {
  return (
    <div aria-label="Session sources" className="source-status-strip" role="status">
      {sources.map((source) => (
        <span
          className={`source-status is-${source.status}`}
          key={source.source}
          title={source.detail ?? undefined}
        >
          <span aria-hidden="true" className="source-dot" />
          <span className="source-copy">
            <span className="source-name">{source.source}</span>
            <span className="source-label">{source.label}</span>
          </span>
        </span>
      ))}
    </div>
  );
}

function UpdateControl({
  state,
  onUpdate,
}: {
  state: UpdateState | null;
  onUpdate: () => void;
}) {
  if (!state || state.status === "checking" || state.status === "upToDate") return null;

  const busy = state.status === "installing" || state.status === "restarting";
  const label =
    state.status === "available"
      ? "Update"
      : state.status === "installing"
        ? "Updating…"
        : state.status === "restarting"
          ? "Restarting…"
          : "Update failed";
  const description =
    state.status === "available"
      ? `Install Scribe ${state.version ?? "update"}`
      : state.status === "error"
        ? `${state.error ?? "The update failed."} Retry update.`
        : label;

  return (
    <button
      aria-label={description}
      className={`update-button is-${state.status}`}
      disabled={busy}
      onClick={onUpdate}
      title={description}
      type="button"
    >
      {label}
    </button>
  );
}

function sessionTime(session: SessionSummary): { short: string; full: string } {
  const date = new Date(session.startedAt);
  if (Number.isNaN(date.getTime())) {
    return { short: session.startedAt, full: session.startedAt };
  }
  return {
    short: date.toLocaleDateString(undefined, { month: "short", day: "numeric" }),
    full: date.toLocaleString(),
  };
}

function sessionName(session: SessionSummary): string {
  const path = session.attachedRepo?.replace(/[\\/]$/, "");
  return path?.split(/[\\/]/).pop() || "Planning session";
}

function SessionHistory({
  sessions,
  currentId,
  onSelect,
  onDelete,
}: {
  sessions: SessionSummary[];
  currentId?: string | null;
  onSelect: (id: string) => void;
  onDelete: (id: string) => void;
}) {
  if (sessions.length === 0) return null;
  return (
    <details className="history-menu">
      <summary aria-label="Show session history">History</summary>
      <div className="history-popover">
        <header className="popover-heading">
          <strong>History</strong>
          <span>Recent planning sessions.</span>
        </header>
        <div className="history-list" role="list">
          {sessions.map((session) => {
            const time = sessionTime(session);
            return (
              <div
                className={session.id === currentId ? "history-row is-current" : "history-row"}
                key={session.id}
                role="listitem"
              >
                <button
                  aria-label={`Open ${sessionName(session)}, ${session.state}, ${time.full}`}
                  onClick={() => onSelect(session.id)}
                  type="button"
                >
                  <span className="history-row-heading">
                    <strong>{sessionName(session)}</strong>
                    <time dateTime={session.startedAt} title={time.full}>{time.short}</time>
                  </span>
                  <span className="history-row-meta">
                    <span className={`session-state is-${session.state}`}>{session.state}</span>
                    {session.hasUnsavedHandoff && <em>Unsaved handoff</em>}
                    {session.dataPruned && <span>Details expired</span>}
                  </span>
                  <code>{session.id}</code>
                </button>
                {(session.state === "complete" || session.state === "interrupted") && (
                  <button
                    aria-label={`Delete session ${session.id}`}
                    className="history-delete"
                    onClick={() => onDelete(session.id)}
                    title="Delete Scribe-owned session data"
                    type="button"
                  >
                    ×
                  </button>
                )}
              </div>
            );
          })}
        </div>
      </div>
    </details>
  );
}

function ChronicleSettings({
  root,
  found,
  choosing,
  onChoose,
}: {
  root: string;
  found: boolean;
  choosing: boolean;
  onChoose: () => void;
}) {
  return (
    <details className="settings-menu">
      <summary aria-label="Show source settings">Sources</summary>
      <div className="settings-popover">
        <header className="popover-heading">
          <strong>Sources</strong>
          <span>Chronicle is optional.</span>
        </header>
        <dl className="source-settings">
          <div>
            <dt>Chronicle</dt>
            <dd>{found ? "Registry detected" : "Registry not detected"}</dd>
          </div>
          <div>
            <dt>Folder</dt>
            <dd><code title={root}>{root}</code></dd>
          </div>
        </dl>
        <button disabled={choosing} onClick={onChoose} type="button">
          {choosing ? "Choosing…" : "Choose Chronicle folder…"}
        </button>
      </div>
    </details>
  );
}

function ChronicleFolderNotice({
  choosing,
  onChoose,
}: {
  choosing: boolean;
  onChoose: () => void;
}) {
  return (
    <div className="chronicle-folder-notice" role="status">
      <span><strong>Chronicle is off.</strong> No registry was detected.</span>
      <button disabled={choosing} onClick={onChoose} type="button">
        {choosing ? "Choosing…" : "Choose Chronicle folder"}
      </button>
    </div>
  );
}

function WaitingForCall({
  state,
  installing,
  choosingChronicle,
  error,
  updateState,
  onInstall,
  onUpdate,
  onChooseChronicle,
  onSelect,
  onDelete,
}: {
  state: ScribeState;
  installing: boolean;
  choosingChronicle: boolean;
  error?: string | null;
  updateState: UpdateState | null;
  onInstall: () => void;
  onUpdate: () => void;
  onChooseChronicle: () => void;
  onSelect: (id: string) => void;
  onDelete: (id: string) => void;
}) {
  const sourceError = state.sources.find((source) => source.status === "error");
  return (
    <main className="app-shell app-centered waiting-shell">
      <WindowTitlebar />
      <header className="waiting-titlebar" data-tauri-drag-region>
        <div className="header-actions">
          <ChronicleSettings
            choosing={choosingChronicle}
            found={state.chronicleRegistryFound}
            onChoose={onChooseChronicle}
            root={state.chronicleRoot}
          />
          <UpdateControl onUpdate={onUpdate} state={updateState} />
          {state.sessions.length > 0 && (
            <SessionHistory
              sessions={state.sessions}
              onDelete={onDelete}
              onSelect={onSelect}
            />
          )}
        </div>
      </header>
      <div className="waiting-content">
        <section aria-labelledby="waiting-title" className="waiting-card">
          <AppIcon className="waiting-mark" />
          <span className="waiting-eyebrow">Tuple call companion</span>
          <h2 id="waiting-title">Waiting for a Tuple call</h2>
          <p>Join a call in Tuple and Scribe will detect it automatically. Start transcription in Tuple when you’re ready.</p>
          <SourceStrip sources={state.sources} />
          {!state.chronicleRegistryFound && (
            <ChronicleFolderNotice choosing={choosingChronicle} onChoose={onChooseChronicle} />
          )}
          {(error || sourceError?.detail) && (
            <div className="waiting-error" role="alert">{error || sourceError?.detail}</div>
          )}
          {!state.integrationInstalled && (
            <button className="primary-action" disabled={installing} onClick={onInstall} type="button">
              {installing ? "Installing…" : "Install Claude integration"}
            </button>
          )}
        </section>
      </div>
    </main>
  );
}

function App() {
  const [state, setState] = useState<ScribeState | null>(null);
  const [updateState, setUpdateState] = useState<UpdateState | null>(null);
  const [chatPaneWidth, setChatPaneWidth] = useState(() => defaultChatPaneWidth(window.innerWidth));
  const [loading, setLoading] = useState(true);
  const [connectionError, setConnectionError] = useState<string | null>(null);
  const [liveWarning, setLiveWarning] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [retryNonce, setRetryNonce] = useState(0);
  const [markingRead, setMarkingRead] = useState(false);
  const [installingIntegration, setInstallingIntegration] = useState(false);
  const [copyingNotes, setCopyingNotes] = useState(false);
  const [savingNotes, setSavingNotes] = useState(false);
  const [choosingChronicle, setChoosingChronicle] = useState(false);
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
    const fitPanesToWindow = () => {
      setChatPaneWidth((width) => constrainChatPaneWidth(width, window.innerWidth));
    };
    window.addEventListener("resize", fitPanesToWindow);
    return () => window.removeEventListener("resize", fitPanesToWindow);
  }, []);

  useEffect(() => {
    let active = true;
    let unlisten: UnlistenFn | undefined;

    const connectUpdater = async () => {
      try {
        unlisten = await listen<UpdateState>("update_state_changed", (event) => {
          if (active) setUpdateState(event.payload);
        });
        if (!active) {
          unlisten();
          return;
        }
        const initialState = await invoke<UpdateState>("get_update_state");
        if (active) setUpdateState(initialState);
      } catch (error) {
        if (active) {
          setUpdateState({
            error: `Could not read update status: ${errorMessage(error)}`,
            status: "error",
          });
        }
      }
    };

    void connectUpdater();
    return () => {
      active = false;
      unlisten?.();
    };
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

  const installIntegration = useCallback(async () => {
    if (installingIntegration) return;
    setActionError(null);
    setInstallingIntegration(true);
    try {
      await invoke("install_claude_integration");
    } catch (error) {
      setActionError(`Claude integration was not installed: ${errorMessage(error)}`);
    } finally {
      setInstallingIntegration(false);
    }
  }, [installingIntegration]);

  const updateApp = useCallback(async () => {
    if (!updateState) return;
    const retryingCheck = updateState.status === "error" && !updateState.version;
    if (updateState.status === "available" || (updateState.status === "error" && updateState.version)) {
      setUpdateState((current) => current ? { ...current, error: null, status: "installing" } : current);
    } else if (retryingCheck) {
      setUpdateState({ status: "checking" });
    } else {
      return;
    }
    try {
      await invoke(retryingCheck ? "check_for_update" : "install_update");
    } catch (error) {
      setUpdateState({
        error: `Update request failed: ${errorMessage(error)}`,
        status: "error",
        version: updateState.version,
      });
    }
  }, [updateState]);

  const selectSession = useCallback(async (id: string) => {
    setActionError(null);
    try {
      await invoke("select_session", { id });
    } catch (error) {
      setActionError(`Could not open that session: ${errorMessage(error)}`);
    }
  }, []);

  const deleteSession = useCallback(async (id: string) => {
    if (!window.confirm("Delete this Scribe session and its internal handoff? Saved copies are not affected.")) return;
    setActionError(null);
    try {
      await invoke("delete_session", { id });
    } catch (error) {
      setActionError(`Could not delete that session: ${errorMessage(error)}`);
    }
  }, []);

  const selectChronicle = useCallback(async (id: string) => {
    setActionError(null);
    try {
      await invoke("select_chronicle", { id });
    } catch (error) {
      setActionError(`Could not attach Chronicle: ${errorMessage(error)}`);
    }
  }, []);

  const chooseChronicleFolder = useCallback(async () => {
    if (!state || choosingChronicle) return;
    setActionError(null);
    setChoosingChronicle(true);
    try {
      const selection = await open({
        defaultPath: state.chronicleRoot,
        directory: true,
        multiple: false,
        title: "Choose Chronicle folder",
      });
      const path = Array.isArray(selection) ? selection[0] : selection;
      if (path) await invoke("choose_chronicle_folder", { path });
    } catch (error) {
      setActionError(`Could not use that Chronicle folder: ${errorMessage(error)}`);
    } finally {
      setChoosingChronicle(false);
    }
  }, [choosingChronicle, state]);

  const copyNotes = useCallback(async () => {
    if (!state || copyingNotes) return;
    setActionError(null);
    setCopyingNotes(true);
    try {
      await navigator.clipboard.writeText(state.markdown);
    } catch (error) {
      setActionError(`Could not copy the handoff: ${errorMessage(error)}`);
    } finally {
      setCopyingNotes(false);
    }
  }, [copyingNotes, state]);

  const saveNotes = useCallback(async () => {
    if (!state?.sessionId || savingNotes) return;
    setActionError(null);
    setSavingNotes(true);
    try {
      const destination = await save({
        defaultPath: "planning-handoff.md",
        filters: [{ name: "Markdown", extensions: ["md"] }],
        title: "Save planning handoff",
      });
      if (destination) await invoke("export_notes", { destination });
    } catch (error) {
      setActionError(`Could not save the handoff: ${errorMessage(error)}`);
    } finally {
      setSavingNotes(false);
    }
  }, [savingNotes, state?.sessionId]);

  if (loading && !state) {
    return <main className="app-shell app-centered"><WindowTitlebar /><LoadingShell /></main>;
  }

  if (!state) {
    return (
      <main className="app-shell app-centered">
        <WindowTitlebar />
        <div className="fatal-error" role="alert">
          <span aria-hidden="true">!</span>
          <h1>Scribe couldn’t open</h1>
          <p>{connectionError || "The backend did not return an initial state."}</p>
          <button onClick={() => setRetryNonce((value) => value + 1)} type="button">Try again</button>
        </div>
      </main>
    );
  }

  if (!state.sessionId) {
    return (
      <WaitingForCall
        choosingChronicle={choosingChronicle}
        error={actionError}
        installing={installingIntegration}
        onChooseChronicle={chooseChronicleFolder}
        onDelete={deleteSession}
        onInstall={installIntegration}
        onUpdate={updateApp}
        onSelect={selectSession}
        state={state}
        updateState={updateState}
      />
    );
  }

  const sourceDetails = state.sources.filter(
    (source) =>
      source.detail && ["stopped", "error", "ambiguous"].includes(source.status),
  );
  const modeNotice =
    state.mode === "waitingTranscription"
      ? "Call found. Waiting for transcription — start transcription in Tuple."
      : state.mode === "waitingClaude"
        ? "Waiting for planning-scribe to attach from a repository."
        : state.mode === "finalizing"
          ? "Tuple call ended. Claude is finishing the handoff."
          : state.mode === "interrupted"
            ? "This session was interrupted. Review or save the handoff, then delete it when no longer needed."
            : null;
  const emptyNotes =
    state.mode === "waitingTranscription"
      ? ["Waiting for transcription", "Start transcription in Tuple. Scribe will attach automatically."]
      : state.mode === "waitingClaude"
        ? ["Waiting for planning-scribe", "Start planning-scribe from the repository for this call."]
        : state.mode === "finalizing"
          ? ["Finalizing the plan", "Claude is making its final notes pass."]
          : ["No notes yet", "The internal handoff will appear here as planning-scribe writes it."];
  const planReady = state.mode === "complete" || state.mode === "interrupted";

  return (
    <main
      className={`app-shell mode-${state.mode}`}
      style={{ "--chat-pane-width": `${chatPaneWidth}px` } as CSSProperties}
    >
      <WindowTitlebar />
      <section aria-label="Scribe messages" className="chat-pane" id="scribe-messages">
        <header className="app-header" data-tauri-drag-region>
          <div className="sidebar-heading" data-tauri-drag-region>
            <h1 data-tauri-drag-region>Review</h1>
            <span data-tauri-drag-region>{planReady ? "Session complete" : "Claude’s stream"}</span>
          </div>
          <div className="header-actions">
            {unreadCount > 0 && (
              <span
                aria-label={`${unreadCount} unread messages`}
                className="unread-count"
                role="status"
              >
                {unreadCount > 99 ? "99+" : unreadCount}
              </span>
            )}
          </div>
        </header>

        {!state.integrationInstalled && (
          <div className="integration-notice" role="status">
            <span><strong>Claude integration is not installed.</strong> Install it once for future sessions.</span>
            <button
              disabled={installingIntegration}
              onClick={installIntegration}
              type="button"
            >
              {installingIntegration ? "Installing…" : "Install"}
            </button>
          </div>
        )}

        {!state.chronicleRegistryFound && (
          <ChronicleFolderNotice choosing={choosingChronicle} onChoose={chooseChronicleFolder} />
        )}

        {(actionError || liveWarning) && (
          <div className="warning-banner" role="alert">
            <span>{actionError || liveWarning}</span>
            {actionError && (
              <button aria-label="Dismiss error" onClick={() => setActionError(null)} type="button">×</button>
            )}
          </div>
        )}

        {sourceDetails.length > 0 && (
          <div
            className={`source-detail-banner ${
              sourceDetails.every((source) => source.status === "ambiguous")
                ? "is-ambiguous"
                : "is-error"
            }`}
            role="alert"
          >
            {sourceDetails.map((source) => (
              <span key={source.source}>
                <strong>{source.source}</strong>
                {source.detail}
              </span>
            ))}
          </div>
        )}

        {modeNotice && (
          <div className={`mode-banner is-${state.mode}`} role="status">
            {modeNotice}
          </div>
        )}

        {state.chronicleCandidates.length > 1 &&
          state.sources.some((source) => source.source === "chronicle" && source.status === "ambiguous") && (
            <div className="chronicle-picker" role="group" aria-label="Choose Chronicle session">
              <span>Chronicle match:</span>
              {state.chronicleCandidates.map((candidate) => (
                <button key={candidate.id} onClick={() => selectChronicle(candidate.id)} type="button">
                  {candidate.projectName} · {new Date(candidate.startedAt).toLocaleTimeString()} · {candidate.state}
                </button>
              ))}
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

        <SourceStrip sources={state.sources} />

        <footer className="read-toolbar">
          <div className="sidebar-tools">
            <ChronicleSettings
              choosing={choosingChronicle}
              found={state.chronicleRegistryFound}
              onChoose={chooseChronicleFolder}
              root={state.chronicleRoot}
            />
            <UpdateControl onUpdate={updateApp} state={updateState} />
            <SessionHistory
              currentId={state.sessionId}
              onDelete={deleteSession}
              onSelect={selectSession}
              sessions={state.sessions}
            />
          </div>
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
            <span className="mark-read-label">{markingRead ? "Marking…" : "Mark as read"}</span>
            {unreadCount > 0 && <span className="button-count">{unreadCount}</span>}
          </button>
        </footer>
      </section>

      <PaneResizer onResize={setChatPaneWidth} width={chatPaneWidth} />

      <section
        aria-label={planReady ? "Planning handoff" : "Live notes"}
        className="notes-pane"
        id="planning-handoff-pane"
      >
        <header className="notes-header" data-tauri-drag-region>
          <div className="notes-title" data-tauri-drag-region>
            <h2 data-tauri-drag-region>Planning handoff</h2>
            <span className="section-label" data-tauri-drag-region>{planReady ? "Plan ready" : "Live notes"}</span>
          </div>
          {planReady ? (
            <div className="handoff-actions">
              {state.handoffSaved && <span className="saved-label"><CheckIcon /> Saved</span>}
              <button disabled={!state.markdown || copyingNotes} onClick={copyNotes} type="button">
                {copyingNotes ? "Copying…" : "Copy"}
              </button>
              <button className="save-as-button" disabled={!state.markdown || savingNotes} onClick={saveNotes} type="button">
                {savingNotes ? "Saving…" : "Save As…"}
              </button>
            </div>
          ) : (
            <span className="notes-path" data-tauri-drag-region title={state.notesPath ?? undefined}>Internal notes · notes.md</span>
          )}
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
              <strong>{emptyNotes[0]}</strong>
              <p>{emptyNotes[1]}</p>
            </div>
          )}
        </div>
      </section>
    </main>
  );
}

export default App;
