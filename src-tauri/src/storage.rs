use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Component, Path, PathBuf},
    process::Command,
};

use chrono::{SecondsFormat, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{
    AppSnapshot, ChatMessage, DecisionEvent, DecisionStatus, DocumentReference, FileReference,
    MessageKind, StaleReferenceEvent,
};

const CONFIG_FILE: &str = ".scribe.json";

#[derive(Clone, Debug)]
pub struct SessionPaths {
    pub notes: PathBuf,
    pub chat: PathBuf,
    pub events: PathBuf,
    pub repo: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    #[serde(default = "default_base_path")]
    base_path: PathBuf,
    call: Option<String>,
    document: Option<PathBuf>,
}

fn default_base_path() -> PathBuf {
    PathBuf::from("docs")
}

pub fn resolve_session() -> Result<SessionPaths, String> {
    let cwd =
        env::current_dir().map_err(|error| format!("cannot read current directory: {error}"))?;

    if let Some(notes) = env::var_os("SCRIBE_NOTES") {
        let notes = absolute_from(&cwd, Path::new(&notes));
        return session_from_notes(notes);
    }

    let config_path = match env::var_os("SCRIBE_CONFIG") {
        Some(path) => absolute_from(&cwd, Path::new(&path)),
        None => find_config(&cwd).ok_or_else(|| {
            format!(
                "no {CONFIG_FILE} found; create one with {{\"call\":\"<call-name>\"}} or set SCRIBE_NOTES"
            )
        })?,
    };
    let raw = fs::read_to_string(&config_path)
        .map_err(|error| format!("cannot read {}: {error}", config_path.display()))?;
    let config: Config = serde_json::from_str(&raw)
        .map_err(|error| format!("invalid {}: {error}", config_path.display()))?;
    let config_dir = config_path.parent().unwrap_or(Path::new("."));

    let configured_path = if let Some(document) = config.document {
        document
    } else {
        let call = config.call.ok_or_else(|| {
            format!(
                "{} must contain either \"call\" or \"document\"",
                config_path.display()
            )
        })?;
        validate_call_name(&call)?;
        config.base_path.join(format!("{call}.md"))
    };

    session_from_notes(absolute_from(config_dir, &configured_path))
}

fn validate_call_name(call: &str) -> Result<(), String> {
    if call.trim().is_empty()
        || Path::new(call).components().count() != 1
        || call == "."
        || call == ".."
    {
        return Err("config \"call\" must be a non-empty file stem, not a path".to_string());
    }
    Ok(())
}

fn find_config(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|directory| directory.join(CONFIG_FILE))
        .find(|path| path.is_file())
}

fn absolute_from(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn session_from_notes(notes: PathBuf) -> Result<SessionPaths, String> {
    if notes.extension().and_then(|extension| extension.to_str()) != Some("md") {
        return Err(format!(
            "notes document must end in .md: {}",
            notes.display()
        ));
    }
    let parent = notes
        .parent()
        .ok_or_else(|| format!("notes document has no parent: {}", notes.display()))?;
    let stem = notes
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| {
            format!(
                "notes document has an invalid file name: {}",
                notes.display()
            )
        })?;
    let repo = git_root(parent)?;

    Ok(SessionPaths {
        notes: notes.clone(),
        chat: parent.join(format!("{stem}.chat.jsonl")),
        events: parent.join(format!("{stem}.events.jsonl")),
        repo,
    })
}

fn git_root(start: &Path) -> Result<PathBuf, String> {
    let existing = start
        .ancestors()
        .find(|path| path.exists())
        .ok_or_else(|| format!("no existing parent for {}", start.display()))?;
    let output = Command::new("git")
        .args([
            "-C",
            &existing.to_string_lossy(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{} is not inside a git repository",
            start.display()
        ));
    }
    let root = String::from_utf8(output.stdout)
        .map_err(|_| "git returned a non-UTF-8 repository path".to_string())?;
    Ok(PathBuf::from(root.trim()))
}

