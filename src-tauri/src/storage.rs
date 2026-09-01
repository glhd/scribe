use std::{
    env, fs,
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::model::{
    AppMode, AppSnapshot, ChatMessage, ChronicleCandidate, DecisionStatus, DocumentReference,
    FileReference, MessageKind, NormalizedEvent, SessionState, SessionSummary, SourceHealth,
    TickResult,
};

const SCHEMA_VERSION: i64 = 3;
const RETAIN_TERMINAL_SESSIONS: usize = 5;

const MIGRATION_1: &str = r#"
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    state TEXT NOT NULL CHECK (state IN ('active', 'finalizing', 'complete', 'interrupted')),
    started_at TEXT NOT NULL,
    call_ended_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL,
    repo_path TEXT,
    notes_path TEXT NOT NULL UNIQUE,
    saved_hash TEXT,
    saved_at TEXT,
    saved_destination TEXT,
    data_pruned INTEGER NOT NULL DEFAULT 0 CHECK (data_pruned IN (0, 1))
);

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE source_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    stable_id TEXT NOT NULL,
    source TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    UNIQUE (session_id, source, stable_id)
);
CREATE INDEX source_events_consumer_order
    ON source_events(session_id, sequence);
CREATE INDEX source_events_chronology
    ON source_events(session_id, occurred_at, sequence);

CREATE TABLE source_state (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    source TEXT NOT NULL,
    status TEXT NOT NULL,
    detail TEXT,
    cursor_json TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (session_id, source)
);

CREATE TABLE chat_messages (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('message', 'ack', 'decision')),
    timestamp TEXT NOT NULL,
    text TEXT NOT NULL,
    reference_json TEXT,
    read INTEGER NOT NULL CHECK (read IN (0, 1)),
    decision_status TEXT CHECK (decision_status IN ('unreviewed', 'approved', 'rejected')),
    UNIQUE (session_id, id)
);

CREATE TABLE file_references (
    session_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    path TEXT NOT NULL,
    line INTEGER,
    end_line INTEGER,
    sha TEXT NOT NULL,
    PRIMARY KEY (session_id, message_id, position),
    FOREIGN KEY (session_id, message_id)
        REFERENCES chat_messages(session_id, id) ON DELETE CASCADE
);

CREATE TABLE decision_reviews (
    session_id TEXT NOT NULL,
    decision_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('approved', 'rejected')),
    reviewed_at TEXT NOT NULL,
    PRIMARY KEY (session_id, decision_id),
    FOREIGN KEY (session_id, decision_id)
        REFERENCES chat_messages(session_id, id) ON DELETE CASCADE
);

CREATE TABLE consumer_cursors (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    consumer TEXT NOT NULL,
    sequence INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (session_id, consumer)
);

CREATE TABLE chronicle_candidates (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    id TEXT NOT NULL,
    log_path TEXT NOT NULL,
    repository TEXT NOT NULL,
    started_at TEXT NOT NULL,
    active INTEGER NOT NULL CHECK (active IN (0, 1)),
    selected INTEGER NOT NULL DEFAULT 0 CHECK (selected IN (0, 1)),
    PRIMARY KEY (session_id, id)
);
"#;

const MIGRATION_2: &str = r#"
ALTER TABLE source_events ADD COLUMN stream_id TEXT;
ALTER TABLE source_events ADD COLUMN source_sequence INTEGER;
CREATE UNIQUE INDEX source_events_stream_sequence
    ON source_events(session_id, source, stream_id, source_sequence)
    WHERE stream_id IS NOT NULL AND source_sequence IS NOT NULL;

DROP TABLE chronicle_candidates;
CREATE TABLE chronicle_candidates (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    id TEXT NOT NULL,
    candidate_json TEXT NOT NULL,
    selected INTEGER NOT NULL DEFAULT 0 CHECK (selected IN (0, 1)),
    PRIMARY KEY (session_id, id)
);
"#;

const MIGRATION_3: &str = r#"
CREATE TABLE consumer_deliveries (
    session_id TEXT NOT NULL,
    consumer TEXT NOT NULL,
    event_sequence INTEGER NOT NULL REFERENCES source_events(sequence) ON DELETE CASCADE,
    delivered_at TEXT NOT NULL,
    PRIMARY KEY (session_id, consumer, event_sequence),
    FOREIGN KEY (session_id, consumer)
        REFERENCES consumer_cursors(session_id, consumer) ON DELETE CASCADE
);
CREATE INDEX consumer_deliveries_lookup
    ON consumer_deliveries(session_id, consumer, event_sequence);
"#;

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
    database: PathBuf,
}

#[derive(Clone, Debug)]
pub struct SessionRecord {
    pub id: String,
    pub state: SessionState,
    pub started_at: String,
    pub repo: Option<PathBuf>,
    pub notes: PathBuf,
    pub saved_hash: Option<String>,
}

#[derive(Clone, Debug)]
pub struct StoredSourceState {
    pub status: String,
    pub cursor_json: Option<String>,
}

impl Store {
    pub fn open() -> Result<Self, String> {
        let root = match env::var_os("SCRIBE_HOME") {
            Some(path) => PathBuf::from(path),
            None => home_dir()?.join(".scribe"),
        };
        Self::open_at(root)
    }

