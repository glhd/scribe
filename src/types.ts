export interface DocumentReference {
  heading: string[];
  snippet: string;
}

export interface FileReference {
  path: string;
  line?: number | null;
  endLine?: number | null;
  sha: string;
}

export type DecisionStatus = "unreviewed" | "approved" | "rejected";
export type SessionState = "active" | "finalizing" | "complete" | "interrupted";
export type AppMode =
  | "waitingCall"
  | "waitingTranscription"
  | "waitingClaude"
  | "active"
  | "finalizing"
  | "complete"
  | "interrupted";

export interface ChatMessage {
  id: string;
  kind: "message" | "ack" | "decision";
  timestamp: string;
  text: string;
  reference?: DocumentReference | null;
  files?: FileReference[];
  read: boolean;
  decisionStatus?: DecisionStatus | null;
}

export interface ScribeState {
  mode: AppMode;
  sessionId?: string | null;
  sessionState?: SessionState | null;
  notesPath?: string | null;
  repoPath?: string | null;
  markdown: string;
  messages: ChatMessage[];
  sources: SourceHealth[];
  sessions: SessionSummary[];
  chronicleCandidates: ChronicleCandidate[];
  chronicleRoot: string;
  chronicleRegistryFound: boolean;
  integrationInstalled: boolean;
  handoffSaved: boolean;
}

export interface SourceHealth {
  source: "tuple" | "claude" | "chronicle";
  status: "live" | "connected" | "waiting" | "stopped" | "ended" | "ambiguous" | "error" | "off";
  label: string;
  detail?: string | null;
}

export interface UpdateState {
  status: "checking" | "upToDate" | "available" | "installing" | "restarting" | "error";
  version?: string | null;
  error?: string | null;
}

export interface SessionSummary {
  id: string;
  state: SessionState;
  startedAt: string;
  updatedAt: string;
  attachedRepo?: string | null;
  hasUnsavedHandoff: boolean;
  dataPruned: boolean;
}

export interface ChronicleCandidate {
  id: string;
  state: "active" | "completed" | "interrupted";
  logPath: string;
  projectName: string;
  projectRoot: string;
  repositories: Array<{ root: string; branch?: string | null }>;
  startedAt: string;
  lastEventAt: string;
  endedAt?: string | null;
}