pub fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn head_sha(session: &SessionPaths) -> Result<String, String> {
    let output = Command::new("git")
        .args([
            "-C",
            &session.repo.to_string_lossy(),
            "rev-parse",
            "--verify",
            "HEAD",
        ])
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !output.status.success() {
        return Err("cannot resolve git HEAD for file references".to_string());
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

pub fn load_messages(path: &Path) -> Result<Vec<ChatMessage>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };

    BufReader::new(file)
        .lines()
        .enumerate()
        .filter_map(|(index, line)| match line {
            Ok(line) if line.trim().is_empty() => None,
            result => Some((index, result)),
        })
        .map(|(index, line)| {
            let line = line.map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            serde_json::from_str(&line).map_err(|error| {
                format!(
                    "invalid JSON in {} at line {}: {error}",
                    path.display(),
                    index + 1
                )
            })
        })
        .collect()
}

pub fn snapshot(session: &SessionPaths) -> Result<AppSnapshot, String> {
    let markdown = match fs::read_to_string(&session.notes) {
        Ok(markdown) => markdown,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(format!(
                "cannot read notes document {}: {error}",
                session.notes.display()
            ))
        }
    };

    Ok(AppSnapshot {
        notes_path: session.notes.to_string_lossy().into_owned(),
        repo_path: session.repo.to_string_lossy().into_owned(),
        markdown,
        messages: load_messages(&session.chat)?,
    })
}

pub fn append_message(session: &SessionPaths, message: &ChatMessage) -> Result<(), String> {
    ensure_parent(&session.chat)?;
    let lock = lock_for(&session.chat)?;
    let messages = load_messages(&session.chat)?;
    if messages.iter().any(|existing| existing.id == message.id) {
        return Err(format!("message ID already exists: {}", message.id));
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&session.chat)
        .map_err(|error| format!("cannot open {}: {error}", session.chat.display()))?;
    serde_json::to_writer(&mut file, message)
        .map_err(|error| format!("cannot encode chat message: {error}"))?;
    file.write_all(b"\n")
        .and_then(|_| file.sync_data())
        .map_err(|error| format!("cannot write {}: {error}", session.chat.display()))?;
    drop(lock);
    Ok(())
}

pub fn unlink(session: &SessionPaths, id: &str) -> Result<(), String> {
    update_messages(session, |messages| {
        let message = messages
            .iter_mut()
            .find(|message| message.id == id)
            .ok_or_else(|| format!("message not found: {id}"))?;
        if message.reference.take().is_none() {
            return Err(format!("message has no document reference: {id}"));
        }
        Ok(())
    })
}

pub fn mark_cli_read(session: &SessionPaths, id: Option<&str>) -> Result<(), String> {
    update_messages(session, |messages| {
        if let Some(id) = id {
            let message = messages
                .iter_mut()
                .find(|message| message.id == id)
                .ok_or_else(|| format!("message not found: {id}"))?;
            if !matches!(message.kind, MessageKind::Ack) {
                message.read = true;
            }
        } else {
            for message in messages {
                if !matches!(message.kind, MessageKind::Ack) {
                    message.read = true;
                }
            }
        }
        Ok(())
    })
}

pub fn mark_read_through(session: &SessionPaths, id: Option<&str>) -> Result<(), String> {
    update_messages(session, |messages| {
        let through = match id {
            Some(id) => messages
                .iter()
                .position(|message| message.id == id)
                .ok_or_else(|| format!("message not found: {id}"))?,
            None => messages.len().saturating_sub(1),
        };
        for message in messages.iter_mut().take(through + 1) {
            if !matches!(message.kind, MessageKind::Ack) {
                message.read = true;
            }
        }
        Ok(())
    })
}

pub fn review_decision(
    session: &SessionPaths,
    id: &str,
    status: DecisionStatus,
) -> Result<(), String> {
    if matches!(status, DecisionStatus::Unreviewed) {
        return Err("a decision can only be approved or rejected".to_string());
    }
    update_messages(session, |messages| {
        let message = messages
            .iter_mut()
            .find(|message| message.id == id)
            .ok_or_else(|| format!("decision not found: {id}"))?;
        if !matches!(message.kind, MessageKind::Decision) {
            return Err(format!("message is not a decision: {id}"));
        }
        match message.decision_status.as_ref() {
            Some(DecisionStatus::Unreviewed) => {
                message.decision_status = Some(status.clone());
            }
            Some(existing) if existing == &status => {}
            _ => return Err(format!("decision has already been reviewed: {id}")),
        }
        Ok(())
    })?;

    let timestamp = now();
    let event_type = match status {
        DecisionStatus::Approved => "decision_approved",
        DecisionStatus::Rejected => "decision_rejected",
        DecisionStatus::Unreviewed => unreachable!(),
    };
    append_event_once(
        session,
        &DecisionEvent {
            timestamp: &timestamp,
            event_type,
            decision_id: id,
        },
        event_type,
        "decisionId",
        id,
    )
}