    pub fn open_at(root: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(root.join("sessions"))
            .and_then(|_| fs::create_dir_all(root.join("locks")))
            .and_then(|_| fs::create_dir_all(root.join("bin")))
            .map_err(|error| {
                format!(
                    "cannot create Scribe storage at {}: {error}",
                    root.display()
                )
            })?;
        let store = Self {
            database: root.join("scribe.db"),
            root,
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn chronicle_root(&self) -> Result<PathBuf, String> {
        if let Some(path) = self
            .connection()?
            .query_row(
                "SELECT value FROM settings WHERE key = 'chronicle_root'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(db_error)?
        {
            return Ok(PathBuf::from(path));
        }
        if let Some(path) = env::var_os("CHRONICLE_HOME") {
            return Ok(PathBuf::from(path));
        }
        Ok(home_dir()?.join(".chronicle"))
    }

    pub fn set_chronicle_root(&self, root: &Path) -> Result<PathBuf, String> {
        let root = fs::canonicalize(root).map_err(|error| {
            format!(
                "cannot resolve Chronicle folder {}: {error}",
                root.display()
            )
        })?;
        if !root.is_dir() {
            return Err(format!(
                "Chronicle folder is not a directory: {}",
                root.display()
            ));
        }
        self.connection()?
            .execute(
                "INSERT INTO settings (key, value) VALUES ('chronicle_root', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![root.to_string_lossy()],
            )
            .map_err(db_error)?;
        Ok(root)
    }

    pub fn set_tuple_discovery_error(&self, error: Option<&str>) -> Result<(), String> {
        let connection = self.connection()?;
        match error {
            Some(error) => connection
                .execute(
                    "INSERT INTO settings (key, value) VALUES ('tuple_discovery_error', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![error],
                )
                .map(|_| ())
                .map_err(db_error),
            None => connection
                .execute(
                    "DELETE FROM settings WHERE key = 'tuple_discovery_error'",
                    [],
                )
                .map(|_| ())
                .map_err(db_error),
        }
    }

    fn tuple_discovery_error(&self) -> Result<Option<String>, String> {
        self.connection()?
            .query_row(
                "SELECT value FROM settings WHERE key = 'tuple_discovery_error'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)
    }

    pub fn session_end(&self, session_id: &str) -> Result<Option<String>, String> {
        let value = self
            .connection()?
            .query_row(
                "SELECT COALESCE(call_ended_at, finished_at) FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(db_error)?
            .flatten();
        Ok(value)
    }

    pub fn lock_path(&self, source: &str, session_id: &str) -> PathBuf {
        self.root
            .join("locks")
            .join(format!("{source}-{}.lock", short_hash(session_id)))
    }

    fn connection(&self) -> Result<Connection, String> {
        let connection = Connection::open(&self.database)
            .map_err(|error| format!("cannot open {}: {error}", self.database.display()))?;
        connection
            .busy_timeout(Duration::from_secs(10))
            .map_err(db_error)?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;")
            .map_err(db_error)?;
        Ok(connection)
    }

    fn migrate(&self) -> Result<(), String> {
        let mut connection = self.connection()?;
        connection
            .query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .map_err(db_error)?;
        let mut version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(db_error)?;
        if version > SCHEMA_VERSION {
            return Err(format!(
                "{} uses schema version {version}, but this Scribe supports {SCHEMA_VERSION}",
                self.database.display()
            ));
        }
        if version < 1 {
            let transaction = connection.transaction().map_err(db_error)?;
            transaction.execute_batch(MIGRATION_1).map_err(db_error)?;
            transaction
                .execute_batch("PRAGMA user_version = 1")
                .map_err(db_error)?;
            transaction.commit().map_err(db_error)?;
            version = 1;
        }
        if version < 2 {
            let transaction = connection.transaction().map_err(db_error)?;
            transaction.execute_batch(MIGRATION_2).map_err(db_error)?;
            transaction
                .execute_batch("PRAGMA user_version = 2")
                .map_err(db_error)?;
            transaction.commit().map_err(db_error)?;
            version = 2;
        }
        if version < 3 {
            let transaction = connection.transaction().map_err(db_error)?;
            transaction.execute_batch(MIGRATION_3).map_err(db_error)?;
            transaction
                .execute_batch("PRAGMA user_version = 3")
                .map_err(db_error)?;
            transaction.commit().map_err(db_error)?;
        }
        Ok(())
    }

    pub fn create_or_resume_session(&self, call_id: &str) -> Result<SessionRecord, String> {
        if call_id.trim().is_empty() {
            return Err("Tuple returned an empty call ID".to_string());
        }
        let notes = self
            .root
            .join("sessions")
            .join(safe_session_directory(call_id))
            .join("notes.md");
        if let Some(parent) = notes.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        if !notes.exists() {
            fs::write(&notes, "")
                .map_err(|error| format!("cannot create {}: {error}", notes.display()))?;
        }

        let timestamp = now();
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        transaction
            .execute(
                "UPDATE sessions SET state = 'interrupted', finished_at = ?1, updated_at = ?1
                 WHERE state = 'active' AND id <> ?2",
                params![timestamp, call_id],
            )
            .map_err(db_error)?;
        transaction
            .execute(
                "INSERT INTO sessions (id, state, started_at, updated_at, notes_path)
                 VALUES (?1, 'active', ?2, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET
                    updated_at = excluded.updated_at,
                    state = CASE
                        WHEN sessions.state = 'interrupted' THEN 'active'
                        ELSE sessions.state
                    END",
                params![call_id, timestamp, notes.to_string_lossy()],
            )
            .map_err(db_error)?;
        transaction
            .execute(
                "INSERT INTO source_state (session_id, source, status, detail, updated_at)
                 VALUES (?1, 'tuple', 'waiting', 'Call found. Start transcription in Tuple.', ?2)
                 ON CONFLICT(session_id, source) DO NOTHING",
                params![call_id, timestamp],
            )
            .map_err(db_error)?;
        set_setting(&transaction, "selected_session", call_id)?;
        transaction.commit().map_err(db_error)?;
        self.session(call_id)
    }

    pub fn current_session(&self) -> Result<Option<SessionRecord>, String> {
        let connection = self.connection()?;
        query_session(
            &connection,
            "SELECT id, state, started_at, updated_at, repo_path, notes_path, saved_hash, data_pruned
             FROM sessions
             WHERE state IN ('active', 'finalizing')
             ORDER BY CASE state WHEN 'active' THEN 0 ELSE 1 END, updated_at DESC
             LIMIT 1",
            [],
        )
    }

    pub fn selected_session(&self) -> Result<Option<SessionRecord>, String> {
        if let Some(active) = self.current_session()? {
            return Ok(Some(active));
        }
        let connection = self.connection()?;
        let selected = connection
            .query_row(
                "SELECT value FROM settings WHERE key = 'selected_session'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(db_error)?;
        if let Some(id) = selected {
            if let Some(session) = query_session(
                &connection,
                "SELECT id, state, started_at, updated_at, repo_path, notes_path, saved_hash, data_pruned
                 FROM sessions WHERE id = ?1",
                params![id],
            )? {
                return Ok(Some(session));
            }
        }
        Ok(None)
    }

    pub fn clear_terminal_selection_for_launch(&self) -> Result<(), String> {
        self.connection()?
            .execute(
                "DELETE FROM settings
                 WHERE key = 'selected_session'
                   AND value IN (
                     SELECT id FROM sessions WHERE state IN ('complete', 'interrupted')
                   )",
                [],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn session(&self, id: &str) -> Result<SessionRecord, String> {
        let connection = self.connection()?;
        query_session(
            &connection,
            "SELECT id, state, started_at, updated_at, repo_path, notes_path, saved_hash, data_pruned
             FROM sessions WHERE id = ?1",
            params![id],
        )?
        .ok_or_else(|| format!("session not found: {id}"))
    }

    pub fn select_session(&self, id: &str) -> Result<(), String> {
        let connection = self.connection()?;
        let state: Option<String> = connection
            .query_row(
                "SELECT state FROM sessions WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let Some(state) = state else {
            return Err(format!("session not found: {id}"));
        };
        if matches!(state.as_str(), "active" | "finalizing") {
            return Ok(());
        }
        connection
            .execute(
                "INSERT INTO settings (key, value) VALUES ('selected_session', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![id],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn attach_repo(&self, session_id: &str, repo: &Path) -> Result<SessionRecord, String> {
        let repo = git_root(repo)?;
        let timestamp = now();
        let connection = self.connection()?;
        let changed = connection
            .execute(
                "UPDATE sessions SET repo_path = ?1, updated_at = ?2
                 WHERE id = ?3 AND state IN ('active', 'finalizing')",
                params![repo.to_string_lossy(), timestamp, session_id],
            )
            .map_err(db_error)?;
        if changed == 0 {
            return Err(format!("active session not found: {session_id}"));
        }
        self.set_source_state(
            session_id,
            "claude",
            "connected",
            Some(&format!("Attached to {}", repo.display())),
            None,
        )?;
        self.session(session_id)
    }

    pub fn touch_session(&self, session_id: &str) -> Result<(), String> {
        self.connection()?
            .execute(
                "UPDATE sessions SET updated_at = ?1 WHERE id = ?2 AND state = 'active'",
                params![now(), session_id],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn mark_call_ended(&self, session_id: &str) -> Result<(), String> {
        let timestamp = now();
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        transaction
            .execute(
                "UPDATE sessions SET state = 'finalizing', call_ended_at = ?1, updated_at = ?1
                 WHERE id = ?2 AND state = 'active'",
                params![timestamp, session_id],
            )
            .map_err(db_error)?;
        upsert_source_state(
            &transaction,
            session_id,
            "tuple",
            "ended",
            Some("Call ended. Claude is finishing the handoff."),
            None,
            &timestamp,
        )?;
        transaction.commit().map_err(db_error)
    }

    pub fn finish_session(&self, session_id: &str) -> Result<(), String> {
        let timestamp = now();
        let connection = self.connection()?;
        let changed = connection
            .execute(
                "UPDATE sessions SET state = 'complete', finished_at = ?1, updated_at = ?1
                 WHERE id = ?2 AND state = 'finalizing'",
                params![timestamp, session_id],
            )
            .map_err(db_error)?;
        if changed == 0 {
            let state = self.session(session_id)?.state;
            return Err(match state {
                SessionState::Active => {
                    "Tuple call is still active; finish after Scribe reports finalizing".to_string()
                }
                SessionState::Complete => "session is already complete".to_string(),
                SessionState::Interrupted => {
                    "an interrupted session cannot be finished".to_string()
                }
                SessionState::Finalizing => "session could not be finished".to_string(),
            });
        }
        self.prune()
    }

    pub fn interrupt_stale_sessions(&self, age: ChronoDuration) -> Result<usize, String> {
        let cutoff = (Utc::now() - age).to_rfc3339_opts(SecondsFormat::Millis, true);
        let timestamp = now();
        let changed = self
            .connection()?
            .execute(
                "UPDATE sessions SET state = 'interrupted', finished_at = ?1, updated_at = ?1
                 WHERE state = 'active' AND updated_at < ?2",
                params![timestamp, cutoff],
            )
            .map_err(db_error)?;
        if changed > 0 {
            self.prune()?;
        }
        Ok(changed)
    }

    pub fn set_source_state(
        &self,
        session_id: &str,
        source: &str,
        status: &str,
        detail: Option<&str>,
        cursor_json: Option<&str>,
    ) -> Result<(), String> {
        let timestamp = now();
        let connection = self.connection()?;
        upsert_source_state(
            &connection,
            session_id,
            source,
            status,
            detail,
            cursor_json,
            &timestamp,
        )
    }

    pub fn source_state(
        &self,
        session_id: &str,
        source: &str,
    ) -> Result<Option<StoredSourceState>, String> {
        self.connection()?
            .query_row(
                "SELECT status, cursor_json FROM source_state
                 WHERE session_id = ?1 AND source = ?2",
                params![session_id, source],
                |row| {
                    Ok(StoredSourceState {
                        status: row.get(0)?,
                        cursor_json: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(db_error)
    }

    pub fn insert_source_events(
        &self,
        session_id: &str,
        events: &[NormalizedEvent],
    ) -> Result<usize, String> {
        if events.is_empty() {
            return Ok(0);
        }
        let mut events = events.to_vec();
        events.sort_by(|left, right| {
            left.occurred_at
                .cmp(&right.occurred_at)
                .then_with(|| left.stable_id.cmp(&right.stable_id))
        });
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        let mut inserted = 0;
        for event in events {
            inserted += transaction
                .execute(
                    "INSERT OR IGNORE INTO source_events
                     (session_id, stable_id, source, stream_id, source_sequence,
                      occurred_at, observed_at, kind, payload_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        session_id,
                        event.stable_id,
                        event.source,
                        event.stream_id,
                        event
                            .source_sequence
                            .and_then(|value| i64::try_from(value).ok()),
                        event.occurred_at,
                        event.observed_at,
                        event.kind,
                        serde_json::to_string(&event.payload).map_err(json_error)?,
                    ],
                )
                .map_err(db_error)?;
        }
        transaction
            .execute(
                "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                params![now(), session_id],
            )
            .map_err(db_error)?;
        transaction.commit().map_err(db_error)?;
        Ok(inserted)
    }

    pub fn tick(
        &self,
        session_id: &str,
        consumer: &str,
        limit: usize,
    ) -> Result<TickResult, String> {
        if consumer.trim().is_empty() || consumer.chars().any(char::is_whitespace) {
            return Err("cursor name must be non-empty and contain no whitespace".to_string());
        }
        if limit == 0 || limit > 10_000 {
            return Err("tick limit must be between 1 and 10000".to_string());
        }
        let session = self.session(session_id)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        transaction
            .execute(
                "INSERT INTO consumer_cursors (session_id, consumer, sequence, updated_at)
                 VALUES (?1, ?2, 0, ?3)
                 ON CONFLICT(session_id, consumer) DO NOTHING",
                params![session_id, consumer, now()],
            )
            .map_err(db_error)?;
        let mut statement = transaction
            .prepare(
                "SELECT event.sequence, event.stable_id, event.source, event.stream_id,
                        event.source_sequence, event.occurred_at, event.observed_at,
                        event.kind, event.payload_json
                 FROM source_events AS event
                 LEFT JOIN consumer_deliveries AS delivery
                   ON delivery.session_id = event.session_id
                  AND delivery.consumer = ?2
                  AND delivery.event_sequence = event.sequence
                 WHERE event.session_id = ?1 AND delivery.event_sequence IS NULL
                 ORDER BY event.occurred_at, event.sequence LIMIT ?3",
            )
            .map_err(db_error)?;
        let delivered = statement
            .query_map(params![session_id, consumer, limit], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    NormalizedEvent {
                        stable_id: row.get(1)?,
                        source: row.get(2)?,
                        stream_id: row.get(3)?,
                        source_sequence: row
                            .get::<_, Option<i64>>(4)?
                            .and_then(|value| u64::try_from(value).ok()),
                        occurred_at: row.get(5)?,
                        observed_at: row.get(6)?,
                        kind: row.get(7)?,
                        payload: serde_json::from_str::<serde_json::Value>(
                            &row.get::<_, String>(8)?,
                        )
                        .unwrap_or(serde_json::Value::Null),
                    },
                ))
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        drop(statement);
        let timestamp = now();
        for (sequence, _) in &delivered {
            transaction
                .execute(
                    "INSERT INTO consumer_deliveries
                     (session_id, consumer, event_sequence, delivered_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![session_id, consumer, sequence, timestamp],
                )
                .map_err(db_error)?;
        }
        let high_water = delivered
            .iter()
            .map(|(sequence, _)| *sequence)
            .max()
            .unwrap_or(0);
        let has_more = transaction
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM source_events AS event
                    LEFT JOIN consumer_deliveries AS delivery
                      ON delivery.session_id = event.session_id
                     AND delivery.consumer = ?2
                     AND delivery.event_sequence = event.sequence
                    WHERE event.session_id = ?1 AND delivery.event_sequence IS NULL
                 )",
                params![session_id, consumer],
                |row| row.get::<_, bool>(0),
            )
            .map_err(db_error)?;
        if !delivered.is_empty() {
            transaction
                .execute(
                    "UPDATE consumer_cursors
                     SET sequence = MAX(sequence, ?1), updated_at = ?2
                     WHERE session_id = ?3 AND consumer = ?4",
                    params![high_water, timestamp, session_id, consumer],
                )
                .map_err(db_error)?;
        }
        transaction.commit().map_err(db_error)?;
        Ok(TickResult {
            session_id: session.id.clone(),
            session_state: session.state,
            notes_path: session.notes.to_string_lossy().into_owned(),
            repo_path: session.repo.map(|path| path.to_string_lossy().into_owned()),
            source_health: self.source_health(&session.id)?,
            events: delivered.into_iter().map(|(_, event)| event).collect(),
            has_more,
        })
    }

    pub fn append_message(&self, session_id: &str, message: &ChatMessage) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        transaction
            .execute(
                "INSERT INTO chat_messages
                 (session_id, id, kind, timestamp, text, reference_json, read, decision_status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    session_id,
                    message.id,
                    message_kind(message.kind),
                    message.timestamp,
                    message.text,
                    message
                        .reference
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()
                        .map_err(json_error)?,
                    message.read,
                    message.decision_status.map(decision_status),
                ],
            )
            .map_err(|error| {
                if error.to_string().contains("UNIQUE constraint failed") {
                    format!("message ID already exists: {}", message.id)
                } else {
                    db_error(error)
                }
            })?;
        for (position, file) in message.files.iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO file_references
                     (session_id, message_id, position, path, line, end_line, sha)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        session_id,
                        message.id,
                        position,
                        file.path,
                        file.line,
                        file.end_line,
                        file.sha,
                    ],
                )
                .map_err(db_error)?;
        }
        transaction
            .execute(
                "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                params![now(), session_id],
            )
            .map_err(db_error)?;
        transaction.commit().map_err(db_error)
    }

    pub fn unlink(&self, session_id: &str, id: &str) -> Result<(), String> {
        let connection = self.connection()?;
        let reference: Option<Option<String>> = connection
            .query_row(
                "SELECT reference_json FROM chat_messages WHERE session_id = ?1 AND id = ?2",
                params![session_id, id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        match reference {
            None => Err(format!("message not found: {id}")),
            Some(None) => Err(format!("message has no document reference: {id}")),
            Some(Some(_)) => {
                connection
                    .execute(
                        "UPDATE chat_messages SET reference_json = NULL
                         WHERE session_id = ?1 AND id = ?2",
                        params![session_id, id],
                    )
                    .map_err(db_error)?;
                Ok(())
            }
        }
    }

    pub fn mark_cli_read(&self, session_id: &str, id: Option<&str>) -> Result<(), String> {
        let connection = self.connection()?;
        let changed = match id {
            Some(id) => connection.execute(
                "UPDATE chat_messages SET read = 1
                 WHERE session_id = ?1 AND id = ?2 AND kind <> 'ack'",
                params![session_id, id],
            ),
            None => connection.execute(
                "UPDATE chat_messages SET read = 1
                 WHERE session_id = ?1 AND kind <> 'ack'",
                params![session_id],
            ),
        }
        .map_err(db_error)?;
        if id.is_some() && changed == 0 {
            return Err(format!(
                "message not found or already read: {}",
                id.unwrap_or_default()
            ));
        }
        Ok(())
    }

    pub fn mark_read_through(&self, session_id: &str, id: Option<&str>) -> Result<(), String> {
        let connection = self.connection()?;
        let through = match id {
            Some(id) => connection
                .query_row(
                    "SELECT sequence FROM chat_messages WHERE session_id = ?1 AND id = ?2",
                    params![session_id, id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(db_error)?
                .ok_or_else(|| format!("message not found: {id}"))?,
            None => i64::MAX,
        };
        connection
            .execute(
                "UPDATE chat_messages SET read = 1
                 WHERE session_id = ?1 AND sequence <= ?2 AND kind <> 'ack'",
                params![session_id, through],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn review_decision(
        &self,
        session_id: &str,
        id: &str,
        status: DecisionStatus,
    ) -> Result<(), String> {
        if status == DecisionStatus::Unreviewed {
            return Err("a decision can only be approved or rejected".to_string());
        }
        let status_text = decision_status(status);
        let timestamp = now();
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        let current: Option<(String, Option<String>)> = transaction
            .query_row(
                "SELECT kind, decision_status FROM chat_messages
                 WHERE session_id = ?1 AND id = ?2",
                params![session_id, id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(db_error)?;
        let Some((kind, current_status)) = current else {
            return Err(format!("decision not found: {id}"));
        };
        if kind != "decision" {
            return Err(format!("message is not a decision: {id}"));
        }
        match current_status.as_deref() {
            Some("unreviewed") => {
                transaction
                    .execute(
                        "UPDATE chat_messages SET decision_status = ?1
                         WHERE session_id = ?2 AND id = ?3",
                        params![status_text, session_id, id],
                    )
                    .map_err(db_error)?;
                transaction
                    .execute(
                        "INSERT INTO decision_reviews (session_id, decision_id, status, reviewed_at)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![session_id, id, status_text, timestamp],
                    )
                    .map_err(db_error)?;
            }
            Some(existing) if existing == status_text => {}
            _ => return Err(format!("decision has already been reviewed: {id}")),
        }
        let kind = format!("decision_{status_text}");
        insert_event(
            &transaction,
            session_id,
            &NormalizedEvent {
                stable_id: format!("decision-review:{id}"),
                source: "scribe".to_string(),
                stream_id: None,
                source_sequence: None,
                occurred_at: timestamp.clone(),
                observed_at: timestamp,
                kind,
                payload: serde_json::json!({ "decisionId": id, "status": status_text }),
            },
        )?;
        transaction.commit().map_err(db_error)
    }

    pub fn report_stale_reference(
        &self,
        session_id: &str,
        message_id: &str,
        locator: &DocumentReference,
    ) -> Result<(), String> {
        let connection = self.connection()?;
        let reference: String = connection
            .query_row(
                "SELECT reference_json FROM chat_messages
                 WHERE session_id = ?1 AND id = ?2",
                params![session_id, message_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?
            .flatten()
            .ok_or_else(|| format!("message or document reference not found: {message_id}"))?;
        let stored: DocumentReference = serde_json::from_str(&reference).map_err(json_error)?;
        if &stored != locator {
            return Err(format!(
                "document reference no longer matches message: {message_id}"
            ));
        }
        let timestamp = now();
        insert_event(
            &connection,
            session_id,
            &NormalizedEvent {
                stable_id: format!("reference-stale:{message_id}"),
                source: "scribe".to_string(),
                stream_id: None,
                source_sequence: None,
                occurred_at: timestamp.clone(),
                observed_at: timestamp,
                kind: "reference_stale".to_string(),
                payload: serde_json::json!({ "messageId": message_id, "locator": locator }),
            },
        )?;
        Ok(())
    }

    pub fn source_health(&self, session_id: &str) -> Result<Vec<SourceHealth>, String> {
        let session = self.session(session_id)?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT source, status, detail FROM source_state WHERE session_id = ?1")
            .map_err(db_error)?;
        let states = statement
            .query_map(params![session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        let find = |source: &str| states.iter().find(|state| state.0 == source);
        let tuple = find("tuple");
        let chronicle = find("chronicle");
        Ok(vec![
            health(
                "tuple",
                tuple.map(|state| state.1.as_str()).unwrap_or("waiting"),
                tuple.and_then(|state| state.2.clone()),
            ),
            if session.repo.is_some() {
                health(
                    "claude",
                    "connected",
                    Some("planning-scribe attached".to_string()),
                )
            } else {
                health(
                    "claude",
                    "waiting",
                    Some("Waiting for planning-scribe from a repository".to_string()),
                )
            },
            health(
                "chronicle",
                chronicle.map(|state| state.1.as_str()).unwrap_or("off"),
                chronicle.and_then(|state| state.2.clone()),
            ),
        ])
    }

    pub fn snapshot(&self) -> Result<AppSnapshot, String> {
        let session = self.selected_session()?;
        let sessions = self.session_summaries()?;
        let chronicle_root = self.chronicle_root()?;
        let chronicle_registry_found = chronicle_root.join("sessions.json").is_file();
        let Some(session) = session else {
            let tuple_error = self.tuple_discovery_error()?;
            return Ok(AppSnapshot {
                mode: AppMode::WaitingCall,
                session_id: None,
                session_state: None,
                notes_path: None,
                repo_path: None,
                markdown: String::new(),
                messages: Vec::new(),
                sources: vec![
                    health(
                        "tuple",
                        if tuple_error.is_some() {
                            "error"
                        } else {
                            "waiting"
                        },
                        tuple_error.or_else(|| Some("Waiting for a Tuple call…".to_string())),
                    ),
                    health("claude", "waiting", None),
                    health("chronicle", "off", None),
                ],
                sessions,
                chronicle_candidates: Vec::new(),
                chronicle_root: chronicle_root.to_string_lossy().into_owned(),
                chronicle_registry_found,
                integration_installed: self.integration_installed(),
                handoff_saved: false,
            });
        };
        let markdown = fs::read_to_string(&session.notes).map_err(|error| {
            format!(
                "cannot read notes document {}: {error}",
                session.notes.display()
            )
        })?;
        let sources = self.source_health(&session.id)?;
        let tuple_status = sources
            .iter()
            .find(|source| source.source == "tuple")
            .map(|source| source.status.as_str());
        let mode = match session.state {
            SessionState::Active if tuple_status == Some("waiting") => {
                AppMode::WaitingTranscription
            }
            SessionState::Active if session.repo.is_none() => AppMode::WaitingClaude,
            SessionState::Active => AppMode::Active,
            SessionState::Finalizing => AppMode::Finalizing,
            SessionState::Complete => AppMode::Complete,
            SessionState::Interrupted => AppMode::Interrupted,
        };
        let handoff_saved = !markdown.trim().is_empty()
            && session.saved_hash.as_deref() == Some(content_hash(markdown.as_bytes()).as_str());
        Ok(AppSnapshot {
            mode,
            session_id: Some(session.id.clone()),
            session_state: Some(session.state),
            notes_path: Some(session.notes.to_string_lossy().into_owned()),
            repo_path: session.repo.map(|path| path.to_string_lossy().into_owned()),
            markdown,
            messages: self.messages(&session.id)?,
            sources,
            sessions,
            chronicle_candidates: self.chronicle_candidates(&session.id)?,
            chronicle_root: chronicle_root.to_string_lossy().into_owned(),
            chronicle_registry_found,
            integration_installed: self.integration_installed(),
            handoff_saved,
        })
    }

    pub fn export_notes(&self, session_id: &str, destination: &Path) -> Result<(), String> {
        let session = self.session(session_id)?;
        if !matches!(
            session.state,
            SessionState::Complete | SessionState::Interrupted
        ) {
            return Err("Save As is available after the session ends".to_string());
        }
        if !destination.is_absolute() {
            return Err("Save As destination must be an absolute path".to_string());
        }
        if destination.starts_with(&self.root) {
            return Err(
                "Choose a Save As destination outside Scribe's internal storage".to_string(),
            );
        }
        let markdown = fs::read(&session.notes)
            .map_err(|error| format!("cannot read {}: {error}", session.notes.display()))?;
        let parent = destination
            .parent()
            .ok_or_else(|| format!("destination has no parent: {}", destination.display()))?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        let temporary = parent.join(format!(
            ".{}.scribe-save-{}",
            destination
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("notes.md"),
            uuid::Uuid::new_v4()
        ));
        fs::write(&temporary, &markdown)
            .and_then(|_| fs::rename(&temporary, destination))
            .map_err(|error| {
                let _ = fs::remove_file(&temporary);
                format!("cannot save {}: {error}", destination.display())
            })?;
        self.connection()?
            .execute(
                "UPDATE sessions SET saved_hash = ?1, saved_at = ?2, saved_destination = ?3,
                 updated_at = ?2 WHERE id = ?4",
                params![
                    content_hash(&markdown),
                    now(),
                    destination.to_string_lossy(),
                    session_id,
                ],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn delete_session(&self, session_id: &str) -> Result<(), String> {
        let session = self.session(session_id)?;
        if matches!(
            session.state,
            SessionState::Active | SessionState::Finalizing
        ) {
            return Err("active and finalizing sessions cannot be deleted".to_string());
        }
        self.connection()?
            .execute("DELETE FROM sessions WHERE id = ?1", params![session_id])
            .map_err(db_error)?;
        self.remove_internal_notes(&session.notes)
    }

    pub fn replace_chronicle_candidates(
        &self,
        session_id: &str,
        candidates: &[ChronicleCandidate],
    ) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        let selected: Option<String> = transaction
            .query_row(
                "SELECT id FROM chronicle_candidates WHERE session_id = ?1 AND selected = 1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        transaction
            .execute(
                "DELETE FROM chronicle_candidates WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(db_error)?;
        let auto = if candidates.len() == 1 {
            Some(candidates[0].id.as_str())
        } else {
            selected.as_deref()
        };
        for candidate in candidates {
            transaction
                .execute(
                    "INSERT INTO chronicle_candidates
                     (session_id, id, candidate_json, selected)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        session_id,
                        candidate.id,
                        serde_json::to_string(candidate).map_err(json_error)?,
                        auto == Some(candidate.id.as_str()),
                    ],
                )
                .map_err(db_error)?;
        }
        transaction.commit().map_err(db_error)
    }

    pub fn chronicle_candidates(
        &self,
        session_id: &str,
    ) -> Result<Vec<ChronicleCandidate>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT candidate_json FROM chronicle_candidates WHERE session_id = ?1")
            .map_err(db_error)?;
        let raw = statement
            .query_map(params![session_id], |row| row.get::<_, String>(0))
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        let mut candidates = raw
            .into_iter()
            .map(|value| serde_json::from_str(&value).map_err(json_error))
            .collect::<Result<Vec<ChronicleCandidate>, _>>()?;
        candidates.sort_by(|left, right| right.started_at.cmp(&left.started_at));
        Ok(candidates)
    }

    pub fn selected_chronicle(
        &self,
        session_id: &str,
    ) -> Result<Option<ChronicleCandidate>, String> {
        let raw = self
            .connection()?
            .query_row(
                "SELECT candidate_json FROM chronicle_candidates
                 WHERE session_id = ?1 AND selected = 1",
                params![session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(db_error)?;
        raw.map(|value| serde_json::from_str(&value).map_err(json_error))
            .transpose()
    }

    pub fn select_chronicle(&self, session_id: &str, candidate_id: &str) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        transaction
            .execute(
                "UPDATE chronicle_candidates SET selected = 0 WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(db_error)?;
        let changed = transaction
            .execute(
                "UPDATE chronicle_candidates SET selected = 1
                 WHERE session_id = ?1 AND id = ?2",
                params![session_id, candidate_id],
            )
            .map_err(db_error)?;
        if changed == 0 {
            return Err(format!("Chronicle session not found: {candidate_id}"));
        }
        transaction.commit().map_err(db_error)
    }

    pub fn prune(&self) -> Result<(), String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT id, notes_path, saved_hash FROM sessions
                 WHERE state IN ('complete', 'interrupted')
                 ORDER BY COALESCE(finished_at, updated_at) DESC, rowid DESC",
            )
            .map_err(db_error)?;
        let terminal = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    PathBuf::from(row.get::<_, String>(1)?),
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        drop(statement);
        let mut remove = Vec::new();
        let mut connection = connection;
        let transaction = connection.transaction().map_err(db_error)?;
        for (index, (id, notes, saved_hash)) in terminal.into_iter().enumerate() {
            if index < RETAIN_TERMINAL_SESSIONS {
                continue;
            }
            let markdown = fs::read(&notes).unwrap_or_default();
            let unsaved = !markdown.is_empty()
                && saved_hash.as_deref() != Some(content_hash(&markdown).as_str());
            if unsaved {
                for table in [
                    "source_events",
                    "source_state",
                    "consumer_cursors",
                    "decision_reviews",
                    "file_references",
                    "chat_messages",
                    "chronicle_candidates",
                ] {
                    transaction
                        .execute(
                            &format!("DELETE FROM {table} WHERE session_id = ?1"),
                            params![id],
                        )
                        .map_err(db_error)?;
                }
                transaction
                    .execute(
                        "UPDATE sessions SET data_pruned = 1 WHERE id = ?1",
                        params![id],
                    )
                    .map_err(db_error)?;
            } else {
                transaction
                    .execute("DELETE FROM sessions WHERE id = ?1", params![id])
                    .map_err(db_error)?;
                remove.push(notes);
            }
        }
        transaction.commit().map_err(db_error)?;
        for notes in remove {
            self.remove_internal_notes(&notes)?;
        }
        Ok(())
    }

    pub fn make_message(
        &self,
        session: &SessionRecord,
        id: String,
        kind: MessageKind,
        text: String,
        reference: Option<DocumentReference>,
        explicit_files: &[String],
    ) -> Result<ChatMessage, String> {
        if text.trim().is_empty() {
            return Err("message text cannot be empty".to_string());
        }
        let repo = session
            .repo
            .as_ref()
            .ok_or_else(|| "planning-scribe has not attached a repository".to_string())?;
        let explicit_specs: Vec<(String, Option<u32>, Option<u32>)> = explicit_files
            .iter()
            .map(|spec| parse_file_spec(spec))
            .collect::<Result<_, _>>()?;
        let mut specs = Vec::new();
        for spec in explicit_specs.into_iter().chain(inferred_file_specs(&text)) {
            if !specs.contains(&spec) {
                specs.push(spec);
            }
        }
        let files = if specs.is_empty() {
            Vec::new()
        } else {
            let sha = head_sha(repo)?;
            specs
                .into_iter()
                .map(|(path, line, end_line)| FileReference {
                    path,
                    line,
                    end_line,
                    sha: sha.clone(),
                })
                .collect()
        };
        Ok(ChatMessage {
            id,
            kind,
            timestamp: now(),
            text,
            reference,
            files,
            read: kind == MessageKind::Ack,
            decision_status: (kind == MessageKind::Decision).then_some(DecisionStatus::Unreviewed),
        })
    }

    fn messages(&self, session_id: &str) -> Result<Vec<ChatMessage>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT id, kind, timestamp, text, reference_json, read, decision_status
                 FROM chat_messages WHERE session_id = ?1 ORDER BY timestamp, sequence",
            )
            .map_err(db_error)?;
        let raw = statement
            .query_map(params![session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        drop(statement);
        raw.into_iter()
            .map(|(id, kind, timestamp, text, reference, read, status)| {
                Ok(ChatMessage {
                    files: load_files(&connection, session_id, &id)?,
                    id,
                    kind: parse_message_kind(&kind)?,
                    timestamp,
                    text,
                    reference: reference
                        .map(|value| serde_json::from_str(&value).map_err(json_error))
                        .transpose()?,
                    read,
                    decision_status: status
                        .map(|value| parse_decision_status(&value))
                        .transpose()?,
                })
            })
            .collect()
    }

    fn session_summaries(&self) -> Result<Vec<SessionSummary>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT id, state, started_at, updated_at, repo_path, notes_path, saved_hash, data_pruned
                 FROM sessions ORDER BY
                    CASE state WHEN 'active' THEN 0 WHEN 'finalizing' THEN 1 ELSE 2 END,
                    COALESCE(finished_at, updated_at) DESC",
            )
            .map_err(db_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    PathBuf::from(row.get::<_, String>(5)?),
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, bool>(7)?,
                ))
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        rows.into_iter()
            .map(
                |(id, state, started_at, updated_at, repo, notes, saved_hash, data_pruned)| {
                    let markdown = fs::read(&notes).unwrap_or_default();
                    Ok(SessionSummary {
                        id,
                        state: parse_session_state(&state)?,
                        started_at,
                        updated_at,
                        attached_repo: repo,
                        has_unsaved_handoff: !markdown.is_empty()
                            && saved_hash.as_deref() != Some(content_hash(&markdown).as_str()),
                        data_pruned,
                    })
                },
            )
            .collect()
    }

    fn remove_internal_notes(&self, notes: &Path) -> Result<(), String> {
        let sessions_root = self.root.join("sessions");
        if !notes.starts_with(&sessions_root) {
            return Err(format!(
                "refusing to delete non-Scribe path: {}",
                notes.display()
            ));
        }
        if let Some(directory) = notes.parent() {
            if directory != sessions_root {
                match fs::remove_dir_all(directory) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!("cannot delete {}: {error}", directory.display()))
                    }
                }
            }
        }
        Ok(())
    }

    fn integration_installed(&self) -> bool {
        self.root.join("bin/scribe").exists()
            && home_dir()
                .map(|home| {
                    home.join(".claude/skills/planning-scribe/SKILL.md")
                        .is_file()
                })
                .unwrap_or(false)
    }
}

fn query_session<P: rusqlite::Params>(
    connection: &Connection,
    sql: &str,
    params: P,
) -> Result<Option<SessionRecord>, String> {
    let raw = connection
        .query_row(sql, params, |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, bool>(7)?,
            ))
        })
        .optional()
        .map_err(db_error)?;
    raw.map(
        |(id, state, started_at, _updated_at, repo, notes, saved_hash, _data_pruned)| {
            Ok(SessionRecord {
                id,
                state: parse_session_state(&state)?,
                started_at,
                repo: repo.map(PathBuf::from),
                notes: PathBuf::from(notes),
                saved_hash,
            })
        },
    )
    .transpose()
}

fn upsert_source_state(
    connection: &Connection,
    session_id: &str,
    source: &str,
    status: &str,
    detail: Option<&str>,
    cursor_json: Option<&str>,
    timestamp: &str,
) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO source_state (session_id, source, status, detail, cursor_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id, source) DO UPDATE SET
                status = excluded.status,
                detail = excluded.detail,
                cursor_json = COALESCE(excluded.cursor_json, source_state.cursor_json),
                updated_at = excluded.updated_at",
            params![session_id, source, status, detail, cursor_json, timestamp],
        )
        .map_err(db_error)?;
    Ok(())
}

fn insert_event(
    connection: &Connection,
    session_id: &str,
    event: &NormalizedEvent,
) -> Result<usize, String> {
    connection
        .execute(
            "INSERT OR IGNORE INTO source_events
             (session_id, stable_id, source, stream_id, source_sequence,
              occurred_at, observed_at, kind, payload_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                session_id,
                event.stable_id,
                event.source,
                event.stream_id,
                event
                    .source_sequence
                    .and_then(|value| i64::try_from(value).ok()),
                event.occurred_at,
                event.observed_at,
                event.kind,
                serde_json::to_string(&event.payload).map_err(json_error)?,
            ],
        )
        .map_err(db_error)
}

fn set_setting(transaction: &Transaction<'_>, key: &str, value: &str) -> Result<(), String> {
    transaction
        .execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(db_error)?;
    Ok(())
}

fn load_files(
    connection: &Connection,
    session_id: &str,
    message_id: &str,
) -> Result<Vec<FileReference>, String> {
    let mut statement = connection
        .prepare(
            "SELECT path, line, end_line, sha FROM file_references
             WHERE session_id = ?1 AND message_id = ?2 ORDER BY position",
        )
        .map_err(db_error)?;
    let files = statement
        .query_map(params![session_id, message_id], |row| {
            Ok(FileReference {
                path: row.get(0)?,
                line: row.get(1)?,
                end_line: row.get(2)?,
                sha: row.get(3)?,
            })
        })
        .map_err(db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_error)?;
    Ok(files)
}

fn health(source: &str, status: &str, detail: Option<String>) -> SourceHealth {
    let label = match status {
        "live" => "Live",
        "connected" => "Connected",
        "waiting" => "Waiting",
        "stopped" => "Stopped",
        "ended" => "Ended",
        "ambiguous" => "Choose source",
        "error" => "Needs attention",
        _ => "Off",
    };
    SourceHealth {
        source: source.to_string(),
        status: status.to_string(),
        label: label.to_string(),
        detail,
    }
}

pub fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn normalize_timestamp(value: &serde_json::Value, fallback: &str) -> String {
    if let Some(value) = value.as_str() {
        if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
            return timestamp
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Millis, true);
        }
        if let Ok(number) = value.parse::<f64>() {
            return timestamp_from_number(number).unwrap_or_else(|| fallback.to_string());
        }
    }
    value
        .as_f64()
        .and_then(timestamp_from_number)
        .unwrap_or_else(|| fallback.to_string())
}

fn timestamp_from_number(value: f64) -> Option<String> {
    let milliseconds = if value.abs() < 100_000_000_000.0 {
        value * 1000.0
    } else {
        value
    };
    DateTime::<Utc>::from_timestamp_millis(milliseconds.round() as i64)
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
}

pub fn stable_hash(value: &[u8]) -> String {
    content_hash(value)
}

fn content_hash(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn short_hash(value: &str) -> String {
    content_hash(value.as_bytes())[..16].to_string()
}

fn safe_session_directory(call_id: &str) -> String {
    if call_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        call_id.to_string()
    } else {
        format!("call-{}", short_hash(call_id))
    }
}

fn home_dir() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "cannot locate the user home directory".to_string())
}

pub fn git_root(start: &Path) -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args([
            "-C",
            &start.to_string_lossy(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{} is not inside a Git repository",
            start.display()
        ));
    }
    let root = String::from_utf8(output.stdout)
        .map_err(|_| "git returned a non-UTF-8 repository path".to_string())?;
    fs::canonicalize(root.trim())
        .map_err(|error| format!("cannot resolve repository {}: {error}", root.trim()))
}

fn head_sha(repo: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .args([
            "-C",
            &repo.to_string_lossy(),
            "rev-parse",
            "--verify",
            "HEAD",
        ])
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !output.status.success() {
        return Err("cannot resolve Git HEAD for file references".to_string());
    }
    let sha = String::from_utf8(output.stdout)
        .map_err(|_| "git returned a non-UTF-8 commit SHA".to_string())?
        .trim()
        .to_string();
    if sha.len() < 7 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("git returned an invalid commit SHA".to_string());
    }
    Ok(sha)
}

pub fn parse_file_spec(spec: &str) -> Result<(String, Option<u32>, Option<u32>), String> {
    let spec = spec.trim();
    if spec.is_empty() || spec.contains("://") {
        return Err(format!("invalid file reference: {spec}"));
    }
    let (path, line, end_line) = match spec.rsplit_once(':') {
        Some((path, suffix)) if suffix.as_bytes().first().is_some_and(u8::is_ascii_digit) => {
            let (line, end_line) = match suffix.split_once('-') {
                Some((start, end)) => (parse_line(start, spec)?, Some(parse_line(end, spec)?)),
                None => (parse_line(suffix, spec)?, None),
            };
            if end_line.is_some_and(|end| end < line) {
                return Err(format!("file reference has a backwards line range: {spec}"));
            }
            (path, Some(line), end_line)
        }
        _ => (spec, None, None),
    };
    let path = path.trim_start_matches("./").replace('\\', "/");
    let parsed = Path::new(&path);
    if path.is_empty()
        || parsed.is_absolute()
        || path.contains(':')
        || parsed
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "file references must be repository-relative paths: {spec}"
        ));
    }
    Ok((path, line, end_line))
}

fn parse_line(value: &str, spec: &str) -> Result<u32, String> {
    let line = value
        .parse::<u32>()
        .map_err(|_| format!("invalid line number in file reference: {spec}"))?;
    if line == 0 {
        return Err(format!("line numbers start at 1: {spec}"));
    }
    Ok(line)
}

fn inferred_file_specs(text: &str) -> Vec<(String, Option<u32>, Option<u32>)> {
    let mut result = Vec::new();
    let mut remaining = text;
    while let Some(open) = remaining.find('`') {
        remaining = &remaining[open + 1..];
        let Some(close) = remaining.find('`') else {
            break;
        };
        let candidate = &remaining[..close];
        if (candidate.contains('/') || candidate.contains('\\'))
            && !candidate.contains(char::is_whitespace)
        {
            if let Ok(spec) = parse_file_spec(candidate) {
                if !result.contains(&spec) {
                    result.push(spec);
                }
            }
        }
        remaining = &remaining[close + 1..];
    }
    result
}

fn message_kind(kind: MessageKind) -> &'static str {
    match kind {
        MessageKind::Message => "message",
        MessageKind::Ack => "ack",
        MessageKind::Decision => "decision",
    }
}

fn decision_status(status: DecisionStatus) -> &'static str {
    match status {
        DecisionStatus::Unreviewed => "unreviewed",
        DecisionStatus::Approved => "approved",
        DecisionStatus::Rejected => "rejected",
    }
}

fn parse_message_kind(value: &str) -> Result<MessageKind, String> {
    match value {
        "message" => Ok(MessageKind::Message),
        "ack" => Ok(MessageKind::Ack),
        "decision" => Ok(MessageKind::Decision),
        _ => Err(format!("database contains invalid message kind: {value}")),
    }
}

fn parse_decision_status(value: &str) -> Result<DecisionStatus, String> {
    match value {
        "unreviewed" => Ok(DecisionStatus::Unreviewed),
        "approved" => Ok(DecisionStatus::Approved),
        "rejected" => Ok(DecisionStatus::Rejected),
        _ => Err(format!(
            "database contains invalid decision status: {value}"
        )),
    }
}

fn parse_session_state(value: &str) -> Result<SessionState, String> {
    match value {
        "active" => Ok(SessionState::Active),
        "finalizing" => Ok(SessionState::Finalizing),
        "complete" => Ok(SessionState::Complete),
        "interrupted" => Ok(SessionState::Interrupted),
        _ => Err(format!("database contains invalid session state: {value}")),
    }
}

fn db_error(error: rusqlite::Error) -> String {
    format!("Scribe database error: {error}")
}

fn json_error(error: serde_json::Error) -> String {
    format!("cannot encode Scribe data: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, Barrier},
        thread,
    };
    use uuid::Uuid;

    struct TestStore {
        store: Store,
        path: PathBuf,
    }

    impl TestStore {
        fn new() -> Self {
            let path = env::temp_dir().join(format!("scribe-test-{}", Uuid::new_v4()));
            let store = Store::open_at(path.clone()).unwrap();
            Self { store, path }
        }
    }

    impl Drop for TestStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn event(id: &str, occurred_at: &str) -> NormalizedEvent {
        NormalizedEvent {
            stable_id: id.to_string(),
            source: "tuple".to_string(),
            stream_id: None,
            source_sequence: None,
            occurred_at: occurred_at.to_string(),
            observed_at: "2026-09-01T12:10:00.000Z".to_string(),
            kind: "speech".to_string(),
            payload: serde_json::json!({ "text": id }),
        }
    }

    #[test]
    fn migrates_database_and_enables_wal() {
        let test = TestStore::new();
        let connection = test.store.connection().unwrap();
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        assert_eq!(
            connection
                .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
    }

    #[test]
    fn migrates_schema_one_data_to_current_schema() {
        let path = env::temp_dir().join(format!("scribe-v1-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        let notes = path.join("legacy-notes.md");
        fs::write(&notes, "# Legacy\n").unwrap();
        let connection = Connection::open(path.join("scribe.db")).unwrap();
        connection.execute_batch(MIGRATION_1).unwrap();
        connection
            .execute(
                "INSERT INTO sessions (id, state, started_at, updated_at, notes_path)
                 VALUES ('legacy-call', 'active', '2026-09-01T12:00:00.000Z',
                         '2026-09-01T12:00:00.000Z', ?1)",
                params![notes.to_string_lossy()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO source_events
                 (session_id, stable_id, source, occurred_at, observed_at, kind, payload_json)
                 VALUES ('legacy-call', 'legacy-event', 'tuple',
                         '2026-09-01T12:00:00.000Z', '2026-09-01T12:00:00.000Z',
                         'speech', '{}')",
                [],
            )
            .unwrap();
        connection.execute_batch("PRAGMA user_version = 1").unwrap();
        drop(connection);

        let store = Store::open_at(path.clone()).unwrap();
        let migrated = store.tick("legacy-call", "migration-test", 10).unwrap();
        assert_eq!(migrated.events.len(), 1);
        assert_eq!(migrated.events[0].stable_id, "legacy-event");
        assert_eq!(migrated.events[0].stream_id, None);
        assert_eq!(
            store
                .connection()
                .unwrap()
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn persisted_chronicle_root_overrides_automatic_discovery() {
        let test = TestStore::new();
        let chosen = test.path.join("chosen-chronicle");
        fs::create_dir_all(&chosen).unwrap();
        let canonical = fs::canonicalize(&chosen).unwrap();
        assert_eq!(test.store.set_chronicle_root(&chosen).unwrap(), canonical);
        assert_eq!(test.store.chronicle_root().unwrap(), canonical);
    }

    #[test]
    fn concurrent_connections_do_not_lose_messages() {
        let test = TestStore::new();
        let session = test.store.create_or_resume_session("call-1").unwrap();
        let repo = env::current_dir().unwrap();
        test.store.attach_repo(&session.id, &repo).unwrap();
        let handles = (0..8)
            .map(|index| {
                let store = test.store.clone();
                let session_id = session.id.clone();
                thread::spawn(move || {
                    store
                        .append_message(
                            &session_id,
                            &ChatMessage {
                                id: format!("message-{index}"),
                                kind: MessageKind::Message,
                                timestamp: format!("2026-09-01T12:00:{index:02}.000Z"),
                                text: format!("message {index}"),
                                reference: None,
                                files: Vec::new(),
                                read: false,
                                decision_status: None,
                            },
                        )
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(test.store.messages(&session.id).unwrap().len(), 8);
    }

    #[test]
    fn cursor_delivery_is_durable_ordered_and_deduplicated() {
        let test = TestStore::new();
        let session = test.store.create_or_resume_session("call-1").unwrap();
        let late = event("late", "2026-09-01T12:00:03.000Z");
        let early = event("early", "2026-09-01T12:00:01.000Z");
        assert_eq!(
            test.store
                .insert_source_events(&session.id, &[late.clone(), late])
                .unwrap(),
            1
        );
        assert_eq!(
            test.store
                .insert_source_events(&session.id, &[early.clone(), early])
                .unwrap(),
            1
        );
        let first = test.store.tick(&session.id, "planning-scribe", 1).unwrap();
        assert_eq!(first.events[0].stable_id, "early");
        assert!(first.has_more);
        let second = test.store.tick(&session.id, "planning-scribe", 10).unwrap();
        assert_eq!(second.events[0].stable_id, "late");
        assert!(!second.has_more);
        assert!(test
            .store
            .tick(&session.id, "planning-scribe", 10)
            .unwrap()
            .events
            .is_empty());

        // A source record that arrives late is still delivered exactly once on
        // the next tick with its original occurrence timestamp.
        let very_early = event("very-early", "2026-09-01T11:59:00.000Z");
        test.store
            .insert_source_events(&session.id, &[very_early])
            .unwrap();
        assert_eq!(
            test.store
                .tick(&session.id, "planning-scribe", 10)
                .unwrap()
                .events[0]
                .stable_id,
            "very-early"
        );
    }

    #[test]
    fn concurrent_ticks_for_one_consumer_deliver_each_event_once() {
        let test = TestStore::new();
        let session = test.store.create_or_resume_session("call-1").unwrap();
        test.store
            .insert_source_events(
                &session.id,
                &[
                    event("first", "2026-09-01T12:00:01.000Z"),
                    event("second", "2026-09-01T12:00:02.000Z"),
                ],
            )
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let handles = (0..2)
            .map(|_| {
                let barrier = barrier.clone();
                let store = test.store.clone();
                let session_id = session.id.clone();
                thread::spawn(move || {
                    barrier.wait();
                    store
                        .tick(&session_id, "planning-scribe", 1)
                        .unwrap()
                        .events[0]
                        .stable_id
                        .clone()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let mut delivered = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        delivered.sort();
        assert_eq!(delivered, ["first", "second"]);
    }

    #[test]
    fn lifecycle_requires_call_end_before_finish() {
        let test = TestStore::new();
        let session = test.store.create_or_resume_session("call-1").unwrap();
        assert!(test.store.finish_session(&session.id).is_err());
        test.store.mark_call_ended(&session.id).unwrap();
        assert_eq!(
            test.store.session(&session.id).unwrap().state,
            SessionState::Finalizing
        );
        test.store.finish_session(&session.id).unwrap();
        assert_eq!(
            test.store.session(&session.id).unwrap().state,
            SessionState::Complete
        );
    }

    #[test]
    fn stale_active_session_becomes_interrupted_on_restart_cleanup() {
        let test = TestStore::new();
        let session = test.store.create_or_resume_session("stale-call").unwrap();
        test.store
            .connection()
            .unwrap()
            .execute(
                "UPDATE sessions SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1",
                params![session.id],
            )
            .unwrap();
        assert_eq!(
            test.store
                .interrupt_stale_sessions(ChronoDuration::hours(12))
                .unwrap(),
            1
        );
        assert_eq!(
            test.store.session("stale-call").unwrap().state,
            SessionState::Interrupted
        );
    }

    #[test]
    fn save_hash_detects_edits_and_delete_never_removes_export() {
        let test = TestStore::new();
        let session = test.store.create_or_resume_session("saved-call").unwrap();
        fs::write(&session.notes, "# Ready\n").unwrap();
        test.store.mark_call_ended(&session.id).unwrap();
        test.store.finish_session(&session.id).unwrap();
        assert!(test
            .store
            .export_notes(&session.id, &test.path.join("plan.md"))
            .is_err());
        let export_root = env::temp_dir().join(format!("scribe-export-test-{}", Uuid::new_v4()));
        let destination = export_root.join("plan.md");
        test.store.export_notes(&session.id, &destination).unwrap();
        assert!(test.store.snapshot().unwrap().handoff_saved);

        fs::write(&session.notes, "# Ready\n\nOne more detail.\n").unwrap();
        assert!(!test.store.snapshot().unwrap().handoff_saved);
        test.store.delete_session(&session.id).unwrap();
        assert!(!session.notes.exists());
        assert_eq!(fs::read_to_string(&destination).unwrap(), "# Ready\n");
        fs::remove_dir_all(export_root).unwrap();
    }

    #[test]
    fn relaunch_returns_to_waiting_while_history_remains_available() {
        let test = TestStore::new();
        let session = test
            .store
            .create_or_resume_session("previous-call")
            .unwrap();
        test.store.mark_call_ended(&session.id).unwrap();
        test.store.finish_session(&session.id).unwrap();
        assert_eq!(
            test.store.snapshot().unwrap().session_id.as_deref(),
            Some("previous-call")
        );

        test.store.clear_terminal_selection_for_launch().unwrap();
        let relaunched = test.store.snapshot().unwrap();
        assert_eq!(relaunched.mode, AppMode::WaitingCall);
        assert_eq!(relaunched.session_id, None);
        assert_eq!(relaunched.sessions.len(), 1);
    }

    #[test]
    fn retention_keeps_five_full_sessions_and_protects_unsaved_handoffs() {
        let test = TestStore::new();
        for index in 0..7 {
            let id = format!("call-{index}");
            let session = test.store.create_or_resume_session(&id).unwrap();
            fs::write(&session.notes, format!("# Plan {index}\n")).unwrap();
            test.store
                .insert_source_events(
                    &id,
                    &[event(&format!("event-{index}"), "2026-09-01T12:00:00.000Z")],
                )
                .unwrap();
            test.store.mark_call_ended(&id).unwrap();
            test.store.finish_session(&id).unwrap();
        }
        let summaries = test.store.session_summaries().unwrap();
        assert_eq!(summaries.len(), 7);
        assert_eq!(summaries.iter().filter(|item| item.data_pruned).count(), 2);
        let oldest = test.store.session("call-0").unwrap();
        assert!(oldest.notes.exists());
        assert!(test
            .store
            .tick("call-0", "debug", 10)
            .unwrap()
            .events
            .is_empty());
    }

    #[test]
    fn parses_file_lines_and_ranges() {
        assert_eq!(
            parse_file_spec("app/Jobs/Sync.php:14-20").unwrap(),
            ("app/Jobs/Sync.php".to_string(), Some(14), Some(20))
        );
        assert!(parse_file_spec("../outside.php:2").is_err());
        assert!(parse_file_spec("app/Foo.php:20-14").is_err());
    }

    #[test]
    fn speech_timestamp_accepts_epoch_seconds() {
        assert_eq!(
            normalize_timestamp(
                &serde_json::json!(1_788_264_000.125),
                "2026-09-01T12:00:01.000Z"
            ),
            "2026-09-01T12:00:00.125Z"
        );
    }
}
