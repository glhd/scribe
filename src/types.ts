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
  notesPath: string;
  repoPath: string;
  markdown: string;
  messages: ChatMessage[];
}