pub fn report_stale_reference(
    session: &SessionPaths,
    message_id: &str,
    locator: &DocumentReference,
) -> Result<(), String> {
    let messages = load_messages(&session.chat)?;
    let message = messages
        .iter()
        .find(|message| message.id == message_id)
        .ok_or_else(|| format!("message not found: {message_id}"))?;
    if message.reference.as_ref() != Some(locator) {
        return Err(format!(
            "document reference no longer matches message: {message_id}"
        ));
    }

    let timestamp = now();
    append_event_once(
        session,
        &StaleReferenceEvent {
            timestamp: &timestamp,
            event_type: "reference_stale",
            message_id,
            locator,
        },
        "reference_stale",
        "messageId",
        message_id,
    )
}

fn append_event_once<T: Serialize>(
    session: &SessionPaths,
    event: &T,
    event_type: &str,
    identity_field: &str,
    identity_value: &str,
) -> Result<(), String> {
    ensure_parent(&session.events)?;
    let lock = lock_for(&session.events)?;
    if File::open(&session.events)
        .ok()
        .into_iter()
        .flat_map(|file| BufReader::new(file).lines().map_while(Result::ok))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
        .any(|existing| {
            existing.get("type").and_then(|value| value.as_str()) == Some(event_type)
                && existing
                    .get(identity_field)
                    .and_then(|value| value.as_str())
                    == Some(identity_value)
        })
    {
        drop(lock);
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&session.events)
        .map_err(|error| format!("cannot open {}: {error}", session.events.display()))?;
    serde_json::to_writer(&mut file, event)
        .map_err(|error| format!("cannot encode app event: {error}"))?;
    file.write_all(b"\n")
        .and_then(|_| file.sync_data())
        .map_err(|error| format!("cannot write {}: {error}", session.events.display()))?;
    drop(lock);
    Ok(())
}

fn update_messages<F>(session: &SessionPaths, mutate: F) -> Result<(), String>
where
    F: FnOnce(&mut Vec<ChatMessage>) -> Result<(), String>,
{
    ensure_parent(&session.chat)?;
    let lock = lock_for(&session.chat)?;
    let mut messages = load_messages(&session.chat)?;
    mutate(&mut messages)?;

    let temporary = session
        .chat
        .with_extension(format!("jsonl.tmp-{}", Uuid::new_v4()));
    let write_result = (|| {
        let file = File::create(&temporary)
            .map_err(|error| format!("cannot create {}: {error}", temporary.display()))?;
        let mut writer = BufWriter::new(file);
        for message in &messages {
            serde_json::to_writer(&mut writer, message)
                .map_err(|error| format!("cannot encode chat message: {error}"))?;
            writer
                .write_all(b"\n")
                .map_err(|error| format!("cannot write {}: {error}", temporary.display()))?;
        }
        writer
            .flush()
            .map_err(|error| format!("cannot flush {}: {error}", temporary.display()))?;
        writer
            .get_ref()
            .sync_all()
            .map_err(|error| format!("cannot sync {}: {error}", temporary.display()))?;
        fs::rename(&temporary, &session.chat).map_err(|error| {
            format!(
                "cannot replace {} with updated chat log: {error}",
                session.chat.display()
            )
        })?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    drop(lock);
    write_result
}

fn ensure_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))
}

fn lock_for(path: &Path) -> Result<File, String> {
    let lock_path = PathBuf::from(format!("{}.lock", path.to_string_lossy()));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("cannot open {}: {error}", lock_path.display()))?;
    file.lock_exclusive()
        .map_err(|error| format!("cannot lock {}: {error}", lock_path.display()))?;
    Ok(file)
}

