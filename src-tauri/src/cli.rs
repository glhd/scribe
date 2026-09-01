use std::{path::Path, process::ExitCode};

use serde_json::json;
use uuid::Uuid;

use crate::{
    model::{DocumentReference, MessageKind},
    sources::{self, TupleClient},
    storage::{SessionRecord, Store},
};

const USAGE: &str = r#"Usage:
  scribe session attach --repo <path>
  scribe session current --json
  scribe session finish
  scribe tick [--wait] --cursor <name> [--timeout <duration>] [--limit <count>]
  scribe say <text> [--ref-heading <A>B>] [--ref-snippet <text>] [--file <path[:line[-end]]>]...
  scribe ack <text> [--file <path[:line[-end]]>]...
  scribe decision <text> --id <id> [--ref-heading <A>B>] [--ref-snippet <text>] [--file <path[:line[-end]]>]...
  scribe unlink <message-id>
  scribe read [<message-id>]

Scribe stores operational data in ~/.scribe/scribe.db. The active Tuple call ID
is the session ID. Run `session attach` from the repository being planned; it
returns the internal handoff path under ~/.scribe/sessions. No command writes a
sidecar or handoff into the repository."#;

pub fn is_cli_invocation(args: &[String]) -> bool {
    args.first()
        .is_some_and(|argument| !argument.starts_with("-psn_"))
}

pub fn run(args: Vec<String>) -> ExitCode {
    let result = Store::open().and_then(|store| {
        let tuple = TupleClient::discover();
        execute_with(&store, &tuple, args)
    });
    match result {
        Ok(message) => {
            if !message.is_empty() {
                println!("{message}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("scribe: {error}");
            ExitCode::from(1)
        }
    }
}

fn execute_with(store: &Store, tuple: &TupleClient, args: Vec<String>) -> Result<String, String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(USAGE.to_string());
    };
    if matches!(command, "help" | "--help" | "-h") {
        return Ok(USAGE.to_string());
    }
    match command {
        "session" => session_command(store, tuple, &args[1..]),
        "tick" => tick_command(store, tuple, &args[1..]),
        "say" | "ack" | "decision" => {
            let session = current_session(store, tuple)?;
            post(store, &session, command, &args[1..])
        }
        "unlink" => {
            let session = current_session(store, tuple)?;
            let id = exactly_one(&args[1..], "unlink requires one message ID")?;
            store.unlink(&session.id, id)?;
            Ok(format!("unlinked {id}"))
        }
        "read" => {
            let session = current_session(store, tuple)?;
            if args.len() > 2 {
                return Err("read accepts at most one message ID".to_string());
            }
            let id = args.get(1).map(String::as_str);
            store.mark_cli_read(&session.id, id)?;
            Ok(match id {
                Some(id) => format!("marked {id} read"),
                None => "marked all messages read".to_string(),
            })
        }
        _ => Err(format!("unknown command: {command}\n\n{USAGE}")),
    }
}

fn session_command(store: &Store, tuple: &TupleClient, args: &[String]) -> Result<String, String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err("session requires attach, current, or finish".to_string());
    };
    match command {
        "attach" => {
            if args.len() != 3 || args[1] != "--repo" {
                return Err("usage: scribe session attach --repo <path>".to_string());
            }
            let session = current_session(store, tuple)?;
            let session = store.attach_repo(&session.id, Path::new(&args[2]))?;
            sources::discover_chronicle(store, &session)?;
            session_json(store, &session)
        }
        "current" => {
            if args.get(1).map(String::as_str) != Some("--json") || args.len() != 2 {
                return Err("usage: scribe session current --json".to_string());
            }
            let session = store.current_session()?.ok_or_else(|| {
                "no active or finalizing Scribe session; join a Tuple call first".to_string()
            })?;
            session_json(store, &session)
        }
        "finish" => {
            if args.len() != 1 {
                return Err("usage: scribe session finish".to_string());
            }
            let _ = sources::collect_once(store, tuple, "1ms");
            let session = store
                .current_session()?
                .ok_or_else(|| "no active or finalizing Scribe session to finish".to_string())?;
            store.finish_session(&session.id)?;
            Ok(serde_json::to_string(&json!({
                "sessionId": session.id,
                "state": "complete"
            }))
            .map_err(json_error)?)
        }
        _ => Err(format!("unknown session command: {command}")),
    }
}

