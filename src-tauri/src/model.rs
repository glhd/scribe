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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageKind {
    Message,
    Ack,
    Decision,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSnapshot {
    pub notes_path: String,
    pub repo_path: String,
    pub markdown: String,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionEvent<'a> {
    pub timestamp: &'a str,
    #[serde(rename = "type")]
    pub event_type: &'a str,
    pub decision_id: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaleReferenceEvent<'a> {
    pub timestamp: &'a str,
    #[serde(rename = "type")]
    pub event_type: &'a str,
    pub message_id: &'a str,
    pub locator: &'a DocumentReference,
}