pub fn make_message(
    session: &SessionPaths,
    id: String,
    kind: MessageKind,
    text: String,
    reference: Option<DocumentReference>,
    explicit_files: &[String],
) -> Result<ChatMessage, String> {
    if text.trim().is_empty() {
        return Err("message text cannot be empty".to_string());
    }

    let mut specs: Vec<(String, Option<u32>, Option<u32>)> = explicit_files
        .iter()
        .map(|spec| parse_file_spec(spec))
        .collect::<Result<_, _>>()?;
    for spec in inferred_file_specs(&text) {
        if !specs.contains(&spec) {
            specs.push(spec);
        }
    }
    let files = if specs.is_empty() {
        Vec::new()
    } else {
        let sha = head_sha(session)?;
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
    let read = matches!(kind, MessageKind::Ack);
    let decision_status =
        matches!(kind, MessageKind::Decision).then_some(DecisionStatus::Unreviewed);

    Ok(ChatMessage {
        id,
        kind,
        timestamp: now(),
        text,
        reference,
        files,
        read,
        decision_status,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_session() -> (SessionPaths, TestDirectory) {
        let directory = env::temp_dir().join(format!("scribe-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        (
            SessionPaths {
                notes: directory.join("call.md"),
                chat: directory.join("call.chat.jsonl"),
                events: directory.join("call.events.jsonl"),
                repo: env::current_dir().unwrap(),
            },
            TestDirectory(directory),
        )
    }

    #[test]
    fn parses_file_lines_and_ranges() {
        assert_eq!(
            parse_file_spec("app/Jobs/Sync.php:14-20").unwrap(),
            ("app/Jobs/Sync.php".to_string(), Some(14), Some(20))
        );
        assert_eq!(
            parse_file_spec("src/App.tsx").unwrap(),
            ("src/App.tsx".to_string(), None, None)
        );
        assert!(parse_file_spec("../outside.php:2").is_err());
        assert!(parse_file_spec("..\\outside.php:2").is_err());
        assert!(parse_file_spec("C:\\outside.php:2").is_err());
        assert!(parse_file_spec("app/Foo.php:20-14").is_err());
    }

    #[test]
    fn infers_only_backticked_paths() {
        assert_eq!(
            inferred_file_specs("Look at `app/Jobs/Sync.php:14` and `value` now."),
            vec![("app/Jobs/Sync.php".to_string(), Some(14), None)]
        );
    }

    #[test]
    fn serializes_event_contract_in_camel_case() {
        let event = DecisionEvent {
            timestamp: "2026-08-31T12:00:00.000Z",
            event_type: "decision_approved",
            decision_id: "retry-placement",
        };
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            serde_json::json!({
                "timestamp": "2026-08-31T12:00:00.000Z",
                "type": "decision_approved",
                "decisionId": "retry-placement"
            })
        );
    }

    #[test]
    fn reviews_a_decision_and_emits_one_durable_event() {
        let (session, _directory) = test_session();
        let decision = ChatMessage {
            id: "retry-placement".to_string(),
            kind: MessageKind::Decision,
            timestamp: now(),
            text: "Retries live in the job.".to_string(),
            reference: None,
            files: Vec::new(),
            read: false,
            decision_status: Some(DecisionStatus::Unreviewed),
        };
        append_message(&session, &decision).unwrap();

        review_decision(&session, &decision.id, DecisionStatus::Approved).unwrap();
        // Retrying after an uncertain IPC response is safe and does not duplicate
        // the event in the timeline.
        review_decision(&session, &decision.id, DecisionStatus::Approved).unwrap();

        let messages = load_messages(&session.chat).unwrap();
        assert_eq!(messages[0].decision_status, Some(DecisionStatus::Approved));
        let events = fs::read_to_string(&session.events).unwrap();
        assert_eq!(events.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&events).unwrap()["decisionId"],
            "retry-placement"
        );
    }

    #[test]
    fn stale_reference_events_are_deduplicated() {
        let (session, _directory) = test_session();
        let locator = DocumentReference {
            heading: vec!["Decisions".to_string(), "Retry placement".to_string()],
            snippet: "Retries live in the job".to_string(),
        };
        let message = ChatMessage {
            id: "message-1".to_string(),
            kind: MessageKind::Message,
            timestamp: now(),
            text: "This changed.".to_string(),
            reference: Some(locator.clone()),
            files: Vec::new(),
            read: false,
            decision_status: None,
        };
        append_message(&session, &message).unwrap();

        report_stale_reference(&session, &message.id, &locator).unwrap();
        report_stale_reference(&session, &message.id, &locator).unwrap();

        let events = fs::read_to_string(&session.events).unwrap();
        assert_eq!(events.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&events).unwrap()["messageId"],
            "message-1"
        );
    }
}
