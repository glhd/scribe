use std::{
    collections::HashSet,
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    process::Command,
};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    model::{ChronicleCandidate, ChronicleRepository, NormalizedEvent, SessionState},
    storage::{self, SessionRecord, Store},
};

#[derive(Clone, Debug)]
pub struct TupleClient {
    executable: PathBuf,
}

#[derive(Debug)]
struct ParsedTupleBatch {
    events: Vec<NormalizedEvent>,
    status: Option<(&'static str, &'static str)>,
    call_ended: bool,
    malformed: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ChronicleCursor {
    path: String,
    offset: u64,
    file_id: Option<u64>,
    last_sequence: u64,
    last_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChronicleRegistry {
    schema_version: u64,
    updated_at: String,
    sessions: Vec<ChronicleRegistrySession>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChronicleRegistrySession {
    id: String,
    state: String,
    log_path: String,
    project_name: String,
    project_root: String,
    repositories: Vec<ChronicleRepository>,
    started_at: String,
    last_event_at: String,
    heartbeat_at: String,
    ended_at: Option<String>,
    ide: ChronicleIde,
    pid: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChronicleIde {
    product: String,
    version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChronicleEnvelope {
    schema_version: u64,
    id: String,
    session_id: String,
    sequence: u64,
    #[serde(rename = "type")]
    kind: String,
    occurred_at: String,
    recorded_at: String,
    redacted: Option<bool>,
    data: Value,
}

#[derive(Debug)]
struct ParsedChronicleChunk {
    events: Vec<NormalizedEvent>,
    consumed: usize,
    last_sequence: u64,
    last_type: Option<String>,
}

impl TupleClient {
    pub fn discover() -> Self {
        if let Some(path) = env::var_os("TUPLE_BIN") {
            return Self::new(PathBuf::from(path));
        }
        for candidate in ["/usr/local/bin/tuple", "/opt/homebrew/bin/tuple"] {
            if Path::new(candidate).is_file() {
                return Self::new(PathBuf::from(candidate));
            }
        }
        Self::new(PathBuf::from("tuple"))
    }

    pub fn new(executable: PathBuf) -> Self {
        Self { executable }
    }

    pub fn current_call(&self) -> Result<Option<String>, String> {
        let output = self
            .command()
            .args(["call", "current", "--format", "json"])
            .output()
            .map_err(|error| tuple_launch_error(&self.executable, error))?;
        if !output.status.success() {
            let stderr = output_text(&output.stderr);
            if stderr.to_ascii_lowercase().contains("not in a call") {
                return Ok(None);
            }
            return Err(format!("Tuple could not report the current call: {stderr}"));
        }
        let value: Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("Tuple returned invalid current-call JSON: {error}"))?;
        let id = value
            .get("id")
            .or_else(|| value.get("call_id"))
            .or_else(|| value.get("callId"))
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| "Tuple current-call JSON did not contain an ID".to_string())?;
        Ok(Some(id.to_string()))
    }

    pub fn collect(
        &self,
        store: &Store,
        session: &SessionRecord,
        timeout: &str,
    ) -> Result<(), String> {
        let lock_path = store.lock_path("tuple", &session.id);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| format!("cannot open {}: {error}", lock_path.display()))?;
        lock.lock_exclusive()
            .map_err(|error| format!("cannot lock {}: {error}", lock_path.display()))?;
        let cursor = format!("scribe-{}", session.id);
        let output = self
            .command()
            .args([
                "--format",
                "json",
                "transcription",
                "show",
                &session.id,
                "--wait",
                "--timeout",
                timeout,
                "--with-events",
                "--cursor",
                &cursor,
            ])
            .output()
            .map_err(|error| tuple_launch_error(&self.executable, error))?;
        FileExt::unlock(&lock).ok();
        if !output.status.success() {
            let stderr = output_text(&output.stderr);
            let lower = stderr.to_ascii_lowercase();
            if lower.contains("transcription")
                && (lower.contains("not") || lower.contains("no recording"))
            {
                let previously_started = store
                    .source_state(&session.id, "tuple")?
                    .is_some_and(|state| matches!(state.status.as_str(), "live" | "stopped"));
                let (status, detail) = if previously_started {
                    (
                        "stopped",
                        "Transcription stopped during the call. Restart it in Tuple if intended.",
                    )
                } else {
                    (
                        "waiting",
                        "Call found. Waiting for transcription — start it in Tuple.",
                    )
                };
                store.set_source_state(&session.id, "tuple", status, Some(detail), None)?;
                return Ok(());
            }
            store.set_source_state(
                &session.id,
                "tuple",
                "error",
                Some(&format!("Tuple transcription reader failed: {stderr}")),
                None,
            )?;
            return Err(format!("Tuple transcription reader failed: {stderr}"));
        }

        let batch = parse_tuple_records(&output.stdout);
        store.insert_source_events(&session.id, &batch.events)?;
        if let Some((status, detail)) = batch.status {
            store.set_source_state(&session.id, "tuple", status, Some(detail), None)?;
        }
        if batch.malformed > 0 {
            store.set_source_state(
                &session.id,
                "tuple",
                "error",
                Some(&format!(
                    "Ignored {} malformed Tuple record{}; durable records were kept.",
                    batch.malformed,
                    if batch.malformed == 1 { "" } else { "s" }
                )),
                None,
            )?;
        }
        if batch.call_ended {
            store.mark_call_ended(&session.id)?;
        }
        Ok(())
    }

    fn command(&self) -> Command {
        Command::new(&self.executable)
    }
}

pub fn ensure_current_session(store: &Store, tuple: &TupleClient) -> Result<SessionRecord, String> {
    if let Some(session) = store.current_session()? {
        return match tuple.current_call() {
            Ok(Some(call_id)) if call_id != session.id => store.create_or_resume_session(&call_id),
            // Existing active/finalizing session writes remain available when
            // Tuple is between calls or its CLI is temporarily unavailable.
            Ok(_) | Err(_) => Ok(session),
        };
    }
    let call_id = tuple.current_call()?.ok_or_else(|| {
        "no active Scribe session; join a Tuple call and open Scribe first".to_string()
    })?;
    store.create_or_resume_session(&call_id)
}

pub fn collect_once(store: &Store, tuple: &TupleClient, timeout: &str) -> Result<(), String> {
    let current_call = tuple.current_call();
    let mut session = store.current_session()?;
    match current_call {
        Ok(Some(call_id)) => {
            if session.as_ref().map(|item| item.id.as_str()) != Some(call_id.as_str()) {
                session = Some(store.create_or_resume_session(&call_id)?);
            }
            if let Some(active) = &session {
                if active.state == SessionState::Active {
                    store.touch_session(&active.id)?;
                    tuple.collect(store, active, timeout)?;
                }
            }
        }
        Ok(None) => {
            // A reader scoped to the stable call ID receives Tuple's explicit
            // call_ended status after the current-call endpoint becomes empty.
            if let Some(active) = &session {
                if active.state == SessionState::Active {
                    tuple.collect(store, active, timeout)?;
                }
            }
        }
        Err(error) => {
            if let Some(active) = &session {
                store.set_source_state(&active.id, "tuple", "error", Some(&error), None)?;
            } else {
                return Err(error);
            }
        }
    }
    if let Some(active) = session {
        discover_chronicle(store, &active)?;
        collect_chronicle(store, &active)?;
    }
    Ok(())
}

fn parse_tuple_records(bytes: &[u8]) -> ParsedTupleBatch {
    let observed_at = storage::now();
    let mut batch = ParsedTupleBatch {
        events: Vec::new(),
        status: None,
        call_ended: false,
        malformed: 0,
    };
    for raw_line in bytes.split(|byte| *byte == b'\n') {
        let raw_line = trim_ascii(raw_line);
        if raw_line.is_empty() {
            continue;
        }
        let record: Value = match serde_json::from_slice(raw_line) {
            Ok(value) => value,
            Err(_) => {
                batch.malformed += 1;
                continue;
            }
        };
        if record.get("kind").and_then(Value::as_str) == Some("status") {
            if record.get("status").and_then(Value::as_str) == Some("call_ended") {
                batch.call_ended = true;
                batch.status = Some(("ended", "Call ended. Claude is finishing the handoff."));
                batch.events.push(NormalizedEvent {
                    stable_id: "tuple:call-ended".to_string(),
                    source: "tuple".to_string(),
                    stream_id: None,
                    source_sequence: None,
                    occurred_at: observed_at.clone(),
                    observed_at: observed_at.clone(),
                    kind: "call_ended".to_string(),
                    payload: record,
                });
            }
            continue;
        }
        let kind = record
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        if matches!(kind.as_str(), "user_audio_started" | "user_audio_stopped") {
            continue;
        }
        let data = record.get("data").and_then(Value::as_object);
        let occurred_value = if kind == "transcription_finished" {
            data.and_then(|data| data.get("start"))
                .or_else(|| record.get("time"))
        } else {
            record.get("time")
        }
        .unwrap_or(&Value::Null);
        let occurred_at = storage::normalize_timestamp(occurred_value, &observed_at);
        let normalized_kind = if kind == "transcription_finished" {
            "speech"
        } else {
            &kind
        };
        if normalized_kind == "speech"
            && data
                .and_then(|data| data.get("text"))
                .and_then(Value::as_str)
                .is_none_or(|text| text.trim().is_empty())
        {
            continue;
        }
        let identity = record
            .get("id")
            .or_else(|| data.and_then(|data| data.get("id")))
            .and_then(value_identity)
            .map(|id| format!("tuple:{kind}:{id}"))
            .unwrap_or_else(|| format!("tuple:{}", storage::stable_hash(raw_line)));
        let payload = if normalized_kind == "speech" {
            serde_json::json!({
                "text": data.and_then(|data| data.get("text")).cloned().unwrap_or(Value::Null),
                "speakerId": data.and_then(|data| data.get("user_id")).cloned().unwrap_or(Value::Null),
                "raw": record.clone(),
            })
        } else {
            record.clone()
        };
        batch.events.push(NormalizedEvent {
            stable_id: identity,
            source: "tuple".to_string(),
            stream_id: None,
            source_sequence: None,
            occurred_at,
            observed_at: observed_at.clone(),
            kind: normalized_kind.to_string(),
            payload,
        });
        match kind.as_str() {
            "transcription_finished" | "transcription_started" | "recording_started" => {
                batch.status = Some(("live", "Transcription is live."));
            }
            "transcription_dropped" => {
                batch.status = Some((
                    "stopped",
                    "Tuple reported a transcription gap. Scribe will not restart it automatically.",
                ));
            }
            "recording_ended" | "transcription_ended" => {
                batch.status = Some((
                    "stopped",
                    "Transcription stopped during the call. Restart it in Tuple if intended.",
                ));
            }
            _ => {}
        }
    }
    batch.events.sort_by(|left, right| {
        left.occurred_at
            .cmp(&right.occurred_at)
            .then_with(|| left.stable_id.cmp(&right.stable_id))
    });
    batch
}

pub fn discover_chronicle(store: &Store, session: &SessionRecord) -> Result<(), String> {
    let Some(repo) = session.repo.as_ref() else {
        store.replace_chronicle_candidates(&session.id, &[])?;
        store.set_source_state(
            &session.id,
            "chronicle",
            "off",
            Some("Attach a repository to discover Chronicle."),
            None,
        )?;
        return Ok(());
    };
    let registry_path = store.chronicle_root()?.join("sessions.json");
    let raw = match fs::read(&registry_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            store.replace_chronicle_candidates(&session.id, &[])?;
            store.set_source_state(&session.id, "chronicle", "off", Some("Not detected"), None)?;
            return Ok(());
        }
        Err(error) => {
            let message = format!(
                "cannot read Chronicle registry {}: {error}",
                registry_path.display()
            );
            store.set_source_state(&session.id, "chronicle", "error", Some(&message), None)?;
            return Err(message);
        }
    };
    let candidates = match parse_chronicle_registry(&raw) {
        Ok(candidates) => candidates,
        Err(error) => {
            let message = format!(
                "invalid Chronicle registry {}: {error}",
                registry_path.display()
            );
            store.set_source_state(&session.id, "chronicle", "error", Some(&message), None)?;
            return Err(message);
        }
    };
    let candidates = match_chronicle_candidates(
        candidates,
        repo,
        &session.started_at,
        store.session_end(&session.id)?.as_deref(),
    );
    store.replace_chronicle_candidates(&session.id, &candidates)?;
    match (candidates.len(), store.selected_chronicle(&session.id)?) {
        (0, _) => {
            store.set_source_state(&session.id, "chronicle", "off", Some("Not detected"), None)
        }
        (1, _) => set_chronicle_candidate_health(store, &session.id, &candidates[0], None),
        (_, Some(candidate)) => {
            set_chronicle_candidate_health(store, &session.id, &candidate, None)
        }
        (_, None) => store.set_source_state(
            &session.id,
            "chronicle",
            "ambiguous",
            Some("Multiple Chronicle sessions match this repository."),
            None,
        ),
    }
}

pub fn collect_chronicle(store: &Store, session: &SessionRecord) -> Result<(), String> {
    let Some(candidate) = store.selected_chronicle(&session.id)? else {
        return Ok(());
    };
    let path = PathBuf::from(&candidate.log_path);
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            store.set_source_state(
                &session.id,
                "chronicle",
                "stopped",
                Some("Chronicle log is not available."),
                None,
            )?;
            return Ok(());
        }
        Err(error) => {
            return Err(format!(
                "cannot read Chronicle log {}: {error}",
                path.display()
            ))
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect Chronicle log {}: {error}", path.display()))?;
    let file_id = file_identity(&metadata);
    let previous_json = store
        .source_state(&session.id, "chronicle")?
        .and_then(|state| state.cursor_json);
    let previous = previous_json
        .as_deref()
        .and_then(|value| serde_json::from_str::<ChronicleCursor>(value).ok());
    let cursor = previous
        .filter(|cursor| {
            cursor.path == candidate.log_path
                && cursor.offset <= metadata.len()
                && (cursor.file_id.is_none() || cursor.file_id == file_id)
        })
        .unwrap_or(ChronicleCursor {
            path: candidate.log_path.clone(),
            offset: 0,
            file_id,
            last_sequence: 0,
            last_type: None,
        });
    file.seek(SeekFrom::Start(cursor.offset))
        .map_err(|error| format!("cannot seek Chronicle log {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("cannot tail Chronicle log {}: {error}", path.display()))?;
    let observed_at = storage::now();
    let chunk = match parse_chronicle_chunk(
        &bytes,
        &candidate,
        cursor.last_sequence,
        cursor.last_type.as_deref(),
        &observed_at,
    ) {
        Ok(chunk) => chunk,
        Err(error) => {
            let message = format!("Invalid Chronicle log {}: {error}", path.display());
            store.set_source_state(
                &session.id,
                "chronicle",
                "error",
                Some(&message),
                previous_json.as_deref(),
            )?;
            return Err(message);
        }
    };
    if candidate.state == "completed" && chunk.last_type.as_deref() != Some("session_ended") {
        let message = format!(
            "Chronicle marks {} completed, but its log does not end with session_ended",
            candidate.id
        );
        store.set_source_state(
            &session.id,
            "chronicle",
            "error",
            Some(&message),
            previous_json.as_deref(),
        )?;
        return Err(message);
    }
    store.insert_source_events(&session.id, &chunk.events)?;
    let cursor = serde_json::to_string(&ChronicleCursor {
        path: candidate.log_path.clone(),
        offset: cursor.offset + chunk.consumed as u64,
        file_id,
        last_sequence: chunk.last_sequence,
        last_type: chunk.last_type.clone(),
    })
    .map_err(|error| format!("cannot encode Chronicle cursor: {error}"))?;
    set_chronicle_candidate_health(store, &session.id, &candidate, Some(&cursor))
}

pub fn validate_chronicle_root(root: &Path) -> Result<(), String> {
    let registry = root.join("sessions.json");
    let raw = fs::read(&registry).map_err(|error| {
        format!(
            "cannot read Chronicle registry {}: {error}",
            registry.display()
        )
    })?;
    parse_chronicle_registry(&raw).map(|_| ())
}

fn parse_chronicle_registry(raw: &[u8]) -> Result<Vec<ChronicleCandidate>, String> {
    let registry: ChronicleRegistry = serde_json::from_slice(raw)
        .map_err(|error| format!("sessions.json does not match schema 1: {error}"))?;
    if registry.schema_version != 1 {
        return Err(format!(
            "unsupported sessions.json schemaVersion {}",
            registry.schema_version
        ));
    }
    validate_chronicle_timestamp(&registry.updated_at, "updatedAt")?;
    let mut ids = HashSet::new();
    let mut candidates = Vec::with_capacity(registry.sessions.len());
    for entry in registry.sessions {
        validate_nonempty(&entry.id, "session id")?;
        if !ids.insert(entry.id.clone()) {
            return Err(format!("duplicate Chronicle session id {}", entry.id));
        }
        if !matches!(entry.state.as_str(), "active" | "completed" | "interrupted") {
            return Err(format!("invalid Chronicle session state {}", entry.state));
        }
        validate_absolute_path(&entry.log_path, "logPath")?;
        validate_absolute_path(&entry.project_root, "projectRoot")?;
        validate_nonempty(&entry.project_name, "projectName")?;
        validate_chronicle_timestamp(&entry.started_at, "startedAt")?;
        validate_chronicle_timestamp(&entry.last_event_at, "lastEventAt")?;
        validate_chronicle_timestamp(&entry.heartbeat_at, "heartbeatAt")?;
        if let Some(ended_at) = &entry.ended_at {
            validate_chronicle_timestamp(ended_at, "endedAt")?;
        }
        if entry.state == "active" && entry.ended_at.is_some() {
            return Err(format!(
                "active Chronicle session {} has an endedAt timestamp",
                entry.id
            ));
        }
        validate_nonempty(&entry.ide.product, "ide.product")?;
        validate_nonempty(&entry.ide.version, "ide.version")?;
        if entry.pid == 0 {
            return Err(format!("Chronicle session {} has an invalid pid", entry.id));
        }
        for repository in &entry.repositories {
            validate_absolute_path(&repository.root, "repositories[].root")?;
        }
        candidates.push(ChronicleCandidate {
            id: entry.id,
            state: entry.state,
            log_path: entry.log_path,
            project_name: entry.project_name,
            project_root: entry.project_root,
            repositories: entry.repositories,
            started_at: entry.started_at,
            last_event_at: entry.last_event_at,
            ended_at: entry.ended_at,
        });
    }
    Ok(candidates)
}

fn match_chronicle_candidates(
    candidates: Vec<ChronicleCandidate>,
    repo: &Path,
    session_start: &str,
    session_end: Option<&str>,
) -> Vec<ChronicleCandidate> {
    let repo = canonical_path(repo);
    let mut matches = candidates
        .into_iter()
        .filter(|candidate| {
            candidate
                .repositories
                .iter()
                .any(|repository| canonical_path(Path::new(&repository.root)) == repo)
        })
        .collect::<Vec<_>>();
    if matches.iter().any(|candidate| candidate.state == "active") {
        matches.retain(|candidate| candidate.state == "active");
    }
    if matches
        .iter()
        .any(|candidate| chronicle_overlaps(candidate, session_start, session_end))
    {
        matches.retain(|candidate| chronicle_overlaps(candidate, session_start, session_end));
    }
    matches.sort_by(|left, right| right.started_at.cmp(&left.started_at));
    matches
}

fn value_identity(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Object(value) => value.get("id").and_then(value_identity),
        _ => None,
    }
}

fn chronicle_overlaps(
    candidate: &ChronicleCandidate,
    session_start: &str,
    session_end: Option<&str>,
) -> bool {
    let Ok(candidate_start) = DateTime::parse_from_rfc3339(&candidate.started_at) else {
        return false;
    };
    let candidate_end = candidate
        .ended_at
        .as_deref()
        .unwrap_or(&candidate.last_event_at);
    let Ok(candidate_end) = DateTime::parse_from_rfc3339(candidate_end) else {
        return false;
    };
    let Ok(session_start) = DateTime::parse_from_rfc3339(session_start) else {
        return false;
    };
    let session_end = session_end
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .unwrap_or_else(|| Utc::now().fixed_offset());
    candidate_start <= session_end && candidate_end >= session_start
}

fn canonical_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn set_chronicle_candidate_health(
    store: &Store,
    session_id: &str,
    candidate: &ChronicleCandidate,
    cursor: Option<&str>,
) -> Result<(), String> {
    let (status, detail) = match candidate.state.as_str() {
        "active" => ("live", "Chronicle detected"),
        "completed" => ("ended", "Chronicle recording completed"),
        "interrupted" => ("stopped", "Chronicle recording was interrupted"),
        _ => ("error", "Chronicle reported an unknown state"),
    };
    store.set_source_state(session_id, "chronicle", status, Some(detail), cursor)
}

fn parse_chronicle_chunk(
    bytes: &[u8],
    candidate: &ChronicleCandidate,
    previous_sequence: u64,
    previous_type: Option<&str>,
    observed_at: &str,
) -> Result<ParsedChronicleChunk, String> {
    let mut consumed = 0;
    let mut last_sequence = previous_sequence;
    let mut last_type = previous_type.map(str::to_string);
    let mut events = Vec::new();
    let mut ids = HashSet::new();
    while let Some(relative_end) = bytes[consumed..].iter().position(|byte| *byte == b'\n') {
        let end = consumed + relative_end;
        let raw_line = bytes[consumed..end]
            .strip_suffix(b"\r")
            .unwrap_or(&bytes[consumed..end]);
        consumed = end + 1;
        if raw_line.is_empty() {
            return Err(format!(
                "empty JSONL record at sequence {}",
                last_sequence + 1
            ));
        }
        if last_type.as_deref() == Some("session_ended") {
            return Err("session_ended is not the final Chronicle record".to_string());
        }
        let envelope: ChronicleEnvelope = serde_json::from_slice(raw_line).map_err(|error| {
            format!(
                "malformed complete JSONL record at sequence {}: {error}",
                last_sequence + 1
            )
        })?;
        validate_chronicle_envelope(&envelope, candidate, last_sequence + 1)?;
        if !ids.insert(envelope.id.clone()) {
            return Err(format!("duplicate Chronicle event id {}", envelope.id));
        }
        last_sequence = envelope.sequence;
        last_type = Some(envelope.kind.clone());
        let mut payload = Map::new();
        payload.insert(
            "recordedAt".to_string(),
            Value::String(envelope.recorded_at),
        );
        payload.insert("data".to_string(), envelope.data);
        if envelope.redacted == Some(true) {
            payload.insert("redacted".to_string(), Value::Bool(true));
            payload.insert(
                "contentTrust".to_string(),
                Value::String("untrusted; read the referenced file".to_string()),
            );
        } else {
            payload.insert(
                "contentTrust".to_string(),
                Value::String("trusted".to_string()),
            );
        }
        events.push(NormalizedEvent {
            stable_id: envelope.id,
            source: "chronicle".to_string(),
            stream_id: Some(candidate.id.clone()),
            source_sequence: Some(envelope.sequence),
            occurred_at: envelope.occurred_at,
            observed_at: observed_at.to_string(),
            kind: envelope.kind,
            payload: Value::Object(payload),
        });
    }
    if last_type.as_deref() == Some("session_ended") && consumed < bytes.len() {
        return Err("session_ended is not the final Chronicle record".to_string());
    }
    events.sort_by(|left, right| {
        left.occurred_at
            .cmp(&right.occurred_at)
            .then_with(|| left.source_sequence.cmp(&right.source_sequence))
    });
    Ok(ParsedChronicleChunk {
        events,
        consumed,
        last_sequence,
        last_type,
    })
}

fn validate_chronicle_envelope(
    envelope: &ChronicleEnvelope,
    candidate: &ChronicleCandidate,
    expected_sequence: u64,
) -> Result<(), String> {
    if envelope.schema_version != 1 {
        return Err(format!(
            "unsupported event schemaVersion {} at sequence {}",
            envelope.schema_version, envelope.sequence
        ));
    }
    validate_nonempty(&envelope.id, "event id")?;
    if envelope.session_id != candidate.id {
        return Err(format!(
            "event sessionId {} does not match {}",
            envelope.session_id, candidate.id
        ));
    }
    if envelope.sequence != expected_sequence {
        return Err(format!(
            "expected Chronicle sequence {expected_sequence}, found {}",
            envelope.sequence
        ));
    }
    if envelope.sequence == 1 && envelope.kind != "session_started" {
        return Err("Chronicle sequence 1 must be session_started".to_string());
    }
    if envelope.sequence != 1 && envelope.kind == "session_started" {
        return Err("session_started must be Chronicle sequence 1".to_string());
    }
    if envelope.redacted.is_some_and(|redacted| !redacted) {
        return Err("redacted may only be present with the value true".to_string());
    }
    validate_chronicle_timestamp(&envelope.occurred_at, "occurredAt")?;
    validate_chronicle_timestamp(&envelope.recorded_at, "recordedAt")?;
    validate_chronicle_event_data(&envelope.kind, &envelope.data, candidate)
}

fn validate_chronicle_event_data(
    kind: &str,
    data: &Value,
    candidate: &ChronicleCandidate,
) -> Result<(), String> {
    let object = data
        .as_object()
        .ok_or_else(|| format!("{kind}.data must be an object"))?;
    match kind {
        "session_started" => {
            validate_keys(
                object,
                &["projectName", "projectRoot", "repositories", "ide", "pid"],
                &[],
                kind,
            )?;
            require_string(object, "projectName", kind)?;
            let root = require_string(object, "projectRoot", kind)?;
            validate_absolute_path(root, "session_started.projectRoot")?;
            if root != candidate.project_root {
                return Err("session_started.projectRoot does not match sessions.json".to_string());
            }
            if require_string(object, "projectName", kind)? != candidate.project_name {
                return Err("session_started.projectName does not match sessions.json".to_string());
            }
            let repositories = object
                .get("repositories")
                .and_then(Value::as_array)
                .ok_or_else(|| "session_started.repositories must be an array".to_string())?;
            for repository in repositories {
                let repository = repository
                    .as_object()
                    .ok_or_else(|| "session_started repository must be an object".to_string())?;
                validate_keys(
                    repository,
                    &["root", "branch"],
                    &[],
                    "session_started repository",
                )?;
                validate_absolute_path(
                    require_string(repository, "root", "session_started repository")?,
                    "session_started.repositories[].root",
                )?;
                require_nullable_string(repository, "branch", "session_started repository")?;
            }
            let ide = object
                .get("ide")
                .and_then(Value::as_object)
                .ok_or_else(|| "session_started.ide must be an object".to_string())?;
            validate_keys(ide, &["product", "version"], &[], "session_started.ide")?;
            require_string(ide, "product", "session_started.ide")?;
            require_string(ide, "version", "session_started.ide")?;
            require_u64(object, "pid", kind)?;
        }
        "session_ended" => {
            validate_keys(object, &["reason", "state"], &[], kind)?;
            let reason = require_string(object, "reason", kind)?;
            if !matches!(reason, "stopped" | "shutdown" | "restarted" | "error") {
                return Err(format!("invalid session_ended reason {reason}"));
            }
            let state = require_string(object, "state", kind)?;
            if !matches!(state, "completed" | "interrupted") {
                return Err(format!("invalid session_ended state {state}"));
            }
            if candidate.state != "active" && state != candidate.state {
                return Err("session_ended state does not match sessions.json".to_string());
            }
        }
        "file_opened" | "file_closed" | "file_created" | "file_deleted" => {
            validate_keys(object, &["path"], &[], kind)?;
            validate_event_path(require_string(object, "path", kind)?, candidate)?;
        }
        "file_selected" => {
            validate_keys(object, &["path"], &["previousPath"], kind)?;
            validate_event_path(require_string(object, "path", kind)?, candidate)?;
            if let Some(path) = optional_string(object, "previousPath", kind)? {
                validate_event_path(path, candidate)?;
            }
        }
        "file_renamed" | "file_moved" => {
            validate_keys(object, &["oldPath", "newPath"], &[], kind)?;
            validate_event_path(require_string(object, "oldPath", kind)?, candidate)?;
            validate_event_path(require_string(object, "newPath", kind)?, candidate)?;
        }
        "selection" => {
            validate_keys(object, &["path", "startLine", "endLine"], &["text"], kind)?;
            validate_event_path(require_string(object, "path", kind)?, candidate)?;
            let start = require_u64(object, "startLine", kind)?;
            let end = require_u64(object, "endLine", kind)?;
            if start > end {
                return Err("selection.startLine must not exceed endLine".to_string());
            }
            optional_string(object, "text", kind)?;
        }
        "visible_area" => {
            validate_keys(object, &["path", "startLine", "endLine"], &[], kind)?;
            validate_event_path(require_string(object, "path", kind)?, candidate)?;
            let start = require_u64(object, "startLine", kind)?;
            let end = require_u64(object, "endLine", kind)?;
            if start > end {
                return Err("visible_area.startLine must not exceed endLine".to_string());
            }
        }
        "document_changed" => {
            validate_keys(object, &["path", "lineCount"], &[], kind)?;
            validate_event_path(require_string(object, "path", kind)?, candidate)?;
            require_u64(object, "lineCount", kind)?;
        }
        "branch_changed" => {
            validate_keys(object, &["repository", "state"], &["branch"], kind)?;
            validate_absolute_path(
                require_string(object, "repository", kind)?,
                "branch_changed.repository",
            )?;
            optional_string(object, "branch", kind)?;
            require_string(object, "state", kind)?;
        }
        "search" => {
            validate_keys(object, &["query"], &[], kind)?;
            require_string(object, "query", kind)?;
        }
        "refactoring" => {
            validate_keys(object, &["refactoringType", "details"], &[], kind)?;
            require_string(object, "refactoringType", kind)?;
            require_string(object, "details", kind)?;
        }
        "refactoring_undo" => {
            validate_keys(object, &["refactoringType"], &[], kind)?;
            require_string(object, "refactoringType", kind)?;
        }
        "shell_command" => {
            validate_keys(object, &["command", "shell"], &["workingDirectory"], kind)?;
            require_string(object, "command", kind)?;
            require_string(object, "shell", kind)?;
            if let Some(path) = optional_string(object, "workingDirectory", kind)? {
                validate_event_path(path, candidate)?;
            }
        }
        "audio_transcription" => {
            return Err("audio_transcription is never valid in Chronicle Scribe mode".to_string());
        }
        _ => return Err(format!("unknown Chronicle event type {kind}")),
    }
    Ok(())
}

fn validate_keys(
    object: &Map<String, Value>,
    required: &[&str],
    optional: &[&str],
    context: &str,
) -> Result<(), String> {
    for key in required {
        if !object.contains_key(*key) {
            return Err(format!("{context}.data is missing {key}"));
        }
    }
    for key in object.keys() {
        if !required.contains(&key.as_str()) && !optional.contains(&key.as_str()) {
            return Err(format!("{context}.data contains unknown field {key}"));
        }
    }
    Ok(())
}

fn require_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a str, String> {
    let value = object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{context}.data.{key} must be a string"))?;
    validate_nonempty(value, &format!("{context}.data.{key}"))?;
    Ok(value)
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<Option<&'a str>, String> {
    object
        .get(key)
        .map(|_| require_string(object, key, context))
        .transpose()
}

fn require_nullable_string(
    object: &Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<(), String> {
    match object.get(key) {
        Some(Value::Null) => Ok(()),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(()),
        _ => Err(format!("{context}.{key} must be a string or null")),
    }
}

fn require_u64(object: &Map<String, Value>, key: &str, context: &str) -> Result<u64, String> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{context}.data.{key} must be a non-negative integer"))
}

fn validate_nonempty(value: &str, name: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{name} must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_absolute_path(value: &str, name: &str) -> Result<(), String> {
    validate_nonempty(value, name)?;
    if !Path::new(value).is_absolute() {
        return Err(format!("{name} must be absolute: {value}"));
    }
    Ok(())
}

fn validate_event_path(value: &str, candidate: &ChronicleCandidate) -> Result<(), String> {
    validate_nonempty(value, "event path")?;
    let path = Path::new(value);
    let project_root = Path::new(&candidate.project_root);
    if path.is_absolute() {
        if path.starts_with(project_root) {
            return Err(format!(
                "internal Chronicle path must be projectRoot-relative: {value}"
            ));
        }
        return Ok(());
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(format!(
            "relative Chronicle path escapes projectRoot: {value}"
        ));
    }
    Ok(())
}

fn validate_chronicle_timestamp(value: &str, name: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    let fixed = bytes.len() == 24
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'T')
        && bytes.get(13) == Some(&b':')
        && bytes.get(16) == Some(&b':')
        && bytes.get(19) == Some(&b'.')
        && bytes.get(23) == Some(&b'Z')
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        });
    if !fixed || DateTime::parse_from_rfc3339(value).is_err() {
        return Err(format!(
            "{name} must be UTC with exactly millisecond precision: {value}"
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.ino())
}

#[cfg(not(unix))]
fn file_identity(_metadata: &fs::Metadata) -> Option<u64> {
    None
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn output_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).trim().to_string();
    if text.is_empty() {
        "unknown error".to_string()
    } else {
        text
    }
}

fn tuple_launch_error(path: &Path, error: std::io::Error) -> String {
    format!(
        "cannot run Tuple CLI at {}: {error}. Install it from Tuple Settings → Integrations → CLI Server",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    const REGISTRY_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/chronicle/sessions.json");
    const LOG_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/chronicle/session.jsonl");

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = env::temp_dir().join(format!("scribe-source-test-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn tuple_speech_aligns_to_spoken_start_time() {
        let batch = parse_tuple_records(
            br#"{"type":"transcription_finished","time":"2026-09-01T12:00:09Z","data":{"start":"2026-09-01T12:00:01Z","user_id":"u1","text":"Hello"}}
{"type":"user_audio_started","time":"2026-09-01T12:00:00Z","data":{}}
"#,
        );
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].kind, "speech");
        assert_eq!(batch.events[0].occurred_at, "2026-09-01T12:00:01.000Z");
        assert_eq!(batch.events[0].payload["text"], "Hello");
        assert_eq!(batch.status.map(|status| status.0), Some("live"));
    }

    #[test]
    fn tuple_call_end_is_terminal_but_recording_end_is_not() {
        let recording = parse_tuple_records(
            br#"{"type":"recording_ended","time":"2026-09-01T12:00:09Z","data":{}}
"#,
        );
        assert!(!recording.call_ended);
        assert_eq!(recording.status.map(|status| status.0), Some("stopped"));
        let ended = parse_tuple_records(
            br#"{"kind":"status","status":"call_ended"}
"#,
        );
        assert!(ended.call_ended);
        assert_eq!(ended.events[0].kind, "call_ended");
    }

    #[cfg(unix)]
    fn tuple_mock(test: &TestDirectory, body: &str) -> TupleClient {
        use std::os::unix::fs::PermissionsExt;

        let executable = test.0.join("tuple-mock");
        fs::write(&executable, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        TupleClient::new(executable)
    }

    #[cfg(unix)]
    #[test]
    fn tuple_cli_mock_catches_up_and_recognizes_call_end() {
        let test = TestDirectory::new();
        let store = Store::open_at(test.0.join("scribe")).unwrap();
        let recorded_args = test.0.join("tuple-args");
        let tuple = tuple_mock(
            &test,
            &format!(
                r#"
if [ "$1" = "call" ]; then
  printf '%s\n' '{{"id":"call-mock"}}'
  exit 0
fi
printf '%s\n' "$*" > '{}'
printf '%s\n' \
  '{{"type":"transcription_finished","time":"2026-09-01T12:00:09.000Z","data":{{"start":"2026-09-01T12:00:01.000Z","user_id":"u1","text":"Hello"}}}}' \
  '{{"kind":"status","status":"call_ended"}}'
"#,
                recorded_args.display()
            ),
        );
        collect_once(&store, &tuple, "1ms").unwrap();
        let args = fs::read_to_string(recorded_args).unwrap();
        for expected in [
            "transcription show call-mock",
            "--wait",
            "--with-events",
            "--cursor scribe-call-mock",
            "--format json",
        ] {
            assert!(
                args.contains(expected),
                "missing Tuple CLI option: {expected}"
            );
        }
        let session = store.current_session().unwrap().unwrap();
        assert_eq!(session.id, "call-mock");
        assert_eq!(session.state, SessionState::Finalizing);
        let tick = store.tick(&session.id, "tuple-mock-test", 10).unwrap();
        let mut kinds = tick
            .events
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>();
        kinds.sort_unstable();
        assert_eq!(
            kinds,
            ["call_ended", "speech"],
            "the backlog and terminal status must both be imported"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tuple_cli_mock_reports_transcription_off_without_starting_it() {
        let test = TestDirectory::new();
        let store = Store::open_at(test.0.join("scribe")).unwrap();
        let tuple = tuple_mock(
            &test,
            r#"
if [ "$1" = "call" ]; then
  printf '%s\n' '{"id":"call-mock"}'
  exit 0
fi
echo 'transcription is not running' >&2
exit 1
"#,
        );
        collect_once(&store, &tuple, "1ms").unwrap();
        let health = store.source_health("call-mock").unwrap();
        let tuple_health = health
            .iter()
            .find(|source| source.source == "tuple")
            .unwrap();
        assert_eq!(tuple_health.status, "waiting");
        assert!(tuple_health
            .detail
            .as_deref()
            .unwrap()
            .contains("start it in Tuple"));

        store
            .set_source_state(
                "call-mock",
                "tuple",
                "live",
                Some("Transcription is live."),
                None,
            )
            .unwrap();
        let session = store.current_session().unwrap().unwrap();
        tuple.collect(&store, &session, "1ms").unwrap();
        let health = store.source_health("call-mock").unwrap();
        let tuple_health = health
            .iter()
            .find(|source| source.source == "tuple")
            .unwrap();
        assert_eq!(tuple_health.status, "stopped");
        assert!(tuple_health
            .detail
            .as_deref()
            .unwrap()
            .contains("stopped during the call"));
    }

    fn fixture_candidate() -> ChronicleCandidate {
        parse_chronicle_registry(REGISTRY_FIXTURE)
            .unwrap()
            .remove(0)
    }

    fn mutate_log_line(index: usize, update: impl FnOnce(&mut Value)) -> Vec<u8> {
        let mut lines = LOG_FIXTURE
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(Vec::from)
            .collect::<Vec<_>>();
        let mut value: Value = serde_json::from_slice(&lines[index]).unwrap();
        update(&mut value);
        lines[index] = serde_json::to_vec(&value).unwrap();
        let mut result = lines.join(&b'\n');
        result.push(b'\n');
        result
    }

    #[test]
    fn chronicle_registry_fixture_matches_schema_one() {
        let candidates = parse_chronicle_registry(REGISTRY_FIXTURE).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].state, "completed");
        assert_eq!(
            candidates[0].repositories[0].branch.as_deref(),
            Some("main")
        );

        let mut invalid: Value = serde_json::from_slice(REGISTRY_FIXTURE).unwrap();
        invalid["schemaVersion"] = Value::from(2);
        assert!(
            parse_chronicle_registry(&serde_json::to_vec(&invalid).unwrap())
                .unwrap_err()
                .contains("schemaVersion")
        );
    }

    #[test]
    fn chronicle_log_fixture_validates_all_types_and_sorts_by_occurrence() {
        let chunk = parse_chronicle_chunk(
            LOG_FIXTURE,
            &fixture_candidate(),
            0,
            None,
            "2026-09-01T12:01:00.000Z",
        )
        .unwrap();
        assert_eq!(chunk.events.len(), 18);
        assert_eq!(chunk.last_sequence, 18);
        assert_eq!(chunk.last_type.as_deref(), Some("session_ended"));
        assert_eq!(chunk.consumed, LOG_FIXTURE.len());
        assert!(chunk
            .events
            .windows(2)
            .all(|events| { events[0].occurred_at <= events[1].occurred_at }));
        let shell = chunk
            .events
            .iter()
            .position(|event| event.kind == "shell_command")
            .unwrap();
        let selection = chunk
            .events
            .iter()
            .position(|event| event.kind == "selection")
            .unwrap();
        assert!(
            shell < selection,
            "late shell history must merge by occurredAt"
        );
        let redacted = &chunk.events[selection];
        assert_eq!(redacted.payload["redacted"], true);
        assert_eq!(
            redacted.payload["contentTrust"],
            "untrusted; read the referenced file"
        );
        assert_eq!(
            redacted.stream_id.as_deref(),
            Some(fixture_candidate().id.as_str())
        );
        assert_eq!(redacted.source_sequence, Some(9));
    }

    #[test]
    fn chronicle_tolerates_only_an_incomplete_final_line() {
        let first_two = LOG_FIXTURE
            .split_inclusive(|byte| *byte == b'\n')
            .take(2)
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        let mut truncated = first_two.clone();
        truncated.extend_from_slice(br#"{"schemaVersion":1"#);
        let chunk = parse_chronicle_chunk(
            &truncated,
            &fixture_candidate(),
            0,
            None,
            "2026-09-01T12:01:00.000Z",
        )
        .unwrap();
        assert_eq!(chunk.events.len(), 2);
        assert_eq!(chunk.consumed, first_two.len());

        let mut malformed_complete = first_two;
        malformed_complete.extend_from_slice(b"not-json\n");
        assert!(parse_chronicle_chunk(
            &malformed_complete,
            &fixture_candidate(),
            0,
            None,
            "2026-09-01T12:01:00.000Z",
        )
        .unwrap_err()
        .contains("malformed complete"));
    }

    #[test]
    fn chronicle_rejects_contract_violations() {
        let cases = [
            mutate_log_line(1, |line| line["sequence"] = Value::from(3)),
            mutate_log_line(1, |line| line["sessionId"] = Value::from("wrong")),
            mutate_log_line(1, |line| line["schemaVersion"] = Value::from(2)),
            mutate_log_line(1, |line| {
                line["occurredAt"] = Value::from("2026-09-01T11:46:00Z")
            }),
            mutate_log_line(1, |line| line["redacted"] = Value::from(false)),
            mutate_log_line(1, |line| {
                line["type"] = Value::from("audio_transcription");
                line["data"] = serde_json::json!({"transcriptionText": "never"});
            }),
            mutate_log_line(1, |line| line["data"]["path"] = Value::from("../secret")),
            mutate_log_line(1, |line| {
                line["data"]["path"] = Value::from("/Users/chris/Code/scribe/src/App.kt")
            }),
        ];
        for invalid in cases {
            assert!(parse_chronicle_chunk(
                &invalid,
                &fixture_candidate(),
                0,
                None,
                "2026-09-01T12:01:00.000Z",
            )
            .is_err());
        }
    }

    #[test]
    fn chronicle_matching_checks_every_repo_then_active_and_overlap() {
        let test = TestDirectory::new();
        let repo = fs::canonicalize(&test.0).unwrap();
        let mut completed = fixture_candidate();
        completed.id = "completed".to_string();
        completed.repositories = vec![ChronicleRepository {
            root: repo.to_string_lossy().into_owned(),
            branch: None,
        }];
        let mut active = completed.clone();
        active.id = "active".to_string();
        active.state = "active".to_string();
        active.ended_at = None;
        let matches = match_chronicle_candidates(
            vec![completed, active.clone()],
            &repo,
            "2026-09-01T11:50:00.000Z",
            Some("2026-09-01T12:10:00.000Z"),
        );
        assert_eq!(
            matches
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["active"]
        );

        let mut also_active = active.clone();
        also_active.id = "also-active".to_string();
        let ambiguous = match_chronicle_candidates(
            vec![active, also_active],
            &repo,
            "2026-09-01T11:50:00.000Z",
            Some("2026-09-01T12:10:00.000Z"),
        );
        assert_eq!(
            ambiguous.len(),
            2,
            "equally good matches must remain ambiguous"
        );
    }

    #[test]
    fn chronicle_tail_recovers_replacement_without_duplicate_imports() {
        let test = TestDirectory::new();
        let store = Store::open_at(test.0.join("scribe")).unwrap();
        let session = store.create_or_resume_session("call-1").unwrap();
        let log = test.0.join("chronicle.jsonl");
        let records = LOG_FIXTURE
            .split_inclusive(|byte| *byte == b'\n')
            .collect::<Vec<_>>();
        fs::write(&log, [records[0], records[1]].concat()).unwrap();
        let mut candidate = fixture_candidate();
        candidate.state = "active".to_string();
        candidate.ended_at = None;
        candidate.log_path = log.to_string_lossy().into_owned();
        store
            .replace_chronicle_candidates(&session.id, &[candidate.clone()])
            .unwrap();
        collect_chronicle(&store, &session).unwrap();
        assert_eq!(
            store.tick(&session.id, "first", 10).unwrap().events.len(),
            2
        );

        let replacement = test.0.join("replacement.jsonl");
        fs::write(&replacement, [records[0], records[1], records[2]].concat()).unwrap();
        fs::rename(&replacement, &log).unwrap();
        collect_chronicle(&store, &session).unwrap();
        let tick = store.tick(&session.id, "first", 10).unwrap();
        assert_eq!(tick.events.len(), 1);
        assert_eq!(tick.events[0].source_sequence, Some(3));
    }
}