fn session_json(store: &Store, session: &SessionRecord) -> Result<String, String> {
    serde_json::to_string(&json!({
        "sessionId": session.id,
        "state": session.state,
        "notesPath": session.notes,
        "repoPath": session.repo,
        "sourceHealth": store.source_health(&session.id)?,
    }))
    .map_err(json_error)
}

fn tick_command(store: &Store, tuple: &TupleClient, args: &[String]) -> Result<String, String> {
    let mut wait = false;
    let mut cursor = None;
    let mut timeout = "30s".to_string();
    let mut limit = 200usize;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--wait" => {
                wait = true;
                index += 1;
            }
            "--cursor" | "--timeout" | "--limit" => {
                let flag = args[index].as_str();
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{flag} requires a value"))?;
                match flag {
                    "--cursor" => cursor = Some(value.clone()),
                    "--timeout" => {
                        validate_timeout(value)?;
                        timeout = value.clone();
                    }
                    "--limit" => {
                        limit = value
                            .parse()
                            .map_err(|_| "--limit must be an integer".to_string())?;
                    }
                    _ => unreachable!(),
                }
                index += 2;
            }
            flag => return Err(format!("unknown tick option: {flag}")),
        }
    }
    let cursor = cursor.ok_or_else(|| "tick requires --cursor <name>".to_string())?;
    let session = current_session(store, tuple)?;
    let source_timeout = if wait { timeout.as_str() } else { "1ms" };
    // Collection is process-safe: GUI and CLI readers serialize on the same
    // per-call lock and share Tuple's durable scribe-<call-id> cursor.
    let _ = sources::collect_once(store, tuple, source_timeout);
    let result = store.tick(&session.id, &cursor, limit)?;
    serde_json::to_string(&result).map_err(json_error)
}

fn validate_timeout(value: &str) -> Result<(), String> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .ok_or_else(|| "--timeout must include ms, s, or m".to_string())?;
    let (number, unit) = value.split_at(split);
    let number = number
        .parse::<u64>()
        .map_err(|_| "--timeout must start with an integer".to_string())?;
    let milliseconds = match unit {
        "ms" => number,
        "s" => number.saturating_mul(1000),
        "m" => number.saturating_mul(60_000),
        _ => return Err("--timeout must use ms, s, or m".to_string()),
    };
    if milliseconds == 0 || milliseconds > 300_000 {
        return Err("--timeout must be between 1ms and 5m".to_string());
    }
    Ok(())
}

fn current_session(store: &Store, tuple: &TupleClient) -> Result<SessionRecord, String> {
    sources::ensure_current_session(store, tuple)
}

fn exactly_one<'a>(args: &'a [String], error: &str) -> Result<&'a str, String> {
    if args.len() != 1 {
        return Err(error.to_string());
    }
    Ok(&args[0])
}

