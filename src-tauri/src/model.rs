use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentReference {
    pub heading: Vec<String>,
    pub snippet: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileReference {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    pub sha: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageKind {
    Message,
    Ack,
    Decision,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionStatus {
    Unreviewed,
    Approved,
    Rejected,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub id: String,
    pub kind: MessageKind,
    pub timestamp: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<DocumentReference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileReference>,
    pub read: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_status: Option<DecisionStatus>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Active,
    Finalizing,
    Complete,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AppMode {
    WaitingCall,
    WaitingTranscription,
    WaitingClaude,
    Active,
    Finalizing,
    Complete,
    Interrupted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceHealth {
    pub source: String,
    pub status: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub state: SessionState,
    pub started_at: String,
    pub updated_at: String,
    pub attached_repo: Option<String>,
    pub has_unsaved_handoff: bool,
    pub data_pruned: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChronicleCandidate {
    pub id: String,
    pub state: String,
    pub log_path: String,
    pub project_name: String,
    pub project_root: String,
    pub repositories: Vec<ChronicleRepository>,
    pub started_at: String,
    pub last_event_at: String,
    pub ended_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChronicleRepository {
    pub root: String,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSnapshot {
    pub mode: AppMode,
    pub session_id: Option<String>,
    pub session_state: Option<SessionState>,
    pub notes_path: Option<String>,
    pub repo_path: Option<String>,
    pub markdown: String,
    pub messages: Vec<ChatMessage>,
    pub sources: Vec<SourceHealth>,
    pub sessions: Vec<SessionSummary>,
    pub chronicle_candidates: Vec<ChronicleCandidate>,
    pub chronicle_root: String,
    pub chronicle_registry_found: bool,
    pub integration_installed: bool,
    pub handoff_saved: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedEvent {
    pub stable_id: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_sequence: Option<u64>,
    pub occurred_at: String,
    pub observed_at: String,
    pub kind: String,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TickResult {
    pub session_id: String,
    pub session_state: SessionState,
    pub notes_path: String,
    pub repo_path: Option<String>,
    pub source_health: Vec<SourceHealth>,
    pub events: Vec<NormalizedEvent>,
    pub has_more: bool,
}