fn post(
    store: &Store,
    session: &SessionRecord,
    command: &str,
    args: &[String],
) -> Result<String, String> {
    let Some(text) = args.first() else {
        return Err(format!("{command} requires message text"));
    };
    if text.starts_with("--") {
        return Err(format!("{command} requires message text before options"));
    }

    let mut id = None;
    let mut heading = None;
    let mut snippet = None;
    let mut files = Vec::new();
    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{flag} requires a value"))?
            .clone();
        match flag {
            "--id" => id = Some(value),
            "--ref-heading" => heading = Some(value),
            "--ref-snippet" => snippet = Some(value),
            "--file" => files.push(value),
            _ => return Err(format!("unknown option for {command}: {flag}")),
        }
        index += 2;
    }

    let reference = match (heading, snippet) {
        (Some(heading), Some(snippet)) => {
            let heading = heading
                .split('>')
                .map(str::trim)
                .map(str::to_string)
                .collect::<Vec<_>>();
            if heading.is_empty() || heading.iter().any(String::is_empty) || snippet.is_empty() {
                return Err("document reference heading and snippet cannot be empty".to_string());
            }
            Some(DocumentReference { heading, snippet })
        }
        (None, None) => None,
        _ => return Err("--ref-heading and --ref-snippet must be supplied together".to_string()),
    };

    let (kind, message_id) = match command {
        "say" => {
            if id.is_some() {
                return Err("--id is only valid for decisions".to_string());
            }
            (MessageKind::Message, Uuid::new_v4().to_string())
        }
        "ack" => {
            if reference.is_some() {
                return Err("ack messages cannot carry a document reference".to_string());
            }
            if id.is_some() {
                return Err("--id is only valid for decisions".to_string());
            }
            (MessageKind::Ack, Uuid::new_v4().to_string())
        }
        "decision" => {
            let id = id.ok_or_else(|| "decision requires --id <id>".to_string())?;
            if id.trim().is_empty() || id.chars().any(char::is_whitespace) {
                return Err("decision ID must be non-empty and contain no whitespace".to_string());
            }
            (MessageKind::Decision, id)
        }
        _ => unreachable!(),
    };
    let message = store.make_message(
        session,
        message_id.clone(),
        kind,
        text.clone(),
        reference,
        &files,
    )?;
    store.append_message(&session.id, &message)?;
    Ok(format!("posted {command} {message_id}"))
}

fn json_error(error: serde_json::Error) -> String {
    format!("cannot encode command response: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::NormalizedEvent, storage};
    use std::{env, fs, path::PathBuf};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = env::temp_dir().join(format!("scribe-cli-test-{}", Uuid::new_v4()));
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
    fn recognizes_only_cli_subcommands() {
        assert!(is_cli_invocation(&["session".to_string()]));
        assert!(is_cli_invocation(&["tick".to_string()]));
        assert!(!is_cli_invocation(&["-psn_0_123".to_string()]));
        assert!(!is_cli_invocation(&[]));
    }

    #[test]
    fn session_and_tick_contracts_are_machine_readable() {
        let directory = TestDirectory::new();
        let store = Store::open_at(directory.0.join("data")).unwrap();
        let session = store.create_or_resume_session("tuple-call-1").unwrap();
        let tuple = TupleClient::new(PathBuf::from("/definitely/missing/tuple"));
        let attach = execute_with(
            &store,
            &tuple,
            vec![
                "session".to_string(),
                "attach".to_string(),
                "--repo".to_string(),
                env::current_dir().unwrap().to_string_lossy().into_owned(),
            ],
        )
        .unwrap();
        let attach: serde_json::Value = serde_json::from_str(&attach).unwrap();
        assert_eq!(attach["sessionId"], "tuple-call-1");
        assert!(attach["notesPath"].as_str().unwrap().ends_with("notes.md"));

        store
            .insert_source_events(
                &session.id,
                &[NormalizedEvent {
                    stable_id: "speech-1".to_string(),
                    source: "tuple".to_string(),
                    stream_id: None,
                    source_sequence: None,
                    occurred_at: "2026-09-01T12:00:00.000Z".to_string(),
                    observed_at: storage::now(),
                    kind: "speech".to_string(),
                    payload: json!({ "text": "hello" }),
                }],
            )
            .unwrap();
        let tick = execute_with(
            &store,
            &tuple,
            vec![
                "tick".to_string(),
                "--cursor".to_string(),
                "planning-scribe".to_string(),
                "--limit".to_string(),
                "10".to_string(),
            ],
        )
        .unwrap();
        let tick: serde_json::Value = serde_json::from_str(&tick).unwrap();
        assert_eq!(tick["events"][0]["stableId"], "speech-1");
        assert_eq!(tick["notesPath"], attach["notesPath"]);
    }

    #[test]
    fn validates_tick_batch_options() {
        assert!(validate_timeout("30s").is_ok());
        assert!(validate_timeout("500ms").is_ok());
        assert!(validate_timeout("0s").is_err());
        assert!(validate_timeout("6m").is_err());
        assert!(validate_timeout("soon").is_err());
    }
}
